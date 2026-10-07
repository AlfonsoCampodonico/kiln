//! Resolving `user[:group]` against the image's `/etc/passwd` and `/etc/group`,
//! with runc's rules (spec §9.6 stage 6):
//! - a name must exist in passwd; a number is a uid even without an entry;
//! - a matched user gets its passwd gid and home;
//! - an explicit group (name, or number even without an entry) replaces the gid;
//! - without one, a user matched in passwd also gets every group listing it.
//!
//! The process's groups are its gid followed by those supplementary groups.

use std::collections::HashSet;

use crate::error::{Failure, Result};

/// The largest id accepted, as in runc: setresuid and setresgid take -1 to mean
/// "unchanged", so ids above `i32::MAX` are refused.
const MAX_ID: u32 = i32::MAX as u32;

/// The kernel's `NGROUPS_MAX`.
const MAX_GROUPS: usize = 65536;

/// A numeric id, if `s` is one in range.
fn id_number(s: &str) -> Option<u32> {
    s.parse::<u32>().ok().filter(|&n| n <= MAX_ID)
}

/// Who the main process runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    /// For `setgroups`: the gid, then supplementary groups, without duplicates.
    pub groups: Vec<u32>,
    pub home: String,
}

struct User<'a> {
    name: &'a str,
    uid: u32,
    gid: u32,
    home: &'a str,
}

struct Group<'a> {
    name: &'a str,
    gid: u32,
    members: Vec<&'a str>,
}

/// Lines are trimmed, as runc trims them. Trailing fields may be missing, as
/// runc allows (`app:x:1000:1000` has no home, so `/`), but a line without a
/// name, uid or gid is skipped: runc would read a missing id as 0, root.
fn users(passwd: &str) -> impl Iterator<Item = User<'_>> {
    passwd.lines().map(str::trim).filter_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 4 || f[0].is_empty() {
            return None;
        }
        Some(User {
            name: f[0],
            uid: id_number(f[2])?,
            gid: id_number(f[3])?,
            home: f.get(5).copied().unwrap_or(""),
        })
    })
}

/// As [`users`]: the member list may be missing (`app:x:1000`), the gid may not.
fn groups(group: &str) -> impl Iterator<Item = Group<'_>> {
    group.lines().map(str::trim).filter_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 3 || f[0].is_empty() {
            return None;
        }
        Some(Group {
            name: f[0],
            gid: id_number(f[2])?,
            members: f
                .get(3)
                .map(|m| m.split(',').map(str::trim).filter(|m| !m.is_empty()).collect())
                .unwrap_or_default(),
        })
    })
}

/// Resolves `spec` (`None` is root). `passwd` and `group` are the files' contents,
/// empty when the image has none.
pub fn resolve(spec: Option<&str>, passwd: &str, group: &str) -> Result<Identity> {
    let spec = spec.unwrap_or("");
    let (user_arg, group_arg) = spec.split_once(':').unwrap_or((spec, ""));
    let user_arg = if user_arg.is_empty() { "0" } else { user_arg };
    let numeric_uid = id_number(user_arg);

    let matched = users(passwd).find(|u| match numeric_uid {
        Some(uid) => u.uid == uid,
        None => u.name == user_arg,
    });
    let (uid, mut gid, home, name) = match (&matched, numeric_uid) {
        (Some(u), _) => (u.uid, u.gid, u.home, Some(u.name)),
        (None, Some(uid)) => (uid, 0, "/", None),
        (None, None) => {
            return Err(Failure::msg(format!(
                "unable to find user {user_arg}: no matching entries in passwd file"
            )));
        }
    };

    let mut supplementary = Vec::new();
    if !group_arg.is_empty() {
        let numeric_gid = id_number(group_arg);
        let found = groups(group).find(|g| match numeric_gid {
            Some(n) => g.gid == n,
            None => g.name == group_arg,
        });
        gid = match (found, numeric_gid) {
            (Some(g), _) => g.gid,
            (None, Some(n)) => n,
            (None, None) => {
                return Err(Failure::msg(format!(
                    "unable to find group {group_arg}: no matching entries in group file"
                )));
            }
        };
    } else if let Some(name) = name {
        supplementary.extend(groups(group).filter(|g| g.members.contains(&name)).map(|g| g.gid));
    }

    let mut all = vec![gid];
    let mut seen = HashSet::from([gid]);
    for g in supplementary {
        if seen.insert(g) {
            all.push(g);
        }
    }
    if all.len() > MAX_GROUPS {
        return Err(Failure::msg(format!(
            "user {user_arg} is in {} groups, more than the {MAX_GROUPS} the kernel allows",
            all.len()
        )));
    }
    Ok(Identity {
        uid,
        gid,
        groups: all,
        home: if home.is_empty() { "/".into() } else { home.into() },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\n\
                          broken line\n\
                          app:x:1000:1000:App:/home/app:/bin/sh\n\
                          nohome:x:1001:1001:::/bin/sh\n";
    const GROUP: &str = "root:x:0:\napp:x:1000:\nextra:x:2000:app,other\nmore:x:3000: app\nstaff:x:50:\nbad\n";

    fn id(uid: u32, gid: u32, groups: &[u32], home: &str) -> Identity {
        Identity {
            uid,
            gid,
            groups: groups.to_vec(),
            home: home.into(),
        }
    }

    #[test]
    fn root_by_default() {
        assert_eq!(resolve(None, PASSWD, GROUP).unwrap(), id(0, 0, &[0], "/root"));
        assert_eq!(resolve(None, "", "").unwrap(), id(0, 0, &[0], "/"));
    }

    #[test]
    fn names_get_passwd_ids_home_and_member_groups() {
        assert_eq!(
            resolve(Some("app"), PASSWD, GROUP).unwrap(),
            id(1000, 1000, &[1000, 2000, 3000], "/home/app")
        );
        assert_eq!(
            resolve(Some("1000"), PASSWD, GROUP).unwrap(),
            id(1000, 1000, &[1000, 2000, 3000], "/home/app")
        );
        assert_eq!(
            resolve(Some("nohome"), PASSWD, GROUP).unwrap(),
            id(1001, 1001, &[1001], "/")
        );
    }

    #[test]
    fn numbers_work_without_entries() {
        assert_eq!(resolve(Some("1234"), "", "").unwrap(), id(1234, 0, &[0], "/"));
        assert_eq!(
            resolve(Some("1234:5678"), "", "").unwrap(),
            id(1234, 5678, &[5678], "/")
        );
    }

    #[test]
    fn an_explicit_group_replaces_gid_and_supplementary_groups() {
        assert_eq!(
            resolve(Some("app:staff"), PASSWD, GROUP).unwrap(),
            id(1000, 50, &[50], "/home/app")
        );
        assert_eq!(
            resolve(Some("app:2000"), PASSWD, GROUP).unwrap(),
            id(1000, 2000, &[2000], "/home/app")
        );
    }

    #[test]
    fn unknown_names_fail() {
        let err = resolve(Some("nobody"), PASSWD, GROUP).unwrap_err();
        assert!(err.message.contains("unable to find user nobody"), "{err}");
        assert!(resolve(Some("app:nogroup"), PASSWD, GROUP).is_err());
        assert!(resolve(Some("4294967296"), "", "").is_err(), "out of range is a name");
    }

    #[test]
    fn ids_above_i32_max_are_refused() {
        // setresuid(-1) means "unchanged", so u32::MAX must never reach it.
        assert!(resolve(Some("4294967295"), "", "").is_err());
        assert!(resolve(Some("2147483648"), "", "").is_err());
        assert!(resolve(Some("1000:4294967295"), PASSWD, GROUP).is_err());
        assert!(resolve(Some("1000:2147483648"), PASSWD, GROUP).is_err());
        assert_eq!(
            resolve(Some("2147483647:2147483647"), "", "").unwrap(),
            id(2147483647, 2147483647, &[2147483647], "/")
        );
        // Out-of-range entries are skipped like other malformed lines.
        let passwd = "big:x:4294967295:0:::/bin/sh\nbig2:x:5:4294967295:::/bin/sh\n";
        assert!(resolve(Some("big"), passwd, "").is_err());
        assert!(resolve(Some("big2"), passwd, "").is_err());
        assert!(resolve(Some("4294967295"), passwd, "").is_err());
        let group = "g:x:4294967295:\nok:x:7:\n";
        assert!(resolve(Some("app:g"), PASSWD, group).is_err());
        assert_eq!(resolve(Some("app:ok"), PASSWD, group).unwrap().gid, 7);
    }

    #[test]
    fn short_and_padded_lines_are_read_as_runc_reads_them() {
        // `echo 'app:x:1000' >> /etc/group` in a Dockerfile, then `USER app:app`.
        let passwd = "  app:x:1000:1000  \nshort:x:1001:1001:Short\n";
        let group = "app:x:1000\nextra:x:2000:app\n  padded:x:3000:short  \n";
        assert_eq!(
            resolve(Some("app:app"), passwd, group).unwrap(),
            id(1000, 1000, &[1000], "/")
        );
        assert_eq!(
            resolve(Some("app"), passwd, group).unwrap(),
            id(1000, 1000, &[1000, 2000], "/")
        );
        assert_eq!(
            resolve(Some("short"), passwd, group).unwrap(),
            id(1001, 1001, &[1001, 3000], "/")
        );
        // Without a uid or gid field a line is skipped: runc would make it root.
        assert!(resolve(Some("noid"), "noid:x\nnogid:x:5\n", "").is_err());
        assert!(resolve(Some("nogid"), "noid:x\nnogid:x:5\n", "").is_err());
        assert!(resolve(Some("app:g"), passwd, "g:x\n").is_err());
    }

    #[test]
    fn supplementary_groups_are_deduplicated_and_bounded() {
        let group = "a:x:5:app\nb:x:5:app\nc:x:1000:app\n";
        assert_eq!(resolve(Some("app"), PASSWD, group).unwrap().groups, vec![1000, 5]);

        let many: String = (1..=MAX_GROUPS as u32 + 1)
            .map(|g| format!("g{g}:x:{g}:app\n"))
            .collect();
        let err = resolve(Some("app"), PASSWD, &many).unwrap_err();
        assert!(err.message.contains("more than the 65536"), "{err}");
        let fits: String = (1..=MAX_GROUPS as u32).map(|g| format!("g{g}:x:{g}:app\n")).collect();
        assert_eq!(resolve(Some("app"), PASSWD, &fits).unwrap().groups.len(), MAX_GROUPS);
    }
}
