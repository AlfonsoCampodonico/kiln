//! The kiln user's ids from `/etc/passwd` and `/etc/group` (design §8): the VM workers
//! drop to it, and the pod's tap is owned by it.

use crate::error::{Error, Result};

/// A user as the worker becomes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups (they must include `kvm`).
    pub groups: Vec<u32>,
    pub home: String,
}

/// Looks `name` up in the two files' text.
pub fn lookup(passwd: &str, group: &str, name: &str) -> Result<Account> {
    let entry = passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() >= 7 && f[0] == name)
        .ok_or_else(|| Error::Config(format!("user {name:?} is not in /etc/passwd")))?;
    let id = |s: &str, what: &str| {
        s.parse::<u32>()
            .map_err(|_| Error::Config(format!("user {name:?} has a malformed {what}")))
    };
    let (uid, gid) = (id(entry[2], "uid")?, id(entry[3], "gid")?);
    if uid == 0 {
        return Err(Error::Config(format!(
            "user {name:?} is root; VMs run as an unprivileged user (kiln T6)"
        )));
    }
    let mut groups = vec![gid];
    for f in group.lines().map(|l| l.split(':').collect::<Vec<_>>()) {
        if f.len() >= 4
            && f[3].split(',').any(|m| m == name)
            && let Ok(g) = f[2].parse::<u32>()
            && !groups.contains(&g)
        {
            groups.push(g);
        }
    }
    Ok(Account {
        name: name.into(),
        uid,
        gid,
        groups,
        home: entry[5].into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\nkiln:x:991:990:kiln VMs:/var/lib/kiln:/usr/sbin/nologin\n";
    const GROUP: &str = "root:x:0:\nkvm:x:992:kiln,alice\nkiln:x:990:\ndocker:x:993:alice\n";

    #[test]
    fn user_lookup() {
        let a = lookup(PASSWD, GROUP, "kiln").unwrap();
        assert_eq!((a.uid, a.gid, a.home.as_str()), (991, 990, "/var/lib/kiln"));
        assert_eq!(a.groups, [990, 992]);
        assert!(
            lookup(PASSWD, GROUP, "nobody")
                .unwrap_err()
                .to_string()
                .contains("not in /etc/passwd")
        );
        assert!(lookup(PASSWD, GROUP, "root").unwrap_err().to_string().contains("root"));
    }
}
