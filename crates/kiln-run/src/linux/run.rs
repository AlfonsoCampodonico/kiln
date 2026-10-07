//! One `kiln run`, start to end (spec §9): resolve, check, prepare the run
//! directory and disks, boot through vmkit, run the session, report.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kiln_image::artifacts::RegistryFetcher;
use kiln_proto::{ExitMethod, GUEST_CID};
use kiln_store::Store;
use vmkit::{Backend, Disk, GuestExit, NetSpec, VmSpec, Vmm, VsockSpec};

use super::console::ConsoleRelay;
use super::session::{Handle, Outcome, Session, SessionOptions, Streams};
use crate::config::{self, Runtime};
use crate::console::{TAIL_LINES, tail};
use crate::error::{Error, Result};
use crate::options::{DEFAULT_DISK, Egress, NetOptions, Protocol, RunOptions, VmmKind};
use crate::persist;
use crate::prepare::{self, InitLayer};
use crate::rundir::{self, Identity, RUN_INFO_VERSION, RunDir, RunInfo, ScratchDir, ScratchInfo};

/// The kernel command line (spec §8.3); vmkit appends the console and its backend's parameters.
pub const CMDLINE: [&str; 7] = [
    "root=/dev/vda",
    "ro",
    "rootfstype=erofs",
    "init=/kiln-init",
    "panic=-1",
    "quiet",
    "loglevel=3",
];

/// How long the guest has to end the VM after its final message (D-3; the
/// scratch disk was synced before `Exited`); longer with `--persist`, whose disk
/// outlives the run.
const END_TIMEOUT: Duration = Duration::from_secs(10);
const END_TIMEOUT_PERSIST: Duration = Duration::from_secs(60);

pub fn backend(kind: VmmKind) -> Backend {
    match kind {
        VmmKind::Firecracker => Backend::Firecracker,
        VmmKind::CloudHypervisor => Backend::CloudHypervisor,
    }
}

pub fn exit_method(vmm: &dyn Vmm) -> ExitMethod {
    match vmm.capabilities().guest_exit {
        GuestExit::Reboot => ExitMethod::Reboot,
        GuestExit::Poweroff => ExitMethod::Poweroff,
    }
}

/// `--net`'s options as vmkit's (spec §9.3).
pub fn net_spec(n: &NetOptions) -> NetSpec {
    use vmkit::net::{Cidr, Egress as E, PortForward, Protocol as P};
    NetSpec {
        egress: match n.egress {
            Egress::Restricted => E::Restricted,
            Egress::DenyAll => E::DenyAll,
            Egress::Open => E::Open,
        },
        allow: n.allow.iter().filter_map(|c| Cidr::new(c.addr, c.prefix)).collect(),
        forwards: n
            .publish
            .iter()
            .map(|p| PortForward {
                protocol: match p.protocol {
                    Protocol::Tcp => P::Tcp,
                    Protocol::Udp => P::Udp,
                },
                address: p.address,
                host: p.host,
                guest: p.guest,
            })
            .collect(),
    }
}

/// The VM: init layer, scratch disk, app layers, and a network when `net` is set (spec §9.1).
#[allow(clippy::too_many_arguments)]
pub fn vm_spec(
    kernel: &Path,
    init_layer: &Path,
    scratch: &Path,
    layers: &[PathBuf],
    cpus: u8,
    memory_mib: u32,
    net: Option<NetSpec>,
    console_log: &Path,
    run_dir: &Path,
) -> VmSpec {
    let mut disks = vec![
        Disk {
            path: init_layer.to_path_buf(),
            read_only: true,
        },
        Disk {
            path: scratch.to_path_buf(),
            read_only: false,
        },
    ];
    disks.extend(layers.iter().map(|p| Disk {
        path: p.clone(),
        read_only: true,
    }));
    VmSpec {
        kernel: kernel.to_path_buf(),
        initramfs: None,
        cmdline: CMDLINE.map(String::from).to_vec(),
        disks,
        vcpus: cpus,
        memory_mib,
        vsock: Some(VsockSpec { guest_cid: GUEST_CID }),
        net,
        console_log: console_log.to_path_buf(),
        run_dir: run_dir.to_path_buf(),
    }
}

/// The VMM's identity, from vmkit's PID file, read while a pidfd pins the process.
pub fn vmm_identity(run_dir: &Path) -> Option<Identity> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::process::{Pid, PidfdFlags, pidfd_open};
    let pid: u32 = std::fs::read_to_string(vmkit::sandbox::pid_file(run_dir))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let fd = pidfd_open(Pid::from_raw(i32::try_from(pid).ok()?)?, PidfdFlags::empty()).ok()?;
    let id = rundir::identity(pid)?;
    // Readable means exited: then the start time may belong to another process.
    let zero = Timespec { tv_sec: 0, tv_nsec: 0 };
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    (poll(&mut fds, Some(&zero)).ok()? == 0).then_some(id)
}

/// A run in progress.
pub struct Run {
    session: Session,
    console: ConsoleRelay,
    /// Dropped before `run_dir`, whose run.json records it; none with `--persist`.
    scratch_dir: Option<ScratchDir>,
    run_dir: RunDir,
    /// The `--persist` disk, locked until the run ends.
    _persist: Option<std::fs::File>,
    /// `KILN_KEEP_RUN_DIR=1` at the start.
    keep: bool,
    /// For the message when the VM ends without the guest's final message.
    vmm: VmmKind,
    memory_mib: u32,
    /// The VMM runs in a cgroup with a memory limit.
    limited: bool,
}

/// How a run ended, for the CLI.
#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    /// The sanitised end of the console when the run failed (T8).
    pub console_tail: Vec<String>,
    /// The run directory and the scratch directory (none with `--persist`), when
    /// kept (`KILN_KEEP_RUN_DIR=1`).
    pub kept: Option<(PathBuf, Option<PathBuf>)>,
    /// The VMM's own logs, in the run directory.
    pub vmm_logs: Vec<PathBuf>,
    /// Guest memory, and whether the VMM's memory was limited to it plus 256 MiB.
    pub memory_mib: u32,
    pub limited: bool,
}

impl Report {
    /// What to say when the VM ended without the guest's final message (no
    /// `Exited` or `InitFailed`): vmkit's end reason cannot tell why (a VMM killed
    /// for its memory use looks `Exited`), so the likely causes are named, with
    /// where to look.
    pub fn no_final_message(&self) -> String {
        let names = |full: bool| {
            self.vmm_logs
                .iter()
                .map(|p| {
                    if full {
                        p.display().to_string()
                    } else {
                        p.file_name().unwrap_or_default().to_string_lossy().into_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let memory = if self.limited {
            format!(
                "the VMM ran out of memory (it may use --memory plus 256 MiB, {} MiB here; the kernel log shows an \
                 out-of-memory kill)",
                u64::from(self.memory_mib) + 256
            )
        } else {
            "the host ran out of memory (the kernel log shows an out-of-memory kill)".to_string()
        };
        let logs = if self.kept.is_some() {
            format!("the VMM's log is in {}", names(true))
        } else {
            format!(
                "KILN_KEEP_RUN_DIR=1 keeps the run directory, with the VMM's log ({}) and console.log",
                names(false)
            )
        };
        format!(
            "the VM ended without reporting the command's exit (the VMM: {:?}). Likely causes: the VMM failed, or {memory}; \
             {logs}",
            self.outcome.end.reason
        )
    }
}

/// The VMM's own log files in a run directory (vmkit writes them there).
pub fn vmm_logs(kind: VmmKind, run_dir: &Path) -> Vec<PathBuf> {
    match kind {
        VmmKind::Firecracker => vec![run_dir.join("sock/firecracker.log"), run_dir.join("firecracker.stderr")],
        VmmKind::CloudHypervisor => vec![run_dir.join("cloud-hypervisor.log")],
    }
}

impl Run {
    /// Resolves, checks and boots. `notice` gets one-line warnings and notes, before
    /// boot. When `interrupted` turns true during setup (the user's SIGINT or
    /// SIGTERM), setup stops with [`Error::Interrupted`], removing what it made.
    pub fn start(
        store: &Store,
        opts: &RunOptions,
        streams: Streams,
        notice: &mut dyn FnMut(&str),
        interrupted: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let check = || if interrupted() { Err(Error::Interrupted) } else { Ok(()) };
        opts.check()?;
        let net = opts.net.as_ref().map(net_spec);
        if let Some(n) = &net {
            // pasta gives the VM's namespace the host's outbound address, so the guest
            // can never reach it (finding 4.1): an exception for it alone is refused,
            // a wider one that includes it is warned about.
            let outbound = vmkit::net::outbound_address();
            n.check_allow(outbound.as_slice())
                .map_err(|e| Error::refused(format!("--egress: {e}")))?;
            if n.egress == vmkit::net::Egress::Open {
                notice("warning: --egress open: the guest can reach private networks, the host and cloud metadata");
            } else if let Some(a) = outbound {
                for c in n.allow.iter().filter(|c| c.contains(a)) {
                    notice(&format!(
                        "warning: --egress allow={c} includes the host's own address {a}, which the guest can never \
                         reach"
                    ));
                }
            }
        }
        // The Lima VM's dev loop (spec §10): the macOS store's latest convert of the image.
        if let Some(from) = std::env::var_os("KILN_IMPORT_FROM").filter(|v| !v.is_empty()) {
            match prepare::import_from(store, Path::new(&from), &opts.image) {
                Ok(Some(note)) => notice(&note),
                Ok(None) => {}
                Err(e) => notice(&format!("warning: importing from KILN_IMPORT_FROM: {e}")),
            }
        }
        check()?;
        // Held until the VMM has every blob open (spec §5.2).
        let lock = store.lock_shared()?;
        let prepared = prepare::prepare(
            store,
            opts,
            prepare::host_arch(),
            &prepare::CompiledPins,
            &RegistryFetcher::from_env(),
        )?;
        check()?;
        for w in &prepared.warnings {
            notice(&format!("warning: {w}"));
        }
        let vmm = backend(opts.vmm).discover()?;
        let caps = vmm.capabilities();
        config::check_devices(
            prepared.layers.len(),
            net.is_some(),
            caps.available_devices(),
            vmm.name(),
        )?;
        let limited = vmkit::cgroups_available();
        if !limited {
            notice("warning: no systemd user session for a cgroup: the VM's memory and CPU use are not limited");
        }

        let base = rundir::base_dir()?;
        let (boot_id, hostname) = (rundir::boot_id(), rundir::hostname());
        let cleanup = rundir::clean_stale(&base, &boot_id, &hostname, rundir::alive);
        if let Some(note) = cleanup.notice(&base) {
            notice(&note);
        }
        // On the store's filesystem, not the run directory's tmpfs (spec §5.3, rev 2.9).
        // Those whose run directories are gone (a reboot or a logout emptied them) are
        // swept by their own identity.
        let scratch_base = rundir::scratch_base(store.root())?;
        let swept = rundir::clean_stale_scratch(
            &scratch_base,
            &boot_id,
            &hostname,
            rundir::alive,
            std::time::SystemTime::now(),
        );
        if let Some(note) = swept.scratch_notice(&scratch_base) {
            notice(&note);
        }
        let mut run_dir = RunDir::create(&base)?;
        let keep = std::env::var_os("KILN_KEEP_RUN_DIR").is_some_and(|v| v == "1");
        if keep {
            run_dir.keep();
        }
        // A persisted disk replaces the run's scratch directory (spec §9.4).
        let scratch_path = opts.persist.is_none().then(|| scratch_base.join(&run_dir.id));
        let mut info = RunInfo {
            version: RUN_INFO_VERSION,
            id: run_dir.id.clone(),
            boot_id,
            hostname,
            kiln: rundir::identity(std::process::id()).ok_or_else(|| Error::refused("cannot read /proc/self/stat"))?,
            vmm: None,
            vmm_kind: opts.vmm.name().into(),
            image: opts.image.clone(),
            custom_kernel: prepared.custom_kernel.as_ref().map(|p| p.display().to_string()),
            custom_init: prepared.custom_init.as_ref().map(|p| p.display().to_string()),
            keep,
            scratch_dir: scratch_path.as_ref().map(|p| p.display().to_string()),
        };
        // Recorded before it exists, so a stale cleanup always knows of it.
        run_dir.write_info(&info)?;
        let mut scratch_dir = match &scratch_path {
            Some(_) => Some(ScratchDir::create(&scratch_base, &ScratchInfo::from(&info))?),
            None => None,
        };
        if keep && let Some(d) = &mut scratch_dir {
            d.keep();
        }

        // The pinned kiln-init's layer is a blob in the store, opened by the VMM while
        // the lock is held; one built from --init goes in the run directory.
        let init_layer = match &prepared.init {
            InitLayer::Stored(path) => path.clone(),
            InitLayer::Built(bytes) => {
                let path = run_dir.path.join("init.erofs");
                std::fs::write(&path, bytes)?;
                path
            }
        };
        let (scratch, scratch_bytes, persist_file) = match (&opts.persist, &scratch_dir) {
            (Some(path), _) => {
                let existing = persist::inspect(path)?;
                let (plan, lines) = persist::decide(
                    path,
                    existing.as_ref(),
                    &prepared.manifest,
                    &prepared.layers,
                    opts.disk,
                    opts.persist_reset,
                    opts.persist_force,
                )?;
                for line in lines {
                    notice(&line);
                }
                let file = persist::apply(path, &plan, &prepared.manifest, &prepared.layers)?;
                let size = file.metadata()?.len();
                (std::fs::canonicalize(path)?, size, Some(file))
            }
            (None, Some(dir)) => {
                let path = dir.path.join("scratch.img");
                let size = opts.disk.unwrap_or(DEFAULT_DISK);
                kiln_image::scratch::create_scratch(&path, size)?;
                (path, size, None)
            }
            (None, None) => return Err(Error::refused("no scratch directory for the run")),
        };
        let window = if opts.tty { super::tty::window_size() } else { None };
        let config = config::build(
            &prepared.process,
            opts,
            &Runtime {
                hostname: config::hostname(&run_dir.id),
                layers: prepared.layers.len() as u32,
                scratch_bytes,
                exit_method: exit_method(vmm.as_ref()),
                window,
                host_env: config::host_env(std::env::vars_os()),
            },
        )?;
        check()?;
        let console = ConsoleRelay::start(&run_dir.path)?;
        let spec = vm_spec(
            &prepared.kernel,
            &init_layer,
            &scratch,
            &prepared.layer_paths,
            opts.cpus,
            opts.memory_mib,
            net,
            &console.fifo,
            &run_dir.path,
        );
        let vm = vmm.create(&spec)?;
        info.vmm = vmm_identity(&run_dir.path);
        if info.vmm.is_none() {
            notice(
                "warning: could not read the VMM's identity for run.json; if kiln is killed, its run directory \
                 may be cleaned up while the VMM is still exiting",
            );
        }
        run_dir.write_info(&info)?;
        if let Some(d) = &scratch_dir {
            d.write_info(&ScratchInfo::from(&info))?;
        }
        drop(lock);
        // Dropping the VM kills it; the run's directories go with `run_dir` and `scratch_dir`.
        check()?;
        let session = Session::start(
            vm,
            config,
            streams,
            SessionOptions {
                boot_timeout: opts.boot_timeout,
                stop_timeout: opts.stop_timeout,
                end_timeout: if opts.persist.is_some() {
                    END_TIMEOUT_PERSIST
                } else {
                    END_TIMEOUT
                },
                escape_keys: opts.tty && opts.interactive,
                ..SessionOptions::default()
            },
        )?;
        Ok(Run {
            session,
            console,
            scratch_dir,
            run_dir,
            _persist: persist_file,
            keep,
            vmm: opts.vmm,
            memory_mib: opts.memory_mib,
            limited,
        })
    }

    pub fn handle(&self) -> Handle {
        self.session.handle()
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir.path
    }

    /// Runs to the end; the run's directories are removed unless kept.
    pub fn wait(self) -> Report {
        let Run {
            session,
            console,
            scratch_dir,
            run_dir,
            _persist,
            keep,
            vmm,
            memory_mib,
            limited,
        } = self;
        let outcome = session.finish();
        let log = console.log.clone();
        console.finish(Duration::from_secs(2));
        let failed = outcome.violation.is_some()
            || outcome.init_failed.is_some()
            || (outcome.exited.is_none() && !outcome.killed);
        let console_tail = if failed { tail(&log, TAIL_LINES) } else { Vec::new() };
        let kept = keep.then(|| (run_dir.path.clone(), scratch_dir.as_ref().map(|d| d.path.clone())));
        let vmm_logs = vmm_logs(vmm, &run_dir.path);
        // The scratch directory first: while run.json lasts, it records it. The
        // persisted disk's lock goes once the VMM has ended.
        drop(scratch_dir);
        drop(run_dir);
        drop(_persist);
        Report {
            outcome,
            console_tail,
            kept,
            vmm_logs,
            memory_mib,
            limited,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmkit::{EndReason, VmEnd};

    fn report(kept: bool, limited: bool) -> Report {
        let dir = Path::new("/run/user/1/kiln/abc");
        Report {
            outcome: Outcome {
                exit_code: 125,
                exited: None,
                init_failed: None,
                stages: vec![3, 4, 5, 6, 7],
                hello: true,
                running: true,
                violation: None,
                killed: false,
                end: VmEnd {
                    reason: EndReason::Exited,
                    code: Some(0),
                    signal: None,
                },
                warnings: Vec::new(),
                hello_after: None,
                running_after: None,
                ended_after: None,
                drained_after: Duration::ZERO,
            },
            console_tail: Vec::new(),
            kept: kept.then(|| (dir.to_path_buf(), Some(PathBuf::from("/s/scratch/abc")))),
            vmm_logs: vmm_logs(VmmKind::Firecracker, dir),
            memory_mib: 512,
            limited,
        }
    }

    /// kiln's copy of vmkit's fixed addressing (the platform-independent config
    /// builds the guest's `Config` from it) matches vmkit's.
    #[test]
    fn the_guest_addressing_is_vmkits() {
        assert_eq!(config::GUEST, vmkit::net::GUEST);
        assert_eq!(config::GATEWAY, vmkit::net::GATEWAY);
        assert_eq!(config::PREFIX, vmkit::net::PREFIX);
    }

    #[test]
    fn net_options_map_onto_vmkit() {
        use std::net::Ipv4Addr;
        let mut n = NetOptions::default();
        let mut set = false;
        n.add_egress("deny-all", &mut set).unwrap();
        n.add_egress("allow=10.1.2.3/16", &mut set).unwrap();
        n.add_egress("allow=192.0.2.7", &mut set).unwrap();
        n.publish = vec!["8080:80".parse().unwrap(), "0.0.0.0:5353:53/udp".parse().unwrap()];
        let spec = net_spec(&n);
        assert_eq!(spec.egress, vmkit::net::Egress::DenyAll);
        assert_eq!(spec.allow[0].to_string(), "10.1.0.0/16");
        let f = &spec.forwards;
        assert_eq!(
            (f[0].address, f[0].host, f[0].guest, f[0].protocol),
            (Ipv4Addr::LOCALHOST, 8080, 80, vmkit::net::Protocol::Tcp)
        );
        assert_eq!(
            (f[1].address, f[1].protocol),
            (Ipv4Addr::UNSPECIFIED, vmkit::net::Protocol::Udp)
        );
        // An exception that is only the host's outbound address is refused before boot;
        // one that includes it still reaches its other addresses.
        assert!(spec.check_allow(&[Ipv4Addr::new(192, 0, 2, 7)]).is_err());
        assert!(spec.check_allow(&[Ipv4Addr::new(10, 1, 9, 9)]).is_ok());
    }

    /// The VM ended without the guest's final message: the likely causes, and
    /// where to look (the run directory goes with the run unless kept).
    #[test]
    fn a_vm_ending_without_a_final_message_names_the_likely_causes() {
        assert_eq!(
            report(false, true).no_final_message(),
            "the VM ended without reporting the command's exit (the VMM: Exited). Likely causes: the VMM failed, \
             or the VMM ran out of memory (it may use --memory plus 256 MiB, 768 MiB here; the kernel log shows an \
             out-of-memory kill); KILN_KEEP_RUN_DIR=1 keeps the run directory, with the VMM's log (firecracker.log, \
             firecracker.stderr) and console.log"
        );
        let kept = report(true, false).no_final_message();
        assert!(kept.contains("the host ran out of memory"), "{kept}");
        assert!(
            kept.ends_with(
                "the VMM's log is in /run/user/1/kiln/abc/sock/firecracker.log, \
                 /run/user/1/kiln/abc/firecracker.stderr"
            ),
            "{kept}"
        );
    }
}
