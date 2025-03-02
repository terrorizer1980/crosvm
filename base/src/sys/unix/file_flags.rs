// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::os::unix::io::AsRawFd;

use libc::fcntl;
use libc::EINVAL;
use libc::F_GETFL;
use libc::O_ACCMODE;
use libc::O_RDONLY;
use libc::O_RDWR;
use libc::O_WRONLY;

use super::errno_result;
use super::Error;
use super::Result;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FileFlags {
    Read,
    Write,
    ReadWrite,
}

impl FileFlags {
    pub fn from_file(file: &dyn AsRawFd) -> Result<FileFlags> {
        // Trivially safe because fcntl with the F_GETFL command is totally safe and we check for
        // error.
        let flags = unsafe { fcntl(file.as_raw_fd(), F_GETFL) };
        if flags == -1 {
            errno_result()
        } else {
            match flags & O_ACCMODE {
                O_RDONLY => Ok(FileFlags::Read),
                O_WRONLY => Ok(FileFlags::Write),
                O_RDWR => Ok(FileFlags::ReadWrite),
                _ => Err(Error::new(EINVAL)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::pipe;
    use super::super::PlatformEvent;
    use super::*;

    #[test]
    fn pipe_pair() {
        let (read_pipe, write_pipe) = pipe(true).unwrap();
        assert_eq!(FileFlags::from_file(&read_pipe).unwrap(), FileFlags::Read);
        assert_eq!(FileFlags::from_file(&write_pipe).unwrap(), FileFlags::Write);
    }

    #[test]
    fn event() {
        let evt = PlatformEvent::new().unwrap();
        assert_eq!(FileFlags::from_file(&evt).unwrap(), FileFlags::ReadWrite);
    }
}
