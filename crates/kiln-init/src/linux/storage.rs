//! Stage 3: layers, the scratch disk and the root overlay (spec §9.4, §9.6).

use std::os::fd::OwnedFd;

use kiln_proto::Config;
use rustix::fs::{Mode, OFlags, SeekFrom};
use rustix::io::Errno;
use rustix::mount::{MountFlags, mount};

use super::{open_dir, sys};
use crate::disks::{SCRATCH, layer_device};
use crate::error::{Context, Failure, Result};
use crate::overlay::{EMPTY_LOWER, UPPER, WORK, layer_dir, mount_data};

const BLOCK: u64 = 4096;

fn mkdir(path: &str) -> Result<()> {
    match rustix::fs::mkdir(path, Mode::from_raw_mode(0o755)) {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(e) => Err(e).context(format!("mkdir {path}")),
    }
}

/// Mounts everything under `/kiln` and returns a descriptor of the scratch
/// filesystem's root, which outlives the pivot.
pub fn mount_all(config: &Config) -> Result<OwnedFd> {
    mount(
        "tmpfs",
        "/kiln",
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        c"mode=0755",
    )
    .context("mount /kiln")?;
    for dir in ["/kiln/layers", "/kiln/rw", "/kiln/root", EMPTY_LOWER] {
        mkdir(dir)?;
    }
    let layers = config.layers as usize;
    for n in 0..layers {
        let (dev, dir) = (layer_device(n), layer_dir(n));
        mkdir(&dir)?;
        mount(dev.as_str(), dir.as_str(), "erofs", MountFlags::RDONLY, None).context(format!("mount layer {dev}"))?;
    }
    mount(SCRATCH, "/kiln/rw", "ext4", MountFlags::empty(), c"noinit_itable").context(format!("mount {SCRATCH}"))?;
    let rw = open_dir("/kiln/rw")?;
    grow(&rw, config.scratch.size_bytes)?;
    mkdir(UPPER)?;
    mkdir(WORK)?;
    let data = std::ffi::CString::new(mount_data(layers)?).expect("no NUL in paths");
    mount("overlay", "/kiln/root", "overlay", MountFlags::empty(), data.as_c_str())
        .context("mount the root overlay")?;
    Ok(rw)
}

/// Grows the scratch filesystem online to `size` bytes (spec §9.4).
fn grow(rw: &OwnedFd, size: u64) -> Result<()> {
    let dev = rustix::fs::open(SCRATCH, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .context(format!("open {SCRATCH}"))?;
    let dev_size = rustix::fs::seek(&dev, SeekFrom::End(0)).context(format!("size of {SCRATCH}"))?;
    if dev_size < size {
        return Err(Failure::msg(format!(
            "{SCRATCH} has {dev_size} bytes, but the scratch disk should have {size}"
        )));
    }
    sys::ext4_resize(rw, size / BLOCK).context(format!("grow the scratch filesystem to {size} bytes"))
}
