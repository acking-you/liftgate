use std::io;
use std::sync::{Mutex, Once};

use tokio::process::{Child, Command};

/// Installs a chained panic hook once per process so supervised ACP process groups / Job Objects
/// are terminated even under `panic = "abort"`.
pub fn install_panic_cleanup_hook() {
    static HOOK_ONCE: Once = Once::new();
    HOOK_ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            cleanup_all_registered_groups();
            prev(info);
        }));
    });
}

/// Configures OS-specific process-group isolation on `Command` prior to spawning across
/// Windows, Linux, and macOS.
pub fn configure_command_group(cmd: &mut Command) {
    cmd.kill_on_drop(true)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat");

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
        // Isolate console CTRL_C_EVENT so `agy-acp-gateway` drains active HTTP streams first
        // (matching `process_group(0)` on Unix) and avoid flashing console windows when headless.
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    #[cfg(unix)]
    {
        // Dedicated process group (`pgid == child_pid`) on Linux & macOS so `killpg` cleans up
        // `agy acp` and every `--process-per-session` worker atomically.
        cmd.process_group(0);
    }

    #[cfg(target_os = "linux")]
    {
        // SAFETY: `prctl(PR_SET_PDEATHSIG, SIGTERM)` is a raw async-signal-safe Linux syscall with
        // zero allocations or locks, ensuring kernel-enforced teardown even on parent `SIGKILL`.
        unsafe {
            cmd.pre_exec(|| {
                let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{Child, Mutex, io};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
    };

    static ACTIVE_JOBS: Mutex<Vec<isize>> = Mutex::new(Vec::new());

    pub(super) fn cleanup_all() {
        let handles: Vec<isize> = match ACTIVE_JOBS.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        for raw in handles {
            // SAFETY: Each registered `raw` value is an open Win32 Job Object handle kept alive
            // until its owning `ProcessGroupGuard` removes it under `ACTIVE_JOBS`.
            unsafe {
                let _ = TerminateJobObject(raw as HANDLE, 1);
            }
        }
    }

    /// Synchronous, runtime-independent Win32 Job Object guard that kills the entire child tree.
    #[derive(Debug)]
    pub struct ProcessGroupGuard {
        job_handle: isize,
    }

    impl ProcessGroupGuard {
        pub fn attach(child: &Child) -> io::Result<Self> {
            let Some(raw_proc) = child.raw_handle() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "child process already exited or lacks raw handle",
                ));
            };

            // SAFETY: Passing null security attributes and null name creates an unnamed Job Object.
            let job: HANDLE = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                // SAFETY: All-zero initialization is valid for Win32 POD struct `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`.
                unsafe { core::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            // SAFETY: `info` points to a valid `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` struct of exact byte size.
            let ok_limit = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok_limit == 0 {
                let err = io::Error::last_os_error();
                // SAFETY: `job` is a valid open handle created above.
                unsafe {
                    let _ = CloseHandle(job);
                }
                return Err(err);
            }

            // SAFETY: Both `job` and `raw_proc` are valid Win32 handles.
            let ok_assign = unsafe { AssignProcessToJobObject(job, raw_proc as HANDLE) };
            if ok_assign == 0 {
                let err = io::Error::last_os_error();
                // SAFETY: `job` is a valid open handle created above.
                unsafe {
                    let _ = CloseHandle(job);
                }
                return Err(err);
            }

            let raw_job = job as isize;
            match ACTIVE_JOBS.lock() {
                Ok(mut guard) => guard.push(raw_job),
                Err(poisoned) => poisoned.into_inner().push(raw_job),
            }

            Ok(Self {
                job_handle: raw_job,
            })
        }

        pub fn terminate_now(&self) {
            if self.job_handle != 0 {
                // SAFETY: `self.job_handle` is a valid open Job Object handle until `Drop`.
                unsafe {
                    let _ = TerminateJobObject(self.job_handle as HANDLE, 1);
                }
            }
        }
    }

    impl Drop for ProcessGroupGuard {
        fn drop(&mut self) {
            if self.job_handle == 0 {
                return;
            }
            let raw = self.job_handle;
            self.job_handle = 0;

            match ACTIVE_JOBS.lock() {
                Ok(mut guard) => guard.retain(|h| *h != raw),
                Err(poisoned) => poisoned.into_inner().retain(|h| *h != raw),
            }

            // SAFETY: `raw` was owned exclusively by this guard and removed from `ACTIVE_JOBS` first.
            unsafe {
                let _ = TerminateJobObject(raw as HANDLE, 1);
                let _ = CloseHandle(raw as HANDLE);
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::{Child, Mutex, io};

    static ACTIVE_PGIDS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

    pub(super) fn cleanup_all() {
        let pgids: Vec<i32> = match ACTIVE_PGIDS.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        for pgid in pgids {
            // SAFETY: Sending SIGKILL to a negative PGID or via `killpg` is async-signal/thread safe.
            unsafe {
                let _ = libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }

    #[derive(Debug)]
    pub struct ProcessGroupGuard {
        pgid: i32,
    }

    impl ProcessGroupGuard {
        pub fn attach(child: &Child) -> io::Result<Self> {
            let pid = child.id().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "child process already exited")
            })?;
            let pgid = i32::try_from(pid).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "child PID exceeds i32")
            })?;
            match ACTIVE_PGIDS.lock() {
                Ok(mut guard) => guard.push(pgid),
                Err(poisoned) => poisoned.into_inner().push(pgid),
            }
            Ok(Self { pgid })
        }

        pub fn terminate_now(&self) {
            if self.pgid > 0 {
                // SAFETY: `pgid` is the dedicated process group leader PID created via `process_group(0)`.
                unsafe {
                    let _ = libc::killpg(self.pgid, libc::SIGKILL);
                }
            }
        }
    }

    impl Drop for ProcessGroupGuard {
        fn drop(&mut self) {
            if self.pgid <= 0 {
                return;
            }
            let pgid = self.pgid;
            self.pgid = 0;
            match ACTIVE_PGIDS.lock() {
                Ok(mut guard) => guard.retain(|p| *p != pgid),
                Err(poisoned) => poisoned.into_inner().retain(|p| *p != pgid),
            }
            // SAFETY: Best-effort non-blocking group termination on drop.
            unsafe {
                let _ = libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

pub use imp::ProcessGroupGuard;

fn cleanup_all_registered_groups() {
    imp::cleanup_all();
}
