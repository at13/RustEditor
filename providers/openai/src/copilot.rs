//! Transparent stdio bridge to the official Copilot SDK provider. It never
//! prompts a CLI for JSON or parses assistant prose as tool calls.
use re_agent::protocol::ModelRequest;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

pub fn default_script() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("copilot-sdk/dist/main.js")))
}

pub fn run(
    request: Option<&ModelRequest>,
    script: Option<&Path>,
    check_auth: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    let script = script.map(Path::to_path_buf).or_else(default_script)
        .filter(|p| p.is_file()).ok_or("Copilot SDK provider is missing. Install/build providers/copilot alongside the editor.")?;
    let mut command = Command::new("node");
    command
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if check_auth {
        command.arg("--check-auth");
    }
    re_platform::child_lifetime::configure(&mut command);
    let mut child = command.spawn().map_err(|_| {
        "Cannot launch Node.js for the Copilot SDK. Install Node.js 20.19+ or 22.12+.".to_owned()
    })?;
    let _lifetime = match re_platform::child_lifetime::Guard::attach(&child) {
        Ok(guard) => guard,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Cannot safely manage the Copilot SDK process".into());
        }
    };
    let mut stdout = child.stdout.take().ok_or("no SDK output pipe")?;
    let stderr = child.stderr.take().ok_or("no SDK error pipe")?;
    // Drain diagnostics without exposing document text, credentials or runtime
    // internals in the editor's persisted conversation.
    let errors = thread::spawn(move || {
        let _ = std::io::copy(&mut stderr.take(1024 * 1024), &mut std::io::sink());
    });
    let mut stdin = child.stdin.take().ok_or("no SDK input pipe")?;
    if let Some(request) = request
        && serde_json::to_writer(&mut stdin, request).is_err()
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Cannot send request to Copilot SDK".into());
    }
    drop(stdin);
    let mut buffer = [0u8; 8192];
    loop {
        let count = stdout
            .read(&mut buffer)
            .map_err(|_| "Cannot read SDK output")?;
        if count == 0 {
            break;
        }
        if out
            .write_all(&buffer[..count])
            .and_then(|()| out.flush())
            .is_err()
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Editor disconnected".into());
        }
    }
    let status = child.wait().map_err(|_| "Cannot wait for Copilot SDK")?;
    let _ = errors.join();
    if !status.success() {
        return Err("Copilot SDK provider failed. Check Node.js and the SDK installation in Agent > Providers.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires Node.js; no credentials or model calls"]
    fn sdk_stdio_bridge_preserves_requests_events_and_auth_metadata() {
        let dir = std::env::temp_dir().join(format!(
            "re-sdk-bridge-{}",
            re_agent::conversation::new_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fixture.mjs");
        std::fs::write(
            &script,
            r#"
if (process.argv.includes('--check-auth')) {
    process.stdout.write(JSON.stringify({is_authenticated:true})+'\n');
} else {
    const chunks=[]; for await (const c of process.stdin) chunks.push(c);
    const request=JSON.parse(Buffer.concat(chunks).toString('utf8'));
    process.stdout.write(JSON.stringify({type:'text',delta:request.model+' 😀'})+'\n');
    process.stdout.write(JSON.stringify({type:'done',reason:'stop'})+'\n');
}
"#,
        )
        .unwrap();
        let mut request = ModelRequest::new("sys", &[], &[]);
        request.model = "test-model".into();
        let mut out = Vec::new();
        run(Some(&request), Some(&script), false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let events: Vec<_> = text
            .lines()
            .filter_map(re_agent::protocol::parse_event)
            .collect();
        assert!(
            matches!(&events[0], re_agent::protocol::Event::Text {delta} if delta == "test-model 😀")
        );
        let mut out = Vec::new();
        run(None, Some(&script), true, &mut out).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out).unwrap()["is_authenticated"],
            true
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_sdk_install_fails_without_launching_a_cli_or_model() {
        let mut out = Vec::new();
        assert!(
            run(
                None,
                Some(Path::new("missing-copilot-sdk/main.js")),
                true,
                &mut out
            )
            .unwrap_err()
            .contains("SDK provider is missing")
        );
        assert!(out.is_empty());
    }
}
