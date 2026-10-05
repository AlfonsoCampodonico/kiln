//! Stage 5: hostname, `/etc` files and the network (spec §9.6).

use std::io::Write;
use std::os::fd::OwnedFd;

use kiln_proto::{Config, Network};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use rustix::net::netlink::SocketAddrNetlink;
use rustix::net::{AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType};

use crate::error::{Context, Failure, Result};
use crate::{etc, netlink};

pub fn configure(config: &Config) -> Result<()> {
    rustix::system::sethostname(config.hostname.as_bytes()).context("sethostname")?;
    let net = config.network.as_ref();
    write_etc("/etc/hostname", &etc::hostname(&config.hostname))?;
    write_etc("/etc/hosts", &etc::hosts(&config.hostname, net))?;
    write_etc("/etc/resolv.conf", &etc::resolv_conf(net))?;
    let mut rtnl = Rtnl::open()?;
    let lo = rtnl.index("lo")?;
    rtnl.request("bring up lo", |seq| netlink::link_up(seq, lo))?;
    if let Some(net) = net {
        eth0(&mut rtnl, net)?;
    }
    Ok(())
}

/// Writes a regular file, replacing a symlink (or file) at `path` instead of following it.
fn write_etc(path: &str, contents: &str) -> Result<()> {
    match rustix::fs::unlink(path) {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(e) => return Err(e).context(format!("remove {path}")),
    }
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::open(path, flags, Mode::from_raw_mode(0o644)).context(format!("create {path}"))?;
    std::fs::File::from(fd)
        .write_all(contents.as_bytes())
        .context(format!("write {path}"))
}

fn eth0(rtnl: &mut Rtnl, net: &Network) -> Result<()> {
    let eth0 = rtnl.index("eth0")?;
    rtnl.request("bring up eth0", |seq| netlink::link_up(seq, eth0))?;
    rtnl.request("add eth0's address", |seq| {
        netlink::add_address(seq, eth0, net.address, net.prefix_len)
    })?;
    rtnl.request("add the default route", |seq| {
        netlink::add_default_route(seq, eth0, net.gateway)
    })
}

/// A `NETLINK_ROUTE` socket.
struct Rtnl {
    fd: OwnedFd,
    seq: u32,
}

impl Rtnl {
    fn open() -> Result<Self> {
        let fd = rustix::net::socket_with(AddressFamily::NETLINK, SocketType::RAW, SocketFlags::CLOEXEC, None)
            .context("open a netlink socket")?;
        Ok(Self { fd, seq: 0 })
    }

    fn index(&self, name: &str) -> Result<u32> {
        rustix::net::netdevice::name_to_index(&self.fd, name).context(format!("find {name}"))
    }

    /// Sends one request and waits for its ACK.
    fn request(&mut self, what: &str, build: impl FnOnce(u32) -> Vec<u8>) -> Result<()> {
        self.seq += 1;
        let msg = build(self.seq);
        rustix::net::sendto(&self.fd, &msg, SendFlags::empty(), &SocketAddrNetlink::new(0, 0)).context(what)?;
        let mut buf = vec![0u8; 8192];
        loop {
            let (n, _) = rustix::net::recv(&self.fd, &mut buf[..], RecvFlags::empty()).context(what)?;
            match netlink::ack(&buf[..n], self.seq) {
                Some(Ok(())) => return Ok(()),
                Some(Err(errno)) => return Err(Failure::os(what, &std::io::Error::from_raw_os_error(errno))),
                None => {}
            }
        }
    }
}
