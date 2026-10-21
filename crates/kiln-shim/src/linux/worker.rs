//! A container's VM worker (design §7.4, §8): the shim binary run as `vm`, as root,
//! with the container's stdio FIFOs as its fds 0-2 and its channel to the shim on the
//! fd [`FD_ENV`] names. As root it joins the container's cgroup and the pod netns and
//! bind-mounts the layers into a private mount namespace; then it drops to the kiln
//! user for good, and only then touches kiln-run and vmkit (kiln T6).

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kiln_proto::sanitize::clean_line;
use kiln_proto::{Config, Scratch, WindowSize};
use kiln_run::{ConsoleRelay, Session, SessionOptions, Streams, VmmKind};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::mount::{MountFlags, MountPropagationFlags, mount_bind, mount_change, mount_remount};
use rustix::process::{Gid, Signal, Uid, set_parent_process_death_signal};
use rustix::thread::{LinkNameSpaceType, UnshareFlags, move_into_link_name_space};
use vmkit::{TapAttach, Vmm};

use super::FD_ENV;
use super::plan::WorkerPlan;
use crate::TAP;
use crate::wire::{FromWorker, Lines, MAX_PLAN, ToWorker, send};

/// The exit code for a worker that could not run the guest (kiln §9.5).
const EXIT_INFRA: i32 = 125;
/// The guest's grace period on kiln's own `Shutdown` (containerd stops with signals).
const SHUTDOWN_GRACE: u32 = 10;

type Fail = String;

fn ctx<T, E: std::fmt::Display>(r: Result<T, E>, what: &str) -> Result<T, Fail> {
    r.map_err(|e| format!("{what}: {e}"))
}

/// Everything prepared before `Start`.
struct Prepared {
    vmm: Box<dyn Vmm>,
    config: Config,
    init_layer: PathBuf,
    scratch: PathBuf,
    layers: Vec<PathBuf>,
    run_dir: PathBuf,
}

pub fn main() -> ! {
    let Some(fd) = std::env::var(FD_ENV).ok().and_then(|v| v.parse::<i32>().ok()) else {
        eprintln!("kiln-shim vm: run by the shim only");
        std::process::exit(EXIT_INFRA);
    };
    // SAFETY: the shim passed this descriptor (its end of a socketpair) open, and nothing
    // else in this process owns it.
    let chan = unsafe { UnixStream::from_raw_fd(fd) };
    let _ = rustix::io::fcntl_setfd(&chan, rustix::io::FdFlags::CLOEXEC);
    let Ok(mut tx) = chan.try_clone() else {
        std::process::exit(EXIT_INFRA);
    };
    let mut lines = Lines::new(chan);
    let Ok(Some(plan)) = lines.read_within::<WorkerPlan>(MAX_PLAN) else {
        std::process::exit(EXIT_INFRA);
    };
    let prepared = match prepare(&plan) {
        Ok(p) => p,
        Err(message) => {
            let _ = send(&mut tx, &FromWorker::Failed { message });
            std::process::exit(EXIT_INFRA);
        }
    };
    if send(&mut tx, &FromWorker::Ready {}).is_err() {
        std::process::exit(EXIT_INFRA);
    }
    // Until Start, a signal ends the container as it would a runc container's init.
    loop {
        match lines.read::<ToWorker>() {
            Ok(Some(ToWorker::Start {})) => break,
            Ok(Some(ToWorker::Kill {})) | Ok(None) | Err(_) => std::process::exit(128 + 9),
            Ok(Some(ToWorker::Signal(sig))) => std::process::exit(128 + sig),
            Ok(Some(_)) => {}
        }
    }
    let code = match boot(&plan, prepared, lines, &mut tx) {
        Ok(code) => code,
        Err(message) => {
            let _ = send(&mut tx, &FromWorker::Failed { message });
            EXIT_INFRA
        }
    };
    std::process::exit(code)
}

/// cgroup v2: create the leaf and move this process into it.
fn join_cgroup(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("cgroup.procs"), std::process::id().to_string())
}

/// The root part (netns, cgroup, layer mounts), then the drop to the kiln user, then
/// everything else that does not need the VM to exist.
fn prepare(plan: &WorkerPlan) -> Result<Prepared, Fail> {
    let _ = set_parent_process_death_signal(Some(Signal::KILL));
    if let Some(dir) = &plan.cgroup
        && let Err(e) = join_cgroup(dir)
    {
        // Accounting, not isolation: the VM still runs (design §8).
        eprintln!("kiln: warning: joining the container's cgroup {}: {e}", dir.display());
    }
    if let Some(netns) = &plan.netns {
        let ns = ctx(File::open(netns), "opening the pod netns")?;
        ctx(
            move_into_link_name_space(ns.as_fd(), Some(LinkNameSpaceType::Network)),
            "joining the pod netns",
        )?;
    }
    // SAFETY: no other thread exists yet, so no thread can observe the unshared state;
    // the flags do not include FILES.
    ctx(
        unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWNS) },
        "a private mount namespace",
    )?;
    ctx(
        mount_change("/", MountPropagationFlags::PRIVATE | MountPropagationFlags::REC),
        "making mounts private",
    )?;
    let a = &plan.account;
    let layer_dir = plan.vm_dir.join("layers");
    ctx(fs::create_dir_all(&layer_dir), "creating the layer directory")?;
    let mut layers = Vec::new();
    for (n, source) in plan.layers.iter().enumerate() {
        let target = layer_dir.join(format!("{n}.erofs"));
        ctx(File::create(&target), "creating a layer mount point")?;
        ctx(
            mount_bind(source, &target),
            &format!("binding layer {}", source.display()),
        )?;
        ctx(
            mount_remount(&target, MountFlags::BIND | MountFlags::RDONLY, ""),
            "making a layer read-only",
        )?;
        layers.push(target);
    }
    // Only now, with the root work done in a directory only root could write to, does the
    // kiln user get it; never following a link, in case one appeared.
    let own = |p: &Path| {
        rustix::fs::chownat(
            rustix::fs::CWD,
            p,
            Some(Uid::from_raw(a.uid)),
            Some(Gid::from_raw(a.gid)),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|e| format!("chown {}: {e}", p.display()))
    };
    own(&layer_dir)?;
    own(&plan.vm_dir)?;
    // The drop, for good: groups, then gid, then uid (a credential change clears the
    // parent death signal, so it is set again).
    let groups: Vec<Gid> = a.groups.iter().map(|&g| Gid::from_raw(g)).collect();
    ctx(rustix::thread::set_thread_groups(&groups), "setgroups")?;
    let gid = Gid::from_raw(a.gid);
    ctx(rustix::thread::set_thread_res_gid(gid, gid, gid), "setgid")?;
    let uid = Uid::from_raw(a.uid);
    ctx(rustix::thread::set_thread_res_uid(uid, uid, uid), "setuid")?;
    let _ = set_parent_process_death_signal(Some(Signal::KILL));
    if rustix::process::geteuid().is_root() {
        return Err("still root after dropping privileges".into());
    }

    let init = ctx(
        fs::read(&plan.init),
        &format!("reading kiln-init {}", plan.init.display()),
    )?;
    let init_bytes = ctx(kiln_image::init_layer(&init, &plan.vm_dir), "building the init layer")?;
    let init_layer = plan.vm_dir.join("init.erofs");
    ctx(fs::write(&init_layer, init_bytes), "writing the init layer")?;
    ctx(fs::create_dir_all(&plan.scratch_dir), "creating the scratch directory")?;
    let scratch = plan.scratch_dir.join("scratch.img");
    ctx(
        kiln_image::scratch::create_scratch(&scratch, plan.scratch_bytes),
        "creating the scratch disk",
    )?;
    let run_dir = plan.vm_dir.join("run");
    ctx(fs::create_dir_all(&run_dir), "creating the run directory")?;
    let kind: VmmKind = plan.vmm.parse()?;
    let vmm = ctx(kiln_run::backend(kind).discover(), "finding the VMM")?;
    ctx(
        kiln_run::config::check_devices(
            layers.len(),
            plan.pod.is_some(),
            vmm.capabilities().available_devices(),
            vmm.name(),
        ),
        "the image",
    )?;
    let config = Config {
        process: plan.process.clone(),
        stop_signal: kiln_proto::signal::DEFAULT_STOP,
        tty: plan.tty.then_some(WindowSize { rows: 24, cols: 80 }),
        interactive: plan.interactive,
        hostname: plan.hostname.clone(),
        network: plan.network.clone(),
        layers: layers.len() as u32,
        scratch: Scratch {
            size_bytes: plan.scratch_bytes,
        },
        exit_method: kiln_run::exit_method(vmm.as_ref()),
        shutdown_grace_secs: SHUTDOWN_GRACE,
    };
    ctx(config.validate(), "the guest's configuration")?;
    ctx(kiln_run::config::check_size(&config), "the guest's configuration")?;
    Ok(Prepared {
        vmm,
        config,
        init_layer,
        scratch,
        layers,
        run_dir,
    })
}

/// stdin that reads as EOF once the shim relays `CloseIO`.
struct Stdin {
    closed: Arc<AtomicBool>,
}

impl Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let stdin = std::io::stdin();
        let wait = Timespec {
            tv_sec: 0,
            tv_nsec: 200_000_000,
        };
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
            if poll(&mut fds, Some(&wait))? > 0 {
                return stdin.lock().read(buf);
            }
        }
    }
}

/// The VM, the session, and the end: the guest's exit code.
fn boot(plan: &WorkerPlan, p: Prepared, mut lines: Lines<UnixStream>, tx: &mut UnixStream) -> Result<i32, Fail> {
    let console = ctx(ConsoleRelay::start(&p.run_dir), "the console relay")?;
    let mut spec = kiln_run::vm_spec(
        &plan.kernel,
        &p.init_layer,
        &p.scratch,
        &p.layers,
        plan.cpus,
        plan.memory_mib,
        None,
        &console.fifo,
        &p.run_dir,
    );
    spec.tap = plan.pod.as_ref().map(|pod| TapAttach {
        tap: TAP.into(),
        mac: pod.mac.clone(),
    });
    let vm = ctx(p.vmm.create(&spec), "creating the VM")?;
    let vmm_pid = fs::read_to_string(vmkit::sandbox::pid_file(&p.run_dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    // Diagnostics go where the container's log reads them: stderr, or the terminal.
    let mut diag: Box<dyn Write> = if plan.tty {
        Box::new(std::io::stdout())
    } else {
        Box::new(std::io::stderr())
    };
    for w in &plan.warnings {
        let _ = writeln!(diag, "{}", clean_line(w));
    }
    let closed = Arc::new(AtomicBool::new(false));
    let streams = Streams {
        stdin: plan
            .interactive
            .then(|| Box::new(Stdin { closed: closed.clone() }) as Box<dyn Read + Send>),
        stdout: Box::new(std::io::stdout()),
        stderr: Box::new(std::io::stderr()),
    };
    let session = ctx(
        Session::start(
            vm,
            p.config,
            streams,
            SessionOptions {
                boot_timeout: Duration::from_secs(plan.boot_timeout_secs),
                stop_timeout: SHUTDOWN_GRACE,
                escape_keys: false,
                ..SessionOptions::default()
            },
        ),
        "starting the VM",
    )?;
    let _ = send(tx, &FromWorker::Started { vmm_pid });
    let handle = session.handle();
    std::thread::spawn(move || {
        loop {
            match lines.read::<ToWorker>() {
                Ok(Some(ToWorker::Signal(sig))) => handle.signal(sig),
                Ok(Some(ToWorker::Kill {})) => handle.kill(),
                Ok(Some(ToWorker::Resize { rows, cols })) => handle.window_size(WindowSize { rows, cols }),
                Ok(Some(ToWorker::CloseStdin {})) => closed.store(true, Ordering::SeqCst),
                Ok(Some(ToWorker::Start {})) => {}
                // The shim is gone: so is the container.
                Ok(None) | Err(_) => {
                    handle.kill();
                    return;
                }
            }
        }
    });
    let outcome = session.finish();
    let log = console.log.clone();
    console.finish(Duration::from_secs(2));
    if let Some(v) = &outcome.violation {
        let _ = writeln!(diag, "kiln: the VM was killed: {}", clean_line(v));
    } else if let Some(f) = &outcome.init_failed {
        let _ = writeln!(diag, "kiln: {}", clean_line(&f.describe()));
    } else if outcome.exited.is_none() && !outcome.killed {
        let _ = writeln!(diag, "kiln: the VM ended without reporting the command's exit");
    }
    let failed =
        outcome.violation.is_some() || outcome.init_failed.is_some() || (outcome.exited.is_none() && !outcome.killed);
    if failed {
        let tail = kiln_run::console::tail(&log, kiln_run::console::TAIL_LINES);
        if !tail.is_empty() {
            let _ = writeln!(diag, "kiln: the end of the guest's console:");
            for l in tail {
                let _ = writeln!(diag, "  | {}", clean_line(&l));
            }
        }
    }
    for w in &outcome.warnings {
        let _ = writeln!(diag, "kiln: warning: {}", clean_line(w));
    }
    let _ = diag.flush();
    let _ = std::io::stdout().flush();
    let _ = send(
        tx,
        &FromWorker::Exited {
            code: outcome.exit_code,
        },
    );
    Ok(outcome.exit_code)
}
