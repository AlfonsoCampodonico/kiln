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
//! - A disk without a readable sidecar is refused unless reset or forced. A sidecar
//!   from a newer kiln is refused even when forced (this kiln cannot tell what the
//!   disk holds, and would downgrade the sidecar); only a reset replaces it.
//! - The disk is locked (`flock`) first, and what is there is inspected and decided
//!   on under the lock, so two runs never share it and no decision is made on a
//!   disk another run is changing. It lives where the user put it, outside the
//!   store's scratch directories, so no cleanup of stale runs ever touches it.
//! - The sidecar is written once the guest is running ([`Persisted::record`]): a run
//!   whose guest never got that far leaves the old sidecar, or none when the disk
//!   was (re)created, never one naming layers the disk does not hold.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
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
}

pub const SIDECAR_VERSION: u32 = 1;

/// The most of a sidecar read: it is a few hundred bytes.
const MAX_SIDECAR: u64 = 64 * 1024;

/// The sidecar's path for a disk.
pub fn sidecar_path(disk: &Path) -> PathBuf {
    let mut s = disk.as_os_str().to_owned();
    s.push(".kiln.json");
    PathBuf::from(s)
}

/// What to do with an existing disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Recreate it with this size, removing what is there.
    Recreate { size: u64 },
    /// Use it, growing the file to `size` when that is larger.
    Reuse { size: u64 },
}

/// The sidecar next to a disk, as kiln can read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// None, or one kiln cannot read.
    Unreadable,
    /// Written by a newer kiln (this version).
    Newer(u32),
    Sidecar(Sidecar),
}

/// What is at the path now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Existing {
    pub size: u64,
    pub sidecar: Found,
}

/// Decides what to do with an existing disk, without touching anything. Returns
/// the plan and the lines to print once it is applied (each starting `warning: `
/// or `note: `).
pub fn decide(
    path: &Path,
    e: &Existing,
    manifest: &Digest,
    layers: &[Digest],
    disk: Option<u64>,
    reset: bool,
    force: bool,
) -> Result<(Plan, Vec<String>)> {
    let shown = path.display();
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
        Found::Newer(v) => {
            return Err(Error::refused(format!(
                "{shown} was last used by a newer kiln (its sidecar {} has version {v}); this kiln cannot \
                 tell what the disk holds, even with --persist-force. Use that kiln, or pass --persist-reset to \
                 recreate it",
                sidecar_path(path).display()
            )));
        }
        Found::Unreadable if force => warnings.push(format!(
            "warning: --persist-force: {shown} has no readable {}; using it anyway",
            sidecar_path(path).display()
        )),
        Found::Unreadable => {
            return Err(Error::refused(format!(
                "{shown} exists but has no readable kiln sidecar ({}); pass --persist-reset to \
                 recreate it, or --persist-force to use it anyway",
                sidecar_path(path).display()
            )));
        }
        Found::Sidecar(s) if s.layers == layers => {
            if s.manifest != *manifest {
                warnings.push(format!(
                    "note: {shown} was last used with the image {}, which has the same layers; reusing it",
                    s.manifest
                ));
            }
        }
        Found::Sidecar(s) => {
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

/// The disk of a run, locked until this is dropped.
#[derive(Debug)]
pub struct Persisted {
    file: File,
    path: PathBuf,
    sidecar: Sidecar,
}

impl Persisted {
    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records the image in the sidecar: once the guest is running, so the disk
    /// holds what the sidecar says.
    pub fn record(&self) -> Result<()> {
        write_sidecar(&self.path, &self.sidecar)
    }

    /// Keeps the lock for as long as this process lives: kiln gave up on a VMM
    /// that may still be using the disk.
    pub fn hold(self) {
        std::mem::forget(self.file);
    }
}

/// Opens the disk at `path` for a run: locks it first (so it is never shared and
/// never decided on while another run changes it), then inspects it and decides
/// ([`decide`]) under the lock, and creates, recreates or grows it. A disk in use
/// by another run is refused before anything changes. Returns the lines to print.
pub fn open(
    path: &Path,
    manifest: &Digest,
    layers: &[Digest],
    disk: Option<u64>,
    reset: bool,
    force: bool,
) -> Result<(Persisted, Vec<String>)> {
    let sidecar = Sidecar {
        version: SIDECAR_VERSION,
        manifest: manifest.clone(),
        layers: layers.to_vec(),
    };
    let (file, lines) = match open_disk(path)? {
        None => {
            let file = create_locked(path, disk.unwrap_or(DEFAULT_DISK))?;
            // Under the new disk's lock: a sidecar left from a disk removed since names
            // nothing this one holds.
            remove_sidecar(path)?;
            (file, Vec::new())
        }
        Some(file) => {
            lock(&file, path)?;
            check(&file, path)?;
            let existing = Existing {
                size: file.metadata()?.len(),
                sidecar: read_sidecar(path),
            };
            let (plan, lines) = decide(path, &existing, manifest, layers, disk, reset, force)?;
            match plan {
                Plan::Recreate { size } => {
                    // The old disk's lock is held until the new one is in place.
                    remove_sidecar(path)?;
                    std::fs::remove_file(path)?;
                    let new = create_locked(path, size)?;
                    drop(file);
                    (new, lines)
                }
                Plan::Reuse { size } => {
                    if file.metadata()?.len() < size {
                        file.set_len(size)?;
                    }
                    (file, lines)
                }
            }
        }
    };
    Ok((
        Persisted {
            file,
            path: path.to_path_buf(),
            sidecar,
        },
        lines,
    ))
}

/// The disk, opened read-write without following a symlink; `None` when nothing
/// is there. Anything but a regular file is refused before it is opened.
fn open_disk(path: &Path) -> Result<Option<File>> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => {
            return Err(Error::refused(format!(
                "--persist {} is not a regular file",
                path.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    match open_nofollow(path) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::refused(format!(
            "--persist {}: {e} (it must be a regular file, not a symlink)",
            path.display()
        ))),
    }
}

/// A new disk at `path`, locked before anyone can see it: the template is written
/// to a temporary sibling, which is locked, then linked into place (never over a
/// file that appeared meanwhile) and its temporary name removed. Another run thus
/// never opens a half-written disk, nor one it could lock first.
fn create_locked(path: &Path, size: u64) -> Result<File> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".kiln-new-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let made = (|| -> Result<File> {
        kiln_image::scratch::create_scratch(&tmp, size)?;
        let file = open_nofollow(&tmp)?;
        lock(&file, &tmp)?;
        match std::fs::hard_link(&tmp, path) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(Error::refused(format!(
                "{} appeared while kiln was creating it (another kiln run?)",
                path.display()
            ))),
            r => Ok(r?),
        }?;
        Ok(file)
    })();
    let _ = std::fs::remove_file(&tmp);
    let file = made?;
    check(&file, path)?;
    Ok(file)
}

/// Locks the disk, retrying for up to 100 ms: a process this one is spawning
/// (any thread's) holds a copy of every descriptor until it execs, which closes
/// them, so a lock just released can look held for a moment. Another run's lock
/// is still held after that.
fn lock(file: &File, path: &Path) -> Result<()> {
    let mut tries = 0;
    while !try_lock(file)? {
        tries += 1;
        if tries < 10 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        return Err(Error::refused(format!(
            "{} is in use by another kiln run",
            path.display()
        )));
    }
    Ok(())
}

/// What was opened is a regular file of this user's, and still the one at `path`
/// (another run may have recreated it between the open and the lock).
#[cfg(unix)]
fn check(file: &File, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let uid = rustix::process::getuid().as_raw();
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != uid {
        return Err(Error::refused(format!(
            "--persist {} must be a regular file owned by uid {uid}",
            path.display()
        )));
    }
    let now = std::fs::symlink_metadata(path).ok();
    if now.is_none_or(|n| (n.dev(), n.ino()) != (m.dev(), m.ino())) {
        return Err(Error::refused(format!(
            "{} changed while kiln opened it (another kiln run?)",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check(_: &File, _: &Path) -> Result<()> {
    Ok(())
}

fn nofollow(o: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
    }
}

fn open_nofollow(path: &Path) -> std::io::Result<File> {
    let mut o = OpenOptions::new();
    o.read(true).write(true);
    nofollow(&mut o);
    o.open(path)
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

/// The sidecar next to `disk`, read without following a symlink: only a regular
/// file of at most [`MAX_SIDECAR`] bytes holding a sidecar of this version counts;
/// one of a later version is reported as such.
pub fn read_sidecar(disk: &Path) -> Found {
    #[derive(Deserialize)]
    struct Versioned {
        version: u32,
    }
    let mut o = OpenOptions::new();
    o.read(true);
    nofollow(&mut o);
    let Ok(f) = o.open(sidecar_path(disk)) else {
        return Found::Unreadable;
    };
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return Found::Unreadable;
    }
    let mut bytes = Vec::new();
    if f.take(MAX_SIDECAR + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_SIDECAR {
        return Found::Unreadable;
    }
    match serde_json::from_slice::<Versioned>(&bytes) {
        Ok(v) if v.version > SIDECAR_VERSION => Found::Newer(v.version),
        Ok(v) if v.version == SIDECAR_VERSION => serde_json::from_slice::<Sidecar>(&bytes)
            .map(Found::Sidecar)
            .unwrap_or(Found::Unreadable),
        _ => Found::Unreadable,
    }
}

fn remove_sidecar(disk: &Path) -> Result<()> {
    match std::fs::remove_file(sidecar_path(disk)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
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
            sidecar: Found::Sidecar(Sidecar {
                version: 1,
                manifest: d(manifest),
                layers: layers.iter().map(|l| d(l)).collect(),
            }),
        }
    }

    #[test]
    fn decisions() {
        let p = Path::new("/disk.img");
        let (m, l) = (d("m"), vec![d("a"), d("b")]);
        let g = 1u64 << 30;
        let same = existing("m", &["a", "b"], g);
        assert_eq!(
            decide(p, &same, &m, &l, None, false, false).unwrap(),
            (Plan::Reuse { size: g }, vec![])
        );
        assert_eq!(
            decide(p, &same, &m, &l, Some(2 * g), false, false).unwrap().0,
            Plan::Reuse { size: 2 * g }
        );
        let err = decide(p, &same, &m, &l, Some(g / 2), false, false).unwrap_err();
        assert!(err.to_string().contains("never shrunk"), "{err}");
        // A reset recreates, at any size.
        assert_eq!(
            decide(p, &same, &m, &l, Some(g / 2), true, false).unwrap().0,
            Plan::Recreate { size: g / 2 }
        );

        let other = existing("m2", &["a", "c"], g);
        let err = decide(p, &other, &m, &l, None, false, false).unwrap_err();
        assert!(
            err.to_string().contains("layers differ") && err.to_string().contains("--persist-reset"),
            "{err}"
        );
        let (plan, warnings) = decide(p, &other, &m, &l, None, false, true).unwrap();
        assert_eq!(plan, Plan::Reuse { size: g });
        assert!(
            warnings.len() == 1 && warnings[0].starts_with("warning: --persist-force"),
            "{warnings:?}"
        );
        // The same layers in another order are other layers.
        let reordered = existing("m", &["b", "a"], g);
        assert!(decide(p, &reordered, &m, &l, None, false, false).is_err());
        // Ruling 2: the same layers under another manifest (env or cmd changed) are
        // reused, with one note.
        let same_layers = existing("m2", &["a", "b"], g);
        let (plan, notes) = decide(p, &same_layers, &m, &l, None, false, false).unwrap();
        assert_eq!(plan, Plan::Reuse { size: g });
        assert!(
            notes.len() == 1 && notes[0].starts_with("note: ") && notes[0].contains("same layers"),
            "{notes:?}"
        );

        let bare = Existing {
            size: g,
            sidecar: Found::Unreadable,
        };
        assert!(decide(p, &bare, &m, &l, None, false, false).is_err());
        assert!(decide(p, &bare, &m, &l, None, false, true).is_ok());
        // A newer kiln's sidecar: refused even when forced (no downgrade); a reset
        // recreates the disk.
        let newer = Existing {
            size: g,
            sidecar: Found::Newer(2),
        };
        for force in [false, true] {
            let err = decide(p, &newer, &m, &l, None, false, force).unwrap_err().to_string();
            assert!(err.contains("newer kiln") && err.contains("version 2"), "{err}");
        }
        assert!(matches!(
            decide(p, &newer, &m, &l, None, true, false).unwrap().0,
            Plan::Recreate { .. }
        ));
    }

    fn sidecar_of(path: &Path) -> Found {
        read_sidecar(path)
    }

    #[test]
    fn open_creates_grows_locks_and_records_once_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        let (m, l) = (d("m"), vec![d("a")]);
        let mb = |n: u64| Some(n << 20);
        let (p, lines) = open(&path, &m, &l, mb(64), false, false).unwrap();
        assert!(lines.is_empty());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 << 20);
        // Not recorded until the guest runs.
        assert_eq!(sidecar_of(&path), Found::Unreadable);
        p.record().unwrap();
        let Found::Sidecar(s) = sidecar_of(&path) else {
            panic!("{:?}", sidecar_of(&path))
        };
        assert_eq!((s.manifest, s.layers), (m.clone(), l.clone()));
        // Locked while in use.
        let err = open(&path, &m, &l, None, false, false).unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
        drop(p);
        // Grows, never shrinks.
        drop(open(&path, &m, &l, mb(128), false, false).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 128 << 20);
        assert!(open(&path, &m, &l, mb(64), false, false).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 128 << 20);
        // A symlink is refused, never followed.
        #[cfg(unix)]
        {
            let link = dir.path().join("link.img");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            let err = open(&link, &m, &l, None, false, true).unwrap_err();
            assert!(err.to_string().contains("not a regular file"), "{err}");
        }
        // Ruling 2: reused under another manifest, the sidecar records the new one,
        // and the note comes back to be printed.
        let m2 = d("m2");
        let (p, lines) = open(&path, &m2, &l, None, false, false).unwrap();
        assert!(lines[0].contains("same layers"), "{lines:?}");
        p.record().unwrap();
        drop(p);
        assert!(matches!(sidecar_of(&path), Found::Sidecar(s) if s.manifest == m2));
        // A disk in use is never recreated under its run.
        let (held, _) = open(&path, &m, &l, None, false, false).unwrap();
        let err = open(&path, &m, &l, mb(64), true, false).unwrap_err();
        assert!(err.to_string().contains("in use"), "{err}");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 128 << 20);
        drop(held);
        // Recreated: the old sidecar goes first, and a run that never gets the guest
        // running leaves none.
        let (p, lines) = open(&path, &d("m3"), &[d("z")], mb(64), true, false).unwrap();
        assert!(lines[0].contains("--persist-reset"), "{lines:?}");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 << 20);
        assert_eq!(sidecar_of(&path), Found::Unreadable);
        drop(p);
        assert!(open(&path, &m, &l, None, false, false).is_err());
        // A sidecar kiln cannot read counts as missing; one too big, or a symlink, too.
        std::fs::write(sidecar_path(&path), b"{}").unwrap();
        assert_eq!(sidecar_of(&path), Found::Unreadable);
        std::fs::write(sidecar_path(&path), vec![b' '; 70_000]).unwrap();
        assert_eq!(sidecar_of(&path), Found::Unreadable);
        #[cfg(unix)]
        {
            let real = dir.path().join("real.json");
            std::fs::write(
                &real,
                serde_json::to_vec(&Sidecar {
                    version: 1,
                    manifest: m.clone(),
                    layers: l.clone(),
                })
                .unwrap(),
            )
            .unwrap();
            std::fs::remove_file(sidecar_path(&path)).unwrap();
            std::os::unix::fs::symlink(&real, sidecar_path(&path)).unwrap();
            assert_eq!(sidecar_of(&path), Found::Unreadable);
        }
        // A newer kiln's sidecar is named, and --persist-force does not overwrite it.
        let _ = std::fs::remove_file(sidecar_path(&path));
        std::fs::write(sidecar_path(&path), br#"{"version": 2, "whatever": true}"#).unwrap();
        assert_eq!(sidecar_of(&path), Found::Newer(2));
        let err = open(&path, &m, &l, None, false, true).unwrap_err();
        assert!(err.to_string().contains("newer kiln"), "{err}");
        assert_eq!(sidecar_of(&path), Found::Newer(2));
    }

    /// A new disk is locked before it appears at its path: it is linked into place
    /// from a locked temporary sibling, which is gone afterwards; a file that
    /// appeared at the path meanwhile is never replaced.
    #[test]
    fn a_new_disk_appears_locked_and_never_replaces_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        let held = create_locked(&path, 64 << 20).unwrap();
        // Whoever opens it now finds it locked.
        let other = open_nofollow(&path).unwrap();
        assert!(!try_lock(&other).unwrap());
        drop(held);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("disk.img")]);
        std::fs::write(&path, b"theirs").unwrap();
        let err = create_locked(&path, 64 << 20).unwrap_err();
        assert!(err.to_string().contains("appeared"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"theirs");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// A disk created where none was clears a sidecar left from an earlier one.
    #[test]
    fn a_stale_sidecar_goes_when_the_disk_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        std::fs::write(
            sidecar_path(&path),
            serde_json::to_vec(&Sidecar {
                version: 1,
                manifest: d("old"),
                layers: vec![d("old")],
            })
            .unwrap(),
        )
        .unwrap();
        let (p, _) = open(&path, &d("m"), &[d("a")], Some(64 << 20), false, false).unwrap();
        assert_eq!(read_sidecar(&path), Found::Unreadable);
        drop(p);
    }

    /// Decisions are made under the disk's lock: a run deciding while another holds
    /// the disk is refused as "in use", whatever the sidecar would have said, and
    /// of runs racing to open the same path at most one holds it at a time.
    #[test]
    fn decisions_are_made_under_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        let (m, a, b) = (d("m"), vec![d("a")], vec![d("b")]);
        let (held, _) = open(&path, &m, &a, Some(64 << 20), false, false).unwrap();
        held.record().unwrap();
        // Another image's layers: without the lock first this would be "layers differ".
        let err = open(&path, &d("m2"), &b, None, false, false).unwrap_err().to_string();
        assert!(err.contains("in use"), "{err}");
        drop(held);

        let gate = std::sync::Arc::new(std::sync::Barrier::new(8));
        let inside = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let (gate, inside, path) = (gate.clone(), inside.clone(), path.clone());
                let layers = if i % 2 == 0 { vec![d("a")] } else { vec![d("b")] };
                std::thread::spawn(move || {
                    gate.wait();
                    let mut opened = 0;
                    for _ in 0..20 {
                        if let Ok((p, _)) = open(&path, &d("m"), &layers, None, i % 4 == 1, false) {
                            assert_eq!(inside.fetch_add(1, std::sync::atomic::Ordering::SeqCst), 0);
                            p.record().unwrap();
                            std::thread::sleep(std::time::Duration::from_millis(1));
                            inside.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                            opened += 1;
                        }
                    }
                    opened
                })
            })
            .collect();
        let opened: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        assert!(opened > 0);
        // Whoever recorded last, the sidecar names one of the two images (or none,
        // after a reset whose new disk another run took first).
        match read_sidecar(&path) {
            Found::Sidecar(s) => assert!(s.layers == a || s.layers == b, "{s:?}"),
            other => assert_eq!(other, Found::Unreadable),
        }
    }
}
