//! Ties a process the engine starts to the app's lifetime, on Windows.
//!
//! The process is put in a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
//! The engine keeps the job's handle for as long as the process is tracked, and
//! the operating system closes every handle a process owns when it exits for
//! any reason: a quit, a crash, being killed from the Task Manager. Closing the
//! last handle to the job kills every process in it, so a dev server a flow
//! started does not outlive the app, and no code has to run at exit for that.
//!
//! This is the same technique `util::process::Child` uses for the processes it
//! starts. The engine starts its own with `tokio::process`, so it cannot reuse
//! that type.
//!
//! A process that has already started something before it is assigned to the
//! job (a window of a few milliseconds after it is spawned) is not caught for
//! what it started in that time; stopping a process still kills the whole tree
//! with `taskkill /T`.

use std::mem::size_of;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

/// A job object that kills what is in it when it is closed. Closing it, by
/// dropping this or by the app exiting, is what kills the process, so keep it
/// for as long as the process should live.
pub(crate) struct ProcessJob(HANDLE);

// SAFETY: a job object's handle can be used from any thread.
unsafe impl Send for ProcessJob {}
unsafe impl Sync for ProcessJob {}

impl ProcessJob {
    /// A new job holding the process `pid`.
    pub(crate) fn for_process(pid: u32) -> windows::core::Result<Self> {
        // SAFETY: plain Win32 calls. The job handle is owned by the returned
        // value (and closed on every error path by its `Drop`), and the process
        // handle is closed right after it has been assigned.
        unsafe {
            let job = Self(CreateJobObjectW(None, None)?);

            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;

            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)?;
            let assigned = AssignProcessToJobObject(job.0, process);
            let _ = CloseHandle(process);
            assigned?;
            Ok(job)
        }
    }
}

impl Drop for ProcessJob {
    fn drop(&mut self) {
        // SAFETY: this value owns the handle. Closing the last handle to the
        // job is what kills the processes in it.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn closing_the_job_kills_the_process() {
        let mut child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .expect("could not start the process");
        let job = ProcessJob::for_process(child.id())
            .expect("could not put the process in a job object");
        assert!(
            child.try_wait().unwrap().is_none(),
            "the process ended on its own"
        );

        drop(job);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the process was not killed when the job closed"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn a_process_that_does_not_exist_is_an_error_and_leaves_nothing_open() {
        // A pid that cannot be a live process.
        assert!(ProcessJob::for_process(u32::MAX).is_err());
    }
}
