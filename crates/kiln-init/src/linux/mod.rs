//! The boot sequence (spec §9.6). Each stage reports `Stage { n }` once the
//! control channel is up. A failure (or a panic, in any thread) prints
//! `kiln-init: <stage>: <error>` on the console, sends `InitFailed` when the
//! channel is up, and ends the VM with `exitMethod`, or `reboot()` before `Config`.
//! After the final message (`Exited` or `InitFailed`) init waits, bounded, for the
//! host to close its side of the control connection, so the message is not lost
//! with the VM.

mod build;
mod control;
mod identity;
mod process;
mod relay;
mod root;
mod seal;
mod storage;
mod supervise;
pub mod sys;
mod volumes;

use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use kiln_proto::{ExitMethod, GuestMessage, HostMessage, InitFailed, ProtoError, Stage, stage_name, write_message};
use rustix::system::RebootCommand;

use crate::error::{Context, Failure, Result};
use sys::Vsock;

/// The stage being run, for failure reports.
static STAGE: AtomicU8 = AtomicU8::new(1);
/// How to end the VM, once `Config` has arrived.
static EXIT: OnceLock<ExitMethod> = OnceLock::new();
/// The control connection's sending side. Only the main thread and the
/// failure path write; the latter only tries the lock, so a panic while
/// sending cannot deadlock.
static CONTROL: OnceLock<Mutex<Vsock>> = OnceLock::new();
/// Why the host's side of the control channel failed (a protocol error, or the
/// host closed it, or sent a second `Config`), set by the control reader. Every
/// later stage, and stage 6 before `Running`, checks it, so a broken host stops the
/// boot at the next stage rather than at stage 7.
static HOST_FAILED: OnceLock<String> = OnceLock::new();
/// The final message (`Exited` or `InitFailed`) has gone out: no exec message may follow
/// it. Set and checked under the `CONTROL` lock.
static FINISHED: AtomicBool = AtomicBool::new(false);
/// After the final message: how long init waits for the host to close its side of the
/// control connection before it ends the VM anyway (an older host never closes it).
/// Well under the host's 10 s end bound, and long enough for a loaded host to close inside it.
const HOST_CLOSE_WAIT: Duration = Duration::from_secs(2);

/// What the supervisor waits for (spec §9.6 stage 7).
#[derive(Debug)]
enum Event {
    Signal(i32),
    Host(HostMessage),
    /// The host closed the control connection.
    HostClosed,
    /// The host broke the protocol.
    HostError(ProtoError),
}

/// Runs PID 1. Never returns: the VM ends.
pub fn run() -> ! {
    std::panic::set_hook(Box::new(|info| fail(Failure::msg(format!("panic: {info}")))));
    if let Err(f) = boot() {
        fail(f);
    }
    end_vm()
}

fn boot() -> Result<()> {
    early()?;
    let (events, rx) = channel();
    enter(2)?;
    let (config, config_at, files) = control::connect(events.clone())?;
    if config.mode == kiln_proto::Mode::Build {
        // `Config::validate` refuses volumes in build mode, so `files` is empty.
        drop(files);
        return build::run(&config, events, &rx);
    }
    enter(3)?;
    let scratch = storage::mount_all(&config)?;
    let mut volumes = volumes::Volumes::prepare(&config, &files)?;
    drop(files);
    enter(4)?;
    root::pivot()?;
    volumes.attach(&config)?;
    enter(5)?;
    identity::configure(&config)?;
    enter(6)?;
    let started = process::start(&config, config_at, events)?;
    // A host that failed during this stage: its recorded reason, not a failed send.
    host_ok()?;
    send(&GuestMessage::Running)?;
    enter(7)?;
    let exited = supervise::run(&config, started, &rx, &mut volumes)?;
    // The scratch disk is synced and read-only before `Exited`: the host may end
    // the VM as soon as it has the message, and `Exited` means the disk is safe.
    storage::finish(&scratch);
    volumes.finish();
    send_final(&GuestMessage::Exited(exited))?;
    Ok(())
}

/// Stage 1: `/proc`, `/sys` and `/dev` in the init root, and Ctrl-Alt-Del as SIGINT.
fn early() -> Result<()> {
    use rustix::mount::{MountFlags, mount};
    let flags = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
    mount("proc", "/proc", "proc", flags, None).context("mount /proc")?;
    mount("sysfs", "/sys", "sysfs", flags, None).context("mount /sys")?;
    // CONFIG_DEVTMPFS_MOUNT kernels have mounted it already.
    if !std::path::Path::new("/dev/null").exists() {
        mount("devtmpfs", "/dev", "devtmpfs", MountFlags::NOSUID, None).context("mount /dev")?;
    }
    rustix::system::reboot(RebootCommand::CadOff).context("reboot(CAD_OFF)")
}

/// Records the stage and reports it once the channel is up; fails instead if
/// the host has broken the protocol or closed the channel.
fn enter(n: u8) -> Result<()> {
    STAGE.store(n, Ordering::SeqCst);
    host_ok()?;
    if n >= 3 {
        send(&GuestMessage::Stage(Stage { n }))?;
    }
    Ok(())
}

/// Fails with the reason the control reader recorded, if the host has failed.
fn host_ok() -> Result<()> {
    match HOST_FAILED.get() {
        Some(why) => Err(Failure::msg(why.clone())),
        None => Ok(()),
    }
}

/// Sends an exec message, unless the final message has gone out (the host reports the
/// execs it has not heard of as killed). A failed send is the host's end: ignored here,
/// the supervisor learns of it from the control reader.
fn send_exec(msg: &GuestMessage) {
    let control = CONTROL.get().expect("the control channel is up");
    let mut stream = control.lock().unwrap_or_else(|e| e.into_inner());
    if !FINISHED.load(Ordering::SeqCst) {
        let _ = write_message(&mut *stream, msg);
    }
}

/// Sends the final message: nothing from an exec follows it. Then waits, bounded, for
/// the host to have read it ([`await_host_close`]).
fn send_final(msg: &GuestMessage) -> Result<()> {
    let control = CONTROL.get().expect("the control channel is up");
    let mut stream = control.lock().unwrap_or_else(|e| e.into_inner());
    FINISHED.store(true, Ordering::SeqCst);
    write_message(&mut *stream, msg).map_err(|e| match HOST_FAILED.get() {
        Some(why) => Failure::msg(why.clone()),
        None => Failure::msg(format!("send to the host: {e}")),
    })?;
    // `CONTROL` stays locked across the wait: an exec sender blocks for up to the bound, which
    // is harmless, since `FINISHED` is set and it would send nothing anyway.
    await_host_close(&stream);
    Ok(())
}

/// Waits until the host has closed its side of the control connection, for at most
/// [`HOST_CLOSE_WAIT`]. A write to the vsock only queues the bytes in the guest's kernel:
/// the VMM moves them to the host's socket later, and a VM that ends first can take the
/// final message with it (the host then sees the connection close without it). A host
/// closes its side once it has read the final message. That guarantees delivery of the
/// final message on port 1024 only: output-stream bytes sent before it may still sit in the
/// VMM's per-connection buffers when the VM ends, as they could before this wait.
/// A host that never closes (an older one) costs the bound; a failed `poll` ends the wait.
fn await_host_close(stream: &Vsock) {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let deadline = Instant::now() + HOST_CLOSE_WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        let Ok(timeout) = Timespec::try_from(left) else { return };
        // RDHUP, or HUP and ERR, which poll always reports; data from the host is not awaited.
        let mut fds = [PollFd::new(stream, PollFlags::RDHUP)];
        match poll(&mut fds, Some(&timeout)) {
            Err(rustix::io::Errno::INTR) => {}
            _ => return,
        }
    }
}

fn send(msg: &GuestMessage) -> Result<()> {
    let control = CONTROL.get().expect("the control channel is up");
    let mut stream = control.lock().unwrap_or_else(|e| e.into_inner());
    write_message(&mut *stream, msg).map_err(|e| match HOST_FAILED.get() {
        // The host closed or broke the channel: that is why the send failed.
        Some(why) => Failure::msg(why.clone()),
        None => Failure::msg(format!("send to the host: {e}")),
    })
}

/// Reports `f` and ends the VM.
fn fail(f: Failure) -> ! {
    let stage = STAGE.load(Ordering::SeqCst);
    let _ = writeln!(std::io::stderr(), "kiln-init: {}: {f}", stage_name(stage));
    let sent = CONTROL
        .get()
        .and_then(|control| control.try_lock().ok())
        .map(|mut stream| {
            FINISHED.store(true, Ordering::SeqCst);
            let msg = InitFailed::new(stage, f.reported_errno(stage), &f.message);
            let ok = write_message(&mut *stream, &GuestMessage::InitFailed(msg)).is_ok();
            (stream, ok)
        });
    sys::kill_all();
    if let Some((stream, true)) = sent {
        await_host_close(&stream);
    }
    end_vm()
}

/// Syncs and ends the VM with `exitMethod`, or reboots before `Config`.
fn end_vm() -> ! {
    rustix::fs::sync();
    let cmd = match EXIT.get() {
        Some(ExitMethod::Poweroff) => RebootCommand::PowerOff,
        Some(ExitMethod::Reboot) | None => RebootCommand::Restart,
    };
    if let Err(e) = rustix::system::reboot(cmd) {
        let _ = writeln!(std::io::stderr(), "kiln-init: reboot: {e}");
    }
    // PID 1 exiting panics the kernel, and `panic=-1` reboots.
    std::process::exit(1)
}

/// An `OwnedFd` for a path, for helpers that take descriptors.
fn open_dir(path: &str) -> Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags};
    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .context(format!("open {path}"))
}
