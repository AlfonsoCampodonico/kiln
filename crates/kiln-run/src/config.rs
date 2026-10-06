//! The guest's `Config` (spec §9.5), from the image's process and the run's
//! options, checked before anything boots: valid, and small enough for one
//! control frame (D-4). Also the device budget (spec §9.7).

use kiln_image::types::Process as ImageProcess;
use kiln_proto::{Config, ExitMethod, HostMessage, MAX_FRAME, Process, ProtoError, Scratch, WindowSize};

use crate::error::{Error, Result};
use crate::options::RunOptions;

/// The run-specific values `Config` needs besides the image and options.
#[derive(Debug, Clone)]
pub struct Runtime {
    pub hostname: String,
    pub layers: u32,
    pub scratch_bytes: u64,
    pub exit_method: ExitMethod,
    /// The terminal's size, for `-t`.
    pub window: Option<WindowSize>,
    /// kiln's own environment, for `--env KEY` without a value.
    pub host_env: Vec<(String, String)>,
}

/// Builds and checks the guest's `Config`.
pub fn build(image: &ImageProcess, opts: &RunOptions, rt: &Runtime) -> Result<Config> {
    let mut env = image.env.clone();
    for e in &opts.env {
        match e.split_once('=') {
            Some(_) => env.push(e.clone()),
            // As Docker: `--env KEY` passes kiln's value, and nothing when it is unset.
            None => {
                if let Some((_, v)) = rt.host_env.iter().find(|(k, _)| k == e) {
                    env.push(format!("{e}={v}"));
                }
            }
        }
    }
    let cmd = if opts.cmd.is_empty() {
        image.cmd.clone()
    } else {
        opts.cmd.clone()
    };
    if image.entrypoint.is_empty() && cmd.is_empty() {
        return Err(Error::refused(
            "the image has no entrypoint or cmd: give a command after --",
        ));
    }
    let stop_signal = match &image.stop_signal {
        Some(s) => kiln_proto::signal::parse_stop_signal(s)?,
        None => kiln_proto::signal::DEFAULT_STOP,
    };
    let config = Config {
        process: Process {
            entrypoint: image.entrypoint.clone(),
            cmd,
            env,
            working_dir: image.working_dir.clone(),
            user: image.user.clone(),
        },
        stop_signal,
        tty: opts.tty.then(|| rt.window.unwrap_or(WindowSize { rows: 24, cols: 80 })),
        interactive: opts.interactive,
        hostname: rt.hostname.clone(),
        network: None,
        layers: rt.layers,
        scratch: Scratch {
            size_bytes: rt.scratch_bytes,
        },
        exit_method: rt.exit_method,
        shutdown_grace_secs: opts.stop_timeout,
    };
    check_size(&config)?;
    Ok(config)
}

/// Encodes `config` as the guest will receive it, refusing one over the frame limit
/// with an error that names what to shrink (D-4). Validates it too.
pub fn check_size(config: &Config) -> Result<()> {
    let mut frame = Vec::new();
    match kiln_proto::write_message(&mut frame, &HostMessage::Config(Box::new(config.clone()))) {
        Ok(()) => Ok(()),
        Err(ProtoError::Oversize(n)) => {
            let argv: usize = config.process.argv().iter().map(|a| a.len()).sum();
            let env: usize = config.process.env.iter().map(String::len).sum();
            Err(Error::refused(format!(
                "the run's configuration encodes to {n} bytes, over the control protocol's \
                 {MAX_FRAME}-byte frame: the command line has {argv} bytes and the environment {env} \
                 bytes ({} variables); pass large values in a file instead",
                config.process.env.len()
            )))
        }
        Err(e) => Err(e.into()),
    }
}

/// The virtio devices a run needs: init and scratch disks, the app layers, vsock
/// and the NIC (spec §9.1). Checked against the VMM's budget before boot.
pub fn check_devices(layers: usize, net: bool, available: u32, vmm: &str) -> Result<()> {
    let fixed = 3 + u32::from(net);
    let needed = u32::try_from(layers).ok().and_then(|n| n.checked_add(fixed));
    if needed.is_none_or(|n| n > available) {
        let needed = needed.map_or_else(|| format!("more than {}", u32::MAX), |n| n.to_string());
        return Err(Error::refused(format!(
            "the image has {layers} layers, which with the init and scratch disks, vsock{} need \
             {needed} virtio devices; {vmm} has room for {available}. Convert the image again with \
             --max-layers {} or fewer",
            if net { " and the network" } else { "" },
            available.saturating_sub(fixed)
        )));
    }
    if u32::try_from(layers).is_ok_and(|n| n > kiln_proto::MAX_LAYERS) {
        return Err(Error::refused(format!(
            "the image has {layers} layers; kiln-init mounts at most {}",
            kiln_proto::MAX_LAYERS
        )));
    }
    Ok(())
}

/// kiln's environment for [`Runtime::host_env`], from `std::env::vars_os()`:
/// entries whose name or value is not UTF-8 are skipped (`--env KEY` then passes
/// nothing for them, as for an unset variable).
pub fn host_env(vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>) -> Vec<(String, String)> {
    vars.into_iter()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

/// A hostname for the guest from the run id, as Docker uses the container id.
pub fn hostname(run_id: &str) -> String {
    run_id.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> Runtime {
        Runtime {
            hostname: "abc".into(),
            layers: 1,
            scratch_bytes: 64 << 20,
            exit_method: ExitMethod::Reboot,
            window: None,
            host_env: vec![("HOSTVAR".into(), "hv".into())],
        }
    }

    fn image() -> ImageProcess {
        ImageProcess {
            entrypoint: vec!["/entry".into()],
            cmd: vec!["default".into()],
            env: vec!["A=1".into()],
            working_dir: Some("/app".into()),
            user: Some("app".into()),
            stop_signal: Some("SIGQUIT".into()),
        }
    }

    #[test]
    fn image_and_options_merge_as_docker_merges_them() {
        let mut o = RunOptions::new("img");
        let c = build(&image(), &o, &rt()).unwrap();
        assert_eq!(c.process.argv(), ["/entry", "default"]);
        assert_eq!(c.stop_signal, 3);
        assert_eq!((c.tty, c.interactive, c.network.is_none()), (None, false, true));
        o.cmd = vec!["other".into(), "arg".into()];
        o.env = vec!["A=2".into(), "HOSTVAR".into(), "UNSET".into()];
        o.tty = true;
        o.interactive = true;
        o.stop_timeout = 3;
        let c = build(&image(), &o, &rt()).unwrap();
        assert_eq!(c.process.argv(), ["/entry", "other", "arg"]);
        assert_eq!(c.process.env, ["A=1", "A=2", "HOSTVAR=hv"]);
        assert_eq!(c.tty, Some(WindowSize { rows: 24, cols: 80 }));
        assert_eq!(c.shutdown_grace_secs, 3);
    }

    #[test]
    fn refusals() {
        let o = RunOptions::new("img");
        let mut p = image();
        p.stop_signal = Some("SIGRTMIN+3".into());
        assert!(build(&p, &o, &rt()).unwrap_err().to_string().contains("real-time"));
        let empty = ImageProcess::default();
        assert!(build(&empty, &o, &rt()).unwrap_err().to_string().contains("after --"));
    }

    #[test]
    fn an_oversized_config_is_refused_before_boot_naming_the_environment() {
        let mut o = RunOptions::new("img");
        o.env = (0..20).map(|i| format!("CERT{i}={}", "x".repeat(4000))).collect();
        let err = build(&image(), &o, &rt()).unwrap_err().to_string();
        assert!(err.contains("environment") && err.contains("21 variables"), "{err}");
        o.env.truncate(10);
        build(&image(), &o, &rt()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_host_variables_are_skipped() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let vars = [
            (OsString::from("GOOD"), OsString::from("yes")),
            (OsString::from("BADVALUE"), OsString::from_vec(b"\xff".to_vec())),
            (OsString::from_vec(b"BAD\xfeNAME".to_vec()), OsString::from("v")),
        ];
        let env = host_env(vars);
        assert_eq!(env, [("GOOD".to_string(), "yes".to_string())]);
        let mut o = RunOptions::new("img");
        o.env = vec!["GOOD".into(), "BADVALUE".into()];
        let r = Runtime { host_env: env, ..rt() };
        assert_eq!(build(&image(), &o, &r).unwrap().process.env, ["A=1", "GOOD=yes"]);
    }

    #[test]
    fn the_device_budget() {
        // Cloud Hypervisor: 31 devices, one its RNG.
        check_devices(26, true, 30, "cloud-hypervisor").unwrap();
        let err = check_devices(27, true, 30, "cloud-hypervisor").unwrap_err().to_string();
        assert!(
            err.contains("31 virtio devices") && err.contains("--max-layers 26"),
            "{err}"
        );
        check_devices(27, false, 30, "cloud-hypervisor").unwrap();
        assert!(check_devices(129, false, 300, "x").is_err());
        // Counts that do not fit a u32 are refused, never wrapped into the budget.
        if let Ok(huge) = usize::try_from(u64::from(u32::MAX) - 1) {
            let err = check_devices(huge, true, 30, "x").unwrap_err().to_string();
            assert!(err.contains("need more than"), "{err}");
            let err = check_devices(huge.saturating_mul(2), false, 30, "x")
                .unwrap_err()
                .to_string();
            assert!(err.contains("need more than"), "{err}");
        }
    }
}
