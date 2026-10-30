//! The OCI spec in kiln terms (design §7.1): the guest's process, its hostname, and
//! the VM's size. CRI has already merged the image's entrypoint and cmd with the pod's
//! `command` and `args` into `process.args`, so the argv goes to the guest whole.

use kiln_proto::Process;

use crate::error::{Error, Result};
use crate::oci::Spec;
use crate::settings::Settings;

/// The smallest guest kiln boots.
const MIN_MEMORY_MIB: u32 = 64;

/// The VM's vCPUs and memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resources {
    pub cpus: u8,
    pub memory_mib: u32,
}

/// The guest's main process: the whole argv as the entrypoint, the spec's environment,
/// directory and numeric ids.
pub fn guest_process(spec: &Spec) -> Result<Process> {
    let p = spec
        .process
        .as_ref()
        .ok_or_else(|| Error::invalid("the spec has no process"))?;
    process(p, "the spec's process")
}

/// An exec's process (`ExecProcessRequest.spec`, an OCI `Process` as JSON), and whether
/// it has a terminal: translated as the main process is.
pub fn exec_process(json: &[u8]) -> Result<(Process, bool)> {
    let p: crate::oci::Process =
        serde_json::from_slice(json).map_err(|e| Error::invalid(format!("the exec's process: {e}")))?;
    Ok((process(&p, "the exec's process")?, p.terminal))
}

fn process(p: &crate::oci::Process, what: &str) -> Result<Process> {
    if p.args.first().is_none_or(|a| a.is_empty()) {
        return Err(Error::invalid(format!("{what} has no command (process.args is empty)")));
    }
    let process = Process {
        entrypoint: p.args.clone(),
        cmd: Vec::new(),
        env: p.env.clone(),
        working_dir: (!p.cwd.is_empty()).then(|| p.cwd.clone()),
        user: Some(format!("{}:{}", p.user.uid, p.user.gid)),
    };
    process
        .validate("the process")
        .map_err(|e| Error::invalid(e.to_string()))?;
    Ok(process)
}

/// kiln-proto's hostname rule: 1-64 of letters, digits, `-` and `.`, starting with a
/// letter or digit.
fn valid_hostname(h: &str) -> bool {
    (1..=64).contains(&h.len())
        && h.as_bytes()[0].is_ascii_alphanumeric()
        && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// The spec's hostname, else the pod's (CRI sets it on the sandbox only, as runc's
/// containers share its UTS namespace), when kiln-proto accepts it; else
/// `kiln-<the first 12 characters of the task id>`.
pub fn hostname(spec: &Spec, pod: Option<&str>, task_id: &str) -> String {
    match spec.hostname.as_deref().filter(|h| !h.is_empty()).or(pod) {
        Some(h) if valid_hostname(h) => h.to_string(),
        _ => {
            let short: String = task_id.chars().filter(char::is_ascii_alphanumeric).take(12).collect();
            if short.is_empty() {
                "kiln".into()
            } else {
                format!("kiln-{short}")
            }
        }
    }
}

/// vCPUs from the CPU limit (quota over period, rounded up), memory from the memory
/// limit (rounded up to a MiB); the settings' defaults without limits.
pub fn resources(spec: &Spec, s: &Settings) -> Resources {
    let r = spec.linux.as_ref().and_then(|l| l.resources.as_ref());
    let cpus = r
        .and_then(|r| r.cpu.as_ref())
        .and_then(|c| match (c.quota, c.period) {
            (Some(q), Some(p)) if q > 0 && p > 0 => Some((q as u64).div_ceil(p)),
            _ => None,
        })
        .map_or(s.default_cpus, |n| n.clamp(1, u64::from(s.max_cpus)) as u8);
    let memory_mib = r
        .and_then(|r| r.memory.as_ref())
        .and_then(|m| m.limit)
        .filter(|&l| l > 0)
        .map_or(s.default_memory_mib, |l| {
            u32::try_from((l as u64).div_ceil(1 << 20))
                .unwrap_or(u32::MAX)
                .max(MIN_MEMORY_MIB)
        });
    Resources { cpus, memory_mib }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: &str) -> Spec {
        serde_json::from_str(json).unwrap()
    }

    fn settings() -> Settings {
        Settings::parse("user = \"kiln\"\nstore = \"/s\"\nkernel = \"/k\"\ninit = \"/i\"\n").unwrap()
    }

    #[test]
    fn exec_processes_translate_like_the_main_one() {
        let json = br#"{"terminal":true,"user":{"uid":1000,"gid":1000},"args":["sh","-c","id"],
            "env":["PATH=/bin"],"cwd":"/srv"}"#;
        let (p, tty) = exec_process(json).unwrap();
        assert!(tty);
        assert_eq!(p.entrypoint, ["sh", "-c", "id"]);
        assert_eq!(
            (p.user.as_deref(), p.working_dir.as_deref()),
            (Some("1000:1000"), Some("/srv"))
        );
        assert!(exec_process(br#"{"args":[]}"#).is_err());
        assert!(exec_process(br#"{"args":["a\u0000b"]}"#).is_err());
        assert!(exec_process(b"nope").is_err());
    }

    #[test]
    fn process() {
        let s = spec(
            r#"{"process":{"args":["sh","-c","echo hi"],"env":["PATH=/bin","A=1"],"cwd":"/w","user":{"uid":1000,"gid":1000}}}"#,
        );
        let p = guest_process(&s).unwrap();
        assert_eq!(p.entrypoint, ["sh", "-c", "echo hi"]);
        assert!(p.cmd.is_empty());
        assert_eq!(p.env, ["PATH=/bin", "A=1"]);
        assert_eq!(p.working_dir.as_deref(), Some("/w"));
        assert_eq!(p.user.as_deref(), Some("1000:1000"));
        let root = guest_process(&spec(r#"{"process":{"args":["id"]}}"#)).unwrap();
        assert_eq!(root.user.as_deref(), Some("0:0"));
        assert_eq!(root.working_dir, None);
        for bad in [r#"{"process":{"args":[]}}"#, r#"{"process":{"args":[""]}}"#] {
            let err = guest_process(&spec(bad)).unwrap_err().to_string();
            assert!(err.contains("no command"), "{err}");
        }
        assert!(guest_process(&spec("{}")).is_err());
    }

    #[test]
    fn hostname_is_kept_or_replaced() {
        let id = "4f0d2c1e9a8b7c6d5e4f";
        assert_eq!(hostname(&spec(r#"{"hostname":"web-0"}"#), None, id), "web-0");
        assert_eq!(hostname(&spec(r#"{"hostname":"web-0"}"#), Some("pod"), id), "web-0");
        assert_eq!(hostname(&spec("{}"), Some("pod"), id), "pod");
        assert_eq!(hostname(&spec(r#"{"hostname":""}"#), Some("pod"), id), "pod");
        for bad in [
            format!(r#"{{"hostname":"{}"}}"#, "a".repeat(70)),
            r#"{"hostname":"under_score"}"#.into(),
            "{}".into(),
        ] {
            assert_eq!(hostname(&spec(&bad), None, id), "kiln-4f0d2c1e9a8b", "{bad}");
        }
    }

    #[test]
    fn resources_from_limits() {
        let s = settings();
        let with = |r: &str| resources(&spec(&format!(r#"{{"linux":{{"resources":{r}}}}}"#)), &s);
        assert_eq!(with(r#"{"cpu":{"quota":150000,"period":100000}}"#).cpus, 2);
        assert_eq!(with(r#"{"cpu":{"quota":2000000,"period":100000}}"#).cpus, 8);
        assert_eq!(with(r#"{"cpu":{"quota":-1,"period":100000}}"#).cpus, 1);
        assert_eq!(with(r#"{"cpu":{"shares":2}}"#).cpus, 1);
        assert_eq!(with(r#"{"memory":{"limit":536870912}}"#).memory_mib, 512);
        assert_eq!(with(r#"{"memory":{"limit":536870913}}"#).memory_mib, 513);
        assert_eq!(with(r#"{"memory":{"limit":1}}"#).memory_mib, 64);
        assert_eq!(with("{}").memory_mib, 1024);
        assert_eq!(
            resources(&spec("{}"), &s),
            Resources {
                cpus: 1,
                memory_mib: 1024
            }
        );
    }
}
