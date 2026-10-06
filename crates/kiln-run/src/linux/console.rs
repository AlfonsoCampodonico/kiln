//! The console FIFO: the VMM writes the guest's serial console into it, and a
//! thread copies it into the `console.log` ring (spec §5.3).

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{CWD, FileType, Mode, OFlags};

use crate::console::{CAP, Ring};
use crate::error::Result;

/// The copying thread; stopped by the VMM closing the FIFO, or by `stop`.
pub struct ConsoleRelay {
    pub fifo: PathBuf,
    pub log: PathBuf,
    wake: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl ConsoleRelay {
    pub fn start(dir: &Path) -> Result<Self> {
        let fifo = dir.join("console.pipe");
        let log = dir.join("console.log");
        rustix::fs::mknodat(CWD, &fifo, FileType::Fifo, Mode::from_raw_mode(0o600), 0).map_err(std::io::Error::from)?;
        // Non-blocking, so opening needs no writer yet; poll waits for one.
        let read = rustix::fs::open(
            &fifo,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let mut ring = Ring::create(&log, CAP)?;
        let (wake_r, wake) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(std::io::Error::from)?;
        let thread = std::thread::Builder::new().name("console".into()).spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let mut fds = [PollFd::new(&read, PollFlags::IN), PollFd::new(&wake_r, PollFlags::IN)];
                match poll(&mut fds, None) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => return,
                }
                let (data, woken) = (!fds[0].revents().is_empty(), !fds[1].revents().is_empty());
                if data {
                    match rustix::io::read(&read, &mut buf) {
                        // Every writer is gone: the VMM exited.
                        Ok(0) => return,
                        Ok(n) => {
                            let _ = ring.write(&buf[..n]);
                        }
                        Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
                        Err(_) => return,
                    }
                } else if woken {
                    return;
                }
            }
        })?;
        Ok(Self {
            fifo,
            log,
            wake,
            thread: Some(thread),
        })
    }

    /// Waits for the console to be copied (the VMM has exited), at most `limit`.
    pub fn finish(mut self, limit: std::time::Duration) {
        let until = std::time::Instant::now() + limit;
        while self.thread.as_ref().is_some_and(|t| !t.is_finished()) && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.stop();
    }

    fn stop(&mut self) {
        let _ = rustix::io::write(&self.wake, b"x");
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ConsoleRelay {
    fn drop(&mut self) {
        self.stop();
    }
}
