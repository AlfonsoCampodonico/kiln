//! Build mode's host side, the parts that do not need a VM (spec §9.8): what a
//! build VM and a step are ([`BuildSpec`], [`RunStep`]), the device budget, the
//! `Config` and `RunStep` messages they become, and the order the guest's build
//! messages must come in ([`BuildProtocol`]). The VM itself is
//! [`BuildVm`](crate::BuildVm) (Linux).
//!
//! Disks, in order (nothing is hot-plugged, so a layer a step stacks later is
//! attached at boot): `vda` the init layer, `vdb` scratch, `vdc` the output disk
//! (raw, created and sized by the caller), `layers` (erofs, bottom first; the
//! first `root` form the boot root), then `cache_disks` (ext4).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use kiln_proto::{
    BuildDisk, Command, Config, DiskKind, ExitMethod, GuestMessage, MAX_BUILD_LOWERS, MAX_FRAME, Mode, Network,
    Process, Scratch, StepExited, StepMount, StepSealed,
};

use crate::error::{Error, Result};
use crate::options::VmmKind;

/// The devices every build VM has: init layer, scratch, output disk, vsock. A VM
/// with `network` has a NIC too.
pub const FIXED_DEVICES: usize = 4;
/// After a step's timeout the host signals its process group with SIGKILL; when
/// the step has not ended this long after, the host kills the VM.
pub const STEP_GRACE: Duration = Duration::from_secs(10);
/// How long a killed VMM gets to exit before the host stops waiting for it. So
/// `run_step` returns within `timeout + STEP_GRACE + KILL_WAIT`.
pub const KILL_WAIT: Duration = Duration::from_secs(5);

/// The disks a build VM can hold besides its fixed devices (layers and cache
/// disks together), given the VMM's available devices.
pub fn device_budget(available_devices: u32, network: bool) -> usize {
    (available_devices as usize).saturating_sub(FIXED_DEVICES + usize::from(network))
}

/// One build VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSpec {
    pub vmm: VmmKind,
    /// The kernel and the static kiln-init binary (this kiln pins neither yet).
    pub kernel: PathBuf,
    pub init: PathBuf,
    /// Read-only erofs layer disks, bottom first.
    pub layers: Vec<PathBuf>,
    /// The first `root` layers are the boot root; the others are attached and
    /// only stacked when a step's `lowers_added` names them.
    pub root: usize,
    /// ext4 cache disks and the one target each is mounted on (read-write).
    pub cache_disks: Vec<(PathBuf, String)>,
    /// The output disk the caller created: raw, `output_size` bytes, a
    /// multiple of 4096. kiln never reads it.
    pub output: PathBuf,
    pub output_size: u64,
    /// A NIC (vmkit's restricted egress); each step turns egress on or off.
    pub network: bool,
    pub memory_mib: u32,
    pub cpus: u8,
    /// The scratch disk holding every step's upper (a multiple of 4096, at least 64 MiB).
    pub scratch_bytes: u64,
    pub boot_timeout: Duration,
    /// `SOURCE_DATE_EPOCH`: when set, every sealed entry's mtime is clamped to
    /// at most this (copied-up parent directories included); otherwise the
    /// guest's times are kept.
    pub source_date_epoch: Option<u64>,
}

/// A secret's value; never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub Vec<u8>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(<{} bytes>)", self.0.len())
    }
}

/// Which of a step's output streams bytes came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// One build step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStep {
    pub id: u32,
    pub command: Command,
    pub env: BTreeMap<String, String>,
    /// Absolute; created as root when missing.
    pub workdir: String,
    pub uid: u32,
    pub gid: u32,
    /// Indices into `BuildSpec.layers` (at or above `root`, each once per VM)
    /// stacked on the root before the step, bottom first.
    pub lowers_added: Vec<usize>,
    /// Targets of the cache disks this step mounts.
    pub mounts: Vec<String>,
    /// Secret targets and values: on a tmpfs in the guest, never in the diff.
    pub secrets: Vec<(String, Secret)>,
    /// Whether the step may reach the network (a VM without one has none).
    pub network: bool,
    /// For the command and its seal together.
    pub timeout: Duration,
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    /// The step outlived its timeout and was killed (with the VM, when it did not end).
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    pub exit: Exit,
    /// After `Exit::Code(0)`: where the guest says the diff is. Guest-asserted:
    /// passed through unchecked but for its shape and id.
    pub sealed: Option<StepSealed>,
}

impl BuildSpec {
    /// Checks the spec before anything is created.
    pub fn check(&self, available_devices: u32) -> Result<()> {
        let refuse = |m: String| Err(Error::refused(m));
        if self.root > self.layers.len() {
            return refuse(format!("root {} of {} layers", self.root, self.layers.len()));
        }
        let disks = self.layers.len() + self.cache_disks.len();
        let budget = device_budget(available_devices, self.network);
        if disks > budget {
            return refuse(format!(
                "{disks} layer and cache disks, and this VMM has room for {budget} besides its fixed devices"
            ));
        }
        if disks > kiln_proto::MAX_LAYERS as usize {
            return refuse(format!(
                "{disks} layer and cache disks (at most {})",
                kiln_proto::MAX_LAYERS
            ));
        }
        if self.output_size == 0 || !self.output_size.is_multiple_of(kiln_proto::DIFF_ALIGN) {
            return refuse(format!(
                "output disk size {} (a positive multiple of 4096)",
                self.output_size
            ));
        }
        let mut targets = BTreeSet::new();
        for (_, t) in &self.cache_disks {
            if !t.starts_with('/') || !targets.insert(t) {
                return refuse(format!("cache disk target {t:?}: absolute and unique"));
            }
        }
        Ok(())
    }

    /// The disk index of layer `i`.
    pub fn layer_disk(i: usize) -> u32 {
        Config::OUTPUT_DISK + 1 + i as u32
    }

    /// The guest's `Config`.
    pub fn config(&self, exit_method: ExitMethod) -> Config {
        let disks = self.layers[self.root..]
            .iter()
            .map(|_| BuildDisk {
                kind: DiskKind::Erofs,
                read_only: true,
            })
            .chain(self.cache_disks.iter().map(|_| BuildDisk {
                kind: DiskKind::Ext4,
                read_only: false,
            }))
            .collect();
        Config {
            process: Process::default(),
            stop_signal: kiln_proto::signal::DEFAULT_STOP,
            tty: None,
            interactive: false,
            hostname: "kiln-build".into(),
            network: self.network.then(guest_network),
            layers: self.root as u32,
            scratch: Scratch {
                size_bytes: self.scratch_bytes,
            },
            exit_method,
            shutdown_grace_secs: 10,
            // kiln-shim's volumes and pod disk are run mode only.
            volumes: Vec::new(),
            pod_disk: None,
            services: Vec::new(),
            ready_timeout_ms: None,
            mode: Mode::Build,
            disks,
            source_date_epoch: self.source_date_epoch,
        }
    }

    /// The lower layers a step's overlay has before its `lowers_added`: the
    /// root layers, the layers stacked so far and the steps sealed so far.
    pub fn lowers(&self, stacked: &BTreeSet<usize>, sealed: usize) -> usize {
        self.root + stacked.len() + sealed
    }

    /// The protocol `RunStep` for `step`; `stacked` holds the layers stacked so
    /// far and `sealed` counts the steps sealed so far. A step that would mount
    /// more than [`MAX_BUILD_LOWERS`] lowers is [`Error::TooManyLowers`].
    pub fn proto_step(&self, step: &RunStep, stacked: &BTreeSet<usize>, sealed: usize) -> Result<kiln_proto::RunStep> {
        let mut seen = BTreeSet::new();
        for &i in &step.lowers_added {
            if i < self.root || i >= self.layers.len() || stacked.contains(&i) || !seen.insert(i) {
                return Err(Error::refused(format!(
                    "step {}: layer {i} is not one left to stack (layers {}..{})",
                    step.id,
                    self.root,
                    self.layers.len()
                )));
            }
        }
        let lowers = self.lowers(stacked, sealed) + step.lowers_added.len();
        if lowers > MAX_BUILD_LOWERS {
            return Err(Error::TooManyLowers {
                step: step.id,
                lowers,
                max: MAX_BUILD_LOWERS,
            });
        }
        let mounts =
            step.mounts
                .iter()
                .map(|t| {
                    let j = self.cache_disks.iter().position(|(_, ct)| ct == t).ok_or_else(|| {
                        Error::refused(format!("step {}: no cache disk has the target {t:?}", step.id))
                    })?;
                    Ok(StepMount {
                        disk: Self::layer_disk(self.layers.len() + j),
                        target: t.clone(),
                        read_only: false,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
        let msg = kiln_proto::RunStep {
            id: step.id,
            command: step.command.clone(),
            env: step.env.iter().map(|(k, v)| format!("{k}={v}")).collect(),
            workdir: step.workdir.clone(),
            uid: step.uid,
            gid: step.gid,
            lowers_added: step.lowers_added.iter().map(|&i| Self::layer_disk(i)).collect(),
            mounts,
            secrets: step
                .secrets
                .iter()
                .map(|(t, s)| kiln_proto::Secret {
                    target: t.clone(),
                    data: s.0.clone(),
                })
                .collect(),
            egress: step.network && self.network,
            timeout_ms: u32::try_from(step.timeout.as_millis()).unwrap_or(u32::MAX).max(1),
        };
        msg.validate()?;
        // Secrets travel inline: the whole message must fit one frame.
        let size = 1 + serde_json::to_vec(&msg).map(|v| v.len()).unwrap_or(usize::MAX);
        if size > MAX_FRAME as usize {
            return Err(Error::refused(format!(
                "step {}: the step's message is {size} bytes, more than a control frame's {MAX_FRAME} \
                 (secrets travel inline: at most about 44 KiB of them)",
                step.id
            )));
        }
        Ok(msg)
    }
}

/// vmkit's tap network, as the guest configures `eth0`: IPv4 only (potter spec
/// §9.5 drops IPv6 for builds), DNS through the gateway.
fn guest_network() -> Network {
    use crate::config::{GATEWAY, GUEST, PREFIX};
    Network {
        address: GUEST,
        prefix_len: PREFIX,
        gateway: GATEWAY,
        dns: vec![GATEWAY],
        search: Vec::new(),
        options: Vec::new(),
        ipv6: None,
    }
}

/// Where the guest is in build mode's message order:
///
/// `Hello` (version 3 or later), `Stage` 3–6 increasing, `Running`, then for
/// each step the host sent: `StepExited` with its id, and after a code of 0,
/// `StepSealed` with its id. `InitFailed` may come at any point after `Hello`
/// and ends the VM. Nothing else is accepted, so the guest has at most one
/// message to queue per state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildProtocol {
    hello: bool,
    stage: u8,
    running: bool,
    /// The step sent and not yet finished.
    step: Option<u32>,
    exited: Option<StepExited>,
    failed: bool,
}

/// What a guest message did to a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildEvent {
    /// Hello, a stage, or `Running`.
    Boot,
    Exited(StepExited),
    Sealed(StepSealed),
    Failed(kiln_proto::InitFailed),
}

impl BuildProtocol {
    pub fn running(&self) -> bool {
        self.running
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    /// The host sends `RunStep { id }`: only when running and between steps.
    pub fn start_step(&mut self, id: u32) -> std::result::Result<(), String> {
        if !self.running || self.step.is_some() || self.failed {
            return Err("a step can only start between steps".into());
        }
        self.step = Some(id);
        self.exited = None;
        Ok(())
    }

    /// The step is over (its outcome was returned, or it was abandoned).
    pub fn end_step(&mut self) {
        self.step = None;
        self.exited = None;
    }

    pub fn accept(&mut self, msg: &GuestMessage) -> std::result::Result<BuildEvent, String> {
        if self.failed {
            return Err("a message after InitFailed".into());
        }
        match msg {
            GuestMessage::Hello(h) => {
                if self.hello || h.protocol < 3 || h.protocol > kiln_proto::PROTOCOL_VERSION {
                    return Err(format!("Hello {} (build mode needs version 3)", h.protocol));
                }
                self.hello = true;
                self.stage = 2;
                return Ok(BuildEvent::Boot);
            }
            _ if !self.hello => return Err("a message before Hello".into()),
            GuestMessage::InitFailed(f) => {
                self.failed = true;
                return Ok(BuildEvent::Failed(f.clone()));
            }
            GuestMessage::Stage(s) if !self.running && s.n > self.stage && (3..=6).contains(&s.n) => {
                self.stage = s.n;
                return Ok(BuildEvent::Boot);
            }
            GuestMessage::Running if !self.running => {
                self.running = true;
                return Ok(BuildEvent::Boot);
            }
            GuestMessage::StepExited(e) if self.step == Some(e.id) && self.exited.is_none() => {
                self.exited = Some(*e);
                return Ok(BuildEvent::Exited(*e));
            }
            GuestMessage::StepSealed(s)
                if self.step == Some(s.id) && self.exited.is_some_and(|e| e.code == Some(0)) =>
            {
                let sealed = s.clone();
                self.step = None;
                return Ok(BuildEvent::Sealed(sealed));
            }
            _ => {}
        }
        Err(format!(
            "{} out of order (stage {}, running {}, step {:?})",
            match msg {
                GuestMessage::Stage(s) => format!("Stage {}", s.n),
                GuestMessage::StepExited(e) => format!("StepExited for step {}", e.id),
                GuestMessage::StepSealed(s) => format!("StepSealed for step {}", s.id),
                GuestMessage::Running => "Running".into(),
                _ => "a run-mode message".into(),
            },
            self.stage,
            self.running,
            self.step
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_proto::{Hello, InitFailed, Stage};

    fn spec() -> BuildSpec {
        BuildSpec {
            vmm: VmmKind::Firecracker,
            kernel: "/k".into(),
            init: "/i".into(),
            layers: vec!["/l0".into(), "/l1".into(), "/l2".into()],
            root: 2,
            cache_disks: vec![("/c0".into(), "/root/.cache".into())],
            output: "/out".into(),
            output_size: 1 << 30,
            network: false,
            memory_mib: 512,
            cpus: 1,
            scratch_bytes: 1 << 30,
            boot_timeout: Duration::from_secs(30),
            source_date_epoch: None,
        }
    }

    fn step() -> RunStep {
        RunStep {
            id: 7,
            command: Command::Shell("true".into()),
            env: BTreeMap::from([("A".into(), "1".into())]),
            workdir: "/".into(),
            uid: 0,
            gid: 0,
            lowers_added: vec![2],
            mounts: vec!["/root/.cache".into()],
            secrets: vec![("/run/secrets/x".into(), Secret(b"s3cret".to_vec()))],
            network: true,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn the_budget_counts_the_nic_only_with_a_network() {
        assert_eq!(device_budget(17, false), 13);
        assert_eq!(device_budget(17, true), 12);
        assert_eq!(device_budget(3, true), 0);
        spec().check(17).unwrap();
        assert!(spec().check(7).is_err(), "4 disks do not fit in 7 - 4");
        assert!(BuildSpec { root: 4, ..spec() }.check(17).is_err());
        assert!(
            BuildSpec {
                output_size: 100,
                ..spec()
            }
            .check(17)
            .is_err()
        );
    }

    #[test]
    fn disks_follow_the_layers_and_steps_name_them_by_index() {
        let s = spec();
        let c = s.config(ExitMethod::Reboot);
        c.validate().unwrap();
        assert_eq!((c.layers, c.disks.len()), (2, 2));
        assert_eq!(c.build_disk(BuildSpec::layer_disk(2)).unwrap().kind, DiskKind::Erofs);
        assert_eq!(c.build_disk(BuildSpec::layer_disk(3)).unwrap().kind, DiskKind::Ext4);
        let m = s.proto_step(&step(), &BTreeSet::new(), 0).unwrap();
        assert_eq!(m.lowers_added, [5]);
        assert_eq!(m.mounts[0].disk, 6);
        assert_eq!(m.env, ["A=1"]);
        assert!(!m.egress, "no network in the VM, no egress");
        assert_eq!(m.timeout_ms, 5000);
        assert!(s.proto_step(&step(), &BTreeSet::from([2]), 0).is_err(), "stacked twice");
        let base = RunStep {
            lowers_added: vec![1],
            ..step()
        };
        assert!(s.proto_step(&base, &BTreeSet::new(), 0).is_err(), "a root layer");
        let unknown = RunStep {
            mounts: vec!["/x".into()],
            ..step()
        };
        assert!(s.proto_step(&unknown, &BTreeSet::new(), 0).is_err());
        assert!(!format!("{:?}", step()).contains("s3cret"));
    }

    /// Ruling 2: a step past the lowers cap is its own error, before anything is
    /// sent; the count is root layers, stacked layers and sealed steps.
    #[test]
    fn a_step_past_the_lowers_cap_is_refused_with_its_own_error() {
        let s = spec();
        assert_eq!(s.lowers(&BTreeSet::from([2]), 5), 8);
        let at_cap = MAX_BUILD_LOWERS - s.root - 1;
        s.proto_step(&step(), &BTreeSet::new(), at_cap).unwrap();
        let err = s.proto_step(&step(), &BTreeSet::new(), at_cap + 1).unwrap_err();
        assert!(
            matches!(
                err,
                Error::TooManyLowers {
                    step: 7,
                    lowers: 231,
                    max: MAX_BUILD_LOWERS
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("boot a new build VM"), "{err}");
    }

    #[test]
    fn source_date_epoch_reaches_the_config() {
        let c = BuildSpec {
            source_date_epoch: Some(1_700_000_000),
            ..spec()
        }
        .config(ExitMethod::Reboot);
        c.validate().unwrap();
        assert_eq!(c.source_date_epoch, Some(1_700_000_000));
        assert_eq!(spec().config(ExitMethod::Reboot).source_date_epoch, None);
    }

    #[test]
    fn secrets_must_fit_a_frame() {
        let big = RunStep {
            secrets: vec![("/s".into(), Secret(vec![7; 50_000]))],
            ..step()
        };
        let err = spec().proto_step(&big, &BTreeSet::new(), 0).unwrap_err().to_string();
        assert!(err.contains("inline"), "{err}");
        let ok = RunStep {
            secrets: vec![("/s".into(), Secret(vec![7; 40_000]))],
            ..step()
        };
        spec().proto_step(&ok, &BTreeSet::new(), 0).unwrap();
    }

    fn boot(p: &mut BuildProtocol) {
        for m in [
            GuestMessage::Hello(Hello { protocol: 3 }),
            GuestMessage::Stage(Stage { n: 3 }),
            GuestMessage::Stage(Stage { n: 6 }),
            GuestMessage::Running,
        ] {
            assert_eq!(p.accept(&m), Ok(BuildEvent::Boot));
        }
    }

    fn exited(id: u32, code: i32) -> GuestMessage {
        GuestMessage::StepExited(StepExited {
            id,
            code: Some(code),
            signal: None,
        })
    }

    fn sealed(id: u32) -> GuestMessage {
        GuestMessage::StepSealed(StepSealed {
            id,
            offset: 0,
            len: 4096,
            sha256: "0".repeat(64),
            entries: 1,
        })
    }

    #[test]
    fn steps_exit_then_seal_with_their_own_id() {
        let mut p = BuildProtocol::default();
        assert!(p.start_step(1).is_err(), "not running yet");
        boot(&mut p);
        p.start_step(1).unwrap();
        assert!(p.start_step(2).is_err(), "one step at a time");
        assert!(p.accept(&exited(2, 0)).is_err(), "another step's id");
        let mut q = p.clone();
        assert!(matches!(p.accept(&exited(1, 0)), Ok(BuildEvent::Exited(_))));
        assert!(p.accept(&exited(1, 0)).is_err(), "twice");
        assert!(p.accept(&sealed(2)).is_err());
        assert!(matches!(p.accept(&sealed(1)), Ok(BuildEvent::Sealed(_))));
        assert!(p.accept(&sealed(1)).is_err(), "after the step");
        p.start_step(2).unwrap();
        assert!(matches!(p.accept(&exited(2, 1)), Ok(BuildEvent::Exited(_))));
        assert!(p.accept(&sealed(2)).is_err(), "no seal after a failure");
        p.end_step();
        p.start_step(3).unwrap();
        assert!(q.accept(&sealed(1)).is_err(), "a seal before the exit");
        assert!(matches!(
            q.accept(&GuestMessage::InitFailed(InitFailed::new(7, None, "x"))),
            Ok(BuildEvent::Failed(_))
        ));
        assert!(q.accept(&exited(1, 0)).is_err());
    }

    #[test]
    fn boot_messages_come_in_order_and_once() {
        let mut p = BuildProtocol::default();
        assert!(p.accept(&GuestMessage::Hello(Hello { protocol: 2 })).is_err());
        let mut p = BuildProtocol::default();
        boot(&mut p);
        assert!(p.accept(&GuestMessage::Running).is_err());
        assert!(p.accept(&GuestMessage::Stage(Stage { n: 6 })).is_err());
        assert!(p.accept(&GuestMessage::Hello(Hello { protocol: 3 })).is_err());
        assert!(p.accept(&exited(1, 0)).is_err(), "no step sent");
    }
}
