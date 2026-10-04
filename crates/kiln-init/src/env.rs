//! The main process's environment (spec §9.6 stage 6).

/// `PATH` when the image sets none, as in Docker.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The image env plus overrides (later entries win, keeping the first position),
/// then `PATH`, `HOSTNAME`, `HOME` and, with a terminal, `TERM=xterm` when unset.
pub fn build(env: &[String], home: &str, hostname: &str, tty: bool) -> Vec<String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in env {
        let (k, v) = entry.split_once('=').unwrap_or((entry, ""));
        match out.iter_mut().find(|(key, _)| key == k) {
            Some(slot) => slot.1 = v.to_string(),
            None => out.push((k.to_string(), v.to_string())),
        }
    }
    let mut defaults = vec![("PATH", DEFAULT_PATH), ("HOSTNAME", hostname), ("HOME", home)];
    if tty {
        defaults.push(("TERM", "xterm"));
    }
    for (k, v) in defaults {
        if !out.iter().any(|(key, _)| key == k) {
            out.push((k.to_string(), v.to_string()));
        }
    }
    out.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// The value of `key` in a `KEY=value` list.
pub fn get<'a>(env: &'a [String], key: &str) -> Option<&'a str> {
    env.iter()
        .filter_map(|e| e.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn adds_defaults_only_when_unset() {
        let env = build(&s(&["A=1"]), "/home/app", "box", false);
        assert_eq!(
            env,
            s(&["A=1", &format!("PATH={DEFAULT_PATH}"), "HOSTNAME=box", "HOME=/home/app"])
        );
        let env = build(
            &s(&["PATH=/x", "HOME=/h", "HOSTNAME=n", "TERM=vt100"]),
            "/",
            "box",
            true,
        );
        assert_eq!(env, s(&["PATH=/x", "HOME=/h", "HOSTNAME=n", "TERM=vt100"]));
        assert!(build(&[], "/", "b", true).contains(&"TERM=xterm".to_string()));
    }

    #[test]
    fn later_entries_override_earlier_ones() {
        let env = build(&s(&["A=1", "B=2", "A=3"]), "/", "b", false);
        assert_eq!(&env[..2], s(&["A=3", "B=2"]));
        assert_eq!(get(&env, "A"), Some("3"));
        assert_eq!(get(&env, "C"), None);
    }
}
