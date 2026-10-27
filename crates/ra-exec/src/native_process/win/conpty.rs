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

use crate::native_process::win::psuedocon::PsuedoCon;
use anyhow::Error;
use filedescriptor::FileDescriptor;
use filedescriptor::Pipe;
use portable_pty::Child;
use portable_pty::MasterPty;
use portable_pty::PtyPair;
use portable_pty::PtySize;
use portable_pty::PtySystem;
use portable_pty::SlavePty;
use portable_pty::cmdbuilder::CommandBuilder;
use std::mem::ManuallyDrop;
use std::os::windows::io::RawHandle;
use std::ptr;
use std::sync::Arc;
use std::sync::Mutex;
use winapi::um::wincon::COORD;

#[derive(Default)]
#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
pub(crate) struct ConPtySystem {}

#[allow(
    clippy::cast_possible_wrap,
    reason = "Preserve the source Windows ABI bit patterns and coordinate conversions."
)]
fn create_conpty_handles(
    size: PtySize,
) -> anyhow::Result<(PsuedoCon, FileDescriptor, FileDescriptor)> {
    let stdin = Pipe::new()?;
    let stdout = Pipe::new()?;

    let con = PsuedoCon::new(
        COORD {
            X: size.cols as i16,
            Y: size.rows as i16,
        },
        stdin.read,
        stdout.write,
    )?;

    Ok((con, stdin.write, stdout.read))
}

#[allow(
    dead_code,
    reason = "Retain the source Windows pseudoconsole ownership API for the unported execution backend."
)]
pub(crate) struct RawConPty {
    con: PsuedoCon,
    input_write: FileDescriptor,
    output_read: FileDescriptor,
}

#[allow(
    dead_code,
    reason = "Retain the source Windows pseudoconsole ownership API for the unported execution backend."
)]
impl RawConPty {
    #[allow(
        clippy::cast_sign_loss,
        reason = "Preserve the source Windows terminal coordinate conversions."
    )]
    pub(crate) fn new(cols: i16, rows: i16) -> anyhow::Result<Self> {
        let (con, input_write, output_read) = create_conpty_handles(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(Self {
            con,
            input_write,
            output_read,
        })
    }

    pub(crate) fn pseudoconsole_handle(&self) -> RawHandle {
        self.con.raw_handle()
    }

    pub(crate) fn into_handles(self) -> (PsuedoCon, FileDescriptor, FileDescriptor) {
        let me = ManuallyDrop::new(self);
        unsafe {
            (
                ptr::read(&raw const me.con),
                ptr::read(&raw const me.input_write),
                ptr::read(&raw const me.output_read),
            )
        }
    }
}

impl PtySystem for ConPtySystem {
    fn openpty(&self, size: PtySize) -> anyhow::Result<PtyPair> {
        let (con, writable, readable) = create_conpty_handles(size)?;

        let master = ConPtyMasterPty {
            inner: Arc::new(Mutex::new(Inner {
                con,
                readable,
                writable: Some(writable),
                size,
            })),
        };

        let slave = ConPtySlavePty {
            inner: master.inner.clone(),
        };

        Ok(PtyPair {
            master: Box::new(master),
            slave: Box::new(slave),
        })
    }
}

#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
struct Inner {
    con: PsuedoCon,
    readable: FileDescriptor,
    writable: Option<FileDescriptor>,
    size: PtySize,
}

impl Inner {
    #[allow(
        clippy::cast_possible_wrap,
        dead_code,
        reason = "Preserve the source Windows ABI bit patterns and coordinate conversions. Retain this source process API for the unported execution backend."
    )]
    pub(crate) fn resize(
        &mut self,
        num_rows: u16,
        num_cols: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<(), Error> {
        self.con.resize(COORD {
            X: num_cols as i16,
            Y: num_rows as i16,
        })?;
        self.size = PtySize {
            rows: num_rows,
            cols: num_cols,
            pixel_width,
            pixel_height,
        };
        Ok(())
    }
}

#[derive(Clone)]
#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
pub(crate) struct ConPtyMasterPty {
    inner: Arc<Mutex<Inner>>,
}

#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
pub(crate) struct ConPtySlavePty {
    inner: Arc<Mutex<Inner>>,
}

impl MasterPty for ConPtyMasterPty {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.resize(size.rows, size.cols, size.pixel_width, size.pixel_height)
    }

    fn get_size(&self) -> Result<PtySize, Error> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.size)
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
        Ok(Box::new(self.inner.lock().unwrap().readable.try_clone()?))
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
        Ok(Box::new(
            self.inner
                .lock()
                .unwrap()
                .writable
                .take()
                .ok_or_else(|| anyhow::anyhow!("writer already taken"))?,
        ))
    }
}

impl SlavePty for ConPtySlavePty {
    fn spawn_command(&self, cmd: CommandBuilder) -> anyhow::Result<Box<dyn Child + Send + Sync>> {
        let mut inner = self.inner.lock().unwrap();
        let child = inner.con.spawn_command(cmd)?;
        Ok(Box::new(child))
    }
}
