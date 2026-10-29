//! Stage 7: supervise (spec §9.6). Reap every child; forward `Signal`; resize the
//! pty on `WindowSize`; on `Shutdown`, SIGINT or SIGTERM send `stopSignal` to the
//! main process and SIGKILL everything when the grace period ends. When the main
//! process exits, every other process is killed (as when a container's PID 1
//! exits), its output is drained to EOF, and `Exited` is returned (the caller then
//! syncs the scratch disk and sends it).
//!
//! Execs and volume updates too (kiln-shim exec and volumes design §3): `ExecStart`
//! starts a process with its stdio on port 1029 and answers `ExecStarted` or
//! `ExecFailed`; when it ends, a thread waits for its output to reach EOF and sends
//! `ExecExited`. `FileUpdate`s are written into their volume.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use kiln_proto::{
    Config, ExecExited, ExecFailed, ExecStarted, Exited, GuestMessage, HostMessage, MAX_EXECS, WindowSize,
};
use rustix::process::{Pid, WaitStatus};
use signal_hook::consts::SIGCHLD;

use super::process::{self, Started, set_window_size};
use super::volumes::Volumes;
use super::{Event, send_exec, sys};
use crate::error::{Failure, Result};

/// A running exec.
struct Exec {
    id: u32,
    outputs: Vec<std::thread::JoinHandle<()>>,
    tty: Option<OwnedFd>,
}

pub fn run(config: &Config, started: Started, events: &Receiver<Event>, volumes: &mut Volumes) -> Result<Exited> {
    let Started { pid, outputs, tty } = started;
    let mut deadline: Option<Instant> = None;
    let mut stopping = false;
    let mut execs: HashMap<Pid, Exec> = HashMap::new();
    let status = loop {
        // Checked first, so a flood of events cannot postpone the kill.
        let event = match deadline {
            Some(at) if Instant::now() >= at => None,
            Some(at) => match events.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    unreachable!("the signal thread holds a sender for the life of the process")
                }
            },
            None => Some(
                events
                    .recv()
                    .expect("the signal thread holds a sender for the life of the process"),
            ),
        };
        let grace = match event {
            None => {
                sys::kill_all();
                deadline = None;
                continue;
            }
            Some(Event::Signal(SIGCHLD)) => {
                let mut main = None;
                for (child, status) in sys::reap() {
                    if child == pid {
                        main = Some(status);
                    } else if let Some(e) = execs.remove(&child) {
                        report(e, status);
                    }
                }
                match main {
                    Some(status) => break status,
                    None => continue,
                }
            }
            Some(Event::Signal(_)) => config.shutdown_grace_secs,
            Some(Event::Host(HostMessage::Shutdown(s))) => s.grace_secs,
            Some(Event::Host(HostMessage::Signal(s))) => {
                sys::signal(pid, s.sig);
                continue;
            }
            Some(Event::Host(HostMessage::WindowSize(size))) => {
                if let Some(master) = &tty {
                    set_window_size(master, size)?;
                }
                continue;
            }
            Some(Event::Host(HostMessage::ExecStart(e))) => {
                if execs.len() >= MAX_EXECS {
                    send_exec(&GuestMessage::ExecFailed(ExecFailed::new(
                        e.id,
                        None,
                        &format!("at most {MAX_EXECS} execs run at once"),
                    )));
                    continue;
                }
                match process::exec(config, &e) {
                    Ok(s) => {
                        execs.insert(
                            s.pid,
                            Exec {
                                id: e.id,
                                outputs: s.outputs,
                                tty: s.tty,
                            },
                        );
                        send_exec(&GuestMessage::ExecStarted(ExecStarted { id: e.id }));
                    }
                    Err(f) => {
                        let errno = if f.exec { f.errno } else { None };
                        send_exec(&GuestMessage::ExecFailed(ExecFailed::new(e.id, errno, &f.message)));
                    }
                }
                continue;
            }
            Some(Event::Host(HostMessage::ExecSignal(s))) => {
                // An exec that has just ended is no longer here; nothing to signal.
                if let Some((&child, _)) = execs.iter().find(|(_, e)| e.id == s.id) {
                    sys::signal(child, s.sig);
                }
                continue;
            }
            Some(Event::Host(HostMessage::ExecWindowSize(w))) => {
                if let Some(master) = execs.values().find(|e| e.id == w.id).and_then(|e| e.tty.as_ref()) {
                    set_window_size(
                        master,
                        WindowSize {
                            rows: w.rows,
                            cols: w.cols,
                        },
                    )?;
                }
                continue;
            }
            Some(Event::Host(HostMessage::FileUpdate(u))) => {
                volumes.update(&u)?;
                continue;
            }
            Some(Event::Host(HostMessage::Config(_))) => {
                return Err(Failure::msg("protocol violation: a second Config"));
            }
            Some(Event::HostError(e)) => return Err(Failure::msg(format!("protocol violation: {e}"))),
            Some(Event::HostClosed) => return Err(Failure::msg("the host closed the control connection")),
        };
        if !stopping {
            stopping = true;
            sys::signal(pid, config.stop_signal);
        }
        let at = Instant::now() + Duration::from_secs(u64::from(grace));
        deadline = Some(deadline.map_or(at, |d| d.min(at)));
    };
    sys::kill_all();
    sys::reap_all();
    for relay in outputs {
        let _ = relay.join();
    }
    drop(tty);
    Ok(exited(status))
}

/// An exec ended: once its output has reached EOF, `ExecExited` (from a thread, so a
/// background process holding its output does not hold the supervisor).
fn report(e: Exec, status: WaitStatus) {
    let Exited { signaled, code } = exited(status);
    let id = e.id;
    let spawned = std::thread::Builder::new().name("exec-exit".into()).spawn(move || {
        for relay in e.outputs {
            let _ = relay.join();
        }
        drop(e.tty);
        send_exec(&GuestMessage::ExecExited(ExecExited { id, signaled, code }));
    });
    if spawned.is_err() {
        send_exec(&GuestMessage::ExecExited(ExecExited { id, signaled, code }));
    }
}

fn exited(status: WaitStatus) -> Exited {
    match (status.exit_status(), status.terminating_signal()) {
        (Some(code), _) => Exited { signaled: false, code },
        (None, Some(sig)) => Exited {
            signaled: true,
            code: sig,
        },
        (None, None) => unreachable!("waitpid without WUNTRACED reports exits and kills only"),
    }
}
