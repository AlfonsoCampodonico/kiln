//! One `kiln run`, start to end (spec §9): resolve, check, prepare the run
//! directory and disks, boot through vmkit, run the session, report.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kiln_proto::{ExitMethod, GUEST_CID};
use kiln_store::Store;
use vmkit::{Backend, Disk, GuestExit, NetSpec, VmSpec, Vmm, VsockSpec};

use super::console::ConsoleRelay;
use super::session::{Handle, Outcome, Session, SessionOptions, Streams};
use crate::config::{self, Runtime};
use crate::console::{TAIL_LINES, tail};
use crate::error::{Error, Result};
use crate::options::{DEFAULT_DISK, RunOptions, VmmKind};
use crate::prepare;
use crate::rundir::{self, Identity, RUN_INFO_VERSION, RunDir, RunInfo};

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

/// How long the guest has to end the VM after its final message (it syncs and
/// remounts the scratch disk first, D-3).
const END_TIMEOUT: Duration = Duration::from_secs(10);

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
    run_dir: RunDir,
}

/// How a run ended, for the CLI.
#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    /// The sanitised end of the console when the run failed (T8).
    pub console_tail: Vec<String>,
    /// The run directory, when kept (`KILN_KEEP_RUN_DIR=1`).
    pub kept: Option<PathBuf>,
}

impl Run {
    /// Resolves, checks and boots. `notice` gets one-line warnings and notes, before boot.
    pub fn start(store: &Store, opts: &RunOptions, streams: Streams, notice: &mut dyn FnMut(&str)) -> Result<Self> {
        opts.check()?;
        // Held until the VMM has every blob open (spec §5.2).
        let lock = store.lock_shared()?;
        let prepared = prepare::prepare(store, opts, prepare::host_arch())?;
        for w in &prepared.warnings {
            notice(&format!("warning: {w}"));
        }
        let vmm = backend(opts.vmm).discover()?;
        let caps = vmm.capabilities();
        config::check_devices(prepared.layers.len(), false, caps.available_devices(), vmm.name())?;
        if !vmkit::cgroups_available() {
            notice("warning: no systemd user session for a cgroup: the VM's memory and CPU use are not limited");
        }

        let base = rundir::base_dir()?;
        let (boot_id, hostname) = (rundir::boot_id(), rundir::hostname());
        rundir::clean_stale(&base, &boot_id, &hostname, rundir::alive);
        let mut run_dir = RunDir::create(&base)?;
        let keep = std::env::var_os("KILN_KEEP_RUN_DIR").is_some_and(|v| v == "1");
        if keep {
            run_dir.keep();
        }
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
        };
        run_dir.write_info(&info)?;

        let init_layer = run_dir.path.join("init.erofs");
        std::fs::write(&init_layer, &prepared.init_layer)?;
        let scratch = run_dir.path.join("scratch.img");
        let scratch_bytes = opts.disk.unwrap_or(DEFAULT_DISK);
        kiln_image::scratch::create_scratch(&scratch, scratch_bytes)?;
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
                host_env: std::env::vars().collect(),
            },
        )?;
        let console = ConsoleRelay::start(&run_dir.path)?;
        let spec = vm_spec(
            &prepared.kernel,
            &init_layer,
            &scratch,
            &prepared.layer_paths,
            opts.cpus,
            opts.memory_mib,
            None,
            &console.fifo,
            &run_dir.path,
        );
        let vm = vmm.create(&spec)?;
        info.vmm = vmm_identity(&run_dir.path);
        run_dir.write_info(&info)?;
        drop(lock);
        let session = Session::start(
            vm,
            config,
            streams,
            SessionOptions {
                boot_timeout: opts.boot_timeout,
                stop_timeout: opts.stop_timeout,
                end_timeout: END_TIMEOUT,
                escape_keys: opts.tty && opts.interactive,
                ..SessionOptions::default()
            },
        )?;
        Ok(Run {
            session,
            console,
            run_dir,
        })
    }

    pub fn handle(&self) -> Handle {
        self.session.handle()
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir.path
    }

    /// Runs to the end; the run directory is removed unless kept.
    pub fn wait(self) -> Report {
        let Run {
            session,
            console,
            run_dir,
        } = self;
        let outcome = session.finish();
        let log = console.log.clone();
        console.finish(Duration::from_secs(2));
        let failed = outcome.violation.is_some()
            || outcome.init_failed.is_some()
            || (outcome.exited.is_none() && !outcome.killed);
        let console_tail = if failed { tail(&log, TAIL_LINES) } else { Vec::new() };
        let kept = std::env::var_os("KILN_KEEP_RUN_DIR")
            .is_some_and(|v| v == "1")
            .then(|| run_dir.path.clone());
        drop(run_dir);
        Report {
            outcome,
            console_tail,
            kept,
        }
    }
}
