use std::io;
use std::process::ExitStatus;
#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};
use tokio::process::{Child, Command};

pub(crate) const PWSH_STDIN_BOOTSTRAP: &str = "$__uri_agent_source = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String([Console]::In.ReadToEnd())); & ([ScriptBlock]::Create($__uri_agent_source))";

/// Owns the platform execution boundary around a root child. Dropping the
/// owner terminates processes that remain in that boundary.
pub(crate) struct ProcessTree {
    #[cfg(unix)]
    process_group: AtomicI32,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl ProcessTree {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        configure_command(command);
        let child = command.kill_on_drop(true).spawn()?;

        #[cfg(unix)]
        let tree = {
            let pid = child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .ok_or_else(|| io::Error::other("failed to get child process group ID"))?;
            Self {
                process_group: AtomicI32::new(pid),
            }
        };

        #[cfg(windows)]
        let tree = match windows_job::assign(&child) {
            Ok(job) => Self { job },
            Err(error) => {
                let mut child = child;
                let _ = child.start_kill();
                return Err(error);
            }
        };

        #[cfg(not(any(unix, windows)))]
        let tree = Self {};

        Ok((child, tree))
    }

    pub(crate) fn terminate(&self) {
        #[cfg(unix)]
        {
            let process_group = self.process_group.swap(0, Ordering::AcqRel);
            if process_group == 0 {
                return;
            }
            // SAFETY: a negative PID targets only the process group created for
            // this child immediately before it was spawned.
            let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
        }

        #[cfg(windows)]
        windows_job::terminate(&self.job);
    }

    pub(crate) async fn terminate_and_wait(&self, child: &mut Child) -> io::Result<ExitStatus> {
        self.terminate();
        // This is a fallback for platforms without a tree primitive and for a
        // root process that escaped before the platform boundary was attached.
        let _ = child.start_kill();
        child.wait().await
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
fn configure_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.as_std_mut().process_group(0);
    let max_descriptor = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let max_descriptor = if max_descriptor > 3 {
        max_descriptor.min(i32::MAX.into()) as i32
    } else {
        1024
    };
    // SAFETY: process_group and descriptor setup run in the post-fork child.
    // The closure calls only async-signal-safe syscalls and does not allocate.
    unsafe {
        command
            .as_std_mut()
            .pre_exec(move || mark_extra_descriptors_close_on_exec(max_descriptor));
    }
}

#[cfg(unix)]
fn mark_extra_descriptors_close_on_exec(max_descriptor: i32) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // CLOEXEC preserves Rust's exec-error pipe until exec while closing all
        // inherited non-stdio descriptors in the executed program.
        let result = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3_u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if result == 0 {
            return Ok(());
        }
    }

    for descriptor in 3..max_descriptor {
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        if flags < 0 {
            continue;
        }
        if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn configure_command(_command: &mut Command) {}

#[cfg(not(any(unix, windows)))]
fn configure_command(_command: &mut Command) {}

#[cfg(windows)]
mod windows_job {
    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr;
    use tokio::process::Child;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    pub(super) fn assign(child: &Child) -> io::Result<OwnedHandle> {
        let raw_job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw_job.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a new owned handle.
        let job = unsafe { OwnedHandle::from_raw_handle(raw_job.cast()) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job.as_raw_handle().cast(),
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&limits).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("failed to get child process handle"))?;
        let assigned = unsafe {
            AssignProcessToJobObject(job.as_raw_handle().cast::<c_void>(), process.cast())
        };
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    pub(super) fn terminate(job: &OwnedHandle) {
        let handle: HANDLE = job.as_raw_handle().cast();
        let _ = unsafe { TerminateJobObject(handle, 1) };
    }
}
