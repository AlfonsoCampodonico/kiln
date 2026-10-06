//! Per-run state (spec §5.3, T7): `$XDG_RUNTIME_DIR/kiln/<run-id>/`, mode 0700, or
//! `/tmp/kiln-<uid>/<run-id>/` without `XDG_RUNTIME_DIR`. Each directory holds
//! `run.json`, the VMM's sockets (`sock/`), its logs, the scratch disk (unless
//! persisted) and `console.log`.
//!
//! A directory is cleaned up by the run that made it. One left behind (kiln was
//! killed) is removed by a later run only when its `run.json` names this host's
//! boot and hostname and neither kiln nor the VMM it names is alive: entries from
//! other boots or hosts (a shared `/tmp`) are never touched ([`Cleanup::foreign`]
//! lists them).

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
        let path = self.path.join("run.json");
        let tmp = self.path.join("run.json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let json = serde_json::to_vec_pretty(info).map_err(|e| Error::invalid("run.json", e.to_string()))?;
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
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
            "run directory {}: {} (it must be a directory, not a symlink)",
            dir.display(),
            std::io::Error::from(e)
        ))
    })?;
    let st = rustix::fs::fstat(&fd).map_err(std::io::Error::from)?;
    let mode = st.st_mode as u32;
    if FileType::from_raw_mode(st.st_mode as _) != FileType::Directory || st.st_uid != uid || mode & 0o077 != 0 {
        return Err(Error::refused(format!(
            "run directory {} must be a directory owned by uid {uid} with mode 0700 (it is owned by {} with mode {:o})",
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

/// Removes run directories under `base` that this host's current boot made and
/// whose processes are all gone. Only real directories are considered; those
/// without a readable `run.json` of this version (a run may be starting, or a
/// newer kiln made it) are left alone, and `run.json` is never read through a
/// symlink.
pub fn clean_stale(base: &Path, boot_id: &str, hostname: &str, alive: impl Fn(&Identity) -> bool) -> Cleanup {
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
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            report.removed.push(name);
        }
    }
    report
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
