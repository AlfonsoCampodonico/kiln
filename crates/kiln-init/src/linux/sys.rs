//! System calls rustix does not wrap safely, and small helpers around rustix.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::io::Errno;
use rustix::net::addr::{SocketAddrArg, SocketAddrLen, SocketAddrOpaque};
use rustix::net::{AddressFamily, Shutdown, SocketFlags, SocketType};
use rustix::process::{Pid, Signal};

/// The kernel's `struct sockaddr_vm`.
#[repr(C)]
struct SockaddrVm {
    family: u16,
    reserved1: u16,
    port: u32,
    cid: u32,
    flags: u8,
    zero: [u8; 3],
}

const _: () = assert!(size_of::<SockaddrVm>() == 16);

/// A vsock address (rustix has none).
struct VsockAddr {
    cid: u32,
    port: u32,
}

// SAFETY: `with_sockaddr` calls `f` with a pointer to a live `sockaddr_vm` on its
// stack, laid out as the kernel's (16 bytes, `repr(C)`), and with its exact size;
// both stay valid for the duration of the call.
#[allow(unsafe_code)]
unsafe impl SocketAddrArg for VsockAddr {
    unsafe fn with_sockaddr<R>(&self, f: impl FnOnce(*const SocketAddrOpaque, SocketAddrLen) -> R) -> R {
        let addr = SockaddrVm {
            family: AddressFamily::VSOCK.as_raw(),
            reserved1: 0,
            port: self.port,
            cid: self.cid,
            flags: 0,
            zero: [0; 3],
        };
        f((&raw const addr).cast(), size_of::<SockaddrVm>() as SocketAddrLen)
    }
}

/// A connected vsock stream.
#[derive(Debug)]
pub struct Vsock(OwnedFd);

impl Vsock {
    pub fn connect(cid: u32, port: u32) -> rustix::io::Result<Self> {
        let fd = rustix::net::socket_with(AddressFamily::VSOCK, SocketType::STREAM, SocketFlags::CLOEXEC, None)?;
        rustix::net::connect(&fd, &VsockAddr { cid, port })?;
        Ok(Self(fd))
    }

    pub fn try_clone(&self) -> rustix::io::Result<Self> {
        rustix::io::fcntl_dupfd_cloexec(&self.0, 0).map(Self)
    }

    /// Half-closes: the host reads EOF.
    pub fn shutdown_write(&self) {
        let _ = rustix::net::shutdown(&self.0, Shutdown::Write);
    }
}

impl AsFd for Vsock {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Read for Vsock {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(rustix::io::read(&self.0, buf)?)
    }
}

impl Write for Vsock {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(rustix::io::write(&self.0, buf)?)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Grows the ext4 filesystem mounted at `dir` to `blocks` 4 KiB blocks, online.
#[allow(unsafe_code)]
pub fn ext4_resize(dir: &OwnedFd, blocks: u64) -> rustix::io::Result<()> {
    use rustix::ioctl::{Setter, ioctl, opcode};
    // `_IOW('f', 16, __u64)` from linux/fs/ext4/ext4.h.
    const EXT4_IOC_RESIZE_FS: rustix::ioctl::Opcode = opcode::write::<u64>(b'f', 16);
    // SAFETY: EXT4_IOC_RESIZE_FS reads one `__u64` (the new block count) through
    // its argument, which is what `Setter<_, u64>` passes; it writes nothing back.
    unsafe { ioctl(dir, Setter::<EXT4_IOC_RESIZE_FS, u64>::new(blocks)) }
}

/// `kill(-1, SIGKILL)`: every process but init.
pub fn kill_all() {
    let _ = rustix::process::kill_process_group(Pid::INIT, Signal::KILL);
}

/// Sends `sig` (1..=31) to `pid`; a process that is gone is not an error.
pub fn signal(pid: Pid, sig: i32) {
    if let Some(sig) = Signal::from_named_raw(sig) {
        let _ = rustix::process::kill_process(pid, sig);
    }
}

/// Reaps every child without blocking. Returns the wait status of `main` if it was among them.
/// (`wait`, i.e. `waitpid(-1)`: rustix's `waitpid(None, ..)` is `waitpid(0)`, which only
/// sees children in init's process group, and the main process has its own session.)
pub fn reap(main: Pid) -> Option<rustix::process::WaitStatus> {
    let mut found = None;
    loop {
        match rustix::process::wait(rustix::process::WaitOptions::NOHANG) {
            Ok(Some((pid, status))) => {
                if pid == main {
                    found = Some(status);
                }
            }
            Ok(None) | Err(_) => return found,
        }
    }
}

/// Waits until init has no children left.
pub fn reap_all() {
    loop {
        match rustix::process::wait(rustix::process::WaitOptions::empty()) {
            Ok(Some(_)) | Err(Errno::INTR) => {}
            Ok(None) | Err(_) => return,
        }
    }
}
