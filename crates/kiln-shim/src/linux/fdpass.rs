//! An exec's FIFOs, from the shim to its VM worker (kiln-shim exec and volumes design
//! §5.5): the shim opens them as root, as it does a task's, and passes them with
//! `SCM_RIGHTS` on a `SOCK_SEQPACKET` socketpair, one datagram per exec. The datagram's
//! 5 bytes are the exec id (u32 LE) and which of stdin, stdout and stderr follow (bits
//! 0-2), in that order.

use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer, SendAncillaryMessage,
    SendFlags, SocketFlags, SocketType, recvmsg, sendmsg, socketpair,
};

/// An exec's stdio, as the worker receives it.
#[derive(Debug, Default)]
pub struct ExecFds {
    pub stdin: Option<OwnedFd>,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
}

/// The shim's end and the worker's.
pub fn pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    Ok(socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?)
}

pub fn send(sock: impl AsFd, id: u32, fds: [Option<BorrowedFd<'_>>; 3]) -> std::io::Result<()> {
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&id.to_le_bytes());
    let present: Vec<BorrowedFd<'_>> = fds.iter().flatten().copied().collect();
    for (bit, fd) in fds.iter().enumerate() {
        if fd.is_some() {
            header[4] |= 1 << bit;
        }
    }
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
    let mut cmsg = SendAncillaryBuffer::new(&mut space);
    if !present.is_empty() && !cmsg.push(SendAncillaryMessage::ScmRights(&present)) {
        return Err(std::io::Error::other("too many fds"));
    }
    sendmsg(sock, &[IoSlice::new(&header)], &mut cmsg, SendFlags::empty())?;
    Ok(())
}

pub fn recv(sock: impl AsFd) -> std::io::Result<(u32, ExecFds)> {
    let mut header = [0u8; 5];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
    let mut cmsg = RecvAncillaryBuffer::new(&mut space);
    let r = recvmsg(
        sock,
        &mut [IoSliceMut::new(&mut header)],
        &mut cmsg,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    if r.bytes != header.len() {
        return Err(std::io::Error::other("a short fd datagram"));
    }
    let mut received: Vec<OwnedFd> = Vec::new();
    for m in cmsg.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = m {
            received.extend(fds);
        }
    }
    let id = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let mut it = received.into_iter();
    let mut take = |bit: u8| if header[4] & (1 << bit) != 0 { it.next() } else { None };
    let fds = ExecFds {
        stdin: take(0),
        stdout: take(1),
        stderr: take(2),
    };
    if it.next().is_some() {
        return Err(std::io::Error::other("an fd datagram with extra fds"));
    }
    Ok((id, fds))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    #[test]
    fn fds_cross_in_order() {
        let (a, b) = pair().unwrap();
        let (r1, w1) = rustix::pipe::pipe().unwrap();
        let (r2, w2) = rustix::pipe::pipe().unwrap();
        send(&a, 7, [None, Some(w1.as_fd()), Some(w2.as_fd())]).unwrap();
        let (id, fds) = recv(&b).unwrap();
        assert_eq!(id, 7);
        assert!(fds.stdin.is_none());
        std::fs::File::from(fds.stdout.unwrap()).write_all(b"out").unwrap();
        std::fs::File::from(fds.stderr.unwrap()).write_all(b"err").unwrap();
        drop((w1, w2));
        let mut s = String::new();
        std::fs::File::from(r1).read_to_string(&mut s).unwrap();
        assert_eq!(s, "out");
        s.clear();
        std::fs::File::from(r2).read_to_string(&mut s).unwrap();
        assert_eq!(s, "err");
    }
}
