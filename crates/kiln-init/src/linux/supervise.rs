//! Stage 7: supervise (spec §9.6, §9.8). Reap every child; forward `Signal` to the
//! main process; resize the pty on `WindowSize`.
//!
//! The run ends with the first of its supervised processes (services and the main
//! process) to exit, or with `Shutdown` (SIGINT or SIGTERM to init alike). Then the
//! others are stopped one at a time in reverse start order (so the main process
//! first), each with its own stop signal. The grace period is split evenly between
//! the processes still to stop: one that has not exited when its share ends gets
//! SIGKILL, and the next one gets its stop signal, so a stuck process cannot cost
//! the ones after it their stop signals. When the grace period ends every process
//! gets SIGKILL. Without services this is version 1's behaviour: on `Shutdown` the
//! main process gets its stop signal (its share is the whole grace period), and
//! when it exits everything else is killed at once.
//!
//! The status reported is the first exit's (naming the service when it was one),
//! or after `Shutdown` the main process's. Once every supervised process has
//! exited, every other process is killed (as when a container's PID 1 exits), the
//! output is drained to EOF, and `Exited` is returned (the caller then syncs the
//! scratch disk and sends it).
//!
//! Execs and volume updates too (kiln-shim exec and volumes design §3), in every
//! phase, the stop sequence included: `ExecStart` starts a process with its stdio
//! on port 1029 and answers `ExecStarted` or `ExecFailed`; when it ends, a thread
//! waits for its output to reach EOF and sends `ExecExited`. An exec's exit never
//! ends the run, and execs are not stopped in turn: they are killed with everything
//! else once the supervised processes have exited. `FileUpdate`s are written into
//! their volume.

use std::collections::{HashMap, VecDeque};
use std::os::fd::OwnedFd;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use kiln_proto::{
    Config, ExecExited, ExecFailed, ExecStarted, Exited, GuestMessage, HostMessage, MAX_EXECS, WindowSize,
};
use rustix::process::{Pid, WaitStatus};
use signal_hook::consts::{SIGCHLD, SIGKILL};

use super::process::{self, Proc, Started, set_window_size};
use super::volumes::Volumes;
use super::{Event, send_exec, sys};
use crate::error::{Failure, Result};

/// A running exec.
struct Exec {
    id: u32,
    outputs: Vec<std::thread::JoinHandle<()>>,
    tty: Option<OwnedFd>,
}

/// Why the supervised processes are being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The process at this index exited first: its status is the run's.
    Exit(usize),
    /// The host (or init's SIGINT, SIGTERM) asked: the main process's status is the run's.
    Shutdown,
}

/// The services and the main process, and how far stopping them has got.
struct Supervisor {
    procs: Vec<Proc>,
    status: Vec<Option<WaitStatus>>,
    ending: Option<Ending>,
    /// Who is stopped next, in reverse start order.
    queue: VecDeque<usize>,
    /// The process last sent its stop signal, until it exits, and when its
    /// share of the grace period ends.
    stopping: Option<(usize, Instant)>,
    /// When every process gets SIGKILL.
    deadline: Option<Instant>,
}

pub fn run(config: &Config, started: Started, events: &Receiver<Event>, volumes: &mut Volumes) -> Result<Exited> {
    let Started { procs, outputs, tty } = started;
    let main = procs.len() - 1;
    let mut s = Supervisor {
        status: vec![None; procs.len()],
        procs,
        ending: None,
        queue: VecDeque::new(),
        stopping: None,
        deadline: None,
    };
    let mut execs: HashMap<Pid, Exec> = HashMap::new();
    while !s.done() {
        // Checked first, so a flood of events cannot postpone the kill.
        let event = match s.timer() {
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
        match event {
            None => s.expire(),
            Some(Event::Signal(SIGCHLD)) => {
                // Each child is a supervised process, an exec, or an orphan init
                // inherited (reaped, nothing more).
                for (child, status) in sys::reap() {
                    if let Some(i) = s.running(child) {
                        s.exited(i, status, config.shutdown_grace_secs);
                    } else if let Some(e) = execs.remove(&child) {
                        report(e, status);
                    }
                }
                s.advance();
            }
            Some(Event::Signal(_)) => s.shutdown(config.shutdown_grace_secs),
            Some(Event::Host(HostMessage::Shutdown(m))) => s.shutdown(m.grace_secs),
            Some(Event::Host(HostMessage::Signal(m))) => {
                // Once the main process has exited its pid may be reused: dropped.
                if s.status[main].is_none() {
                    sys::signal(s.procs[main].pid, m.sig);
                }
            }
            Some(Event::Host(HostMessage::WindowSize(size))) => {
                if let Some(master) = &tty {
                    set_window_size(master, size)?;
                }
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
                    Ok(x) => {
                        execs.insert(
                            x.pid,
                            Exec {
                                id: e.id,
                                outputs: x.outputs,
                                tty: x.tty,
                            },
                        );
                        send_exec(&GuestMessage::ExecStarted(ExecStarted { id: e.id }));
                    }
                    Err(f) => {
                        let errno = if f.exec { f.errno } else { None };
                        send_exec(&GuestMessage::ExecFailed(ExecFailed::new(e.id, errno, &f.message)));
                    }
                }
            }
            Some(Event::Host(HostMessage::ExecSignal(m))) => {
                // An exec that has just ended is no longer here; nothing to signal.
                if let Some((&child, _)) = execs.iter().find(|(_, e)| e.id == m.id) {
                    sys::signal(child, m.sig);
                }
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
            }
            Some(Event::Host(HostMessage::FileUpdate(u))) => {
                // The app keeps the files it has, as when kubelet cannot write an update.
                if let Err(f) = volumes.update(&u) {
                    eprintln!("kiln-init: a volume update failed: {f}");
                }
            }
            Some(Event::Host(HostMessage::Config(_))) => {
                return Err(Failure::msg("protocol violation: a second Config"));
            }
            Some(Event::HostError(e)) => return Err(Failure::msg(format!("protocol violation: {e}"))),
            Some(Event::HostClosed) => return Err(Failure::msg("the host closed the control connection")),
        }
    }
    sys::kill_all();
    sys::reap_all();
    for relay in outputs {
        let _ = relay.join();
    }
    drop(tty);
    let first = match s.ending {
        Some(Ending::Exit(i)) => i,
        Some(Ending::Shutdown) | None => main,
    };
    let status = s.status[first].expect("every supervised process has exited");
    let mut e = exited(status);
    e.service = s.procs[first].name.clone();
    Ok(e)
}

impl Supervisor {
    fn done(&self) -> bool {
        self.status.iter().all(Option::is_some)
    }

    /// The supervised process `pid` is, if it has not been reaped yet: once reaped,
    /// its pid may be reused by another child (an exec).
    fn running(&self, pid: Pid) -> Option<usize> {
        (0..self.procs.len()).find(|&i| self.procs[i].pid == pid && self.status[i].is_none())
    }

    /// Records the exit of process `i`; the first one starts the end, with the
    /// grace period `grace` (`Config.shutdownGraceSecs`).
    fn exited(&mut self, i: usize, status: WaitStatus, grace: u32) {
        self.status[i] = Some(status);
        if self.ending.is_none() {
            self.ending = Some(Ending::Exit(i));
            self.begin(grace);
        }
    }

    /// `Shutdown`: stop everything (main first); a later request can shorten
    /// the deadline, never extend it.
    fn shutdown(&mut self, grace: u32) {
        if self.ending.is_none() {
            self.ending = Some(Ending::Shutdown);
            self.begin(grace);
            self.advance();
        } else {
            self.shorten(grace);
        }
    }

    /// Queues every process still running, in reverse start order, and sets
    /// the deadline.
    fn begin(&mut self, grace: u32) {
        self.queue = (0..self.procs.len())
            .rev()
            .filter(|&i| self.status[i].is_none())
            .collect();
        self.shorten(grace);
    }

    fn shorten(&mut self, grace: u32) {
        let now = Instant::now();
        let at = now + Duration::from_secs(u64::from(grace));
        let deadline = self.deadline.map_or(at, |d| d.min(at));
        self.deadline = Some(deadline);
        // The process being stopped gets its share of what is left, if that is sooner.
        if let Some((i, end)) = self.stopping {
            self.stopping = Some((i, end.min(share(now, deadline, self.left() + 1))));
        }
    }

    /// When the loop must act without an event: the end of the current share
    /// or of the grace period, whichever is first.
    fn timer(&self) -> Option<Instant> {
        match (self.stopping, self.deadline) {
            (Some((_, end)), Some(d)) => Some(end.min(d)),
            (_, d) => d,
        }
    }

    /// The timer fired: at the end of the grace period every process gets
    /// SIGKILL; at the end of a share only the process being stopped does, and
    /// the next one gets its stop signal.
    fn expire(&mut self) {
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            sys::kill_all();
            self.deadline = None;
            self.stopping = None;
            self.queue.clear();
            return;
        }
        if let Some((i, _)) = self.stopping.take()
            && self.status[i].is_none()
        {
            sys::signal(self.procs[i].pid, SIGKILL);
        }
        self.advance();
    }

    /// The processes still queued to stop that are running.
    fn left(&self) -> usize {
        self.queue.iter().filter(|&&i| self.status[i].is_none()).count()
    }

    /// Sends the next stop signal once the process last stopped has exited
    /// (or was killed when its share ended).
    fn advance(&mut self) {
        if self.ending.is_none() {
            return;
        }
        if self.stopping.is_some_and(|(i, _)| self.status[i].is_none()) {
            return;
        }
        self.stopping = None;
        let Some(deadline) = self.deadline else {
            return;
        };
        while let Some(i) = self.queue.pop_front() {
            if self.status[i].is_none() {
                sys::signal(self.procs[i].pid, self.procs[i].stop_signal);
                let now = Instant::now();
                self.stopping = Some((i, share(now, deadline, self.left() + 1)));
                return;
            }
        }
        // Nothing left to stop gracefully: what remains (nothing supervised,
        // only processes they left behind, and execs) is killed by `run`.
    }
}

/// The end of one process's share when `left` processes (it included) split
/// what remains of the grace period until `deadline` evenly.
fn share(now: Instant, deadline: Instant, left: usize) -> Instant {
    let left = u32::try_from(left.max(1)).unwrap_or(u32::MAX);
    now + deadline.saturating_duration_since(now) / left
}

/// An exec ended: once its output has reached EOF, `ExecExited` (from a thread, so a
/// background process holding its output does not hold the supervisor).
fn report(e: Exec, status: WaitStatus) {
    let Exited { signaled, code, .. } = exited(status);
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
        (Some(code), _) => Exited::main(false, code),
        (None, Some(sig)) => Exited::main(true, sig),
        (None, None) => unreachable!("waitpid without WUNTRACED reports exits and kills only"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grace_period_is_split_evenly_between_the_processes_left() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(9);
        assert_eq!(share(now, deadline, 1), deadline);
        assert_eq!(share(now, deadline, 3), now + Duration::from_secs(3));
        assert_eq!(share(now, deadline, 0), deadline);
        // A deadline already passed leaves no share.
        assert_eq!(share(deadline, now, 2), deadline);
    }

    /// A supervised process already reaped is never matched again: its pid may have
    /// been reused (by an exec started while stopping) and must not overwrite its status.
    #[test]
    fn a_reaped_process_is_not_matched_by_its_pid_again() {
        // Reaped by `waitpid` below, for a real `WaitStatus`.
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new("/bin/true").spawn().unwrap();
        let pid = Pid::from_raw(child.id() as i32).unwrap();
        let (_, status) = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty())
            .unwrap()
            .unwrap();
        let other = Pid::from_raw(i32::MAX).unwrap();
        let proc = |pid| Proc {
            name: None,
            pid,
            stop_signal: 15,
        };
        let s = Supervisor {
            procs: vec![proc(pid), proc(other)],
            status: vec![Some(status), None],
            ending: Some(Ending::Exit(0)),
            queue: VecDeque::new(),
            stopping: None,
            deadline: None,
        };
        assert_eq!(s.running(pid), None);
        assert_eq!(s.running(other), Some(1));
    }
}
