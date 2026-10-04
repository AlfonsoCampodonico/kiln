//! Resolving `user[:group]` against the image's `/etc/passwd` and `/etc/group`,
//! with runc's rules (spec §9.6 stage 6):
//! - a name must exist in passwd; a number is a uid even without an entry;
//! - a matched user gets its passwd gid and home;
//! - an explicit group (name, or number even without an entry) replaces the gid;
//! - without one, a user matched in passwd also gets every group listing it.
//!
//! The process's groups are its gid followed by those supplementary groups.

use crate::error::{Failure, Result};

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

/// Lines that are not well formed are skipped, as libc does.
fn users(passwd: &str) -> impl Iterator<Item = User<'_>> {
    passwd.lines().filter_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 || f[0].is_empty() {
            return None;
        }
        Some(User {
            name: f[0],
            uid: f[2].parse().ok()?,
            gid: f[3].parse().ok()?,
            home: f[5],
        })
    })
}

fn groups(group: &str) -> impl Iterator<Item = Group<'_>> {
    group.lines().filter_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 4 || f[0].is_empty() {
            return None;
        }
        Some(Group {
            name: f[0],
            gid: f[2].parse().ok()?,
            members: f[3].split(',').map(str::trim).filter(|m| !m.is_empty()).collect(),
        })
    })
}

/// Resolves `spec` (`None` is root). `passwd` and `group` are the files' contents,
/// empty when the image has none.
pub fn resolve(spec: Option<&str>, passwd: &str, group: &str) -> Result<Identity> {
    let spec = spec.unwrap_or("");
    let (user_arg, group_arg) = spec.split_once(':').unwrap_or((spec, ""));
    let user_arg = if user_arg.is_empty() { "0" } else { user_arg };
    let numeric_uid = user_arg.parse::<u32>().ok();

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
        let numeric_gid = group_arg.parse::<u32>().ok();
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
    for g in supplementary {
        if !all.contains(&g) {
            all.push(g);
        }
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
}
