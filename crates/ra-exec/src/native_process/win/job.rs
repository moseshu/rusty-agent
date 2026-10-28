// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use filedescriptor::OwnedHandle;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::RawHandle;
use std::sync::Mutex;
use tokio::process::Child;
use tokio::process::Command;
use winapi::shared::ntdef::NT_SUCCESS;
use winapi::shared::ntdef::NTSTATUS;
use winapi::um::jobapi2::AssignProcessToJobObject;
use winapi::um::jobapi2::CreateJobObjectW;
use winapi::um::jobapi2::SetInformationJobObject;
use winapi::um::jobapi2::TerminateJobObject;
use winapi::um::processthreadsapi::OpenProcess;
use winapi::um::processthreadsapi::TerminateProcess;
use winapi::um::winbase::CREATE_NO_WINDOW;
use winapi::um::winbase::CREATE_SUSPENDED;
use winapi::um::winnt::HANDLE;
use winapi::um::winnt::JOB_OBJECT_LIMIT_BREAKAWAY_OK;
use winapi::um::winnt::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use winapi::um::winnt::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use winapi::um::winnt::JobObjectExtendedLimitInformation;
use winapi::um::winnt::PROCESS_SET_QUOTA;
use winapi::um::winnt::PROCESS_SUSPEND_RESUME;
use winapi::um::winnt::PROCESS_TERMINATE;

#[link(name = "ntdll")]
unsafe extern "system" {
    #[allow(
        dead_code,
        reason = "Retain this source Windows contained-process operation for the unported execution backend."
    )]
    fn NtResumeProcess(process_handle: HANDLE) -> NTSTATUS;
}

/// Owns a Windows Job Object used to terminate a spawned process tree.
#[derive(Debug)]
pub struct JobObject {
    handle: OwnedHandle,
    // A mutex makes the state check, Job Object API call, and state update
    // atomic with respect to concurrent preserve and terminate requests.
    preserve_descendants: Mutex<bool>,
}

impl JobObject {
    /// Creates a Job Object configured to terminate all members when its last handle closes.
    pub(crate) fn create() -> io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };

        Self::set_limit_flags(
            &handle,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        )?;

        Ok(Self {
            handle,
            preserve_descendants: Mutex::new(false),
        })
    }

    /// Creates a Job Object whose owned descendants cannot explicitly break away.
    #[allow(
        dead_code,
        reason = "Retain this source Windows contained-process operation for the unported execution backend."
    )]
    pub(crate) fn create_without_breakaway() -> io::Result<Self> {
        let job = Self::create()?;
        Self::set_limit_flags(&job.handle, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)?;
        Ok(job)
    }

    /// Captures an owned process handle before its numeric identifier can be reused.
    #[allow(
        dead_code,
        reason = "Retain this source Windows contained-process operation for the unported execution backend."
    )]
    pub(crate) fn open_process_handle(
        process_id: u32,
    ) -> io::Result<std::os::windows::io::OwnedHandle> {
        let handle = unsafe {
            OpenProcess(PROCESS_TERMINATE, /*bInheritHandle*/ 0, process_id)
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle.cast()) })
    }

    /// Terminates the exact process identified by a previously captured handle.
    #[allow(
        dead_code,
        reason = "Retain this source Windows contained-process operation for the unported execution backend."
    )]
    pub(crate) fn terminate_process_handle(
        handle: &std::os::windows::io::OwnedHandle,
    ) -> io::Result<()> {
        let terminated = unsafe {
            TerminateProcess(handle.as_raw_handle().cast(), /*uExitCode*/ 1)
        };
        if terminated == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "Preserve source Windows ABI field sizes; the native structures and bounded buffers fit these fields."
    )]
    fn set_limit_flags(handle: &OwnedHandle, flags: u32) -> io::Result<()> {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = flags;
        let configured = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle().cast(),
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of_mut!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Assigns a running process to this job.
    ///
    /// Assignment is not retroactive: descendants created before this call
    /// completes are not guaranteed to become members of the job.
    pub(crate) fn assign_process(&self, process_handle: RawHandle) -> io::Result<()> {
        let assigned = unsafe {
            AssignProcessToJobObject(self.handle.as_raw_handle().cast(), process_handle.cast())
        };
        if assigned == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Prepares a background child to be assigned to this job before it runs.
    ///
    /// Replaces all creation flags with `CREATE_SUSPENDED | CREATE_NO_WINDOW`.
    /// Interactive children that need to inherit a console require an explicit launch.
    #[allow(
        clippy::unused_self,
        reason = "Preserve the source operation signature shared by native backends."
    )]
    pub(crate) fn prepare_suspended_spawn(&self, command: &mut Command) {
        command
            .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED)
            .kill_on_drop(true);
    }

    /// Assigns and resumes a suspended child, returning whether assignment succeeded.
    ///
    /// Nested jobs can reject assignment. Such a child is resumed without
    /// containment so callers can preserve their existing compatibility fallback.
    #[allow(
        dead_code,
        reason = "Retain this source Windows contained-process operation for the unported execution backend."
    )]
    pub(crate) fn assign_and_resume_process(&self, process_id: u32) -> io::Result<bool> {
        let process = unsafe {
            OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_SUSPEND_RESUME,
                /*bInheritHandle*/ 0,
                process_id,
            )
        };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process.cast()) };
        let assignment = self.assign_process(process.as_raw_handle());

        let status = unsafe { NtResumeProcess(process.as_raw_handle().cast()) };
        if !NT_SUCCESS(status) {
            unsafe {
                TerminateProcess(process.as_raw_handle().cast(), /*uExitCode*/ 1)
            };
            return Err(io::Error::other(format!(
                "failed to resume suspended process: NTSTATUS {status:#x}"
            )));
        }

        match assignment {
            Ok(()) => Ok(true),
            Err(error) => {
                log::warn!(
                    "Windows process job assignment unavailable for pid {process_id}: {error}"
                );
                Ok(false)
            }
        }
    }

    /// Starts a console-free background process, falling back when containment is unavailable.
    /// The returned job owns the contained process tree when assignment succeeds.
    #[allow(
        dead_code,
        reason = "Retain this source Windows contained-process operation for the unported execution backend."
    )]
    pub fn spawn_background(command: &mut Command) -> io::Result<(Child, Option<Self>)> {
        Self::spawn_background_with_job(command, Self::create())
    }

    #[allow(
        dead_code,
        reason = "Retain this source Windows contained-process operation for the unported execution backend."
    )]
    fn spawn_background_with_job(
        command: &mut Command,
        job: io::Result<Self>,
    ) -> io::Result<(Child, Option<Self>)> {
        if let Ok((child, job)) =
            job.and_then(|job| job.spawn_contained(command).map(|child| (child, job)))
        {
            Ok((child, Some(job)))
        } else {
            // Remove suspension before retrying, including after failed assignment.
            command.creation_flags(CREATE_NO_WINDOW).kill_on_drop(true);
            command.spawn().map(|child| (child, None))
        }
    }

    /// Starts a background child without a console, assigning it to this job before it runs.
    ///
    /// Replaces existing creation flags as in [`Self::prepare_suspended_spawn`].
    #[allow(
        dead_code,
        reason = "Retain this source Windows contained-process operation for the unported execution backend."
    )]
    pub(crate) fn spawn_contained(&self, command: &mut Command) -> io::Result<Child> {
        self.prepare_suspended_spawn(command);
        let child = command.spawn()?;
        let process_handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("missing child process handle"))?;
        self.assign_process(process_handle)?;

        let status = unsafe { NtResumeProcess(process_handle.cast()) };
        if !NT_SUCCESS(status) {
            return Err(io::Error::other(format!(
                "failed to resume contained process: NTSTATUS {status:#x}"
            )));
        }

        Ok(child)
    }

    /// Allows contained descendants to keep running after the root exits normally.
    ///
    /// This disables both explicit job termination and kill-on-close for this
    /// object. Calls race safely with [`Self::terminate`]: whichever operation
    /// acquires the state lock first determines whether the process tree is
    /// preserved or terminated.
    pub fn preserve_descendants(&self) -> io::Result<()> {
        let mut preserve_descendants = self
            .preserve_descendants
            .lock()
            .map_err(|_| io::Error::other("job state lock poisoned"))?;
        if *preserve_descendants {
            return Ok(());
        }

        Self::set_limit_flags(&self.handle, JOB_OBJECT_LIMIT_BREAKAWAY_OK)?;
        *preserve_descendants = true;
        Ok(())
    }

    /// Terminates every process currently assigned to the job.
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub fn terminate(&self) -> io::Result<()> {
        let preserve_descendants = self
            .preserve_descendants
            .lock()
            .map_err(|_| io::Error::other("job state lock poisoned"))?;
        if *preserve_descendants {
            return Ok(());
        }

        let terminated = unsafe {
            TerminateJobObject(self.handle.as_raw_handle().cast(), /*uExitCode*/ 1)
        };
        if terminated == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl AsRawHandle for JobObject {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}
