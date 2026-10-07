//! Per-run state (spec §5.3, T7): `$XDG_RUNTIME_DIR/kiln/<run-id>/`, mode 0700, or
//! `/tmp/kiln-<uid>/<run-id>/` without `XDG_RUNTIME_DIR`. Each directory holds
//! `run.json`, the VMM's sockets (`sock/`), its logs and `console.log`.
//!
//! The scratch disk (unless persisted) is not there: `XDG_RUNTIME_DIR` is usually
//! a tmpfs, whose pages would be the guest's disk held in host memory (and charged
//! to the VMM's memory limit). It lives in the run's scratch directory,
//! `$KILN_HOME/scratch/<run-id>/`, mode 0700, on the store's filesystem
//! ([`ScratchDir`]); `run.json` records it.
//!
//! A run's directories are cleaned up by the run that made them. Those left
//! behind (kiln was killed) are removed by a later run only when the `run.json`
//! names this host's boot and hostname and neither kiln nor the VMM it names is
//! alive: entries from other boots or hosts (a shared `/tmp`) are never touched
//! ([`Cleanup::foreign`] lists them). Scratch directories also carry their own
//! identity (`scratch.json`), so one whose run directory is gone (a reboot or a
//! logout emptied the tmpfs) is still removed once its run is over
//! ([`clean_stale_scratch`]). `kiln gc` never touches scratch directories.

use std::fs::{DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A process, identified across PID reuse by its start time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub pid: u32,
    /// Clock ticks since boot (`/proc/<pid>/stat` field 22).
    pub start_time: u64,
}

/// `run.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunInfo {
    pub version: u32,
    pub id: String,
    pub boot_id: String,
    pub hostname: String,
    /// The `kiln` process that owns the run.
    pub kiln: Identity,
    /// The VMM, once it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmm: Option<Identity>,
    pub vmm_kind: String,
    pub image: String,
    /// The kernel's path when it is not a pinned one (`--kernel`, or an image's
    /// custom kernel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_kernel: Option<String>,
    /// `--init`'s path, when kiln-init is not the pinned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_init: Option<String>,
    /// Kept for debugging (`KILN_KEEP_RUN_DIR=1`): never cleaned up by later runs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep: bool,
    /// The run's scratch directory ([`ScratchDir`]), removed with the run directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_dir: Option<String>,
}

pub const RUN_INFO_VERSION: u32 = 1;

/// One run's directory, removed when dropped unless kept.
#[derive(Debug)]
pub struct RunDir {
    pub id: String,
    pub path: PathBuf,
    keep: bool,
}

impl RunDir {
    /// A fresh directory under `base` (see [`base_dir`]), named by a random id.
    pub fn create(base: &Path) -> Result<Self> {
        let id = random_id()?;
        let path = base.join(&id);
        dir_builder().create(&path)?;
        Ok(Self { id, path, keep: false })
    }

    /// Leaves the directory in place (for debugging: `KILN_KEEP_RUN_DIR=1`).
    pub fn keep(&mut self) {
        self.keep = true;
    }

    /// Replaces `run.json` atomically: written and synced to a temporary file,
    /// then renamed over it.
    pub fn write_info(&self, info: &RunInfo) -> Result<()> {
        write_json(&self.path, "run.json", info)
    }
}

/// Replaces `dir/name` atomically: written and synced to a temporary file, then
/// renamed over it.
fn write_json(dir: &Path, name: &'static str, value: &impl Serialize) -> Result<()> {
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let json = serde_json::to_vec_pretty(value).map_err(|e| Error::invalid(name, e.to_string()))?;
    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    f.write_all(&json)?;
    f.sync_all()?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// The name of the store's directory of scratch directories: `$KILN_HOME/scratch`.
pub const SCRATCH: &str = "scratch";

/// One run's scratch directory, `$KILN_HOME/scratch/<run-id>/` (spec §5.3, rev
/// 2.9): it holds the scratch disk, on the store's filesystem rather than in the
/// run directory's tmpfs, and its own identity, `scratch.json`
/// ([`ScratchInfo`]), so it can be cleaned up when its run directory is gone
/// (the tmpfs was emptied by a reboot or a logout). Removed when dropped unless
/// kept.
#[derive(Debug)]
pub struct ScratchDir {
    pub path: PathBuf,
    keep: bool,
}

/// `scratch.json`: whose a scratch directory is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScratchInfo {
    pub version: u32,
    pub id: String,
    pub boot_id: String,
    pub hostname: String,
    /// The `kiln` process that owns the run.
    pub kiln: Identity,
    /// The VMM, once it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmm: Option<Identity>,
    /// Kept for debugging (`KILN_KEEP_RUN_DIR=1`): never cleaned up by later runs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep: bool,
}

impl From<&RunInfo> for ScratchInfo {
    fn from(r: &RunInfo) -> Self {
        Self {
            version: RUN_INFO_VERSION,
            id: r.id.clone(),
            boot_id: r.boot_id.clone(),
            hostname: r.hostname.clone(),
            kiln: r.kiln,
            vmm: r.vmm,
            keep: r.keep,
        }
    }
}

impl ScratchDir {
    /// A fresh directory `<base>/<id>` (see [`scratch_base`]), mode 0700, with its
    /// identity written at once.
    pub fn create(base: &Path, info: &ScratchInfo) -> Result<Self> {
        let path = base.join(&info.id);
        dir_builder().create(&path)?;
        let dir = Self { path, keep: false };
        dir.write_info(info)?;
        Ok(dir)
    }

    /// Replaces `scratch.json` atomically (once the VMM's identity is known).
    pub fn write_info(&self, info: &ScratchInfo) -> Result<()> {
        write_json(&self.path, "scratch.json", info)
    }

    /// Leaves the directory in place (`KILN_KEEP_RUN_DIR=1`).
    pub fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// `<store root>/scratch`: created 0700 if missing, and refused unless it is a real
/// directory owned by this user and closed to everyone else (as [`base_dir`]).
pub fn scratch_base(store_root: &Path) -> Result<PathBuf> {
    let base = store_root.join(SCRATCH);
    secure(&base, rustix::process::getuid().as_raw(), "scratch directory")?;
    Ok(base)
}

fn dir_builder() -> DirBuilder {
    let mut b = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b
}

/// `$XDG_RUNTIME_DIR/kiln`, else `/tmp/kiln-<uid>`: created 0700 if missing, and
/// refused unless it is a real directory (not a symlink) owned by this user and
/// closed to everyone else.
pub fn base_dir() -> Result<PathBuf> {
    let uid = rustix::process::getuid().as_raw();
    let base = base_path(std::env::var_os("XDG_RUNTIME_DIR"), uid);
    secure_dir(&base, uid)?;
    Ok(base)
}

/// Where [`base_dir`] is, given `XDG_RUNTIME_DIR`. As the XDG spec says, a relative
/// value is invalid and ignored.
fn base_path(xdg_runtime_dir: Option<std::ffi::OsString>, uid: u32) -> PathBuf {
    match xdg_runtime_dir.map(PathBuf::from).filter(|p| p.is_absolute()) {
        Some(x) => x.join("kiln"),
        None => PathBuf::from(format!("/tmp/kiln-{uid}")),
    }
}

/// Creates `dir` 0700 if missing; then checks it without following a symlink.
pub fn secure_dir(dir: &Path, uid: u32) -> Result<()> {
    secure(dir, uid, "run directory")
}

fn secure(dir: &Path, uid: u32, what: &str) -> Result<()> {
    match dir_builder().create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    use rustix::fs::{FileType, Mode, OFlags};
    let fd = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| {
        Error::refused(format!(
            "{what} {}: {} (it must be a directory, not a symlink)",
            dir.display(),
            std::io::Error::from(e)
        ))
    })?;
    let st = rustix::fs::fstat(&fd).map_err(std::io::Error::from)?;
    let mode = st.st_mode as u32;
    if FileType::from_raw_mode(st.st_mode as _) != FileType::Directory || st.st_uid != uid || mode & 0o077 != 0 {
        return Err(Error::refused(format!(
            "{what} {} must be a directory owned by uid {uid} with mode 0700 (it is owned by {} with mode {:o})",
            dir.display(),
            st.st_uid,
            mode & 0o7777
        )));
    }
    Ok(())
}

fn random_id() -> Result<String> {
    let mut b = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// This boot's id (Linux), or empty where there is none.
pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub fn hostname() -> String {
    rustix::system::uname().nodename().to_string_lossy().into_owned()
}

/// The identity of a live process (Linux; `None` elsewhere or when it is gone).
pub fn identity(pid: u32) -> Option<Identity> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_start_time(&stat).map(|start_time| Identity { pid, start_time })
}

/// Field 22 of `/proc/<pid>/stat`, counted after the command name's closing `)`
/// (which may itself contain spaces and parentheses).
fn parse_start_time(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Whether `id` still names a running process. Only a process that is gone
/// (`/proc/<pid>` missing: ENOENT or ESRCH) or a different start time (the pid was
/// reused) counts as dead; anything that cannot be read (EMFILE, `hidepid`, another
/// PID namespace) counts as alive, so a live run's directory is never removed.
pub fn alive(id: &Identity) -> bool {
    alive_in(Path::new("/proc"), id)
}

fn alive_in(proc_root: &Path, id: &Identity) -> bool {
    match std::fs::read_to_string(proc_root.join(id.pid.to_string()).join("stat")) {
        Ok(stat) => parse_start_time(&stat).is_none_or(|t| t == id.start_time),
        Err(e) => {
            !(e.kind() == std::io::ErrorKind::NotFound
                || e.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()))
        }
    }
}

/// What [`clean_stale`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cleanup {
    pub removed: Vec<String>,
    /// From another boot or host: left alone.
    pub foreign: Vec<String>,
}

impl Cleanup {
    /// The note for the user about run directories left alone (spec §5.3), if any.
    pub fn notice(&self, base: &Path) -> Option<String> {
        self.note(base, "run", "belong", "to another boot or host")
    }

    /// The note for the user about scratch directories left alone, if any.
    pub fn scratch_notice(&self, base: &Path) -> Option<String> {
        self.note(
            base,
            "scratch",
            "belong",
            "to another host or have no identity kiln can read",
        )
    }

    fn note(&self, base: &Path, what: &str, verb: &str, whose: &str) -> Option<String> {
        if self.foreign.is_empty() {
            return None;
        }
        let mut names = self.foreign.clone();
        names.sort();
        let one = names.len() == 1;
        Some(format!(
            "note: {} {what} director{} in {} {verb}{} {whose} and {} left alone: {}",
            names.len(),
            if one { "y" } else { "ies" },
            base.display(),
            if one { "s" } else { "" },
            if one { "was" } else { "were" },
            names.join(", ")
        ))
    }
}

/// Removes run directories under `base` that this host's current boot made and
/// whose processes are all gone, each with the scratch directory its `run.json`
/// records. Only real directories are considered; those without a readable
/// `run.json` of this version (a run may be starting, or a newer kiln made it) are
/// left alone, and `run.json` is never read through a symlink. A run directory
/// stays while its scratch directory could not be removed, so a later run retries.
pub fn clean_stale(base: &Path, boot_id: &str, hostname: &str, alive: impl Fn(&Identity) -> bool) -> Cleanup {
    let uid = rustix::process::getuid().as_raw();
    let mut report = Cleanup::default();
    let Ok(entries) = std::fs::read_dir(base) else {
        return report;
    };
    for entry in entries.flatten() {
        // Not following a symlink: only directories are runs.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(bytes) = read_run_json(&entry.path().join("run.json")) else {
            continue;
        };
        let Ok(info) = serde_json::from_slice::<RunInfo>(&bytes) else {
            continue;
        };
        if info.version != RUN_INFO_VERSION {
            continue;
        }
        if info.boot_id != boot_id || info.hostname != hostname || boot_id.is_empty() {
            report.foreign.push(name);
            continue;
        }
        if info.keep || alive(&info.kiln) || info.vmm.as_ref().is_some_and(&alive) {
            continue;
        }
        if !info
            .scratch_dir
            .as_deref()
            .is_none_or(|s| remove_scratch(Path::new(s), &info.id, uid))
        {
            continue;
        }
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            report.removed.push(name);
        }
    }
    report
}

/// How long a scratch directory without a readable `scratch.json` is assumed to
/// be one a run is creating right now.
pub const SCRATCH_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Removes scratch directories under `base` (`$KILN_HOME/scratch`) whose runs are
/// over, whether or not their run directories still exist (a reboot or a logout
/// empties the run directories' tmpfs). A directory is removed when its
/// `scratch.json` names this host and either another boot (every process of that
/// boot is gone) or this boot with kiln and the VMM it names both dead, unless it
/// was kept. Only real directories of this user's are considered, and
/// `scratch.json` is never read through a symlink. Directories of other hosts (a
/// store shared over NFS), and those whose `scratch.json` cannot be read once
/// they are older than [`SCRATCH_GRACE`], are left alone and reported; younger
/// ones may be a run that is being set up, and are skipped.
pub fn clean_stale_scratch(
    base: &Path,
    boot_id: &str,
    hostname: &str,
    alive: impl Fn(&Identity) -> bool,
    now: std::time::SystemTime,
) -> Cleanup {
    use std::os::unix::fs::MetadataExt;
    let uid = rustix::process::getuid().as_raw();
    let mut report = Cleanup::default();
    let Ok(entries) = std::fs::read_dir(base) else {
        return report;
    };
    for entry in entries.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.is_dir() || meta.uid() != uid {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let info = read_run_json(&entry.path().join("scratch.json"))
            .and_then(|b| serde_json::from_slice::<ScratchInfo>(&b).ok())
            .filter(|i| i.version == RUN_INFO_VERSION && i.id == name);
        let Some(info) = info else {
            let young = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_none_or(|age| age < SCRATCH_GRACE);
            if !young {
                report.foreign.push(name);
            }
            continue;
        };
        if info.hostname != hostname || boot_id.is_empty() {
            report.foreign.push(name);
            continue;
        }
        let over = info.boot_id != boot_id || !(alive(&info.kiln) || info.vmm.as_ref().is_some_and(&alive));
        if info.keep || !over {
            continue;
        }
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            report.removed.push(name);
        }
    }
    report
}

/// Removes a stale run's scratch directory; false when it is still there. Only a
/// path of the shape kiln makes, `/…/scratch/<id>` naming a real directory of this
/// user's, is removed, so `run.json` cannot point the cleanup at anything else;
/// any other path counts as gone.
fn remove_scratch(path: &Path, id: &str, uid: u32) -> bool {
    use std::ffi::OsStr;
    use std::os::unix::fs::MetadataExt;
    let shaped = path.is_absolute()
        && path.components().all(|c| !matches!(c, std::path::Component::ParentDir))
        && path.file_name() == Some(OsStr::new(id))
        && path.parent().and_then(Path::file_name) == Some(OsStr::new(SCRATCH));
    if !shaped {
        return true;
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && m.uid() == uid => std::fs::remove_dir_all(path).is_ok(),
        Ok(_) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// The most of `run.json` read: it is a few hundred bytes.
const MAX_RUN_JSON: u64 = 64 * 1024;

/// `run.json`'s bytes, unless it is missing, a symlink, not a regular file or too big.
fn read_run_json(path: &Path) -> Option<Vec<u8>> {
    let mut o = OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
    }
    let f = o.open(path).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    f.take(MAX_RUN_JSON + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= MAX_RUN_JSON).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, boot: &str, host: &str, pid: u32) -> RunInfo {
        RunInfo {
            version: 1,
            id: id.into(),
            boot_id: boot.into(),
            hostname: host.into(),
            kiln: Identity { pid, start_time: 1 },
            vmm: None,
            vmm_kind: "firecracker".into(),
            image: "img".into(),
            custom_kernel: None,
            custom_init: None,
            keep: false,
            scratch_dir: None,
        }
    }

    #[test]
    fn stale_runs_of_this_boot_are_removed_and_others_kept() {
        let base = tempfile::tempdir().unwrap();
        let mk = |i: &RunInfo| {
            let d = RunDir {
                id: i.id.clone(),
                path: base.path().join(&i.id),
                keep: true,
            };
            std::fs::create_dir(&d.path).unwrap();
            d.write_info(i).unwrap();
        };
        mk(&info("dead", "b", "h", 1));
        mk(&info("live", "b", "h", 2));
        mk(&info("other-boot", "b2", "h", 1));
        mk(&info("other-host", "b", "h2", 1));
        mk(&RunInfo {
            keep: true,
            ..info("kept", "b", "h", 1)
        });
        std::fs::create_dir(base.path().join("starting")).unwrap();
        let r = clean_stale(base.path(), "b", "h", |id| id.pid == 2);
        assert_eq!(r.removed, ["dead"]);
        let mut foreign = r.foreign.clone();
        foreign.sort();
        assert_eq!(foreign, ["other-boot", "other-host"]);
        for kept in ["live", "other-boot", "other-host", "starting", "kept"] {
            assert!(base.path().join(kept).exists(), "{kept}");
        }
        // Without a boot id nothing counts as this boot's.
        assert!(clean_stale(base.path(), "", "h", |_| false).removed.is_empty());
        // The foreign ones are reported.
        let note = r.notice(Path::new("/b")).unwrap();
        assert_eq!(
            note,
            "note: 2 run directories in /b belong to another boot or host and were left alone: \
             other-boot, other-host"
        );
        assert_eq!(Cleanup::default().notice(Path::new("/b")), None);
    }

    /// Only real directories with a regular `run.json` of this version are runs:
    /// a symlinked run.json, a symlink to a run directory and another version are
    /// left alone, and only directories are reported as foreign.
    #[cfg(unix)]
    #[test]
    fn stale_cleanup_follows_no_symlinks_and_checks_the_version() {
        use std::os::unix::fs::symlink;
        let base = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let mk = |dir: &Path, i: &RunInfo| {
            std::fs::create_dir(dir).unwrap();
            std::fs::write(dir.join("run.json"), serde_json::to_vec(i).unwrap()).unwrap();
        };
        // A dead run's run.json, reached through a symlink.
        mk(&elsewhere.path().join("real"), &info("real", "b", "h", 1));
        std::fs::create_dir(base.path().join("linked-json")).unwrap();
        symlink(
            elsewhere.path().join("real/run.json"),
            base.path().join("linked-json/run.json"),
        )
        .unwrap();
        // A symlink to a dead run's directory, and one to another boot's.
        symlink(elsewhere.path().join("real"), base.path().join("linked-dir")).unwrap();
        mk(&elsewhere.path().join("other"), &info("other", "b2", "h", 1));
        symlink(elsewhere.path().join("other"), base.path().join("linked-foreign")).unwrap();
        // A file, and a dead run of another run.json version.
        std::fs::write(base.path().join("file"), b"x").unwrap();
        mk(
            &base.path().join("v2"),
            &RunInfo {
                version: 2,
                ..info("v2", "b", "h", 1)
            },
        );
        let r = clean_stale(base.path(), "b", "h", |_| false);
        assert_eq!(r, Cleanup::default());
        for kept in ["linked-json", "linked-dir", "linked-foreign", "file", "v2"] {
            assert!(base.path().join(kept).symlink_metadata().is_ok(), "{kept}");
        }
        assert!(elsewhere.path().join("real/run.json").exists());
    }

    /// A stale run's scratch directory goes with it; a live or kept run's stays, and
    /// a run.json naming anything but `/…/scratch/<id>` removes nothing else.
    #[test]
    fn stale_runs_take_their_scratch_directories_with_them() {
        let base = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let scratch = scratch_base(store.path()).unwrap();
        let other = tempfile::tempdir().unwrap();
        let mk = |i: &RunInfo| {
            std::fs::create_dir(base.path().join(&i.id)).unwrap();
            std::fs::write(base.path().join(&i.id).join("run.json"), serde_json::to_vec(i).unwrap()).unwrap();
        };
        let with_scratch = |i: RunInfo| {
            let s = ScratchDir::create(&scratch, &ScratchInfo::from(&i)).unwrap();
            std::fs::write(s.path.join("scratch.img"), b"disk").unwrap();
            let mut s = s;
            s.keep();
            RunInfo {
                scratch_dir: Some(s.path.display().to_string()),
                ..i
            }
        };
        mk(&with_scratch(info("dead", "b", "h", 1)));
        mk(&with_scratch(info("live", "b", "h", 2)));
        mk(&with_scratch(RunInfo {
            keep: true,
            ..info("kept", "b", "h", 1)
        }));
        // Recorded but never made (kiln died in between).
        mk(&RunInfo {
            scratch_dir: Some(scratch.join("unmade").display().to_string()),
            ..info("unmade", "b", "h", 1)
        });
        // Not of kiln's shape: another directory, and a scratch path of another id.
        std::fs::write(other.path().join("precious"), b"x").unwrap();
        mk(&RunInfo {
            scratch_dir: Some(other.path().display().to_string()),
            ..info("odd", "b", "h", 1)
        });
        std::fs::create_dir(scratch.join("someone")).unwrap();
        mk(&RunInfo {
            scratch_dir: Some(scratch.join("someone").display().to_string()),
            ..info("wrong-id", "b", "h", 1)
        });
        let mut r = clean_stale(base.path(), "b", "h", |id| id.pid == 2);
        r.removed.sort();
        assert_eq!(r.removed, ["dead", "odd", "unmade", "wrong-id"]);
        assert!(!scratch.join("dead").exists());
        for kept in ["live", "kept", "someone"] {
            assert!(scratch.join(kept).exists(), "{kept}");
        }
        assert!(other.path().join("precious").exists());
        // The scratch base is private, and refused when it is not.
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&scratch).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(scratch_base(store.path()).is_err());
        // A scratch directory is removed when dropped.
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
        let x = ScratchInfo::from(&info("x", "b", "h", 1));
        let s = ScratchDir::create(&scratch, &x).unwrap();
        let path = s.path.clone();
        assert!(ScratchDir::create(&scratch, &x).is_err(), "a run id is never reused");
        drop(s);
        assert!(!path.exists());
    }

    /// Scratch directories are swept by their own identity, with or without a run
    /// directory: another boot of this host, or this boot with kiln and the VMM
    /// dead, is removed; a live run's, a kept one, another host's and one being
    /// created stay, and those that cannot be identified are reported.
    #[test]
    fn scratch_directories_are_swept_by_their_own_identity() {
        use std::time::{Duration, SystemTime};
        let store = tempfile::tempdir().unwrap();
        let scratch = scratch_base(store.path()).unwrap();
        let mk = |i: RunInfo, vmm: Option<u32>| {
            let mut s = ScratchDir::create(&scratch, &ScratchInfo::from(&i)).unwrap();
            if let Some(pid) = vmm {
                s.write_info(&ScratchInfo {
                    vmm: Some(Identity { pid, start_time: 1 }),
                    ..ScratchInfo::from(&i)
                })
                .unwrap();
            }
            std::fs::write(s.path.join("scratch.img"), b"disk").unwrap();
            s.keep();
        };
        // Live pids are 2 and 3; every other pid is dead.
        mk(info("dead", "b", "h", 1), None);
        mk(info("dead-vmm-too", "b", "h", 1), Some(4));
        mk(info("old-boot", "b0", "h", 2), Some(3));
        mk(info("live", "b", "h", 2), None);
        mk(info("live-vmm", "b", "h", 1), Some(3));
        mk(
            RunInfo {
                keep: true,
                ..info("kept", "b", "h", 1)
            },
            None,
        );
        mk(info("other-host", "b", "h2", 1), None);
        // Being created (no identity yet), and old with an unreadable identity.
        std::fs::create_dir(scratch.join("creating")).unwrap();
        std::fs::create_dir(scratch.join("unknown")).unwrap();
        std::fs::write(scratch.join("unknown/scratch.json"), b"{").unwrap();
        // Not directories: left alone, not reported.
        std::fs::write(scratch.join("file"), b"x").unwrap();
        std::os::unix::fs::symlink(store.path(), scratch.join("link")).unwrap();
        let alive = |id: &Identity| id.pid == 2 || id.pid == 3;

        let now = SystemTime::now();
        let mut r = clean_stale_scratch(&scratch, "b", "h", alive, now);
        r.removed.sort();
        r.foreign.sort();
        assert_eq!(r.removed, ["dead", "dead-vmm-too", "old-boot"]);
        assert_eq!(r.foreign, ["other-host"], "the young unidentified ones are skipped");
        for kept in [
            "live",
            "live-vmm",
            "kept",
            "other-host",
            "creating",
            "unknown",
            "file",
            "link",
        ] {
            assert!(scratch.join(kept).symlink_metadata().is_ok(), "{kept}");
        }
        assert!(store.path().join("scratch").exists());

        // A minute later the unidentified ones are reported, never removed.
        let later = now + SCRATCH_GRACE + Duration::from_secs(1);
        let mut r = clean_stale_scratch(&scratch, "b", "h", alive, later);
        r.foreign.sort();
        assert!(r.removed.is_empty(), "{r:?}");
        assert_eq!(r.foreign, ["creating", "other-host", "unknown"]);
        assert_eq!(
            r.scratch_notice(Path::new("/s")).unwrap(),
            "note: 3 scratch directories in /s belong to another host or have no identity kiln can read and \
             were left alone: creating, other-host, unknown"
        );
        // Without a boot id nothing is judged.
        assert!(
            clean_stale_scratch(&scratch, "", "h", |_| false, now)
                .removed
                .is_empty()
        );
    }

    #[test]
    fn only_a_vanished_process_or_another_start_time_is_dead() {
        let root = tempfile::tempdir().unwrap();
        let id = Identity {
            pid: 42,
            start_time: 98765,
        };
        let stat = |start: u64| format!("42 (sh) S 1 42 42 0 -1 4194560 100 0 0 0 0 0 0 0 20 0 1 0 {start} 1000 10");
        assert!(!alive_in(root.path(), &id), "no /proc/42: gone");
        std::fs::create_dir(root.path().join("42")).unwrap();
        std::fs::write(root.path().join("42/stat"), stat(98765)).unwrap();
        assert!(alive_in(root.path(), &id));
        std::fs::write(root.path().join("42/stat"), stat(1)).unwrap();
        assert!(!alive_in(root.path(), &id), "the pid was reused");
        // Unreadable (here a directory: EISDIR; in life EMFILE, EACCES): alive.
        std::fs::remove_file(root.path().join("42/stat")).unwrap();
        std::fs::create_dir(root.path().join("42/stat")).unwrap();
        assert!(alive_in(root.path(), &id));
    }

    #[test]
    fn a_relative_xdg_runtime_dir_is_ignored() {
        assert_eq!(base_path(Some("/run/user/7".into()), 7), Path::new("/run/user/7/kiln"));
        for bad in [None, Some("".into()), Some("relative/dir".into())] {
            assert_eq!(base_path(bad, 7), Path::new("/tmp/kiln-7"));
        }
    }

    #[test]
    fn start_time_parsing_survives_odd_names() {
        let stat = "123 (a) b (c)) S 1 123 123 0 -1 4194560 100 0 0 0 0 0 0 0 20 0 1 0 98765 1000 10";
        assert_eq!(parse_start_time(stat), Some(98765));
        assert_eq!(parse_start_time("garbage"), None);
    }

    #[cfg(unix)]
    #[test]
    fn run_directories_are_private_and_symlinks_refused() {
        use std::os::unix::fs::PermissionsExt;
        let uid = rustix::process::getuid().as_raw();
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("kiln");
        secure_dir(&base, uid).unwrap();
        assert_eq!(std::fs::metadata(&base).unwrap().permissions().mode() & 0o777, 0o700);
        let run = RunDir::create(&base).unwrap();
        assert_eq!(run.id.len(), 16);
        assert_eq!(
            std::fs::metadata(&run.path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let path = run.path.clone();
        drop(run);
        assert!(!path.exists());
        // An open directory or a symlink is refused.
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(secure_dir(&base, uid).is_err());
        let link = t.path().join("link");
        std::os::unix::fs::symlink(t.path(), &link).unwrap();
        assert!(secure_dir(&link, uid).is_err());
        assert!(secure_dir(&t.path().join("x"), uid + 1).is_err());
    }
}
