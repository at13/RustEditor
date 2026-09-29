//! `plugin.json` manifests and discovery.
//!
//! ```json
//! {
//!   "api": 0,
//!   "id": "sort-lines",
//!   "name": "Sort Lines",
//!   "version": "1.0.0",
//!   "kind": "wasm",
//!   "module": "sort_lines.wasm",
//!   "capabilities": [],
//!   "commands": [
//!     { "id": "sort", "title": "Sort Lines", "input": "selection", "output": "replace" }
//!   ]
//! }
//! ```
//!
//! Process plugins use `"kind": "process"`, `"command": ["prog", "arg"]` and
//! optionally `"timeout_seconds"`.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::API_VERSION;

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_COMMANDS: usize = 64;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Sandboxed WebAssembly module (path inside the plugin folder).
    Wasm { module: String },
    /// A program run as the user. `argv[0]` is resolved inside the plugin
    /// folder if it exists there, otherwise on `PATH`.
    Process {
        argv: Vec<String>,
        timeout: Duration,
    },
}

impl Kind {
    pub fn is_sandboxed(&self) -> bool {
        matches!(self, Self::Wasm { .. })
    }
}

/// What a plugin may receive or do beyond the current selections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// The whole document text (including unsaved edits) and its path.
    Document,
    /// Sends data off the machine (process plugins only; user consents).
    Network,
}

impl Capability {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "document" => Some(Self::Document),
            "network" => Some(Self::Network),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Network => "network",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// The text of every selection.
    Selection,
    /// The whole document (requires [`Capability::Document`]).
    Document,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// Replace each input with the matching output.
    Replace,
    /// Show text in a read-only viewer.
    Show,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub id: String,
    pub title: String,
    pub input: Input,
    pub output: Output,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: Kind,
    pub capabilities: Vec<Capability>,
    pub commands: Vec<Command>,
}

/// A loaded plugin.
#[derive(Debug)]
pub struct Plugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
}

impl Plugin {
    /// Editor command id for one of this plugin's commands.
    pub fn command_id(&self, command: &Command) -> String {
        format!("plugin.{}.{}", self.manifest.id, command.id)
    }

    /// Identifies exactly what the user approved: a changed version, kind,
    /// entry point or capability set asks again.
    pub fn approval_key(&self) -> String {
        let m = &self.manifest;
        let entry = match &m.kind {
            Kind::Wasm { module } => format!("wasm:{module}"),
            Kind::Process { argv, .. } => format!("process:{}", argv.join("\u{1f}")),
        };
        let caps: Vec<_> = m.capabilities.iter().map(|c| c.name()).collect();
        format!(
            "plugin:{}@{}:{:016x}",
            m.id,
            m.version,
            fnv1a(&format!("{entry}|{}", caps.join(",")))
        )
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.manifest.capabilities.contains(&capability)
    }
}

/// Stable hash (unlike `DefaultHasher`, it never changes between builds).
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn is_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 40
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("`{key}` must be a string"))
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text).context("plugin.json is not valid JSON")?;
        let api = value
            .get("api")
            .and_then(Value::as_u64)
            .context("`api` must be a number")?;
        if api != API_VERSION {
            bail!("plugin API {api} is not supported (this editor supports {API_VERSION})");
        }
        let id = string(&value, "id")?;
        if !is_id(&id) {
            bail!("`id` must be 1-40 chars of a-z, 0-9, '-' or '_'");
        }
        let name = string(&value, "name")?;
        let version = string(&value, "version")?;
        let mut capabilities = Vec::new();
        for cap in value
            .get("capabilities")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let name = cap.as_str().context("capabilities must be strings")?;
            let cap = Capability::parse(name)
                .with_context(|| format!("unknown capability `{name}` (use document, network)"))?;
            if !capabilities.contains(&cap) {
                capabilities.push(cap);
            }
        }
        capabilities.sort();
        let kind = match string(&value, "kind")?.as_str() {
            "wasm" => {
                if capabilities.contains(&Capability::Network) {
                    bail!("WebAssembly plugins cannot use the network (they are sandboxed)");
                }
                Kind::Wasm {
                    module: string(&value, "module")?,
                }
            }
            "process" => {
                let argv: Vec<String> = value
                    .get("command")
                    .and_then(Value::as_array)
                    .context("`command` must be an array of strings")?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<_>>()
                    .context("`command` must be an array of strings")?;
                if argv.first().is_none_or(String::is_empty) {
                    bail!("`command` must name a program");
                }
                let timeout = match value.get("timeout_seconds") {
                    None => DEFAULT_TIMEOUT,
                    Some(v) => Duration::from_secs(
                        v.as_u64().context("`timeout_seconds` must be a number")?,
                    )
                    .min(MAX_TIMEOUT),
                };
                Kind::Process { argv, timeout }
            }
            other => bail!("unknown kind `{other}` (use wasm or process)"),
        };
        let raw_commands = value
            .get("commands")
            .and_then(Value::as_array)
            .context("`commands` must be an array")?;
        if raw_commands.is_empty() || raw_commands.len() > MAX_COMMANDS {
            bail!("a plugin must have 1-{MAX_COMMANDS} commands");
        }
        let mut commands: Vec<Command> = Vec::new();
        for raw in raw_commands {
            let id = string(raw, "id")?;
            if !is_id(&id) {
                bail!("command id `{id}` must be 1-40 chars of a-z, 0-9, '-' or '_'");
            }
            if commands.iter().any(|c| c.id == id) {
                bail!("duplicate command id `{id}`");
            }
            let input = match raw
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or("selection")
            {
                "selection" => Input::Selection,
                "document" => Input::Document,
                other => bail!("command `{id}`: unknown input `{other}`"),
            };
            if input == Input::Document && !capabilities.contains(&Capability::Document) {
                bail!(
                    "command `{id}` reads the document but the plugin lacks the `document` capability"
                );
            }
            let output = match raw
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or("replace")
            {
                "replace" => Output::Replace,
                "show" => Output::Show,
                other => bail!("command `{id}`: unknown output `{other}`"),
            };
            commands.push(Command {
                title: string(raw, "title")?,
                id,
                input,
                output,
            });
        }
        Ok(Self {
            id,
            name,
            version,
            kind,
            capabilities,
            commands,
        })
    }
}

/// Plugins found in one folder (one subfolder per plugin).
#[derive(Debug, Default)]
pub struct Registry {
    pub plugins: Vec<Arc<Plugin>>,
    /// Folders that could not be loaded, with the reason.
    pub errors: Vec<String>,
}

impl Registry {
    pub fn load(dir: &Path) -> Self {
        let mut registry = Self::default();
        let Ok(entries) = fs::read_dir(dir) else {
            return registry; // no plugins folder
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for plugin_dir in dirs {
            match load_one(&plugin_dir) {
                Ok(plugin) => {
                    if registry
                        .plugins
                        .iter()
                        .any(|p| p.manifest.id == plugin.manifest.id)
                    {
                        registry.errors.push(format!(
                            "{}: duplicate plugin id `{}`",
                            plugin_dir.display(),
                            plugin.manifest.id
                        ));
                    } else {
                        registry.plugins.push(Arc::new(plugin));
                    }
                }
                Err(error) => registry
                    .errors
                    .push(format!("{}: {error:#}", plugin_dir.display())),
            }
        }
        registry
    }

    /// Finds the plugin and command for an editor command id.
    pub fn find(&self, command_id: &str) -> Option<(Arc<Plugin>, Command)> {
        let rest = command_id.strip_prefix("plugin.")?;
        let (plugin_id, command) = rest.split_once('.')?;
        let plugin = self.plugins.iter().find(|p| p.manifest.id == plugin_id)?;
        let command = plugin.manifest.commands.iter().find(|c| c.id == command)?;
        Some((plugin.clone(), command.clone()))
    }
}

fn load_one(dir: &Path) -> Result<Plugin> {
    let path = dir.join("plugin.json");
    let meta = fs::metadata(&path).context("no plugin.json")?;
    if meta.len() > MAX_MANIFEST_BYTES {
        bail!("plugin.json is too large");
    }
    let manifest = Manifest::parse(&fs::read_to_string(&path)?)?;
    if let Kind::Wasm { module } = &manifest.kind {
        let module = crate::inside(dir, module)?;
        if !module.is_file() {
            bail!("module {} not found", module.display());
        }
    }
    Ok(Plugin {
        manifest,
        dir: dir.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WASM: &str = r#"{
        "api": 0, "id": "demo", "name": "Demo", "version": "1.0.0",
        "kind": "wasm", "module": "demo.wasm",
        "commands": [
            {"id": "upper", "title": "Upper"},
            {"id": "count", "title": "Count", "input": "selection", "output": "show"}
        ]
    }"#;

    #[test]
    fn parses_wasm_and_process_manifests() {
        let m = Manifest::parse(WASM).unwrap();
        assert_eq!(
            m.kind,
            Kind::Wasm {
                module: "demo.wasm".into()
            }
        );
        assert_eq!(m.commands[0].input, Input::Selection);
        assert_eq!(m.commands[0].output, Output::Replace);
        assert_eq!(m.commands[1].output, Output::Show);
        let p = Manifest::parse(
            r#"{"api":0,"id":"ai","name":"AI","version":"2","kind":"process",
                "command":["python","ask.py"],"timeout_seconds":5,
                "capabilities":["network","document","network"],
                "commands":[{"id":"ask","title":"Ask","input":"document","output":"show"}]}"#,
        )
        .unwrap();
        assert_eq!(
            p.kind,
            Kind::Process {
                argv: vec!["python".into(), "ask.py".into()],
                timeout: Duration::from_secs(5)
            }
        );
        assert_eq!(
            p.capabilities,
            vec![Capability::Document, Capability::Network]
        );
    }

    #[test]
    fn rejects_bad_manifests_with_reasons() {
        let cases = [
            (r#"{"api":1}"#, "not supported"),
            (r#"{"api":0,"id":"Bad Id"}"#, "`id`"),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"wasm","module":"x.wasm","capabilities":["network"],"commands":[{"id":"a","title":"A"}]}"#,
                "cannot use the network",
            ),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"wasm","module":"x.wasm","capabilities":["files"],"commands":[{"id":"a","title":"A"}]}"#,
                "unknown capability",
            ),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"wasm","module":"x.wasm","commands":[{"id":"a","title":"A","input":"document"}]}"#,
                "lacks the `document` capability",
            ),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"native","commands":[]}"#,
                "unknown kind",
            ),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"wasm","module":"m","commands":[]}"#,
                "1-64 commands",
            ),
            (
                r#"{"api":0,"id":"x","name":"X","version":"1","kind":"process","command":[],"commands":[{"id":"a","title":"A"}]}"#,
                "name a program",
            ),
        ];
        for (text, needle) in cases {
            let error = format!("{:#}", Manifest::parse(text).unwrap_err());
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn approval_key_changes_when_trust_relevant_fields_change() {
        let plugin = |text: &str| Plugin {
            manifest: Manifest::parse(text).unwrap(),
            dir: PathBuf::new(),
        };
        let base = plugin(WASM).approval_key();
        assert_eq!(base, plugin(WASM).approval_key());
        assert_ne!(base, plugin(&WASM.replace("1.0.0", "1.0.1")).approval_key());
        assert_ne!(
            base,
            plugin(&WASM.replace("demo.wasm", "other.wasm")).approval_key()
        );
        assert!(base.starts_with("plugin:demo@1.0.0:"));
    }

    #[test]
    fn registry_loads_folders_and_reports_bad_ones() {
        let root = std::env::temp_dir().join(format!("re-plugin-registry-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("good")).unwrap();
        fs::write(root.join("good/plugin.json"), WASM).unwrap();
        fs::write(root.join("good/demo.wasm"), b"\0asm").unwrap();
        fs::create_dir_all(root.join("missing-module")).unwrap();
        fs::write(
            root.join("missing-module/plugin.json"),
            WASM.replace("\"demo\"", "\"other\""),
        )
        .unwrap();
        fs::create_dir_all(root.join("escape")).unwrap();
        fs::write(
            root.join("escape/plugin.json"),
            WASM.replace("\"demo\"", "\"esc\"")
                .replace("demo.wasm", "../good/demo.wasm"),
        )
        .unwrap();
        let registry = Registry::load(&root);
        assert_eq!(registry.plugins.len(), 1, "{:?}", registry.errors);
        assert_eq!(registry.errors.len(), 2, "{:?}", registry.errors);
        assert!(
            registry
                .errors
                .iter()
                .any(|e| e.contains("inside the plugin folder"))
        );
        let (plugin, command) = registry.find("plugin.demo.count").unwrap();
        assert_eq!(plugin.command_id(&command), "plugin.demo.count");
        assert!(registry.find("plugin.demo.nope").is_none());
        assert!(registry.find("file.save").is_none());
        assert!(Registry::load(&root.join("absent")).plugins.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
}
