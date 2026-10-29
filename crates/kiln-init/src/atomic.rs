//! A files volume in kubelet's atomic-writer layout (kiln-shim exec and volumes design
//! §3.1): the files live in a generation directory `..kiln.<n>`, `..data` links to the
//! current one, and each top-level name `x` is a link `x -> ..data/x`. A new generation
//! is written beside the current one and swapped in with one `rename` of `..data`, so a
//! reader sees either the old files or the new ones, never a mix, as with kubelet.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use kiln_proto::{FileUpdate, MAX_VOLUME_BYTES, MAX_VOLUME_FILES};

use crate::error::{Context, Failure, Result};

pub const DATA: &str = "..data";
const DATA_TMP: &str = "..data_tmp";

/// One files volume, rooted at `base` (a directory only init writes).
#[derive(Debug)]
pub struct Writer {
    base: PathBuf,
    generation: u64,
    current: Option<String>,
    pending: Option<Pending>,
}

#[derive(Debug)]
struct Pending {
    dir: String,
    files: Vec<String>,
    bytes: u64,
}

impl Writer {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            generation: 0,
            current: None,
            pending: None,
        }
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Writes `u`'s files into the generation being built (starting one if none is);
    /// with `commit`, swaps it in. True when a generation was committed.
    pub fn apply(&mut self, u: &FileUpdate) -> Result<bool> {
        if self.pending.is_none() {
            self.generation += 1;
            let dir = format!("..kiln.{}", self.generation);
            let path = self.base.join(&dir);
            let _ = fs::remove_dir_all(&path);
            mkdir(&path)?;
            self.pending = Some(Pending {
                dir,
                files: Vec::new(),
                bytes: 0,
            });
        }
        let p = self.pending.as_mut().expect("a generation is being built");
        let root = self.base.join(&p.dir);
        for f in &u.files {
            p.bytes += f.data.len() as u64;
            if p.bytes > MAX_VOLUME_BYTES {
                return Err(Failure::msg(format!(
                    "a files volume's update is over {MAX_VOLUME_BYTES} bytes"
                )));
            }
            let path = root.join(&f.path);
            if f.append {
                if p.files.last() != Some(&f.path) {
                    return Err(Failure::msg(format!(
                        "protocol violation: FileUpdate appends to {:?}, which it did not just write",
                        f.path
                    )));
                }
                let mut file = OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .context(format!("open {}", f.path))?;
                file.write_all(&f.data).context(format!("write {}", f.path))?;
                continue;
            }
            if p.files.contains(&f.path) {
                return Err(Failure::msg(format!(
                    "protocol violation: FileUpdate writes {:?} twice",
                    f.path
                )));
            }
            p.files.push(f.path.clone());
            if p.files.len() > MAX_VOLUME_FILES {
                return Err(Failure::msg(format!(
                    "a files volume's update has over {MAX_VOLUME_FILES} files"
                )));
            }
            if let Some(parent) = path.parent() {
                mkdir_all(&root, parent)?;
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .context(format!("create {}", f.path))?;
            file.write_all(&f.data).context(format!("write {}", f.path))?;
            // Owner first: chown clears the set-id bits a mode may carry.
            std::os::unix::fs::fchown(&file, Some(f.uid), Some(f.gid)).context(format!("chown {}", f.path))?;
            file.set_permissions(fs::Permissions::from_mode(f.mode))
                .context(format!("chmod {}", f.path))?;
        }
        if !u.commit {
            return Ok(false);
        }
        let p = self.pending.take().expect("a generation is being built");
        self.swap(p)?;
        Ok(true)
    }

    /// kubelet's swap: `..data_tmp` to the new generation, renamed over `..data`; then
    /// the top-level links for new names, stale ones removed, the old generation removed.
    fn swap(&mut self, p: Pending) -> Result<()> {
        let tmp = self.base.join(DATA_TMP);
        let _ = fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&p.dir, &tmp).context("link ..data_tmp")?;
        fs::rename(&tmp, self.base.join(DATA)).context("rename ..data_tmp to ..data")?;
        let mut names: Vec<&str> = p.files.iter().map(|f| f.split('/').next().unwrap_or(f)).collect();
        names.sort_unstable();
        names.dedup();
        for name in &names {
            let link = self.base.join(name);
            if fs::symlink_metadata(&link).is_err() {
                std::os::unix::fs::symlink(Path::new(DATA).join(name), &link).context(format!("link {name}"))?;
            }
        }
        for entry in fs::read_dir(&self.base).context("read a files volume")? {
            let entry = entry.context("read a files volume")?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("..") && !names.contains(&name.as_ref()) {
                fs::remove_file(entry.path()).context(format!("remove the stale link {name}"))?;
            }
        }
        if let Some(old) = self.current.replace(p.dir) {
            fs::remove_dir_all(self.base.join(old)).context("remove the old generation")?;
        }
        Ok(())
    }

    /// The volume's one file (`subPath` volumes): its path through `..data`.
    pub fn single_file(&self) -> Result<PathBuf> {
        let dir = self
            .current
            .as_ref()
            .ok_or_else(|| Failure::msg("a single-file volume has no content"))?;
        let mut entries = fs::read_dir(self.base.join(dir))
            .context("read a single-file volume")?
            .collect::<std::io::Result<Vec<_>>>()
            .context("read a single-file volume")?;
        match (entries.pop(), entries.is_empty()) {
            (Some(e), true) if e.file_type().is_ok_and(|t| t.is_file()) => Ok(self.base.join(DATA).join(e.file_name())),
            _ => Err(Failure::msg("a single-file volume must hold exactly one file")),
        }
    }
}

fn mkdir(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(Failure::os(format!("mkdir {}", path.display()), &e)),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).context(format!("chmod {}", path.display()))
}

/// The directories from `root` (exclusive) down to `dir`, each `0755`.
fn mkdir_all(root: &Path, dir: &Path) -> Result<()> {
    let Ok(rel) = dir.strip_prefix(root) else {
        return Ok(());
    };
    let mut at = root.to_path_buf();
    for c in rel.components() {
        at.push(c);
        mkdir(&at)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use kiln_proto::{VolumeFile, file_updates};

    use super::*;

    fn owner(dir: &Path) -> (u32, u32) {
        let m = fs::metadata(dir).unwrap();
        (m.uid(), m.gid())
    }

    fn files(dir: &Path, list: &[(&str, &str, u32)]) -> Vec<VolumeFile> {
        let (uid, gid) = owner(dir);
        list.iter()
            .map(|(p, d, mode)| VolumeFile {
                path: p.to_string(),
                mode: *mode,
                uid,
                gid,
                data: d.as_bytes().to_vec(),
            })
            .collect()
    }

    fn write(w: &mut Writer, f: &[VolumeFile]) {
        let frames = file_updates(0, f);
        let n = frames.len();
        for (i, u) in frames.iter().enumerate() {
            assert_eq!(w.apply(u).unwrap(), i + 1 == n);
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn generations_swap_like_kubelets() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let mut w = Writer::new(base);
        write(
            &mut w,
            &files(base, &[("app.conf", "v1", 0o644), ("tls/key", "k", 0o400)]),
        );
        assert_eq!(names(base), ["..data", "..kiln.1", "app.conf", "tls"]);
        assert_eq!(fs::read_to_string(base.join("app.conf")).unwrap(), "v1");
        assert_eq!(
            fs::read_link(base.join("app.conf")).unwrap(),
            Path::new("..data/app.conf")
        );
        assert_eq!(fs::read_link(base.join("..data")).unwrap(), Path::new("..kiln.1"));
        let key = fs::metadata(base.join("tls/key")).unwrap();
        assert_eq!(key.mode() & 0o7777, 0o400);
        assert_eq!(fs::metadata(base.join("..kiln.1/tls")).unwrap().mode() & 0o777, 0o755);

        // An update: app.conf changes, tls goes, new appears.
        write(&mut w, &files(base, &[("app.conf", "v2", 0o644), ("new", "n", 0o644)]));
        assert_eq!(names(base), ["..data", "..kiln.2", "app.conf", "new"]);
        assert_eq!(fs::read_to_string(base.join("app.conf")).unwrap(), "v2");
        assert_eq!(fs::read_to_string(base.join("new")).unwrap(), "n");
    }

    #[test]
    fn large_files_span_frames_and_one_file_volumes_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let mut w = Writer::new(base);
        let big = "x".repeat(300 << 10);
        write(&mut w, &files(base, &[("nginx.conf", &big, 0o644)]));
        assert_eq!(fs::read_to_string(base.join("nginx.conf")).unwrap(), big);
        assert_eq!(w.single_file().unwrap(), base.join("..data/nginx.conf"));
        write(&mut w, &files(base, &[("a", "", 0o644), ("b", "", 0o644)]));
        assert!(w.single_file().is_err());
    }

    #[test]
    fn bad_sequences_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let mut w = Writer::new(base);
        let mut frames = file_updates(0, &files(base, &[("a", "1", 0o644)]));
        frames[0].files[0].append = true;
        assert!(w.apply(&frames[0]).is_err(), "an append to nothing");
        let mut w = Writer::new(base);
        let mut u = file_updates(0, &files(base, &[("a", "1", 0o644)])).remove(0);
        u.files.push(u.files[0].clone());
        assert!(w.apply(&u).is_err(), "the same file twice");
    }
}
