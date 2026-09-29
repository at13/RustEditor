//! Single instance: the first process becomes the primary and listens; later
//! launches forward their (absolute) paths to it and exit.
//!
//! - A lock file (`File::try_lock`) in the per-user runtime directory decides
//!   who is primary. The OS releases it if the process dies.
//! - Windows: a named pipe `\\.\pipe\rusteditor-<random>` (remote clients
//!   rejected). The random name is stored in the per-user runtime directory,
//!   so other users cannot find or pre-create it.
//! - Unix: a Unix domain socket in the private (0700) runtime directory.
//!
//! No TCP port is opened. Messages are length-limited (see `read_paths`).

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

const MAGIC: &[u8; 8] = b"RUSTEDT2";
const MAX_PATHS: usize = 1_024;
const MAX_PATH_BYTES: usize = 32_768;
const CONNECT_RETRIES: usize = 50;
const RETRY_DELAY: Duration = Duration::from_millis(40);
const FORWARD_TIMEOUT: Duration = Duration::from_secs(3);
const LOCK_FILE: &str = "instance.lock";
/// Holds the pipe name (Windows only; Unix uses a socket file).
#[cfg_attr(not(windows), allow(dead_code))]
const ENDPOINT_FILE: &str = "instance.endpoint";

pub enum InstanceRole {
    /// This process is the editor; call [`InstanceServer::start`].
    Primary(InstanceServer),
    /// The paths were handed to the running editor; exit.
    Forwarded,
}

pub struct InstanceServer {
    lock: File,
    listener: sys::Listener,
}

impl InstanceServer {
    /// Accepts forwarded paths on a background thread and passes them to
    /// `deliver`. Stops once `deliver` returns `false`.
    pub fn start(self, deliver: impl Fn(Vec<PathBuf>) -> bool + Send + 'static) -> io::Result<()> {
        thread::Builder::new()
            .name("rusteditor-instance".to_owned())
            .spawn(move || {
                let _lock = self.lock; // held for the life of the process
                self.listener.serve(&mut |stream| {
                    let Ok(paths) = read_paths(stream) else {
                        return true;
                    };
                    let keep_going = deliver(paths);
                    let _ = stream.write_all(&[1]);
                    let _ = stream.flush();
                    keep_going
                });
            })
            .map(|_| ())
    }
}

/// Becomes the primary instance, or forwards `paths` to the running one.
/// Relative paths are made absolute first (the primary has its own working
/// directory).
pub fn acquire(paths: &[PathBuf]) -> io::Result<InstanceRole> {
    acquire_in(&crate::paths::runtime_dir()?, paths)
}

/// [`acquire`] with an explicit runtime directory (used by tests).
pub fn acquire_in(dir: &Path, paths: &[PathBuf]) -> io::Result<InstanceRole> {
    crate::paths::ensure_private_dir(dir)?;
    let paths: Vec<PathBuf> = paths
        .iter()
        .map(|path| std::path::absolute(path).unwrap_or_else(|_| path.clone()))
        .collect();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))?;
    match lock.try_lock() {
        Ok(()) => {
            let listener = sys::Listener::bind(dir)?;
            Ok(InstanceRole::Primary(InstanceServer { lock, listener }))
        }
        Err(fs::TryLockError::WouldBlock) => {
            let mut last_error = None;
            for _ in 0..CONNECT_RETRIES {
                match forward_with_timeout(dir, &paths) {
                    Ok(()) => return Ok(InstanceRole::Forwarded),
                    Err(error) => last_error = Some(error),
                }
                thread::sleep(RETRY_DELAY);
            }
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the running RustEditor did not accept the files: {}",
                    last_error.map_or_else(String::new, |e| e.to_string())
                ),
            ))
        }
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Sends the paths, giving up if the primary does not answer in time.
fn forward_with_timeout(dir: &Path, paths: &[PathBuf]) -> io::Result<()> {
    let (done, wait) = mpsc::channel();
    let dir = dir.to_path_buf();
    let paths = paths.to_vec();
    thread::Builder::new()
        .name("rusteditor-forward".to_owned())
        .spawn(move || {
            let result = sys::connect(&dir).and_then(|mut stream| {
                write_paths(&mut stream, &paths)?;
                stream.flush()?;
                let mut ack = [0_u8; 1];
                stream.read_exact(&mut ack)?;
                if ack != [1] {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the running instance rejected the request",
                    ));
                }
                Ok(())
            });
            let _ = done.send(result);
        })?;
    wait.recv_timeout(FORWARD_TIMEOUT).unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "no answer from the running instance",
        ))
    })
}

fn write_paths(writer: &mut impl Write, paths: &[PathBuf]) -> io::Result<()> {
    if paths.len() > MAX_PATHS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many paths in one request",
        ));
    }
    let mut message = Vec::new();
    message.extend_from_slice(MAGIC);
    message.extend_from_slice(&(paths.len() as u32).to_le_bytes());
    for path in paths {
        let bytes = path.to_string_lossy();
        if bytes.len() > MAX_PATH_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is too long",
            ));
        }
        message.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        message.extend_from_slice(bytes.as_bytes());
    }
    writer.write_all(&message)
}

fn read_paths(reader: &mut impl Read) -> io::Result<Vec<PathBuf>> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message.to_owned());
    let mut magic = [0_u8; MAGIC.len()];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(invalid("invalid RustEditor instance message"));
    }
    let count = read_u32(reader)? as usize;
    if count > MAX_PATHS {
        return Err(invalid("instance message contains too many paths"));
    }
    let mut paths = Vec::with_capacity(count);
    for _ in 0..count {
        let length = read_u32(reader)? as usize;
        if length > MAX_PATH_BYTES {
            return Err(invalid("instance message contains an oversized path"));
        }
        let mut bytes = vec![0_u8; length];
        reader.read_exact(&mut bytes)?;
        let path = String::from_utf8(bytes).map_err(|_| invalid("path is not UTF-8"))?;
        paths.push(PathBuf::from(path));
    }
    Ok(paths)
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

/// 128 unguessable bits from the OS-seeded hasher keys (no RNG dependency).
#[cfg_attr(not(windows), allow(dead_code))]
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut token = String::new();
    for salt in 0..2u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(salt);
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        hasher.write_u32(std::process::id());
        token.push_str(&format!("{:016x}", hasher.finish()));
    }
    token
}

#[cfg(windows)]
mod sys {
    //! Named pipes via a minimal, hand-declared Win32 surface (no extra
    //! dependency). All `unsafe` is confined to this module.

    use std::{
        ffi::c_void,
        fs::{File, OpenOptions},
        io,
        os::windows::{
            ffi::OsStrExt,
            io::{FromRawHandle, RawHandle},
        },
        path::Path,
    };

    use super::{ENDPOINT_FILE, random_token};

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
    const PIPE_WAIT: u32 = 0x0000_0000;
    const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    const ERROR_PIPE_CONNECTED: i32 = 535;
    const ERROR_ACCESS_DENIED: i32 = 5;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer: u32,
            in_buffer: u32,
            default_timeout: u32,
            security: *mut c_void,
        ) -> Handle;
        fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
        fn DisconnectNamedPipe(pipe: Handle) -> i32;
        fn FlushFileBuffers(file: Handle) -> i32;
    }

    fn wide(name: &str) -> Vec<u16> {
        std::ffi::OsStr::new(name)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    fn create(name: &str, first: bool) -> io::Result<File> {
        let name = wide(name);
        let flags = PIPE_ACCESS_DUPLEX
            | if first {
                FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                0
            };
        // SAFETY: `name` is NUL-terminated UTF-16; null security attributes
        // use the default DACL (full control for the creator only, plus
        // read for others), which is sufficient because the name is secret.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                64 * 1024,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `handle` is a valid, owned pipe handle.
        Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
    }

    pub struct Listener {
        name: String,
        next: File,
    }

    impl Listener {
        pub fn bind(dir: &Path) -> io::Result<Self> {
            // Retry if someone already owns a pipe with this name.
            let mut last = None;
            for _ in 0..5 {
                let name = format!(r"\\.\pipe\rusteditor-{}", random_token());
                match create(&name, true) {
                    Ok(first) => {
                        std::fs::write(dir.join(ENDPOINT_FILE), &name)?;
                        return Ok(Self { name, next: first });
                    }
                    Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                        last = Some(error)
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(last.unwrap_or_else(|| io::Error::other("cannot create the instance pipe")))
        }

        pub fn serve(mut self, handle: &mut dyn FnMut(&mut File) -> bool) {
            loop {
                let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&self.next) as Handle;
                // SAFETY: `raw` is the live pipe handle owned by `self.next`.
                let connected = unsafe { ConnectNamedPipe(raw, std::ptr::null_mut()) } != 0
                    || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED);
                // Keep the name owned while serving this client.
                let Ok(spare) = create(&self.name, false) else {
                    return;
                };
                let mut current = std::mem::replace(&mut self.next, spare);
                if connected && !handle(&mut current) {
                    return;
                }
                let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&current) as Handle;
                // SAFETY: `raw` is the live pipe handle owned by `current`.
                unsafe {
                    FlushFileBuffers(raw);
                    DisconnectNamedPipe(raw);
                }
            }
        }
    }

    pub fn connect(dir: &Path) -> io::Result<File> {
        let name = std::fs::read_to_string(dir.join(ENDPOINT_FILE))?;
        let name = name.trim();
        if !name.starts_with(r"\\.\pipe\rusteditor-") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected instance endpoint",
            ));
        }
        OpenOptions::new().read(true).write(true).open(name)
    }
}

#[cfg(unix)]
mod sys {
    use std::{
        io,
        os::unix::net::{UnixListener, UnixStream},
        path::Path,
        time::Duration,
    };

    const SOCKET: &str = "instance.sock";

    pub struct Listener(UnixListener);

    impl Listener {
        pub fn bind(dir: &Path) -> io::Result<Self> {
            let path = dir.join(SOCKET);
            // Stale socket from a crashed primary; we hold the lock.
            let _ = std::fs::remove_file(&path);
            Ok(Self(UnixListener::bind(path)?))
        }

        pub fn serve(self, handle: &mut dyn FnMut(&mut UnixStream) -> bool) {
            for stream in self.0.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                if !handle(&mut stream) {
                    return;
                }
            }
        }
    }

    pub fn connect(dir: &Path) -> io::Result<UnixStream> {
        let stream = UnixStream::connect(dir.join(SOCKET))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// A fresh directory under one parent. A running server keeps its lock
    /// and pipe/socket open until the test process exits, so directories
    /// from earlier runs are removed here instead of at the end.
    fn temp_dir(name: &str) -> PathBuf {
        let parent = std::env::temp_dir().join("re-platform-tests");
        let current = format!("{}-", std::process::id());
        if let Ok(entries) = fs::read_dir(&parent) {
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().starts_with(&current) {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        parent.join(format!("{}-{name}-{}", std::process::id(), random_token()))
    }

    #[test]
    fn path_messages_round_trip() {
        let paths = vec![
            PathBuf::from(r"C:\files\one.txt"),
            PathBuf::from("/home/u/files with spaces/two 😀.json"),
        ];
        let mut message = Vec::new();
        write_paths(&mut message, &paths).unwrap();
        assert_eq!(read_paths(&mut Cursor::new(message)).unwrap(), paths);
    }

    #[test]
    fn malformed_and_oversized_messages_are_rejected() {
        let bad_magic = read_paths(&mut Cursor::new(b"NOTRUST!".to_vec())).unwrap_err();
        assert_eq!(bad_magic.kind(), io::ErrorKind::InvalidData);
        let mut too_many = MAGIC.to_vec();
        too_many.extend_from_slice(&(MAX_PATHS as u32 + 1).to_le_bytes());
        assert!(read_paths(&mut Cursor::new(too_many)).is_err());
        let mut too_long = MAGIC.to_vec();
        too_long.extend_from_slice(&1u32.to_le_bytes());
        too_long.extend_from_slice(&(MAX_PATH_BYTES as u32 + 1).to_le_bytes());
        assert!(read_paths(&mut Cursor::new(too_long)).is_err());
    }

    #[test]
    fn tokens_are_unique() {
        assert_ne!(random_token(), random_token());
        assert_eq!(random_token().len(), 32);
    }

    #[test]
    fn second_launch_forwards_absolute_paths_to_the_primary() {
        let dir = temp_dir("instance");
        let InstanceRole::Primary(server) = acquire_in(&dir, &[]).unwrap() else {
            panic!("first launch must be primary");
        };
        let (sender, received) = mpsc::channel();
        server
            .start(move |paths| sender.send(paths).is_ok())
            .unwrap();

        let relative = PathBuf::from("relative/notes.txt");
        let absolute = std::env::temp_dir().join("absolute.txt");
        let role = acquire_in(&dir, &[relative.clone(), absolute.clone()]).unwrap();
        assert!(matches!(role, InstanceRole::Forwarded));
        let paths = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(paths[0], std::path::absolute(&relative).unwrap());
        assert!(paths[0].is_absolute());
        assert_eq!(paths[1], absolute);

        // A second forward works too (the listener keeps serving).
        assert!(matches!(
            acquire_in(&dir, std::slice::from_ref(&absolute)).unwrap(),
            InstanceRole::Forwarded
        ));
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            vec![absolute]
        );
        // No TCP: the endpoint is a pipe name / socket in the private dir.
        #[cfg(windows)]
        assert!(
            fs::read_to_string(dir.join(ENDPOINT_FILE))
                .unwrap()
                .starts_with(r"\\.\pipe\rusteditor-")
        );
        #[cfg(unix)]
        assert!(dir.join("instance.sock").exists());
    }

    #[test]
    fn garbage_from_a_client_does_not_stop_the_server() {
        let dir = temp_dir("garbage");
        let InstanceRole::Primary(server) = acquire_in(&dir, &[]).unwrap() else {
            panic!("primary");
        };
        let (sender, received) = mpsc::channel();
        server
            .start(move |paths| sender.send(paths).is_ok())
            .unwrap();
        {
            let mut stream = sys::connect(&dir).unwrap();
            stream.write_all(b"garbage!garbage!").unwrap();
        }
        let path = std::env::temp_dir().join("after-garbage.txt");
        assert!(matches!(
            acquire_in(&dir, std::slice::from_ref(&path)).unwrap(),
            InstanceRole::Forwarded
        ));
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            vec![path]
        );
    }

    #[test]
    fn a_forged_endpoint_is_refused() {
        let dir = temp_dir("forged");
        crate::paths::ensure_private_dir(&dir).unwrap();
        fs::write(dir.join(ENDPOINT_FILE), r"C:\Windows\System32\config").unwrap();
        #[cfg(windows)]
        assert_eq!(
            sys::connect(&dir).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
