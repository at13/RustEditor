//! Model providers: separate programs speaking [`crate::protocol`], and a
//! scripted provider for tests and demos.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

use serde::{Deserialize, Serialize};

use crate::protocol::{Event, ModelRequest, parse_event};

/// Receives events (from a provider thread).
pub type Sink = Box<dyn FnMut(Event) + Send>;

/// A running request; dropping or cancelling it stops the provider.
pub struct ProviderRun {
    child: Arc<Mutex<Option<Child>>>,
}

impl ProviderRun {
    pub fn cancel(&self) {
        if let Some(mut child) = self.child.lock().expect("child").take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ProviderRun {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub trait Provider: Send + Sync {
    /// Starts one model turn; events arrive on `sink` from another thread,
    /// ending with `Done` or `Error`.
    fn start(&self, request: ModelRequest, sink: Sink) -> ProviderRun;
}

/// `<config>/providers/<id>/provider.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    /// Program and arguments; the program is looked up in the provider's
    /// folder first, then on PATH.
    pub command: Vec<String>,
    pub model: String,
    /// Where data goes, shown in the consent question (e.g. a URL, or
    /// "this computer" for a local model).
    pub destination: String,
    #[serde(skip)]
    pub dir: PathBuf,
}

impl ProviderConfig {
    /// Approval key for sending text to this provider: changes when the
    /// program, model or destination changes.
    pub fn egress_key(&self) -> String {
        let text = format!(
            "{}|{}|{}",
            self.command.join("\u{1f}"),
            self.model,
            self.destination
        );
        let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
        });
        format!("provider:{}:{hash:016x}", self.id)
    }
}

/// Loads every `*/provider.json` under `dir`; returns configs and problems.
pub fn load_configs(dir: &Path) -> (Vec<ProviderConfig>, Vec<String>) {
    let mut configs = Vec::new();
    let mut errors = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (configs, errors);
    };
    let mut folders: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    folders.sort();
    for folder in folders {
        let file = folder.join("provider.json");
        let result = std::fs::read_to_string(&file)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                serde_json::from_str::<ProviderConfig>(&text).map_err(|e| e.to_string())
            });
        match result {
            Ok(mut config) if !config.command.is_empty() && !config.id.is_empty() => {
                config.dir = folder;
                configs.push(config);
            }
            Ok(_) => errors.push(format!(
                "{}: `id` and `command` are required",
                file.display()
            )),
            Err(error) => errors.push(format!("{}: {error}", file.display())),
        }
    }
    (configs, errors)
}

/// Runs a provider program per request.
pub struct ProcessProvider {
    pub config: ProviderConfig,
}

impl Provider for ProcessProvider {
    fn start(&self, mut request: ModelRequest, mut sink: Sink) -> ProviderRun {
        request.model = self.config.model.clone();
        let child_slot = Arc::new(Mutex::new(None));
        let run = ProviderRun {
            child: child_slot.clone(),
        };
        let (name, args) = self
            .config
            .command
            .split_first()
            .expect("non-empty command");
        let local = self.config.dir.join(name);
        let program = if local.is_file() {
            local
        } else {
            PathBuf::from(name)
        };
        let mut command = Command::new(&program);
        command
            .args(args)
            .current_dir(if self.config.dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                &self.config.dir
            })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = format!("cannot start provider `{}`: {error}", program.display());
                thread::spawn(move || sink(Event::Error { message }));
                return run;
            }
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *child_slot.lock().expect("child") = Some(child);
        let body = serde_json::to_vec(&request).unwrap_or_default();
        thread::spawn(move || {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(&body);
            }
        });
        let errors = thread::spawn(move || {
            let mut text = String::new();
            if let Some(stderr) = stderr {
                let _ = stderr.take(16 * 1024).read_to_string(&mut text);
            }
            text
        });
        thread::spawn(move || {
            let mut finished = false;
            if let Some(stdout) = stdout {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if let Some(event) = parse_event(&line) {
                        finished |= matches!(event, Event::Done { .. } | Event::Error { .. });
                        sink(event);
                        if finished {
                            break;
                        }
                    }
                }
            }
            let status = child_slot
                .lock()
                .expect("child")
                .take()
                .map(|mut c| c.wait());
            if !finished {
                let stderr = errors.join().unwrap_or_default();
                let message = match status {
                    // Killed by cancel: the session already stopped.
                    None => return,
                    Some(Ok(status)) => format!(
                        "the provider exited ({status}) without finishing. {}",
                        stderr.trim()
                    ),
                    Some(Err(error)) => format!("the provider failed: {error}"),
                };
                sink(Event::Error { message });
            }
        });
        run
    }
}

/// Replays canned turns (tests, demos). Records the requests it received.
#[derive(Default)]
pub struct ScriptedProvider {
    turns: Mutex<std::collections::VecDeque<Vec<Event>>>,
    pub requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedProvider {
    pub fn new(turns: Vec<Vec<Event>>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl Provider for ScriptedProvider {
    fn start(&self, request: ModelRequest, mut sink: Sink) -> ProviderRun {
        self.requests.lock().expect("requests").push(request);
        let turn = self
            .turns
            .lock()
            .expect("turns")
            .pop_front()
            .unwrap_or_else(|| {
                vec![Event::Error {
                    message: "scripted provider has no more turns".into(),
                }]
            });
        thread::spawn(move || {
            for event in turn {
                sink(event);
            }
        });
        ProviderRun {
            child: Arc::new(Mutex::new(None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    fn collect(provider: &dyn Provider) -> Vec<Event> {
        let (tx, rx) = mpsc::channel();
        let _run = provider.start(
            ModelRequest::new("s", &[], &[]),
            Box::new(move |e| {
                let _ = tx.send(e);
            }),
        );
        let mut events = Vec::new();
        while let Ok(event) = rx.recv_timeout(Duration::from_secs(20)) {
            let end = matches!(event, Event::Done { .. } | Event::Error { .. });
            events.push(event);
            if end {
                break;
            }
        }
        events
    }

    fn script_dir(name: &str, lines: &str) -> (PathBuf, Vec<String>) {
        let dir =
            std::env::temp_dir().join(format!("re-agent-provider-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if cfg!(windows) {
            let body: String = lines.lines().map(|l| format!("@echo {l}\r\n")).collect();
            std::fs::write(dir.join("p.cmd"), format!("@more >NUL\r\n{body}")).unwrap();
            (dir, vec!["cmd".into(), "/C".into(), "p.cmd".into()])
        } else {
            let body: String = lines
                .lines()
                .map(|l| format!("printf '%s\\n' '{l}'\n"))
                .collect();
            std::fs::write(dir.join("p.sh"), format!("cat >/dev/null\n{body}")).unwrap();
            (dir, vec!["sh".into(), "p.sh".into()])
        }
    }

    #[test]
    fn process_provider_streams_events_from_a_program() {
        let (dir, command) = script_dir(
            "ok",
            "{\"type\":\"text\",\"delta\":\"Hi\"}\n{\"type\":\"done\",\"reason\":\"stop\"}",
        );
        let provider = ProcessProvider {
            config: ProviderConfig {
                id: "t".into(),
                name: "T".into(),
                command,
                model: "m".into(),
                destination: "this computer".into(),
                dir: dir.clone(),
            },
        };
        let events = collect(&provider);
        assert_eq!(events[0], Event::Text { delta: "Hi".into() });
        assert!(matches!(events.last(), Some(Event::Done { .. })));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_or_silent_providers_report_errors() {
        let provider = ProcessProvider {
            config: ProviderConfig {
                id: "x".into(),
                name: "X".into(),
                command: vec!["definitely-not-a-provider-xyz".into()],
                model: "m".into(),
                destination: "d".into(),
                dir: PathBuf::new(),
            },
        };
        let events = collect(&provider);
        assert!(matches!(&events[0], Event::Error { message } if message.contains("cannot start")));
        let (dir, command) = script_dir("silent", "{\"type\":\"text\",\"delta\":\"partial\"}");
        let provider = ProcessProvider {
            config: ProviderConfig {
                id: "s".into(),
                name: "S".into(),
                command,
                model: "m".into(),
                destination: "d".into(),
                dir: dir.clone(),
            },
        };
        let events = collect(&provider);
        assert!(
            matches!(events.last(), Some(Event::Error { message }) if message.contains("without finishing")),
            "{events:?}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn configs_load_from_folders_and_egress_keys_track_destination() {
        let root = std::env::temp_dir().join(format!("re-agent-configs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("local")).unwrap();
        std::fs::write(
            root.join("local/provider.json"),
            r#"{"id":"local","name":"Local","command":["ollama-bridge"],"model":"llama3","destination":"this computer"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("bad")).unwrap();
        std::fs::write(root.join("bad/provider.json"), "{").unwrap();
        let (configs, errors) = load_configs(&root);
        assert_eq!(configs.len(), 1);
        assert_eq!(errors.len(), 1);
        let mut other = configs[0].clone();
        assert_eq!(other.egress_key(), configs[0].egress_key());
        other.destination = "https://example.com".into();
        assert_ne!(other.egress_key(), configs[0].egress_key());
        std::fs::remove_dir_all(root).unwrap();
    }
}
