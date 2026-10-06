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

    pub fn write_info(&self, info: &RunInfo) -> Result<()> {
        let path = self.path.join("run.json");
        let tmp = self.path.join("run.json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(info).expect("serialisable"))?;
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
    let base = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join("kiln"),
        None => PathBuf::from(format!("/tmp/kiln-{uid}")),
    };
    secure_dir(&base, uid)?;
    Ok(base)
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

/// Whether `id` still names a running process.
pub fn alive(id: &Identity) -> bool {
    identity(id.pid) == Some(*id)
}

/// What [`clean_stale`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cleanup {
    pub removed: Vec<String>,
    /// From another boot or host: left alone.
    pub foreign: Vec<String>,
}

/// Removes run directories under `base` that this host's current boot made and
/// whose processes are all gone. Directories without a readable `run.json` are
/// left alone (a run may be starting).
pub fn clean_stale(base: &Path, boot_id: &str, hostname: &str, alive: impl Fn(&Identity) -> bool) -> Cleanup {
    let mut report = Cleanup::default();
    let Ok(entries) = std::fs::read_dir(base) else {
        return report;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(bytes) = std::fs::read(entry.path().join("run.json")) else {
            continue;
        };
        let Ok(info) = serde_json::from_slice::<RunInfo>(&bytes) else {
            continue;
        };
        if info.boot_id != boot_id || info.hostname != hostname || boot_id.is_empty() {
            report.foreign.push(name);
            continue;
        }
        if info.keep || alive(&info.kiln) || info.vmm.as_ref().is_some_and(&alive) {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) && std::fs::remove_dir_all(entry.path()).is_ok() {
            report.removed.push(name);
        }
    }
    report
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
