//! The boot sequence (spec §9.6). Each stage reports `Stage { n }` once the
//! control channel is up. A failure (or a panic, in any thread) prints
//! `kiln-init: <stage>: <error>` on the console, sends `InitFailed` when the
//! channel is up, and ends the VM with `exitMethod`, or `reboot()` before `Config`.

mod control;
mod identity;
mod process;
mod relay;
mod root;
mod storage;
mod supervise;
pub mod sys;

use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Mutex, OnceLock};

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
    let config = control::connect(events.clone())?;
    enter(3)?;
    let scratch = storage::mount_all(&config)?;
    enter(4)?;
    root::pivot()?;
    enter(5)?;
    identity::configure(&config)?;
    enter(6)?;
    let started = process::start(&config, events)?;
    // A host that failed during this stage: its recorded reason, not a failed send.
    host_ok()?;
    send(&GuestMessage::Running)?;
    enter(7)?;
    let exited = supervise::run(&config, started, &rx)?;
    send(&GuestMessage::Exited(exited))?;
    storage::finish(&scratch);
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
    if let Some(control) = CONTROL.get()
        && let Ok(mut stream) = control.try_lock()
    {
        let msg = InitFailed::new(stage, f.reported_errno(stage), &f.message);
        let _ = write_message(&mut *stream, &GuestMessage::InitFailed(msg));
    }
    sys::kill_all();
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
