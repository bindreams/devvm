//! The hold on Unix: an flock'd file inherited by a child.
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command};

pub struct Hold(File);

impl Hold {
    pub fn create(path: &Path) -> Self {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        Self(file)
    }

    /// Makes the child inherit the descriptor (std opens files close-on-exec).
    pub fn inherit_into(&self, command: &mut Command) {
        let fd = self.0.as_raw_fd();
        // SAFETY: fcntl is async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

/// Whether the hold could be taken on a fresh open; released again at once.
pub fn try_take(path: &Path) -> bool {
    let file = File::open(path).unwrap();
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    assert_eq!(error.raw_os_error(), Some(libc::EWOULDBLOCK), "flock: {error}");
    false
}

/// waitpid(WNOHANG).
pub fn has_exited(child: &mut Child) -> bool {
    child.try_wait().unwrap().is_some()
}
