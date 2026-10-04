//! Stage 7: supervise (spec §9.6). Reap every child; forward `Signal`; resize the
//! pty on `WindowSize`; on `Shutdown`, SIGINT or SIGTERM send `stopSignal` to the
//! main process and SIGKILL everything when the grace period ends. When the main
//! process exits, every other process is killed (as when a container's PID 1
//! exits), its output is drained to EOF, and `Exited` is returned.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use kiln_proto::{Config, Exited, HostMessage};
use rustix::process::WaitStatus;
use signal_hook::consts::SIGCHLD;

use super::process::{Started, set_window_size};
use super::{Event, sys};
use crate::error::{Failure, Result};

pub fn run(config: &Config, started: Started, events: &Receiver<Event>) -> Result<Exited> {
    let Started { pid, outputs, tty } = started;
    let mut deadline: Option<Instant> = None;
    let mut stopping = false;
    let status = loop {
        let event = match deadline {
            Some(at) => match events.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => unreachable!("init holds a sender"),
            },
            None => Some(events.recv().expect("init holds a sender")),
        };
        let grace = match event {
            None => {
                sys::kill_all();
                deadline = None;
                continue;
            }
            Some(Event::Signal(SIGCHLD)) => match sys::reap(pid) {
                Some(status) => break status,
                None => continue,
            },
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
