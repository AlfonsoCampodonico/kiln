//! The rebase gates (spec §16.3) and the env and label merge (§16.5). Every gate
//! is a pure function of what the caller read, so each can be tested alone.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use kiln_erofs::Xattrs;
use kiln_oci::Platform;
use kiln_store::Digest;

use crate::convert::same_platform;
use crate::error::Result;
use crate::types::{BaseRecord, Process};

/// Why one gate refused a rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub gate: &'static str,
    pub reason: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.gate, self.reason)
    }
}

pub type Gate = std::result::Result<(), Refusal>;

fn refuse(gate: &'static str, reason: impl Into<String>) -> Gate {
    Err(Refusal {
        gate,
        reason: reason.into(),
    })
}

/// `platform`: the image's platform exists in both bases.
pub fn platform(image: &Platform, old: &[Platform], new: &[Platform]) -> Gate {
    for (which, list) in [("old", old), ("new", new)] {
        if !list.iter().any(|p| same_platform(p, image)) {
            let have: Vec<String> = list.iter().map(ToString::to_string).collect();
            return refuse(
                "platform",
                format!("the {which} base has no {image} image (it has {})", have.join(", ")),
            );
        }
    }
    Ok(())
}

/// `bottom`: the image records a base, and its bottom layers are the old base's.
pub fn bottom(record: Option<&BaseRecord>, image: &[Digest], old: &[Digest]) -> Gate {
    let Some(r) = record else {
        return refuse(
            "bottom",
            "the image records no base; re-convert it with `kiln convert --base`",
        );
    };
    if r.layers != old.len() || image.get(..r.layers) != Some(old) {
        return refuse(
            "bottom",
            format!(
                "the image's bottom {} layers are not the old base's {} layers (it was built on {})",
                r.layers,
                old.len(),
                r.name
            ),
        );
    }
    if r.layers == image.len() {
        return refuse("bottom", "the image has no layers above its base");
    }
    Ok(())
}

/// `KEY=value` lines of an os-release file, with quotes removed.
fn os_release(text: &str) -> BTreeMap<&str, &str> {
    text.lines()
        .filter_map(|l| l.trim().split_once('='))
        .map(|(k, v)| (k.trim(), v.trim().trim_matches(|c| c == '"' || c == '\'')))
        .collect()
}

/// The first two dot-separated components: `8.4.12` → `8.4`, `12` → `12`.
fn major_minor(v: &str) -> String {
    v.trim_start_matches('v')
        .split('.')
        .take(2)
        .collect::<Vec<_>>()
        .join(".")
}

fn release(f: &BTreeMap<&str, &str>, other: &BTreeMap<&str, &str>) -> (String, String) {
    match (f.get("VERSION_CODENAME"), other.get("VERSION_CODENAME")) {
        (Some(a), Some(_)) => ("VERSION_CODENAME".into(), a.to_string()),
        _ => (
            "VERSION_ID".into(),
            major_minor(f.get("VERSION_ID").copied().unwrap_or("")),
        ),
    }
}

/// `distro`: both bases' os-release show the same distribution and release.
pub fn distro(old: Option<&str>, new: Option<&str>) -> Gate {
    let (old, new) = match (old, new) {
        (None, None) => return Ok(()),
        (Some(_), None) => return refuse("distro", "the new base has no os-release file"),
        (None, Some(_)) => return refuse("distro", "the old base has no os-release file"),
        (Some(o), Some(n)) => (os_release(o), os_release(n)),
    };
    let id = |f: &BTreeMap<&str, &str>| f.get("ID").copied().unwrap_or("").to_string();
    if id(&old) != id(&new) {
        return refuse("distro", format!("ID changes from {:?} to {:?}", id(&old), id(&new)));
    }
    let ((key, a), (_, b)) = (release(&old, &new), release(&new, &old));
    if a != b {
        return refuse("distro", format!("{key} changes from {a:?} to {b:?}"));
    }
    Ok(())
}

/// `runtime`: every `<lang>.version` label of the old base keeps its major.minor.
/// `None` is a base whose labels are unknown (converted before kiln recorded
/// them, with its source config gone): it is refused, since an absent label set
/// cannot be told from an empty one.
pub fn runtime(old: Option<&BTreeMap<String, String>>, new: Option<&BTreeMap<String, String>>) -> Gate {
    let (old, new) = match (old, new) {
        (Some(o), Some(n)) => (o, n),
        (o, _) => {
            let which = if o.is_none() { "old" } else { "new" };
            return refuse(
                "runtime",
                format!(
                    "the {which} base's labels are unknown (converted before kiln recorded labels, and its source \
                     config is gone); re-convert it"
                ),
            );
        }
    };
    for (k, v) in old.iter().filter(|(k, _)| k.ends_with(".version")) {
        match new.get(k) {
            None => return refuse("runtime", format!("the new base drops the label {k}")),
            Some(n) if major_minor(n) != major_minor(v) => {
                return refuse(
                    "runtime",
                    format!("{k} changes from {v} to {n} (only patch releases may change)"),
                );
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// `NAME=value` → (`NAME`, `value`); an entry without `=` is a name with no value.
fn env_pair(e: &str) -> (&str, Option<&str>) {
    match e.split_once('=') {
        Some((k, v)) => (k, Some(v)),
        None => (e, None),
    }
}

fn env_map(env: &[String]) -> BTreeMap<&str, Option<&str>> {
    env.iter().map(|e| env_pair(e)).collect()
}

/// `env`: the new base defines every variable the old one did.
pub fn env(old: &[String], new: &[String]) -> Gate {
    let new = env_map(new);
    let dropped: Vec<&str> = old
        .iter()
        .map(|e| env_pair(e).0)
        .filter(|k| !new.contains_key(k))
        .collect();
    if dropped.is_empty() {
        Ok(())
    } else {
        refuse("env", format!("the new base drops {}", dropped.join(", ")))
    }
}

/// `process`: the bases agree on how the image starts.
pub fn process(old: &Process, new: &Process) -> Gate {
    let fields: [(&str, String, String); 5] = [
        (
            "entrypoint",
            format!("{:?}", old.entrypoint),
            format!("{:?}", new.entrypoint),
        ),
        ("cmd", format!("{:?}", old.cmd), format!("{:?}", new.cmd)),
        ("user", format!("{:?}", old.user), format!("{:?}", new.user)),
        (
            "working directory",
            format!("{:?}", old.working_dir),
            format!("{:?}", new.working_dir),
        ),
        (
            "stop signal",
            format!("{:?}", old.stop_signal),
            format!("{:?}", new.stop_signal),
        ),
    ];
    for (name, a, b) in fields {
        if a != b {
            return refuse("process", format!("{name} changes from {a} to {b}"));
        }
    }
    Ok(())
}

/// What a base shows at one path, for the shadowed-path gate. mtimes are left out
/// on purpose (spec §16.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSig {
    /// `S_IFMT` bits.
    pub kind: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Overlay xattrs excluded.
    pub xattrs: Xattrs,
    /// Regular files only.
    pub size: u64,
    pub content: Option<Digest>,
    pub target: Option<Vec<u8>>,
    pub rdev: (u32, u32),
}

impl PathSig {
    /// What differs between two signatures, for the refusal.
    fn diff(&self, new: &PathSig) -> String {
        if self.kind != new.kind {
            return "its type changes".into();
        }
        let mut what = Vec::new();
        if self.content != new.content || self.size != new.size {
            what.push("content".to_string());
        }
        if self.mode != new.mode {
            what.push(format!("mode {:o} → {:o}", self.mode, new.mode));
        }
        if (self.uid, self.gid) != (new.uid, new.gid) {
            what.push(format!("owner {}:{} → {}:{}", self.uid, self.gid, new.uid, new.gid));
        }
        if self.xattrs != new.xattrs {
            what.push("xattrs".into());
        }
        if self.target != new.target {
            what.push("symlink target".into());
        }
        if self.rdev != new.rdev {
            what.push("device numbers".into());
        }
        format!("{} differs", what.join(", "))
    }
}

/// A base's merged view, as the shadowed-path gate reads it.
pub trait BaseView {
    fn sig(&mut self, path: &[u8]) -> Result<Option<PathSig>>;
    /// Every path below `path`, `path` excluded.
    fn subtree(&mut self, path: &[u8]) -> Result<Vec<Vec<u8>>>;
}

/// A path an app layer contains. `hides` is set for whiteouts, opaque directories
/// and non-directories, which hide the base's whole subtree at that path.
/// `implicit` marks a directory the layer created only for its children: its
/// attributes are re-derived from the new base, so only its type is compared.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Shadowed {
    pub path: Vec<u8>,
    pub hides: bool,
    pub implicit: bool,
}

/// How many differing paths a `shadow` refusal names.
pub const SHADOW_REPORT_MAX: usize = 20;

/// `shadow`: every base path the app shadows is identical in both bases. Returns
/// the gate's verdict; reading a base can still fail.
pub fn shadow(shadowed: &[Shadowed], old: &mut dyn BaseView, new: &mut dyn BaseView) -> Result<Gate> {
    // Path → whether it is compared in full (false: only its type, for implicit dirs).
    let mut paths: BTreeMap<Vec<u8>, bool> = BTreeMap::new();
    for s in shadowed {
        *paths.entry(s.path.clone()).or_default() |= !s.implicit;
        if s.hides {
            for p in old.subtree(&s.path)?.into_iter().chain(new.subtree(&s.path)?) {
                paths.insert(p, true);
            }
        }
    }
    let mut diffs = Vec::new();
    for (p, full) in &paths {
        let (a, b) = (old.sig(p)?, new.sig(p)?);
        if !full {
            // An implicit directory masks whatever the new base has there; only a
            // directory merges with it as before.
            match (&a, &b) {
                (_, Some(n)) if n.kind != kiln_erofs::ondisk::S_IFDIR && a.as_ref().map(|o| o.kind) != Some(n.kind) => {
                    diffs.push(format!(
                        "/{}: the new base has a non-directory where the app's layers create a directory",
                        String::from_utf8_lossy(p)
                    ));
                }
                _ => {}
            }
            continue;
        }
        let reason = match (a, b) {
            (None, None) => continue,
            (Some(_), None) => "the new base removes it".to_string(),
            (None, Some(_)) => "the new base adds it".to_string(),
            (Some(a), Some(b)) if a == b => continue,
            (Some(a), Some(b)) => a.diff(&b),
        };
        diffs.push(format!("/{}: {reason}", String::from_utf8_lossy(p)));
    }
    if diffs.is_empty() {
        return Ok(Ok(()));
    }
    let total = diffs.len();
    let shown: Vec<String> = diffs.into_iter().take(SHADOW_REPORT_MAX).collect();
    let more = if total > SHADOW_REPORT_MAX {
        format!(" (and {} more)", total - SHADOW_REPORT_MAX)
    } else {
        String::new()
    };
    Ok(refuse(
        "shadow",
        format!(
            "{total} path{} the app shadows differ{} between the bases, so the app's copies may be stale: {}{more}",
            if total == 1 { "" } else { "s" },
            if total == 1 { "s" } else { "" },
            shown.join("; ")
        ),
    ))
}

/// The rebased env (spec §16.5): inherited values follow the new base, values the
/// app set itself stay, and variables the new base adds are appended.
pub fn merge_env(image: &[String], old: &[String], new: &[String]) -> Vec<String> {
    let (old_m, new_m) = (env_map(old), env_map(new));
    let mut out: Vec<String> = image
        .iter()
        .map(|e| {
            let (k, v) = env_pair(e);
            match (old_m.get(k), new_m.get(k)) {
                (Some(ov), Some(nv)) if *ov == v => match nv {
                    Some(nv) => format!("{k}={nv}"),
                    None => k.to_string(),
                },
                _ => e.clone(),
            }
        })
        .collect();
    let have: BTreeSet<&str> = image.iter().map(|e| env_pair(e).0).collect();
    out.extend(
        new.iter()
            .filter(|e| {
                let k = env_pair(e).0;
                !have.contains(k) && !old_m.contains_key(k)
            })
            .cloned(),
    );
    out
}

/// The rebased labels: the same rule as [`merge_env`], keyed by name. An inherited
/// label the new base drops is dropped.
pub fn merge_labels(
    image: &BTreeMap<String, String>,
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in image {
        if old.get(k) == Some(v) {
            if let Some(n) = new.get(k) {
                out.insert(k.clone(), n.clone());
            }
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in new {
        if !image.contains_key(k) && !old.contains_key(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn m(v: &[(&str, &str)]) -> BTreeMap<String, String> {
        v.iter().map(|(k, x)| (k.to_string(), x.to_string())).collect()
    }

    fn plat(p: &str) -> Platform {
        Platform::parse(p).unwrap()
    }

    #[test]
    fn platform_needs_the_image_platform_in_both_bases() {
        let arm = plat("linux/arm64");
        let one = std::slice::from_ref(&arm);
        assert!(platform(&arm, one, &[plat("linux/amd64"), arm.clone()]).is_ok());
        let r = platform(&arm, one, &[plat("linux/amd64")]).unwrap_err();
        assert_eq!(r.gate, "platform");
        assert!(r.reason.contains("new base has no linux/arm64"), "{r}");
        assert!(platform(&plat("linux/arm64/v8"), &[plat("linux/arm64/v7")], one).is_err());
        assert!(
            platform(&plat("linux/arm64/v8"), one, one).is_ok(),
            "unrecorded variant"
        );
    }

    fn record(layers: usize) -> BaseRecord {
        BaseRecord {
            name: "base:1".into(),
            manifest_digest: Digest::of(b"m"),
            layers,
            source_layers: layers,
        }
    }

    #[test]
    fn bottom_compares_layer_digests() {
        let (a, b, c) = (Digest::of(b"a"), Digest::of(b"b"), Digest::of(b"c"));
        let image = [a.clone(), b.clone(), c.clone()];
        assert!(bottom(Some(&record(2)), &image, &[a.clone(), b.clone()]).is_ok());
        assert!(
            bottom(None, &image, std::slice::from_ref(&a))
                .unwrap_err()
                .reason
                .contains("--base")
        );
        let r = bottom(Some(&record(2)), &image, &[a.clone(), c.clone()]).unwrap_err();
        assert!(r.reason.contains("not the old base's"), "{r}");
        assert!(
            bottom(Some(&record(1)), &image, &[a.clone(), b.clone()]).is_err(),
            "count differs"
        );
        assert!(
            bottom(Some(&record(3)), &image, &image).is_err(),
            "nothing above the base"
        );
    }

    #[test]
    fn distro_compares_id_and_release() {
        let deb = |v: &str, c: &str| format!("ID=debian\nVERSION_ID=\"{v}\"\nVERSION_CODENAME={c}\n");
        assert!(distro(None, None).is_ok());
        assert!(distro(Some(&deb("12", "bookworm")), Some(&deb("12", "bookworm"))).is_ok());
        let r = distro(Some(&deb("12", "bookworm")), Some(&deb("13", "trixie"))).unwrap_err();
        assert!(r.reason.contains("VERSION_CODENAME"), "{r}");
        assert!(
            distro(
                Some("ID=alpine\nVERSION_ID=3.20.3"),
                Some("ID=alpine\nVERSION_ID=3.20.5")
            )
            .is_ok()
        );
        assert!(
            distro(
                Some("ID=alpine\nVERSION_ID=3.20.3"),
                Some("ID=alpine\nVERSION_ID=3.21.0")
            )
            .is_err()
        );
        let r = distro(Some("ID=alpine\nVERSION_ID=3.20"), Some("ID=debian\nVERSION_ID=3.20")).unwrap_err();
        assert!(r.reason.contains("ID changes"), "{r}");
        assert!(distro(Some("ID=alpine"), None).is_err());
        assert!(distro(None, Some("ID=alpine")).is_err());
    }

    #[test]
    fn runtime_keeps_major_minor() {
        let old = m(&[("php.version", "8.4.12"), ("node.version", "24.1.0"), ("other", "1")]);
        let ok = m(&[("php.version", "8.4.14"), ("node.version", "24.1.5")]);
        assert!(runtime(Some(&old), Some(&ok)).is_ok());
        let r = runtime(
            Some(&old),
            Some(&m(&[("php.version", "8.5.0"), ("node.version", "24.1.0")])),
        )
        .unwrap_err();
        assert!(r.reason.contains("php.version changes from 8.4.12 to 8.5.0"), "{r}");
        let r = runtime(Some(&old), Some(&m(&[("php.version", "8.4.12")]))).unwrap_err();
        assert!(r.reason.contains("drops the label node.version"), "{r}");
        let empty = BTreeMap::new();
        assert!(runtime(Some(&empty), Some(&empty)).is_ok());
        // Unknown is not empty: a base converted before labels were recorded.
        let r = runtime(None, Some(&ok)).unwrap_err();
        assert!(r.reason.contains("old base's labels are unknown"), "{r}");
        assert!(
            runtime(Some(&old), None)
                .unwrap_err()
                .reason
                .contains("new base's labels")
        );
    }

    #[test]
    fn env_refuses_a_dropped_variable() {
        let old = s(&["PATH=/bin", "PHP_FPM_REQUEST_TERMINATE_TIMEOUT=60", "FLAG"]);
        assert!(
            env(
                &old,
                &s(&["PATH=/usr/bin", "PHP_FPM_REQUEST_TERMINATE_TIMEOUT=30", "FLAG", "NEW=1"])
            )
            .is_ok()
        );
        let r = env(&old, &s(&["PATH=/bin"])).unwrap_err();
        assert_eq!(r.gate, "env");
        assert!(r.reason.contains("PHP_FPM_REQUEST_TERMINATE_TIMEOUT, FLAG"), "{r}");
    }

    #[test]
    fn process_refuses_any_change() {
        let p = Process {
            entrypoint: s(&["docker-php-entrypoint"]),
            cmd: s(&["php-fpm"]),
            working_dir: Some("/var/www".into()),
            ..Default::default()
        };
        assert!(process(&p, &p.clone()).is_ok());
        for (q, what) in [
            (
                Process {
                    cmd: s(&["sh"]),
                    ..p.clone()
                },
                "cmd",
            ),
            (
                Process {
                    entrypoint: vec![],
                    ..p.clone()
                },
                "entrypoint",
            ),
            (
                Process {
                    user: Some("www-data".into()),
                    ..p.clone()
                },
                "user",
            ),
            (
                Process {
                    working_dir: None,
                    ..p.clone()
                },
                "working directory",
            ),
            (
                Process {
                    stop_signal: Some("SIGQUIT".into()),
                    ..p.clone()
                },
                "stop signal",
            ),
        ] {
            let r = process(&p, &q).unwrap_err();
            assert!(r.reason.starts_with(what), "{r}");
        }
    }

    /// An in-memory base view.
    struct MapView(BTreeMap<Vec<u8>, PathSig>);

    impl BaseView for MapView {
        fn sig(&mut self, path: &[u8]) -> Result<Option<PathSig>> {
            Ok(self.0.get(path).cloned())
        }
        fn subtree(&mut self, path: &[u8]) -> Result<Vec<Vec<u8>>> {
            let prefix = [path, b"/"].concat();
            Ok(self.0.keys().filter(|k| k.starts_with(&prefix)).cloned().collect())
        }
    }

    fn file(content: &[u8], mode: u32) -> PathSig {
        PathSig {
            kind: kiln_erofs::ondisk::S_IFREG,
            mode,
            uid: 0,
            gid: 0,
            xattrs: Xattrs::new(),
            size: content.len() as u64,
            content: Some(Digest::of(content)),
            target: None,
            rdev: (0, 0),
        }
    }

    fn dir(mode: u32) -> PathSig {
        PathSig {
            kind: kiln_erofs::ondisk::S_IFDIR,
            size: 0,
            content: None,
            ..file(b"", mode)
        }
    }

    fn view(entries: &[(&str, PathSig)]) -> MapView {
        MapView(
            entries
                .iter()
                .map(|(p, s)| (p.as_bytes().to_vec(), s.clone()))
                .collect(),
        )
    }

    fn sh(path: &str, hides: bool) -> Shadowed {
        Shadowed {
            path: path.as_bytes().to_vec(),
            hides,
            implicit: false,
        }
    }

    fn implicit(path: &str, hides: bool) -> Shadowed {
        Shadowed {
            implicit: true,
            ..sh(path, hides)
        }
    }

    fn symlink(target: &[u8]) -> PathSig {
        PathSig {
            kind: kiln_erofs::ondisk::S_IFLNK,
            size: 0,
            content: None,
            target: Some(target.to_vec()),
            ..file(b"", 0o777)
        }
    }

    #[test]
    fn shadow_compares_only_the_type_of_implicit_dirs() {
        let mut old = view(&[("var", dir(0o755)), ("var/run", dir(0o755))]);
        let mut new = view(&[("var", dir(0o750)), ("var/run", symlink(b"/run"))]);
        // The mode change of an implicit dir is re-derived, not refused.
        assert!(shadow(&[implicit("var", false)], &mut old, &mut new).unwrap().is_ok());
        // A symlink the implicit dir would mask is.
        let r = shadow(
            &[implicit("var", false), implicit("var/run", false)],
            &mut old,
            &mut new,
        )
        .unwrap()
        .unwrap_err();
        assert!(r.reason.contains("/var/run: the new base has a non-directory"), "{r}");
        // Absent before and a directory now merges as usual.
        let mut fresh = view(&[("var", dir(0o755)), ("var/run", dir(0o755))]);
        let mut none = view(&[("var", dir(0o755))]);
        assert!(
            shadow(&[implicit("var/run", false)], &mut none, &mut fresh)
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn shadow_expands_an_implicit_opaque_dir() {
        let mut old = view(&[("etc/conf.d", dir(0o755)), ("etc/conf.d/a", file(b"a", 0o644))]);
        let mut new = view(&[
            ("etc/conf.d", dir(0o700)),
            ("etc/conf.d/a", file(b"a", 0o644)),
            ("etc/conf.d/b", file(b"b", 0o644)),
        ]);
        let r = shadow(&[implicit("etc/conf.d", true)], &mut old, &mut new)
            .unwrap()
            .unwrap_err();
        assert!(r.reason.contains("/etc/conf.d/b: the new base adds it"), "{r}");
        assert!(
            !r.reason.contains("/etc/conf.d: mode"),
            "its own attributes are re-derived: {r}"
        );
    }

    #[test]
    fn shadow_refuses_a_stale_copied_up_file() {
        let fpm = "usr/local/etc/php-fpm.d/laravel-cloud-fpm.conf";
        let mut old = view(&[
            ("usr", dir(0o755)),
            (fpm, file(b"timeout=${T}", 0o644)),
            ("etc/x", file(b"x", 0o644)),
        ]);
        let mut new = view(&[
            ("usr", dir(0o755)),
            (fpm, file(b"timeout=30", 0o644)),
            ("etc/x", file(b"y", 0o644)),
        ]);
        let r = shadow(&[sh("usr", false), sh(fpm, true)], &mut old, &mut new)
            .unwrap()
            .unwrap_err();
        assert_eq!(r.gate, "shadow");
        assert!(r.reason.contains(&format!("/{fpm}: content differs")), "{r}");
        assert!(r.reason.starts_with("1 path the app shadows differs"), "{r}");
        assert!(
            !r.reason.contains("etc/x"),
            "unshadowed changes are what a rebase is for"
        );
    }

    #[test]
    fn shadow_passes_identical_paths_and_ignores_new_children_of_plain_dirs() {
        let mut old = view(&[("etc", dir(0o755)), ("etc/app.conf", file(b"a", 0o644))]);
        let mut new = view(&[
            ("etc", dir(0o755)),
            ("etc/app.conf", file(b"a", 0o644)),
            ("etc/new", file(b"n", 0o644)),
        ]);
        assert!(
            shadow(
                &[sh("etc", false), sh("etc/app.conf", true), sh("gone", true)],
                &mut old,
                &mut new
            )
            .unwrap()
            .is_ok()
        );
    }

    #[test]
    fn shadow_compares_whole_hidden_subtrees_and_attributes() {
        let mut old = view(&[("opt", dir(0o755)), ("opt/a", file(b"a", 0o644)), ("srv", dir(0o755))]);
        let mut new = view(&[
            ("opt", dir(0o755)),
            ("opt/a", file(b"a", 0o600)),
            ("opt/b", file(b"b", 0o644)),
            ("srv", dir(0o700)),
            ("added", file(b"", 0o644)),
        ]);
        let r = shadow(
            &[sh("opt", true), sh("srv", false), sh("added", true)],
            &mut old,
            &mut new,
        )
        .unwrap()
        .unwrap_err();
        for want in [
            "/opt/a: mode 644 → 600 differs",
            "/opt/b: the new base adds it",
            "/srv: mode 755 → 700 differs",
            "/added: the new base adds it",
        ] {
            assert!(r.reason.contains(want), "{want} in {r}");
        }
        let mut gone = view(&[("opt", dir(0o755))]);
        let r = shadow(&[sh("opt", true)], &mut old, &mut gone).unwrap().unwrap_err();
        assert!(r.reason.contains("/opt/a: the new base removes it"), "{r}");
    }

    #[test]
    fn shadow_caps_the_listed_paths() {
        let entries: Vec<(String, PathSig)> = (0..30).map(|i| (format!("d/f{i:02}"), file(b"a", 0o644))).collect();
        let mut old = MapView(
            entries
                .iter()
                .map(|(p, s)| (p.as_bytes().to_vec(), s.clone()))
                .collect(),
        );
        let mut new = MapView(
            entries
                .iter()
                .map(|(p, _)| (p.as_bytes().to_vec(), file(b"b", 0o644)))
                .collect(),
        );
        let r = shadow(&[sh("d", true)], &mut old, &mut new).unwrap().unwrap_err();
        assert!(r.reason.starts_with("30 paths"), "{r}");
        assert!(r.reason.ends_with("(and 10 more)"), "{r}");
    }

    #[test]
    fn env_merge_follows_the_new_base_only_for_inherited_values() {
        let old = s(&["PATH=/a", "PHP_VERSION=8.4.12", "TZ=UTC"]);
        let new = s(&["PATH=/a", "PHP_VERSION=8.4.14", "TZ=UTC", "ADDED=1"]);
        let image = s(&["PATH=/app/bin:/a", "PHP_VERSION=8.4.12", "TZ=UTC", "APP_ENV=prod"]);
        assert_eq!(
            merge_env(&image, &old, &new),
            s(&[
                "PATH=/app/bin:/a",
                "PHP_VERSION=8.4.14",
                "TZ=UTC",
                "APP_ENV=prod",
                "ADDED=1"
            ])
        );
    }

    #[test]
    fn label_merge_follows_the_new_base_only_for_inherited_values() {
        let old = m(&[("php.version", "8.4.12"), ("vendor", "laravel"), ("gone", "x")]);
        let new = m(&[("php.version", "8.4.14"), ("vendor", "laravel"), ("added", "1")]);
        let image = m(&[
            ("php.version", "8.4.12"),
            ("vendor", "acme"),
            ("gone", "x"),
            ("app", "y"),
        ]);
        assert_eq!(
            merge_labels(&image, &old, &new),
            m(&[
                ("php.version", "8.4.14"),
                ("vendor", "acme"),
                ("app", "y"),
                ("added", "1")
            ])
        );
    }
}
