#[cfg(windows)]
use crate::ResultExt;
use anyhow::{Context as _, Result};
use std::process::Stdio;

/// A wrapper around `smol::process::Child` that ensures all subprocesses
/// are killed when the process is terminated by using process groups.
pub struct Child {
    process: smol::process::Child,
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
}

#[cfg(windows)]
fn create_job_object_for_process(pid: u32) -> Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle as _;

    use windows::Win32::{
        Foundation::CloseHandle,
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
                PROCESS_TERMINATE,
            },
        },
    };

    unsafe {
        let job = CreateJobObjectW(None, None)?;
        let mut limit_info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limit_info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&limit_info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )?;

        let process = OpenProcess(
            PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )?;

        let assigned = AssignProcessToJobObject(job, process).is_ok();
        let _ = CloseHandle(process);
        if !assigned {
            let _ = CloseHandle(job);
            anyhow::bail!("failed to assign process {pid} to job object");
        }

        Ok(std::os::windows::io::OwnedHandle::from_raw_handle(
            job.0 as *mut _,
        ))
    }
}

impl std::ops::Deref for Child {
    type Target = smol::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.process
    }
}

impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process
    }
}

impl Child {
    #[cfg(not(windows))]
    pub fn spawn(
        mut command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        crate::set_pre_exec_to_start_new_session(&mut command);
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("failed to spawn command {command:?}"))?;
        Ok(Self { process })
    }

    #[cfg(windows)]
    pub fn spawn(
        command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("failed to spawn command {command:?}"))?;
        let pid = process.id();
        let job = create_job_object_for_process(pid)
            .with_context(|| format!("failed to create job object for process {pid}"))
            .log_err();

        Ok(Self { process, job })
    }

    pub fn into_inner(self) -> smol::process::Child {
        self.process
    }

    #[cfg(not(windows))]
    pub fn kill(&mut self) -> Result<()> {
        let pid = self.process.id();
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn kill(&mut self) -> Result<()> {
        use std::os::windows::io::AsRawHandle as _;

        use std::os::windows::process::CommandExt as _;
        use windows::Win32::{Foundation::HANDLE, System::JobObjects::TerminateJobObject};

        // Process has already exited.
        if self.process.try_status()?.is_some() {
            return Ok(());
        }

        if let Some(job) = &self.job
            && unsafe { TerminateJobObject(HANDLE(job.as_raw_handle() as _), 1).is_ok() }
        {
            return Ok(());
        }

        let pid = self.process.id().to_string();
        let killed_tree = std::process::Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .creation_flags(0x08000000)
            .status();

        if let Ok(status) = killed_tree
            && status.success()
        {
            return Ok(());
        }

        // Fallback to direct kill if taskkill is unavailable or failed.
        if self.process.try_status()?.is_none() {
            self.process.kill()?;
        }
        Ok(())
    }
}
