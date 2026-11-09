//! API keys in the OS credential store, never in provider.json.
use std::io;

fn target(id: &str) -> io::Result<String> {
    if !matches!(id, "openai" | "anthropic") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unknown credential id",
        ));
    }
    Ok(format!("RustEditor/provider/{id}"))
}

pub fn set(id: &str, secret: &str) -> io::Result<()> {
    if secret.is_empty() || !secret.bytes().all(|b| b.is_ascii_graphic()) || secret.len() > 2048 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid API key",
        ));
    }
    backend::set(&target(id)?, secret)
}
pub fn get(id: &str) -> io::Result<Option<String>> {
    backend::get(&target(id)?)
}
pub fn delete(id: &str) -> io::Result<()> {
    backend::delete(&target(id)?)
}

#[cfg(windows)]
mod backend {
    use super::*;
    use windows_sys::Win32::Security::Credentials::*;
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    pub fn set(target: &str, secret: &str) -> io::Result<()> {
        let mut target = wide(target);
        let mut user = wide("RustEditor");
        let mut bytes = secret.as_bytes().to_vec();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: user.as_mut_ptr(),
            ..Default::default()
        };
        // SAFETY: all pointers stay live for the synchronous call.
        let ok = unsafe { CredWriteW(&credential, 0) };
        bytes.fill(0);
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub fn get(target: &str) -> io::Result<Option<String>> {
        let target = wide(target);
        let mut credential = std::ptr::null_mut();
        // SAFETY: out pointer is valid; CredFree releases its allocation.
        if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(1168) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        let result = unsafe {
            let c = &*credential;
            let bytes = if c.CredentialBlobSize == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize)
            };
            String::from_utf8(bytes.to_vec()).map_err(io::Error::other)
        };
        unsafe { CredFree(credential.cast()) };
        result.map(Some)
    }
    pub fn delete(target: &str) -> io::Result<()> {
        let target = wide(target);
        // SAFETY: target is a valid null-terminated string.
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(1168) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

// Secret Service via secret-tool: key goes over stdin, not argv.
#[cfg(not(windows))]
mod backend {
    use super::*;
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    fn unavailable() -> io::Error {
        io::Error::other(
            "OS credential service unavailable; no plaintext fallback. Set the provider's API-key environment variable instead.",
        )
    }
    pub fn set(target: &str, secret: &str) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            // security's interactive password prompt avoids putting the key in argv.
            let mut child = Command::new("security")
                .args(["-i"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            // Only validated ASCII keys; escape quotes and backslashes for security's parser.
            let escaped = secret.replace('\\', "\\\\").replace('"', "\\\"");
            writeln!(
                child.stdin.take().unwrap(),
                "add-generic-password -U -a RustEditor -s {target} -w \"{escaped}\""
            )?;
            return if child.wait()?.success() {
                Ok(())
            } else {
                Err(unavailable())
            };
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut child = Command::new("secret-tool")
                .args(["store", "--label=RustEditor API key", "service", target])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| unavailable())?;
            child.stdin.take().unwrap().write_all(secret.as_bytes())?;
            if child.wait()?.success() {
                Ok(())
            } else {
                Err(unavailable())
            }
        }
    }
    pub fn get(target: &str) -> io::Result<Option<String>> {
        #[cfg(target_os = "macos")]
        let output = Command::new("security")
            .args([
                "find-generic-password",
                "-a",
                "RustEditor",
                "-s",
                target,
                "-w",
            ])
            .output()?;
        #[cfg(not(target_os = "macos"))]
        let output = Command::new("secret-tool")
            .args(["lookup", "service", target])
            .output()
            .map_err(|_| unavailable())?;
        if !output.status.success() {
            return Ok(None);
        }
        let text = String::from_utf8(output.stdout).map_err(io::Error::other)?;
        Ok(Some(text.trim_end_matches(['\r', '\n']).to_owned()))
    }
    pub fn delete(target: &str) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        let status = Command::new("security")
            .args(["delete-generic-password", "-a", "RustEditor", "-s", target])
            .status()?;
        #[cfg(not(target_os = "macos"))]
        let status = Command::new("secret-tool")
            .args(["clear", "service", target])
            .status()
            .map_err(|_| unavailable())?;
        if status.success() {
            Ok(())
        } else {
            Err(unavailable())
        }
    }
}

/// Opens the CLI's own OAuth UI. No token passes through the editor.
pub fn copilot_login() -> io::Result<std::process::Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("copilot")
            .args(["login", "--web-flow"])
            .creation_flags(0x0000_0010)
            .spawn() // CREATE_NEW_CONSOLE
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("osascript")
            .args([
                "-e",
                "tell application \"Terminal\" to do script \"copilot login\"",
            ])
            .spawn()
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        std::process::Command::new("x-terminal-emulator")
            .args(["-e", "copilot", "login"])
            .spawn()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_ids_and_invalid_keys_never_reach_the_os_store() {
        assert!(set("untrusted", "secret").is_err());
        assert!(set("openai", "").is_err());
        assert!(set("anthropic", "secret\n").is_err());
        assert!(set("openai", "secret\0").is_err());
        assert!(get("../../x").is_err());
    }
}
