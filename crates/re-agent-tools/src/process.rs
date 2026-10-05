//! `run_command`: runs a program (off by default; always asks).

use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use re_agent::{
    Action, Approval, Prepared, Remember, Risk, Tool, ToolContext, ToolOutput, ToolSpec,
};
use re_tasks::CancelToken;
use serde_json::{Value, json};

use crate::{cap, scope};

const TIMEOUT: Duration = Duration::from_secs(120);

pub struct RunCommand;

impl Tool for RunCommand {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description:
                "Run a program with arguments (no shell) in the conversation's folder. The user \
                          approves every run. Returns the exit code and output."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "array", "items": {"type": "string"}, "description": "Program and arguments."},
                    "cwd": {"type": "string"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            risk: Risk::Process,
        }
    }

    fn prepare(&self, ctx: &ToolContext, args: &Value) -> Result<Prepared, String> {
        let argv: Vec<String> = args["command"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        if argv.first().is_none_or(String::is_empty) {
            return Err("`command` must name a program".into());
        }
        let cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(raw) => scope::resolve(ctx.scope.as_deref(), raw)?.path,
            None => ctx.scope.clone().ok_or("no folder to run in; give `cwd`")?,
        };
        let shown = argv
            .iter()
            .map(|a| {
                if a.contains(' ') {
                    format!("\"{a}\"")
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        Ok(Prepared::Ask(
            Approval {
                title: format!("Run: {shown}"),
                detail: format!(
                    "In {}. The program runs with your full access to files and the network.",
                    cwd.display()
                ),
                diff: Vec::new(),
                risk: Risk::Process,
                remember: Remember::Once,
            },
            Action::Worker(Box::new(move |cancel| run(&argv, &cwd, TIMEOUT, cancel))),
        ))
    }
}

/// Runs `argv` in `cwd`; returns exit code, stdout and stderr (capped).
pub fn run(argv: &[String], cwd: &Path, timeout: Duration, cancel: &CancelToken) -> ToolOutput {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return ToolOutput::error(format!("cannot start `{}`: {e}", argv[0])),
    };
    let capture = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(pipe) = pipe {
                let _ = pipe.take(256 * 1024).read_to_end(&mut text);
            }
            String::from_utf8_lossy(&text).into_owned()
        })
    };
    let out = capture(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = capture(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if cancel.is_cancelled() || started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => return ToolOutput::error(e.to_string()),
        }
    };
    let (stdout, stderr) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    let head = match status {
        Some(s) => format!(
            "exit code: {}",
            s.code().map_or("none".into(), |c| c.to_string())
        ),
        None if cancel.is_cancelled() => "cancelled".into(),
        None => format!("stopped after {} s", timeout.as_secs()),
    };
    let text = format!("{head}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
    ToolOutput {
        content: cap(text, "output was long"),
        is_error: !status.is_some_and(|s| s.success()),
        effects: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{run as run_tool, temp_dir};

    #[test]
    fn always_asks_and_reports_exit_code_and_output() {
        let root = temp_dir("run");
        let c = ToolContext {
            scope: Some(root.clone()),
            ..Default::default()
        };
        let argv = if cfg!(windows) {
            json!(["cmd", "/C", "echo hello"])
        } else {
            json!(["sh", "-c", "echo hello"])
        };
        let prepared = RunCommand.prepare(&c, &json!({"command": argv})).unwrap();
        let Prepared::Ask(approval, _) = &prepared else {
            panic!("ask");
        };
        assert_eq!(approval.risk, Risk::Process);
        assert!(approval.detail.contains("full access"));
        let out = run_tool(prepared);
        assert!(out.content.starts_with("exit code: 0"), "{}", out.content);
        assert!(out.content.contains("hello"));
        let missing = run_tool(
            RunCommand
                .prepare(&c, &json!({"command": ["no-such-program-xyz"]}))
                .unwrap(),
        );
        assert!(missing.is_error);
        std::fs::remove_dir_all(root).unwrap();
    }
}
