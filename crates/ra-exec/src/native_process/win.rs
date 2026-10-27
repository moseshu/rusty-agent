// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(clippy::unwrap_used)]

// This file is copied from https://github.com/wezterm/wezterm (MIT license).
// Copyright (c) 2018-Present Wez Furlong
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

// Local modifications:
// - Place spawned processes in a Job Object so kill operations terminate the
//   full process tree, while normal root exit preserves background descendants.

use anyhow::Context as _;
use filedescriptor::OwnedHandle;
use portable_pty::Child;
use portable_pty::ChildKiller;
use portable_pty::ExitStatus;
use std::io::Error as IoError;
use std::io::Result as IoResult;
use std::os::windows::io::AsRawHandle;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use winapi::shared::minwindef::DWORD;
use winapi::um::minwinbase::STILL_ACTIVE;
use winapi::um::processthreadsapi::{GetExitCodeProcess, GetProcessId};
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::INFINITE;

pub(crate) mod conpty;
mod job;
mod procthreadattr;
mod psuedocon;

pub(crate) use conpty::ConPtySystem;
pub(crate) use job::JobObject;
pub(crate) use psuedocon::conpty_supported;

#[derive(Debug)]
#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
pub(crate) struct WinChild {
    proc: Mutex<OwnedHandle>,
    job: Arc<JobObject>,
}

impl WinChild {
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) fn new(proc: OwnedHandle, job: Arc<JobObject>) -> Self {
        Self {
            proc: Mutex::new(proc),
            job,
        }
    }

    #[allow(
        clippy::unnecessary_wraps,
        dead_code,
        reason = "Preserve the source fallible operation signature shared by native backends. Retain this source process API for the unported execution backend."
    )]
    fn is_complete(&mut self) -> IoResult<Option<ExitStatus>> {
        let mut status: DWORD = 0;
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle().cast(), &raw mut status) };
        if res != 0 {
            if status == STILL_ACTIVE {
                Ok(None)
            } else {
                self.preserve_descendants();
                Ok(Some(ExitStatus::with_exit_code(status)))
            }
        } else {
            Ok(None)
        }
    }

    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    fn do_kill(&mut self) -> IoResult<()> {
        self.job.terminate()
    }

    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    fn preserve_descendants(&self) {
        if let Err(err) = self.job.preserve_descendants() {
            log::warn!("ConPTY failed to preserve descendants after root exit: {err}");
        }
    }
}

impl ChildKiller for WinChild {
    fn kill(&mut self) -> IoResult<()> {
        if let Err(err) = self.do_kill() {
            log::warn!("ConPTY failed to terminate process tree: {err}");
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(WinChildKiller {
            job: Arc::clone(&self.job),
        })
    }
}

#[derive(Debug)]
#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
pub(crate) struct WinChildKiller {
    job: Arc<JobObject>,
}

impl ChildKiller for WinChildKiller {
    fn kill(&mut self) -> IoResult<()> {
        self.job.terminate()
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(WinChildKiller {
            job: Arc::clone(&self.job),
        })
    }
}

impl Child for WinChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        self.is_complete()
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        if let Ok(Some(status)) = self.try_wait() {
            return Ok(status);
        }
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        unsafe {
            WaitForSingleObject(proc.as_raw_handle().cast(), INFINITE);
        }
        let mut status: DWORD = 0;
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle().cast(), &raw mut status) };
        if res != 0 {
            self.preserve_descendants();
            Ok(ExitStatus::with_exit_code(status))
        } else {
            Err(IoError::last_os_error())
        }
    }

    fn process_id(&self) -> Option<u32> {
        let res = unsafe { GetProcessId(self.proc.lock().unwrap().as_raw_handle().cast()) };
        if res == 0 { None } else { Some(res) }
    }

    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        let proc = self.proc.lock().unwrap();
        Some(proc.as_raw_handle())
    }
}

impl std::future::Future for WinChild {
    type Output = anyhow::Result<ExitStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<anyhow::Result<ExitStatus>> {
        match self.is_complete() {
            Ok(Some(status)) => Poll::Ready(Ok(status)),
            Err(err) => Poll::Ready(Err(err).context("Failed to retrieve process exit status")),
            Ok(None) => {
                let proc = self.proc.lock().unwrap().try_clone()?;
                let waker = cx.waker().clone();
                std::thread::spawn(move || {
                    unsafe {
                        WaitForSingleObject(proc.as_raw_handle().cast(), INFINITE);
                    }
                    waker.wake();
                });
                Poll::Pending
            }
        }
    }
}
