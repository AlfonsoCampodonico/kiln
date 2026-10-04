//! Stage 4: the guest's root (spec §9.6): the API filesystems Docker provides, then
//! `pivot_root` into the overlay and detach the init root.

use std::ffi::CStr;

use rustix::fs::Mode;
use rustix::io::Errno;
use rustix::mount::{MountFlags, UnmountFlags, mount};

use crate::error::{Context, Result};

const ROOT: &str = "/kiln/root";

fn mkdir(path: &str) -> Result<()> {
    match rustix::fs::mkdir(path, Mode::from_raw_mode(0o755)) {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(e) => Err(e).context(format!("mkdir {path}")),
    }
}

fn mount_at(fs: &str, at: &str, flags: MountFlags, data: Option<&CStr>) -> Result<()> {
    let target = format!("{ROOT}{at}");
    mkdir(&target)?;
    mount(fs, target.as_str(), fs, flags, data).context(format!("mount {fs} on {at}"))
}

/// Replaces whatever is at `/dev/<name>` with a symlink to `target`.
fn dev_link(name: &str, target: &str) -> Result<()> {
    let path = format!("{ROOT}/dev/{name}");
    match rustix::fs::unlink(path.as_str()) {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(e) => return Err(e).context(format!("remove /dev/{name}")),
    }
    rustix::fs::symlink(target, path.as_str()).context(format!("symlink /dev/{name}"))
}

pub fn pivot() -> Result<()> {
    let hard = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
    // Missing mount points are created in the upper layer.
    mkdir(&format!("{ROOT}/etc"))?;
    mount_at("proc", "/proc", hard, None)?;
    mount_at("sysfs", "/sys", hard, None)?;
    mount_at("cgroup2", "/sys/fs/cgroup", hard, None)?;
    mount_at("devtmpfs", "/dev", MountFlags::NOSUID, Some(c"mode=0755"))?;
    mount_at(
        "devpts",
        "/dev/pts",
        MountFlags::NOSUID | MountFlags::NOEXEC,
        Some(c"newinstance,ptmxmode=0666,mode=0620,gid=5"),
    )?;
    let shm = format!("{ROOT}/dev/shm");
    mkdir(&shm)?;
    mount(
        "tmpfs",
        shm.as_str(),
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        c"mode=1777",
    )
    .context("mount tmpfs on /dev/shm")?;
    mount_at("mqueue", "/dev/mqueue", hard, None)?;
    for (name, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
        ("ptmx", "pts/ptmx"),
    ] {
        dev_link(name, target)?;
    }
    rustix::process::chdir(ROOT).context("chdir to the new root")?;
    rustix::process::pivot_root(".", ".").context("pivot_root")?;
    rustix::mount::unmount(".", UnmountFlags::DETACH).context("detach the init root")?;
    rustix::process::chdir("/").context("chdir /")
}
