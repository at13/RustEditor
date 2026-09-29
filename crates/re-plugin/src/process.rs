//! Process plugins: one run of the plugin's program per command. It reads a
//! request (JSON) on stdin and writes a response (JSON) on stdout. It runs
//! as the user (full trust); the host limits only time and output size.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use crate::MAX_MESSAGE_BYTES;

const MAX_STDERR_BYTES: u64 = 16 * 1024;

/// `argv[0]` inside the plugin folder if it exists there, else as given
/// (found on `PATH`).
fn program(dir: &Path, name: &str) -> PathBuf {
    match crate::inside(dir, name) {
        Ok(local) if local.is_file() => local,
        _ => PathBuf::from(name),
    }
}

pub fn run(dir: &Path, argv: &[String], timeout: Duration, request: &[u8]) -> Result<Vec<u8>> {
    let (name, args) = argv.split_first().context("empty command")?;
    let mut command = Command::new(program(dir, name));
    command
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start `{name}`"))?;
    // Write stdin and read stdout/stderr on threads so a plugin that writes
    // before reading cannot deadlock the pipes.
    let mut stdin = child.stdin.take().context("no stdin")?;
    let request = request.to_vec();
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(&request);
    });
    let stdout = child.stdout.take().context("no stdout")?;
    let reader = thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout
            .take(MAX_MESSAGE_BYTES as u64 + 1)
            .read_to_end(&mut out);
        out
    });
    let stderr = child.stderr.take().context("no stderr")?;
    let errors = thread::spawn(move || {
        let mut err = Vec::new();
        let _ = stderr.take(MAX_STDERR_BYTES).read_to_end(&mut err);
        err
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the plugin did not finish within {} s and was stopped",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(15));
    };
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    let err = String::from_utf8_lossy(&errors.join().unwrap_or_default()).into_owned();
    if out.len() > MAX_MESSAGE_BYTES {
        bail!(
            "the plugin's output is larger than {} MiB",
            MAX_MESSAGE_BYTES >> 20
        );
    }
    if !status.success() {
        let detail = err.trim();
        bail!(
            "the plugin exited with {status}{}{}",
            if detail.is_empty() { "" } else { ": " },
            detail.chars().take(500).collect::<String>()
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Vec<String> {
        if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), script.into()]
        } else {
            vec!["sh".into(), "-c".into(), script.into()]
        }
    }

    /// A plugin folder with a script that answers with `response`.
    fn script_plugin(name: &str, response: &str) -> (PathBuf, Vec<String>) {
        let dir =
            std::env::temp_dir().join(format!("re-plugin-proc-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let argv = if cfg!(windows) {
            // `more` drains stdin; the response is echoed verbatim.
            std::fs::write(
                dir.join("plugin.cmd"),
                format!(
                    "@more >NUL
@echo {response}
"
                ),
            )
            .unwrap();
            vec!["cmd".into(), "/C".into(), "plugin.cmd".into()]
        } else {
            std::fs::write(
                dir.join("plugin.sh"),
                format!(
                    "cat >/dev/null
printf '%s\n' '{response}'
"
                ),
            )
            .unwrap();
            vec!["sh".into(), "plugin.sh".into()]
        };
        (dir, argv)
    }

    #[test]
    fn runs_a_program_in_the_plugin_folder_and_reads_its_stdout() {
        let (dir, argv) = script_plugin("ok", r#"{"show":"hi"}"#);
        let request = "x".repeat(200_000); // larger than a pipe buffer
        let out = run(&dir, &argv, Duration::from_secs(30), request.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(r#"{"show":"hi"}"#), "{out:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failures_report_the_exit_status_and_stderr() {
        let script = if cfg!(windows) {
            "echo broken 1>&2 & exit 3"
        } else {
            "echo broken >&2; exit 3"
        };
        let error = run(
            &std::env::temp_dir(),
            &shell(script),
            Duration::from_secs(20),
            b"{}",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("broken"), "{error}");
        assert!(
            run(
                &std::env::temp_dir(),
                &["definitely-not-a-program-xyz".into()],
                Duration::from_secs(5),
                b"{}"
            )
            .unwrap_err()
            .to_string()
            .contains("cannot start")
        );
    }

    #[test]
    fn slow_plugins_are_stopped_at_the_timeout() {
        let script = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >NUL"
        } else {
            "sleep 30"
        };
        let started = Instant::now();
        let error = run(
            &std::env::temp_dir(),
            &shell(script),
            Duration::from_secs(1),
            b"{}",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("did not finish"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(15));
    }
}
