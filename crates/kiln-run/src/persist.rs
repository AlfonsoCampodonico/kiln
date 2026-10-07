//! `--persist PATH` (spec §9.4): a scratch disk kept across runs, with a host-side
//! sidecar `<PATH>.kiln.json` naming the image it belongs to. kiln never parses the
//! guest-written ext4: the sidecar is all it trusts, and it writes the sidecar itself.
//!
//! - A missing disk is created from the template (`--disk`, default 4 GiB).
//! - An existing disk is reused only with the same app layers, in the same order:
//!   overlayfs (with `xino`) forbids changing lower layers under an upper directory.
//!   Only the layers matter, so an image whose manifest changed but whose layers did
//!   not (another env or cmd) reuses it, and the sidecar records the new manifest
//!   (ruling 2). `--persist-reset` recreates it; `--persist-force` reuses it with
//!   other layers anyway, with a warning. The two exclude each other.
//! - A disk is never truncated: a `--disk` smaller than it is refused, a larger
//!   one grows the file (the guest grows the filesystem at boot).
//! - A disk without a readable sidecar is refused unless reset or forced.
//! - The disk is locked (`flock`) for the run, so two runs never share it. It lives
//!   where the user put it, outside the store's scratch directories, so no cleanup
//!   of stale runs ever touches it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use kiln_store::Digest;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::options::DEFAULT_DISK;

/// The sidecar's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Sidecar {
    pub version: u32,
    /// The platform manifest the disk was last used with.
    pub manifest: Digest,
    /// Its app layers, lowest first.
    pub layers: Vec<Digest>,
    pub size: u64,
}

pub const SIDECAR_VERSION: u32 = 1;

/// The sidecar's path for a disk.
pub fn sidecar_path(disk: &Path) -> PathBuf {
    let mut s = disk.as_os_str().to_owned();
    s.push(".kiln.json");
    PathBuf::from(s)
}

/// What to do with the disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Create it with this size.
    Create { size: u64 },
    /// (Re)create it, removing what is there.
    Recreate { size: u64 },
    /// Use it, growing the file to `size` when that is larger.
    Reuse { size: u64 },
}

/// What is at the path now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Existing {
    pub size: u64,
    /// `None`: no sidecar, or one kiln cannot read.
    pub sidecar: Option<Sidecar>,
}

/// Decides, without touching anything. Returns the plan and the lines to print
/// (each starting `warning: ` or `note: `).
pub fn decide(
    path: &Path,
    existing: Option<&Existing>,
    manifest: &Digest,
    layers: &[Digest],
    disk: Option<u64>,
    reset: bool,
    force: bool,
) -> Result<(Plan, Vec<String>)> {
    let shown = path.display();
    let Some(e) = existing else {
        return Ok((
            Plan::Create {
                size: disk.unwrap_or(DEFAULT_DISK),
            },
            Vec::new(),
        ));
    };
    if reset {
        return Ok((
            Plan::Recreate {
                size: disk.unwrap_or(DEFAULT_DISK),
            },
            vec![format!("warning: --persist-reset: recreating {shown}")],
        ));
    }
    let mut warnings = Vec::new();
    match &e.sidecar {
        None if force => warnings.push(format!(
            "warning: --persist-force: {shown} has no readable {}; using it anyway",
            sidecar_path(path).display()
        )),
        None => {
            return Err(Error::refused(format!(
                "{shown} exists but has no readable kiln sidecar ({}); pass --persist-reset to \
                 recreate it, or --persist-force to use it anyway",
                sidecar_path(path).display()
            )));
        }
        Some(s) if s.layers == layers => {
            if s.manifest != *manifest {
                warnings.push(format!(
                    "note: {shown} was last used with the image {}, which has the same layers; reusing it",
                    s.manifest
                ));
            }
        }
        Some(s) => {
            if !force {
                return Err(Error::refused(format!(
                    "{shown} was used with another image ({}) whose layers differ; overlayfs cannot keep its \
                     files over other layers. Pass --persist-reset to recreate it, or --persist-force to reuse \
                     it anyway",
                    s.manifest
                )));
            }
            warnings.push(format!(
                "warning: --persist-force: {shown} was used with other layers; files from them may misbehave"
            ));
        }
    }
    let size = match disk {
        Some(d) if d < e.size => {
            return Err(Error::refused(format!(
                "--disk {d} is smaller than {shown} ({} bytes); persisted disks are never shrunk",
                e.size
            )));
        }
        Some(d) => d,
        None => e.size,
    };
    Ok((Plan::Reuse { size }, warnings))
}

/// Reads what is at `path`: a regular file (never followed through a symlink), and
/// its sidecar if kiln can parse it.
pub fn inspect(path: &Path) -> Result<Option<Existing>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() {
        return Err(Error::refused(format!(
            "--persist {} is not a regular file",
            path.display()
        )));
    }
    let sidecar = std::fs::read(sidecar_path(path))
        .ok()
        .and_then(|b| serde_json::from_slice::<Sidecar>(&b).ok())
        .filter(|s| s.version == SIDECAR_VERSION);
    Ok(Some(Existing {
        size: meta.len(),
        sidecar,
    }))
}

/// Opens the disk for the run (creating, recreating or growing it per `plan`),
/// takes an exclusive lock on it so two runs never share it, and writes the sidecar.
/// A disk in use by another run is refused before anything changes.
pub fn apply(path: &Path, plan: &Plan, manifest: &Digest, layers: &[Digest]) -> Result<File> {
    let size = match *plan {
        Plan::Recreate { size } => {
            // Held while the old disk is replaced: never under a run using it.
            let _old = locked(path)?;
            std::fs::remove_file(path)?;
            create(path, size)?;
            size
        }
        Plan::Create { size } => {
            create(path, size)?;
            size
        }
        Plan::Reuse { size } => size,
    };
    let file = locked(path)?;
    if file.metadata()?.len() < size {
        file.set_len(size)?;
    }
    let sidecar = Sidecar {
        version: SIDECAR_VERSION,
        manifest: manifest.clone(),
        layers: layers.to_vec(),
        size: file.metadata()?.len(),
    };
    write_sidecar(path, &sidecar)?;
    Ok(file)
}

/// The disk, opened without following a symlink and locked for this run.
fn locked(path: &Path) -> Result<File> {
    let file = open_nofollow(path)?;
    if !try_lock(&file)? {
        return Err(Error::refused(format!(
            "{} is in use by another kiln run",
            path.display()
        )));
    }
    Ok(file)
}

/// Creates the disk from the template; never over a file that appeared meanwhile.
fn create(path: &Path, size: u64) -> Result<()> {
    match kiln_image::scratch::create_scratch(path, size) {
        Err(kiln_image::ImageError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(Error::refused(format!(
                "{} appeared while kiln was creating it (another kiln run?)",
                path.display()
            )))
        }
        r => Ok(r?),
    }
}

fn open_nofollow(path: &Path) -> Result<File> {
    let mut o = OpenOptions::new();
    o.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    Ok(o.open(path)?)
}

#[cfg(unix)]
fn try_lock(file: &File) -> Result<bool> {
    use rustix::fs::{FlockOperation, flock};
    match flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(false),
        Err(e) => Err(std::io::Error::from(e).into()),
    }
}

#[cfg(not(unix))]
fn try_lock(_: &File) -> Result<bool> {
    Ok(true)
}

/// Writes the sidecar atomically (temp file, then rename).
fn write_sidecar(disk: &Path, s: &Sidecar) -> Result<()> {
    let path = sidecar_path(disk);
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let bytes = serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Digest {
        Digest::of(s.as_bytes())
    }

    fn existing(manifest: &str, layers: &[&str], size: u64) -> Existing {
        Existing {
            size,
            sidecar: Some(Sidecar {
                version: 1,
                manifest: d(manifest),
                layers: layers.iter().map(|l| d(l)).collect(),
                size,
            }),
        }
    }

    #[test]
    fn decisions() {
        let p = Path::new("/disk.img");
        let (m, l) = (d("m"), vec![d("a"), d("b")]);
        let g = 1u64 << 30;
        assert_eq!(
            decide(p, None, &m, &l, None, false, false).unwrap().0,
            Plan::Create { size: DEFAULT_DISK }
        );
        let same = existing("m", &["a", "b"], g);
        assert_eq!(
            decide(p, Some(&same), &m, &l, None, false, false).unwrap(),
            (Plan::Reuse { size: g }, vec![])
        );
        assert_eq!(
            decide(p, Some(&same), &m, &l, Some(2 * g), false, false).unwrap().0,
            Plan::Reuse { size: 2 * g }
        );
        let err = decide(p, Some(&same), &m, &l, Some(g / 2), false, false).unwrap_err();
        assert!(err.to_string().contains("never shrunk"), "{err}");
        // A reset recreates, at any size.
        assert_eq!(
            decide(p, Some(&same), &m, &l, Some(g / 2), true, false).unwrap().0,
            Plan::Recreate { size: g / 2 }
        );

        let other = existing("m2", &["a", "c"], g);
        let err = decide(p, Some(&other), &m, &l, None, false, false).unwrap_err();
        assert!(
            err.to_string().contains("layers differ") && err.to_string().contains("--persist-reset"),
            "{err}"
        );
        let (plan, warnings) = decide(p, Some(&other), &m, &l, None, false, true).unwrap();
        assert_eq!(plan, Plan::Reuse { size: g });
        assert!(
            warnings.len() == 1 && warnings[0].starts_with("warning: --persist-force"),
            "{warnings:?}"
        );
        // The same layers in another order are other layers.
        let reordered = existing("m", &["b", "a"], g);
        assert!(decide(p, Some(&reordered), &m, &l, None, false, false).is_err());
        // Ruling 2: the same layers under another manifest (env or cmd changed) are
        // reused, with one note.
        let same_layers = existing("m2", &["a", "b"], g);
        let (plan, notes) = decide(p, Some(&same_layers), &m, &l, None, false, false).unwrap();
        assert_eq!(plan, Plan::Reuse { size: g });
        assert!(
            notes.len() == 1 && notes[0].starts_with("note: ") && notes[0].contains("same layers"),
            "{notes:?}"
        );

        let bare = Existing { size: g, sidecar: None };
        assert!(decide(p, Some(&bare), &m, &l, None, false, false).is_err());
        assert!(decide(p, Some(&bare), &m, &l, None, false, true).is_ok());
    }

    #[test]
    fn apply_creates_grows_locks_and_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        let (m, l) = (d("m"), vec![d("a")]);
        assert_eq!(inspect(&path).unwrap(), None);
        let f = apply(&path, &Plan::Create { size: 64 << 20 }, &m, &l).unwrap();
        let e = inspect(&path).unwrap().unwrap();
        assert_eq!(e.size, 64 << 20);
        assert_eq!(e.sidecar.as_ref().unwrap().layers, l);
        // Locked while in use.
        let err = apply(&path, &Plan::Reuse { size: 64 << 20 }, &m, &l).unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
        drop(f);
        // Grows, never shrinks.
        drop(apply(&path, &Plan::Reuse { size: 128 << 20 }, &m, &l).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 128 << 20);
        assert_eq!(inspect(&path).unwrap().unwrap().sidecar.unwrap().size, 128 << 20);
        drop(apply(&path, &Plan::Reuse { size: 64 << 20 }, &m, &l).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 128 << 20);
        // A symlink is refused, never followed.
        #[cfg(unix)]
        {
            let link = dir.path().join("link.img");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(inspect(&link).is_err());
        }
        // Ruling 2: reused under another manifest, the sidecar records the new one.
        let m2 = d("m2");
        drop(apply(&path, &Plan::Reuse { size: 128 << 20 }, &m2, &l).unwrap());
        assert_eq!(inspect(&path).unwrap().unwrap().sidecar.unwrap().manifest, m2);
        // A disk in use is never recreated under its run.
        let held = apply(&path, &Plan::Reuse { size: 128 << 20 }, &m, &l).unwrap();
        let err = apply(&path, &Plan::Recreate { size: 64 << 20 }, &m, &l).unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 128 << 20);
        drop(held);
        drop(apply(&path, &Plan::Recreate { size: 64 << 20 }, &m, &l).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 << 20);
        // Created only where nothing is.
        let err = apply(&path, &Plan::Create { size: 64 << 20 }, &m, &l).unwrap_err();
        assert!(err.to_string().contains("appeared"), "{err}");
        // A sidecar kiln cannot read counts as missing.
        std::fs::write(sidecar_path(&path), b"{}").unwrap();
        assert_eq!(inspect(&path).unwrap().unwrap().sidecar, None);
    }
}
