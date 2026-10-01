//! Tar path normalization (spec §7.4 "Paths").

use crate::error::{Error, Result, lossy};
use crate::limits::Limits;
use crate::ondisk::NAME_LEN_MAX;

/// Normalizes a raw tar path to kiln's canonical relative form.
pub(crate) fn normalize(raw: &[u8], limits: &Limits) -> Result<Vec<u8>> {
    if raw.contains(&0) {
        return Err(Error::InvalidPath {
            path: lossy(raw),
            reason: "contains NUL",
        });
    }
    if raw.len() > limits.max_path_len {
        return Err(Error::LimitExceeded {
            limit: "path length",
            max: limits.max_path_len as u64,
            path: lossy(&raw[..64.min(raw.len())]),
        });
    }
    let mut parts: Vec<&[u8]> = Vec::new();
    for comp in raw.split(|&b| b == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                if parts.pop().is_none() {
                    return Err(Error::PathEscapesRoot { path: lossy(raw) });
                }
            }
            c if c.len() > NAME_LEN_MAX => {
                return Err(Error::InvalidPath {
                    path: lossy(raw),
                    reason: "component longer than 255 bytes",
                });
            }
            c => parts.push(c),
        }
    }
    if parts.len() > limits.max_path_depth {
        return Err(Error::LimitExceeded {
            limit: "path depth",
            max: limits.max_path_depth as u64,
            path: lossy(raw),
        });
    }
    Ok(parts.join(&b'/'))
}

pub(crate) fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|&b| b == b'/').filter(|c| !c.is_empty())
}

pub(crate) fn split_parent(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (&path[..0], path),
    }
}

#[cfg(test)]
mod tests {
    use super::{components, normalize, split_parent};
    use crate::{Error, Limits};

    fn n(p: &str) -> Vec<u8> {
        normalize(p.as_bytes(), &Limits::default()).unwrap()
    }

    #[test]
    fn canonical_forms() {
        assert_eq!(n("./usr/bin/"), b"usr/bin");
        assert_eq!(n("/usr//bin"), b"usr/bin");
        assert_eq!(n("usr/./bin"), b"usr/bin");
        assert_eq!(n("usr/lib/../bin"), b"usr/bin");
        assert_eq!(n("./"), b"");
        assert_eq!(n("a/.."), b"");
    }

    #[test]
    fn rejects_escape_nul_and_long_components() {
        let l = Limits::default();
        assert!(matches!(normalize(b"../etc", &l), Err(Error::PathEscapesRoot { .. })));
        assert!(matches!(
            normalize(b"a/../../b", &l),
            Err(Error::PathEscapesRoot { .. })
        ));
        assert!(matches!(normalize(b"a\0b", &l), Err(Error::InvalidPath { .. })));
        let long = vec![b'x'; 256];
        assert!(matches!(normalize(&long, &l), Err(Error::InvalidPath { .. })));
    }

    #[test]
    fn enforces_length_and_depth_limits() {
        let l = Limits {
            max_path_len: 10,
            max_path_depth: 3,
            ..Limits::default()
        };
        assert!(matches!(
            normalize(b"aaaaaaaaaaa", &l),
            Err(Error::LimitExceeded { .. })
        ));
        assert!(matches!(normalize(b"a/b/c/d", &l), Err(Error::LimitExceeded { .. })));
        assert_eq!(normalize(b"a/b/c", &l).unwrap(), b"a/b/c");
    }

    #[test]
    fn split_and_components() {
        assert_eq!(split_parent(b"a/b/c"), (&b"a/b"[..], &b"c"[..]));
        assert_eq!(split_parent(b"a"), (&b""[..], &b"a"[..]));
        assert_eq!(components(b"a/b").collect::<Vec<_>>(), vec![&b"a"[..], &b"b"[..]]);
        assert_eq!(components(b"").count(), 0);
    }
}
