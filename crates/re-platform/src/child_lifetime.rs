//! Keep provider-owned CLI processes from outliving their provider on Stop.
use std::{
    io,
    process::{Child, Command},
};

/// On Linux, ask the kernel to kill this child if its parent disappears.
pub fn configure(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = unsafe { libc::getpid() };
        // SAFETY: the child hook only calls async-signal-safe OS functions.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW (not used for interactive login)
    }
    #[cfg(all(not(target_os = "linux"), not(windows)))]
    let _ = command;
}

/// Windows job handle. Closing the provider process also closes the job,
/// killing the child and its descendants. Not used for interactive login.
pub struct Guard {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}
impl Guard {
    pub fn attach(child: &Child) -> io::Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::{Foundation::CloseHandle, System::JobObjects::*};
            // SAFETY: null attributes/name; child handle remains valid for assignment.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&info) as u32,
                ) != 0
                    && AssignProcessToJobObject(job, child.as_raw_handle().cast()) != 0
            };
            if !ok {
                let error = io::Error::last_os_error();
                unsafe {
                    CloseHandle(job);
                }
                return Err(error);
            }
            Ok(Self { job })
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Ok(Self {})
        }
    }
}
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn dropping_the_job_kills_only_its_owned_child() {
        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let guard = Guard::attach(&child).unwrap();
        let started = std::time::Instant::now();
        drop(guard);
        let _ = child.wait().unwrap(); // Closing a job may terminate with exit code zero.
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}
