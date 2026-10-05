//! Per-user directories.
//!
//! | | Windows | macOS | Linux / other Unix |
//! |-|-|-|-|
//! | config | `%APPDATA%\RustEditor` | `~/Library/Application Support/RustEditor` | `$XDG_CONFIG_HOME/rusteditor` or `~/.config/rusteditor` |
//! | runtime | `%LOCALAPPDATA%\RustEditor` | `$TMPDIR/rusteditor` | `$XDG_RUNTIME_DIR/rusteditor` or `/tmp/rusteditor-<uid>` |
//!
//! The runtime directory holds the single-instance lock and endpoint. It is
//! created private to the user (0700 on Unix; on Windows the per-user
//! profile's ACL applies) and rejected if another user could write to it.

use std::{
    io,
    path::{Path, PathBuf},
};

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Where user settings (keymap) live.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        env_dir("APPDATA").map(|dir| dir.join("RustEditor"))
    } else if cfg!(target_os = "macos") {
        env_dir("HOME").map(|home| home.join("Library/Application Support/RustEditor"))
    } else {
        env_dir("XDG_CONFIG_HOME")
            .or_else(|| env_dir("HOME").map(|home| home.join(".config")))
            .map(|dir| dir.join("rusteditor"))
    }
}

pub fn keymap_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("keymap.toml"))
}

/// Where per-user application data lives (agent conversations, staging).
/// Not synced settings: `%LOCALAPPDATA%\RustEditor` on Windows,
/// `~/Library/Application Support/RustEditor` on macOS,
/// `$XDG_DATA_HOME/rusteditor` or `~/.local/share/rusteditor` elsewhere.
pub fn data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        env_dir("LOCALAPPDATA").map(|dir| dir.join("RustEditor"))
    } else if cfg!(target_os = "macos") {
        env_dir("HOME").map(|home| home.join("Library/Application Support/RustEditor"))
    } else {
        env_dir("XDG_DATA_HOME")
            .or_else(|| env_dir("HOME").map(|home| home.join(".local/share")))
            .map(|dir| dir.join("rusteditor"))
    }
}

/// Private folder for AI agent data (conversations may contain file text).
pub fn agents_dir() -> io::Result<PathBuf> {
    let dir = data_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no per-user data directory"))?
        .join("agents");
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Converts a `file://` URL (as delivered by macOS "open with") to a path.
pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None; // remote host
    }
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1), bytes.get(i + 2)) {
            (b'%', Some(&h), Some(&l)) if hex(h).is_some() && hex(l).is_some() => {
                decoded.push((hex(h)? * 16 + hex(l)?) as u8);
                i += 3;
            }
            (byte, _, _) => {
                decoded.push(byte);
                i += 1;
            }
        }
    }
    let path = String::from_utf8(decoded).ok()?;
    // `file:///C:/x` on Windows.
    let path = match path.as_bytes() {
        [b'/', drive, b':', ..] if cfg!(windows) && drive.is_ascii_alphabetic() => &path[1..],
        _ => &path,
    };
    Some(PathBuf::from(path))
}

/// Per-user directory for the instance lock and endpoint; created private.
pub fn runtime_dir() -> io::Result<PathBuf> {
    let dir = default_runtime_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no per-user runtime directory"))?;
    ensure_private_dir(&dir)?;
    Ok(dir)
}

fn default_runtime_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        env_dir("LOCALAPPDATA").map(|dir| dir.join("RustEditor"))
    } else if cfg!(target_os = "macos") {
        // $TMPDIR is per-user on macOS.
        Some(std::env::temp_dir().join("rusteditor"))
    } else {
        env_dir("XDG_RUNTIME_DIR")
            .map(|dir| dir.join("rusteditor"))
            .or_else(unix_fallback_runtime_dir)
    }
}

#[cfg(unix)]
fn unix_fallback_runtime_dir() -> Option<PathBuf> {
    Some(std::env::temp_dir().join(format!("rusteditor-{}", unix::uid())))
}

#[cfg(not(unix))]
fn unix_fallback_runtime_dir() -> Option<PathBuf> {
    None
}

/// Creates `dir` if needed and checks that only the current user can use it.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        unix::ensure_private_dir(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

#[cfg(unix)]
pub(crate) mod unix {
    use std::{
        fs::{self, DirBuilder},
        io,
        os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        path::Path,
    };

    unsafe extern "C" {
        fn getuid() -> u32;
    }

    pub fn uid() -> u32 {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { getuid() }
    }

    pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
        match DirBuilder::new().recursive(true).mode(0o700).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        // Do not follow a symlink planted by someone else.
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir() || meta.uid() != uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is not a directory owned by you", dir.display()),
            ));
        }
        if meta.mode() & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_are_per_user_and_named_for_the_app() {
        let config = config_dir().expect("config dir");
        let name = config.file_name().unwrap().to_string_lossy().to_lowercase();
        assert_eq!(name, "rusteditor");
        assert_eq!(keymap_path().unwrap(), config.join("keymap.toml"));
        let data = data_dir().expect("data dir");
        assert!(data.to_string_lossy().to_lowercase().contains("rusteditor"));
        let runtime = default_runtime_dir().expect("runtime dir");
        assert!(
            runtime
                .to_string_lossy()
                .to_lowercase()
                .contains("rusteditor")
        );
    }

    #[test]
    fn file_urls_become_paths() {
        assert_eq!(
            file_url_to_path("file:///Users/me/My%20Notes/%F0%9F%98%80.txt"),
            Some(PathBuf::from("/Users/me/My Notes/😀.txt"))
        );
        assert_eq!(
            file_url_to_path("file://localhost/tmp/a.txt"),
            Some(PathBuf::from("/tmp/a.txt"))
        );
        assert_eq!(file_url_to_path("file://server/share/a.txt"), None);
        assert_eq!(file_url_to_path("https://example.com/a.txt"), None);
        #[cfg(windows)]
        assert_eq!(
            file_url_to_path("file:///C:/dir/a%25b.txt"),
            Some(PathBuf::from("C:/dir/a%b.txt"))
        );
    }

    #[test]
    fn private_dir_is_created() {
        let dir = std::env::temp_dir().join(format!("re-platform-private-{}", std::process::id()));
        ensure_private_dir(&dir.join("nested")).unwrap();
        assert!(dir.join("nested").is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("nested"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
