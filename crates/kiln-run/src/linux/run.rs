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
    /// Dropped before `run_dir`, whose run.json records it.
    scratch_dir: ScratchDir,
    run_dir: RunDir,
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
    /// The run directory and the scratch directory, when kept (`KILN_KEEP_RUN_DIR=1`).
    pub kept: Option<(PathBuf, PathBuf)>,
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
        // Held until the VMM has every blob open (spec §5.2).
        let lock = store.lock_shared()?;
        let prepared = prepare::prepare(store, opts, prepare::host_arch())?;
        check()?;
        for w in &prepared.warnings {
            notice(&format!("warning: {w}"));
        }
        let vmm = backend(opts.vmm).discover()?;
        let caps = vmm.capabilities();
        config::check_devices(prepared.layers.len(), false, caps.available_devices(), vmm.name())?;
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
        let scratch_path = scratch_base.join(&run_dir.id);
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
            scratch_dir: Some(scratch_path.display().to_string()),
        };
        // Recorded before it exists, so a stale cleanup always knows of it.
        run_dir.write_info(&info)?;
        let mut scratch_dir = ScratchDir::create(&scratch_base, &ScratchInfo::from(&info))?;
        if keep {
            scratch_dir.keep();
        }

        let init_layer = run_dir.path.join("init.erofs");
        std::fs::write(&init_layer, &prepared.init_layer)?;
        let scratch = scratch_dir.path.join("scratch.img");
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
            None,
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
        scratch_dir.write_info(&ScratchInfo::from(&info))?;
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
                end_timeout: END_TIMEOUT,
                escape_keys: opts.tty && opts.interactive,
                ..SessionOptions::default()
            },
        )?;
        Ok(Run {
            session,
            console,
            scratch_dir,
            run_dir,
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
        let kept = keep.then(|| (run_dir.path.clone(), scratch_dir.path.clone()));
        let vmm_logs = vmm_logs(vmm, &run_dir.path);
        // The scratch directory first: while run.json lasts, it records it.
        drop(scratch_dir);
        drop(run_dir);
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
            kept: kept.then(|| (dir.to_path_buf(), PathBuf::from("/s/scratch/abc"))),
            vmm_logs: vmm_logs(VmmKind::Firecracker, dir),
            memory_mib: 512,
            limited,
        }
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
