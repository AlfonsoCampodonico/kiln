//! A pod's volumes inside the guest (kiln-shim exec and volumes design §2): what
//! `Config.volumes` declares, and the `FileUpdate` frames that carry a files volume's
//! content, at `Config` and whenever kubelet changes it.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{ProtoError, Result};

/// The most volumes one `Config` declares.
pub const MAX_VOLUMES: usize = 64;
/// The most bytes of file content in one generation of a files volume.
pub const MAX_VOLUME_BYTES: u64 = 8 << 20;
/// The most files in one generation of a files volume.
pub const MAX_VOLUME_FILES: usize = 1024;
/// The longest relative path inside a volume, in bytes.
pub const MAX_PATH_BYTES: usize = 4096;
/// The most file content one `FileUpdate` frame carries: base64 makes it 4/3 larger,
/// and the frame also holds the paths.
const CHUNK: usize = 40 << 10;
/// What one frame's entries may add up to, estimated from their data and paths.
const FRAME_BUDGET: usize = 60 << 10;

/// One mount in the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Volume {
    /// Where it is mounted, absolute in the container's root.
    pub path: String,
    pub kind: VolumeKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum VolumeKind {
    /// Files from `FileUpdate`, in kubelet's atomic-writer layout, mounted read-only.
    /// With `file`, the volume holds one file, mounted onto `path` (a `subPath`).
    Files {
        #[serde(default, skip_serializing_if = "is_false")]
        file: bool,
    },
    /// The directory `name` on the pod disk (`Config.pod_disk`).
    EmptyDir { name: String },
    /// A fresh tmpfs (`emptyDir` with `medium: Memory`, or an OCI tmpfs mount).
    Tmpfs {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size_bytes: Option<u64>,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Part of one generation of a files volume. A generation is the frames up to and
/// including the one with `commit`; then the guest swaps it in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileUpdate {
    /// The volume's index in `Config.volumes`.
    pub volume: u32,
    #[serde(default)]
    pub files: Vec<FileEntry>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub commit: bool,
}

/// One file, or with `append` more bytes of the file the previous entry named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileEntry {
    /// Relative to the volume; parent directories are implied.
    pub path: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    #[serde(serialize_with = "to_base64", deserialize_with = "from_base64")]
    pub data: Vec<u8>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub append: bool,
}

fn to_base64<S: Serializer>(data: &[u8], s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&STANDARD.encode(data))
}

fn from_base64<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<u8>, D::Error> {
    let text = String::deserialize(d)?;
    STANDARD.decode(text).map_err(serde::de::Error::custom)
}

/// A whole file of a volume, as the host read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VolumeFile {
    pub path: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    #[serde(serialize_with = "to_base64", deserialize_with = "from_base64")]
    pub data: Vec<u8>,
}

fn invalid(name: &'static str, reason: impl Into<String>) -> ProtoError {
    ProtoError::invalid(name, reason)
}

/// A path inside a volume: relative, its components neither empty, `.` nor `..`, no
/// NUL or control character, and its first component not starting with `..`, which
/// kubelet's layout reserves (`..data`, the generation directories).
pub fn check_relative_path(name: &'static str, path: &str) -> Result<()> {
    let bad = |why: &str| Err(invalid(name, format!("path {path:?}: {why}")));
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return bad("empty or too long");
    }
    if path.chars().any(char::is_control) {
        return bad("a control character");
    }
    if path.starts_with("..") {
        return bad("names starting with .. are reserved");
    }
    if path
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == ".." || c.len() > 255)
    {
        return bad("not a relative path of plain names");
    }
    Ok(())
}

/// An absolute path in the guest: no NUL or control character, no `.` or `..`
/// component, and not `/` itself.
pub fn check_absolute_path(name: &'static str, path: &str) -> Result<()> {
    match path.strip_prefix('/') {
        Some(rest) if !rest.is_empty() && check_components(rest) => Ok(()),
        _ => Err(invalid(
            name,
            format!("mount path {path:?} is not an absolute path below /"),
        )),
    }
}

fn check_components(rest: &str) -> bool {
    rest.len() < MAX_PATH_BYTES
        && !rest.chars().any(char::is_control)
        && rest
            .trim_end_matches('/')
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && c.len() <= 255)
}

impl Volume {
    pub fn validate(&self) -> Result<()> {
        check_absolute_path("Config", &self.path)?;
        match &self.kind {
            VolumeKind::Files { .. } => Ok(()),
            VolumeKind::EmptyDir { name } => {
                if name.contains('/') || check_relative_path("Config", name).is_err() {
                    return Err(invalid("Config", format!("emptyDir name {name:?}")));
                }
                Ok(())
            }
            VolumeKind::Tmpfs { size_bytes: Some(0) } => Err(invalid("Config", "a tmpfs of 0 bytes")),
            VolumeKind::Tmpfs { .. } => Ok(()),
        }
    }
}

impl FileUpdate {
    pub fn validate(&self) -> Result<()> {
        for f in &self.files {
            check_relative_path("FileUpdate", &f.path)?;
            if f.mode > 0o7777 {
                return Err(invalid("FileUpdate", format!("mode {:o}", f.mode)));
            }
        }
        Ok(())
    }
}

/// The frames that carry one whole generation of volume `volume`: files are split into
/// entries of at most 40 KiB (`append` for the rest of a file), entries are packed into
/// frames that fit [`crate::MAX_FRAME`], and the last frame commits.
pub fn file_updates(volume: u32, files: &[VolumeFile]) -> Vec<FileUpdate> {
    let mut frames = Vec::new();
    let mut current = FileUpdate {
        volume,
        files: Vec::new(),
        commit: false,
    };
    let mut used = 0;
    for f in files {
        let mut chunks: Vec<&[u8]> = f.data.chunks(CHUNK).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        for (i, chunk) in chunks.into_iter().enumerate() {
            // base64 is 4/3 of the data; a path can at worst double when JSON escapes it.
            let cost = chunk.len().div_ceil(3) * 4 + 2 * f.path.len() + 128;
            if used + cost > FRAME_BUDGET && !current.files.is_empty() {
                frames.push(std::mem::replace(
                    &mut current,
                    FileUpdate {
                        volume,
                        files: Vec::new(),
                        commit: false,
                    },
                ));
                used = 0;
            }
            used += cost;
            current.files.push(FileEntry {
                path: f.path.clone(),
                mode: f.mode,
                uid: f.uid,
                gid: f.gid,
                data: chunk.to_vec(),
                append: i > 0,
            });
        }
    }
    current.commit = true;
    frames.push(current);
    frames
}

/// The limits on one generation: at most [`MAX_VOLUME_FILES`] files and
/// [`MAX_VOLUME_BYTES`] bytes.
pub fn check_generation(files: &[VolumeFile]) -> Result<()> {
    let bytes: u64 = files.iter().map(|f| f.data.len() as u64).sum();
    if files.len() > MAX_VOLUME_FILES || bytes > MAX_VOLUME_BYTES {
        return Err(invalid(
            "FileUpdate",
            format!(
                "{} files of {bytes} bytes (at most {MAX_VOLUME_FILES} files and {MAX_VOLUME_BYTES} bytes)",
                files.len()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostMessage, Message, read_message, write_message};

    fn file(path: &str, len: usize) -> VolumeFile {
        VolumeFile {
            path: path.into(),
            mode: 0o644,
            uid: 0,
            gid: 2000,
            data: (0..len).map(|i| (i % 251) as u8).collect(),
        }
    }

    #[test]
    fn volumes_serialise_compactly() {
        let cases = [
            (VolumeKind::Files { file: false }, r#"{"files":{}}"#),
            (VolumeKind::Files { file: true }, r#"{"files":{"file":true}}"#),
            (
                VolumeKind::EmptyDir { name: "cache".into() },
                r#"{"emptyDir":{"name":"cache"}}"#,
            ),
            (
                VolumeKind::Tmpfs {
                    size_bytes: Some(1 << 20),
                },
                r#"{"tmpfs":{"sizeBytes":1048576}}"#,
            ),
            (VolumeKind::Tmpfs { size_bytes: None }, r#"{"tmpfs":{}}"#),
        ];
        for (kind, json) in cases {
            assert_eq!(serde_json::to_string(&kind).unwrap(), json);
            assert_eq!(serde_json::from_str::<VolumeKind>(json).unwrap(), kind);
        }
        assert!(serde_json::from_str::<VolumeKind>(r#"{"files":{"x":1}}"#).is_err());
        assert!(serde_json::from_str::<VolumeKind>(r#"{"hostPath":{}}"#).is_err());
    }

    #[test]
    fn paths_are_checked() {
        for ok in ["key", "a/b/c", "tls.crt", ".hidden", "a..b"] {
            check_relative_path("t", ok).unwrap();
        }
        for bad in [
            "",
            "/abs",
            "a//b",
            "a/./b",
            "a/../b",
            "..",
            "..data",
            "..2026_10_30",
            "a/",
            "a\nb",
            "a\0b",
        ] {
            assert!(check_relative_path("t", bad).is_err(), "{bad:?}");
        }
        assert!(check_relative_path("t", &"a".repeat(256)).is_err());
        for ok in ["/etc/config", "/var/run/secrets/kubernetes.io/serviceaccount", "/data/"] {
            check_absolute_path("t", ok).unwrap();
        }
        for bad in ["/", "relative", "/a/../b", "/a/./b", "/a//b", "", "/a\nb"] {
            assert!(check_absolute_path("t", bad).is_err(), "{bad:?}");
        }
        let v = |kind| Volume {
            path: "/x".into(),
            kind,
        };
        assert!(v(VolumeKind::EmptyDir { name: "a/b".into() }).validate().is_err());
        assert!(v(VolumeKind::EmptyDir { name: "..x".into() }).validate().is_err());
        assert!(v(VolumeKind::Tmpfs { size_bytes: Some(0) }).validate().is_err());
        v(VolumeKind::EmptyDir { name: "e".into() }).validate().unwrap();
    }

    #[test]
    fn updates_fit_frames_and_rebuild_the_files() {
        let files = vec![
            file("small", 10),
            file("empty", 0),
            file("big/blob", 200 << 10),
            file(&"p".repeat(200), 30 << 10),
        ];
        let frames = file_updates(3, &files);
        assert!(frames.len() > 1);
        assert!(frames.iter().all(|f| f.volume == 3));
        assert_eq!(frames.iter().filter(|f| f.commit).count(), 1);
        assert!(frames.last().unwrap().commit);
        // Every frame encodes within MAX_FRAME, and the files come back whole.
        let mut rebuilt: Vec<VolumeFile> = Vec::new();
        for f in frames {
            let mut buf = Vec::new();
            write_message(&mut buf, &HostMessage::FileUpdate(f)).unwrap();
            let Some(HostMessage::FileUpdate(back)) = read_message::<_, HostMessage>(&mut buf.as_slice()).unwrap()
            else {
                panic!("not a FileUpdate");
            };
            for e in back.files {
                if e.append {
                    let last = rebuilt.last_mut().unwrap();
                    assert_eq!(last.path, e.path);
                    last.data.extend(e.data);
                } else {
                    rebuilt.push(VolumeFile {
                        path: e.path,
                        mode: e.mode,
                        uid: e.uid,
                        gid: e.gid,
                        data: e.data,
                    });
                }
            }
        }
        assert_eq!(rebuilt, files);
        let none = file_updates(0, &[]);
        assert_eq!(none.len(), 1);
        assert!(none[0].commit && none[0].files.is_empty());
    }

    #[test]
    fn file_updates_are_validated() {
        let mut u = file_updates(0, &[file("a", 1)]).remove(0);
        HostMessage::FileUpdate(u.clone()).validate().unwrap();
        u.files[0].mode = 0o10000;
        assert!(HostMessage::FileUpdate(u.clone()).validate().is_err());
        u.files[0].mode = 0o600;
        u.files[0].path = "..data".into();
        assert!(HostMessage::FileUpdate(u).validate().is_err());
        let bad = br#"{"volume":0,"files":[{"path":"a","mode":420,"uid":0,"gid":0,"data":"!!"}]}"#;
        assert!(HostMessage::decode(13, bad).is_err(), "not base64");
    }

    #[test]
    fn generation_limits() {
        check_generation(&[file("a", 1 << 20)]).unwrap();
        assert!(check_generation(&[file("a", (8 << 20) + 1)]).is_err());
        let many: Vec<_> = (0..1025).map(|i| file(&format!("f{i}"), 0)).collect();
        assert!(check_generation(&many).is_err());
    }
}
