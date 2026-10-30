//! A container's VM worker (design §7.4, §8): the shim binary run as `vm`, as root,
//! with the container's stdio FIFOs as its fds 0-2 and its channel to the shim on the
//! fd [`FD_ENV`] names. As root it joins the container's cgroup and the pod netns and
//! bind-mounts the layers into a private mount namespace; then it drops to the kiln
//! user for good, and only then touches kiln-run and vmkit (kiln T6).
//!
//! Once the VM runs it also starts execs (their FIFOs arrive on the fd channel named
//! by [`FDS_ENV`]) and hands kubelet's volume updates to the guest (kiln-shim exec and
//! volumes design §5.3, §5.5).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kiln_proto::sanitize::clean_line;
use kiln_proto::{Config, ExecStart, HostMessage, Scratch, WindowSize, file_updates};
use kiln_run::exec::{ExecEvent, ExecStreams};
use kiln_run::{ConsoleRelay, Session, SessionOptions, Streams, VmmKind};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::mount::{MountFlags, MountPropagationFlags, mount_bind, mount_change, mount_remount};
use rustix::process::{Gid, Signal, Uid, set_parent_process_death_signal};
use rustix::thread::{LinkNameSpaceType, UnshareFlags, move_into_link_name_space};
use vmkit::{TapAttach, Vmm};

use super::plan::WorkerPlan;
use super::{FD_ENV, FDS_ENV, fdpass};
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
    // SAFETY: as for the channel: the shim's socketpair end, owned by nothing else here.
    let fds = std::env::var(FDS_ENV)
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
    if let Some(f) = &fds {
        let _ = rustix::io::fcntl_setfd(f, rustix::io::FdFlags::CLOEXEC);
    }
    let mut lines = Lines::new(chan);
    let Ok(Some(plan)) = lines.read_within::<WorkerPlan>(MAX_PLAN) else {
        std::process::exit(EXIT_INFRA);
    };
    let prepared = match prepare(&plan) {
        Ok(p) => p,
        Err(message) => {
            let _ = send(&mut tx, &FromWorker::Failed { message });
            end(&plan, EXIT_INFRA);
        }
    };
    if send(&mut tx, &FromWorker::Ready {}).is_err() {
        end(&plan, EXIT_INFRA);
    }
    // Until Start, a signal ends the container as it would a runc container's init.
    loop {
        match lines.read_within::<ToWorker>(MAX_PLAN) {
            Ok(Some(ToWorker::Start {})) => break,
            Ok(Some(ToWorker::Kill {})) | Ok(None) | Err(_) => exited(&plan, &mut tx, 128 + 9),
            Ok(Some(ToWorker::Signal(sig))) => exited(&plan, &mut tx, 128 + sig),
            Ok(Some(_)) => {}
        }
    }
    let code = match boot(&plan, prepared, lines, &mut tx, fds) {
        Ok(code) => code,
        Err(message) => {
            let _ = send(&mut tx, &FromWorker::Failed { message });
            exited(&plan, &mut tx, EXIT_INFRA)
        }
    };
    end(&plan, code)
}

/// Ends a container that did not run to the guest's own exit, telling the shim the code
/// (it reports only what the worker says: a worker that just vanished is 255).
fn exited(plan: &WorkerPlan, tx: &mut UnixStream, code: i32) -> ! {
    let _ = send(tx, &FromWorker::Exited { code });
    end(plan, code)
}

/// Removes the scratch disk and exits. Only as the kiln user, whose store it is in: root
/// never deletes inside a directory another user controls (and before the drop to that
/// user, the disk does not exist yet).
fn end(plan: &WorkerPlan, code: i32) -> ! {
    if !rustix::process::geteuid().is_root() {
        let _ = fs::remove_dir_all(&plan.scratch_dir);
    }
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
    // A leftover (a worker killed with its shim) would make the disk's exclusive create fail.
    let _ = fs::remove_dir_all(&plan.scratch_dir);
    ctx(fs::create_dir_all(&plan.scratch_dir), "creating the scratch directory")?;
    let scratch = plan.scratch_dir.join("scratch.img");
    ctx(
        kiln_image::scratch::create_scratch(&scratch, plan.scratch_bytes),
        "creating the scratch disk",
    )?;
    if let Some(disk) = &plan.pod_disk {
        // The pod's earlier containers left it here; the first one creates it.
        if let Some(dir) = disk.parent() {
            ctx(fs::create_dir_all(dir), "creating the pod disk's directory")?;
        }
        if !disk.exists() {
            // Made beside it and renamed: a worker killed half-way leaves no partial disk
            // for the next container to attach.
            let partial = disk.with_extension("partial");
            let _ = fs::remove_file(&partial);
            ctx(
                kiln_image::scratch::create_scratch(&partial, plan.pod_disk_bytes),
                "creating the pod disk",
            )?;
            ctx(fs::rename(&partial, disk), "creating the pod disk")?;
        }
    }
    let run_dir = plan.vm_dir.join("run");
    ctx(fs::create_dir_all(&run_dir), "creating the run directory")?;
    let kind: VmmKind = plan.vmm.parse()?;
    let vmm = ctx(kiln_run::backend(kind).discover(), "finding the VMM")?;
    ctx(
        kiln_run::config::check_devices_with(
            layers.len(),
            plan.pod_disk.is_some(),
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
        volumes: plan.volumes.clone(),
        pod_disk: plan.pod_disk.as_ref().map(|_| Scratch {
            size_bytes: plan.pod_disk_bytes,
        }),
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

/// A stdin FIFO that reads as EOF once the shim relays `CloseIO`.
struct Stdin {
    file: File,
    closed: Arc<AtomicBool>,
}

impl Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let wait = Timespec {
            tv_sec: 0,
            tv_nsec: 200_000_000,
        };
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let mut fds = [PollFd::new(&self.file, PollFlags::IN)];
            if poll(&mut fds, Some(&wait))? > 0 {
                return self.file.read(buf);
            }
        }
    }
}

/// Exec `id` with the FIFOs the shim passed for it.
fn exec_streams(fds: fdpass::ExecFds, tty: bool, closed: &Arc<AtomicBool>) -> ExecStreams {
    let out = |fd: Option<OwnedFd>| -> Option<Box<dyn Write + Send>> {
        fd.map(|f| Box::new(File::from(f)) as Box<dyn Write + Send>)
    };
    ExecStreams {
        stdin: fds.stdin.map(|f| {
            Box::new(Stdin {
                file: File::from(f),
                closed: closed.clone(),
            }) as Box<dyn Read + Send>
        }),
        stdout: out(fds.stdout).unwrap_or_else(|| Box::new(std::io::sink())),
        stderr: if tty { None } else { out(fds.stderr) },
    }
}

/// The VM, the session, and the end: the guest's exit code.
fn boot(
    plan: &WorkerPlan,
    p: Prepared,
    mut lines: Lines<UnixStream>,
    tx: &mut UnixStream,
    fds: Option<OwnedFd>,
) -> Result<i32, Fail> {
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
    // The pod disk is the disk after the layers (kiln-proto `Config.pod_disk`).
    if let Some(disk) = &plan.pod_disk {
        spec.disks.push(vmkit::Disk {
            path: disk.clone(),
            read_only: false,
        });
    }
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
            .then(|| {
                let file = std::io::stdin()
                    .as_fd()
                    .try_clone_to_owned()
                    .map(File::from)
                    .map_err(|e| format!("stdin: {e}"));
                file.map(|file| {
                    Box::new(Stdin {
                        file,
                        closed: closed.clone(),
                    }) as Box<dyn Read + Send>
                })
            })
            .transpose()?,
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
                exec: true,
                after_config: plan
                    .files
                    .iter()
                    .flat_map(|(volume, files)| file_updates(*volume, files))
                    .map(HostMessage::FileUpdate)
                    .collect(),
                ..SessionOptions::default()
            },
        ),
        "starting the VM",
    )?;
    // From here on the exec reports share the channel: one line at a time.
    let shared = Arc::new(Mutex::new(ctx(tx.try_clone(), "the shim's channel")?));
    let say = {
        let shared = shared.clone();
        move |m: &FromWorker| {
            let mut tx = shared.lock().unwrap_or_else(|e| e.into_inner());
            let _ = send(&mut *tx, m);
        }
    };
    say(&FromWorker::Started { vmm_pid });
    let (report, reports) = std::sync::mpsc::channel::<(u32, ExecEvent)>();
    // Id 0 is no exec's: it marks a flush, answered on `flushed`.
    let (flush, flushed) = std::sync::mpsc::channel::<()>();
    std::thread::spawn({
        let say = say.clone();
        move || {
            for (id, e) in reports {
                if id == 0 {
                    let _ = flush.send(());
                    continue;
                }
                say(&match e {
                    ExecEvent::Started => FromWorker::ExecStarted { id },
                    ExecEvent::Failed(m) => FromWorker::ExecFailed {
                        id,
                        message: clean_line(&m),
                    },
                    ExecEvent::Exited(code) => FromWorker::ExecExited { id, code },
                });
            }
        }
    });
    // Volume updates, one generation at a time (they wait for the guest to read them).
    let (updates, pending) = std::sync::mpsc::channel::<(u32, Vec<kiln_proto::VolumeFile>)>();
    std::thread::spawn({
        let handle = session.handle();
        move || {
            while let Ok(first) = pending.recv() {
                // Only the newest generation of each volume matters.
                let mut batch = vec![first];
                batch.extend(pending.try_iter());
                let mut newest: Vec<(u32, Vec<kiln_proto::VolumeFile>)> = Vec::new();
                for (volume, files) in batch {
                    newest.retain(|(v, _)| *v != volume);
                    newest.push((volume, files));
                }
                for (volume, files) in newest {
                    if !handle.update_files(file_updates(volume, &files)) {
                        eprintln!("kiln: warning: a volume update could not reach the guest");
                    }
                }
            }
        }
    });
    let handle = session.handle();
    let flusher = report.clone();
    std::thread::spawn(move || {
        let mut closers: HashMap<u32, Arc<AtomicBool>> = HashMap::new();
        loop {
            match lines.read_within::<ToWorker>(MAX_PLAN) {
                Ok(Some(ToWorker::Signal(sig))) => handle.signal(sig),
                Ok(Some(ToWorker::Kill {})) => handle.kill(),
                Ok(Some(ToWorker::Resize { rows, cols })) => handle.window_size(WindowSize { rows, cols }),
                Ok(Some(ToWorker::CloseStdin {})) => closed.store(true, Ordering::SeqCst),
                Ok(Some(ToWorker::Start {})) => {}
                Ok(Some(ToWorker::Exec { id, process, tty })) => {
                    let received = match &fds {
                        Some(f) => fdpass::recv(f).map_err(|e| e.to_string()),
                        None => Err("no fd channel".into()),
                    };
                    let started = match received {
                        Ok((got, f)) if got == id => {
                            let closer = Arc::new(AtomicBool::new(false));
                            let streams = exec_streams(f, tty, &closer);
                            closers.insert(id, closer);
                            let start = ExecStart {
                                id,
                                process,
                                tty: tty.then_some(WindowSize { rows: 24, cols: 80 }),
                                stdin: false,
                            };
                            handle.exec(start, streams, report.clone())
                        }
                        Ok((got, _)) => Err(format!("the stdio of exec {got} arrived for exec {id}")),
                        Err(e) => Err(format!("receiving the exec's stdio: {e}")),
                    };
                    if let Err(m) = started {
                        let _ = report.send((id, ExecEvent::Failed(m)));
                    }
                }
                Ok(Some(ToWorker::ExecSignal { id, signal })) => handle.exec_signal(id, signal),
                Ok(Some(ToWorker::ExecResize { id, rows, cols })) => {
                    handle.exec_window_size(id, WindowSize { rows, cols })
                }
                Ok(Some(ToWorker::ExecCloseStdin { id })) => {
                    if let Some(c) = closers.get(&id) {
                        c.store(true, Ordering::SeqCst);
                    }
                }
                Ok(Some(ToWorker::Files { volume, files })) => {
                    let _ = updates.send((volume, files));
                }
                // The shim is gone: so is the container.
                Ok(None) | Err(_) => {
                    handle.kill();
                    return;
                }
            }
        }
    });
    let outcome = session.finish();
    // The execs' last reports (the session ended them) reach the shim before `Exited`.
    if flusher.send((0, ExecEvent::Started)).is_ok() {
        let _ = flushed.recv_timeout(Duration::from_secs(2));
    }
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
    say(&FromWorker::Exited {
        code: outcome.exit_code,
    });
    Ok(outcome.exit_code)
}
