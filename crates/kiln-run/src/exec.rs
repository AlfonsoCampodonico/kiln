//! The host's record of a session's execs (kiln-shim exec and volumes design §4,
//! threat S5). Ids are the host's. Every exec message the guest sends and every
//! stream it connects on port 1029 is checked here, against an exec the host started,
//! in its expected state, at most once. Anything else is a violation. An exec's exit is
//! reported once the guest's `ExecExited` has arrived and its output streams have
//! reached EOF, so its output is complete when its exit is known.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard};

use kiln_proto::{ExecStream, GuestMessage, MAX_EXECS};

/// The exit code reported for an exec the VM's end cut short (killed, as runc reports
/// the execs of a container whose init died).
pub const EXEC_KILLED: i32 = 137;

/// Where an exec's events go, with its id.
pub type ExecReport = Sender<(u32, ExecEvent)>;

/// What happened to an exec, in this order: `Started` or `Failed`, then `Exited`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEvent {
    Started,
    /// It never ran; the guest's message (untrusted: sanitise it before printing).
    Failed(String),
    /// Its exit code, or `128 + signal`.
    Exited(i32),
}

/// Where an exec's stdio goes. With a terminal, `stdout` is the terminal's output
/// and `stdin` its input, and `stderr` is unused.
pub struct ExecStreams {
    pub stdin: Option<Box<dyn Read + Send>>,
    pub stdout: Box<dyn Write + Send>,
    pub stderr: Option<Box<dyn Write + Send>>,
}

/// What a connected stream relays.
pub enum Relay {
    /// Host to guest; EOF is a half-close.
    In(Box<dyn Read + Send>),
    /// Guest to host.
    Out(Box<dyn Write + Send>),
    /// The terminal: its output, and its input when the exec has one.
    Tty {
        input: Option<Box<dyn Read + Send>>,
        output: Box<dyn Write + Send>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Sent,
    Started,
    Exited(i32),
}

struct Entry {
    state: State,
    tty: bool,
    stdin: bool,
    streams: Option<ExecStreams>,
    connected: BTreeSet<ExecStream>,
    /// Output streams not yet at EOF.
    open: u8,
    report: ExecReport,
}

impl Entry {
    fn expects(&self, s: ExecStream) -> bool {
        match s {
            ExecStream::Tty => self.tty,
            ExecStream::Stdin => !self.tty && self.stdin,
            ExecStream::Stdout | ExecStream::Stderr => !self.tty,
        }
    }

    fn send(&self, id: u32, e: ExecEvent) {
        let _ = self.report.send((id, e));
    }
}

/// A session's execs.
#[derive(Default)]
pub struct Execs {
    map: Mutex<BTreeMap<u32, Entry>>,
    /// The VM has ended: no exec is added any more.
    ended: Mutex<bool>,
}

impl Execs {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<u32, Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records an exec the host is about to start. Refused for a reused id, beyond
    /// [`MAX_EXECS`] at once, and once the VM has ended.
    pub fn add(&self, id: u32, tty: bool, streams: ExecStreams, report: ExecReport) -> Result<(), String> {
        if *self.ended.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err("the VM has ended".into());
        }
        let mut map = self.lock();
        if id == 0 || map.contains_key(&id) {
            return Err(format!("exec id {id} is in use"));
        }
        if map.len() >= MAX_EXECS {
            return Err(format!("at most {MAX_EXECS} execs run at once"));
        }
        map.insert(
            id,
            Entry {
                state: State::Sent,
                tty,
                stdin: streams.stdin.is_some(),
                streams: Some(streams),
                connected: BTreeSet::new(),
                open: if tty { 1 } else { 2 },
                report,
            },
        );
        Ok(())
    }

    /// Checks a guest exec message and reports it; `Err` is a violation.
    pub fn guest(&self, msg: &GuestMessage) -> Result<(), String> {
        let mut map = self.lock();
        let (id, name) = match msg {
            GuestMessage::ExecStarted(m) => (m.id, "ExecStarted"),
            GuestMessage::ExecExited(m) => (m.id, "ExecExited"),
            GuestMessage::ExecFailed(m) => (m.id, "ExecFailed"),
            _ => return Ok(()),
        };
        let Some(e) = map.get_mut(&id) else {
            return Err(format!("{name} for exec {id}, which the host did not start"));
        };
        match (msg, e.state) {
            (GuestMessage::ExecStarted(_), State::Sent) => {
                e.state = State::Started;
                e.send(id, ExecEvent::Started);
            }
            (GuestMessage::ExecFailed(f), State::Sent) => {
                e.send(id, ExecEvent::Failed(f.message.clone()));
                map.remove(&id);
            }
            (GuestMessage::ExecExited(x), State::Started) => {
                e.state = State::Exited(x.exit_code());
                Self::settle(&mut map, id);
            }
            (_, state) => return Err(format!("{name} for exec {id} in state {state:?}")),
        }
        Ok(())
    }

    /// A connection on port 1029 opened with `(id, stream)`: what it relays. `Err` is a
    /// violation (an exec the host did not start, a stream it does not have, a second
    /// connection for one).
    pub fn connect(&self, id: u32, stream: ExecStream) -> Result<Relay, String> {
        let mut map = self.lock();
        let Some(e) = map.get_mut(&id) else {
            return Err(format!(
                "a {stream:?} stream for exec {id}, which the host did not start"
            ));
        };
        if !e.expects(stream) || !e.connected.insert(stream) {
            return Err(format!("an unexpected {stream:?} stream for exec {id}"));
        }
        let s = e.streams.as_mut().expect("streams are kept until the exec is done");
        let gone = || format!("exec {id}'s {stream:?} stream is gone");
        Ok(match stream {
            ExecStream::Stdin => Relay::In(s.stdin.take().ok_or_else(gone)?),
            ExecStream::Stdout => Relay::Out(std::mem::replace(&mut s.stdout, Box::new(std::io::sink()))),
            // An exec without a stderr sink (containerd gave none): its stderr is discarded.
            ExecStream::Stderr => Relay::Out(s.stderr.take().unwrap_or_else(|| Box::new(std::io::sink()))),
            ExecStream::Tty => Relay::Tty {
                input: s.stdin.take(),
                output: std::mem::replace(&mut s.stdout, Box::new(std::io::sink())),
            },
        })
    }

    /// An output stream of exec `id` reached EOF.
    pub fn output_closed(&self, id: u32) {
        let mut map = self.lock();
        if let Some(e) = map.get_mut(&id) {
            e.open = e.open.saturating_sub(1);
            Self::settle(&mut map, id);
        }
    }

    /// Reports the exit once both the guest's `ExecExited` and EOF on every output
    /// stream have arrived, and forgets the exec.
    fn settle(map: &mut BTreeMap<u32, Entry>, id: u32) {
        if let Some(e) = map.get(&id)
            && let State::Exited(code) = e.state
            && e.open == 0
        {
            e.send(id, ExecEvent::Exited(code));
            map.remove(&id);
        }
    }

    /// The VM has ended: every exec not yet reported ends now (one that never started
    /// fails, the others are killed).
    pub fn end(&self) {
        *self.ended.lock().unwrap_or_else(|e| e.into_inner()) = true;
        let mut map = self.lock();
        for (id, e) in std::mem::take(&mut *map) {
            match e.state {
                State::Sent => e.send(id, ExecEvent::Failed("the VM ended before the exec started".into())),
                State::Started => e.send(id, ExecEvent::Exited(EXEC_KILLED)),
                State::Exited(code) => e.send(id, ExecEvent::Exited(code)),
            }
        }
    }

    /// The streams the host's execs may still connect: what a guest may have open on
    /// port 1029 before it has said which stream each is.
    pub fn unconnected(&self) -> usize {
        self.lock()
            .values()
            .map(|e| {
                [
                    ExecStream::Stdin,
                    ExecStream::Stdout,
                    ExecStream::Stderr,
                    ExecStream::Tty,
                ]
                .into_iter()
                .filter(|s| e.expects(*s) && !e.connected.contains(s))
                .count()
            })
            .sum()
    }

    /// Execs not yet reported.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, channel};

    use kiln_proto::{ExecExited, ExecFailed, ExecStarted};

    use super::*;

    fn streams(stdin: bool) -> ExecStreams {
        ExecStreams {
            stdin: stdin.then(|| Box::new(std::io::empty()) as Box<dyn Read + Send>),
            stdout: Box::new(std::io::sink()),
            stderr: Some(Box::new(std::io::sink())),
        }
    }

    fn execs() -> (Execs, ExecReport, Receiver<(u32, ExecEvent)>) {
        let (tx, rx) = channel();
        (Execs::default(), tx, rx)
    }

    fn started(id: u32) -> GuestMessage {
        GuestMessage::ExecStarted(ExecStarted { id })
    }

    fn exited(id: u32, code: i32) -> GuestMessage {
        GuestMessage::ExecExited(ExecExited {
            id,
            signaled: false,
            code,
        })
    }

    #[test]
    fn exit_waits_for_the_output() {
        let (x, tx, rx) = execs();
        x.add(1, false, streams(true), tx).unwrap();
        assert!(matches!(x.connect(1, ExecStream::Stdout).unwrap(), Relay::Out(_)));
        assert!(matches!(x.connect(1, ExecStream::Stderr).unwrap(), Relay::Out(_)));
        assert!(matches!(x.connect(1, ExecStream::Stdin).unwrap(), Relay::In(_)));
        x.guest(&started(1)).unwrap();
        assert_eq!(rx.try_recv().unwrap(), (1, ExecEvent::Started));
        x.output_closed(1);
        x.guest(&exited(1, 3)).unwrap();
        assert!(rx.try_recv().is_err(), "stderr is still open");
        x.output_closed(1);
        assert_eq!(rx.try_recv().unwrap(), (1, ExecEvent::Exited(3)));
        assert!(x.is_empty());
        assert!(x.guest(&exited(1, 3)).is_err(), "a retired id");
    }

    #[test]
    fn forged_ids_streams_and_sequences_are_violations() {
        let (x, tx, _rx) = execs();
        x.add(1, false, streams(false), tx.clone()).unwrap();
        x.add(2, true, streams(true), tx.clone()).unwrap();
        assert!(x.add(2, false, streams(false), tx.clone()).is_err(), "a reused id");
        assert!(x.add(0, false, streams(false), tx).is_err());
        assert_eq!(x.unconnected(), 2 + 1, "stdout and stderr, and a terminal");
        assert!(x.connect(9, ExecStream::Stdout).is_err(), "not started by the host");
        assert!(x.connect(1, ExecStream::Stdin).is_err(), "no stdin requested");
        assert!(x.connect(1, ExecStream::Tty).is_err(), "not a terminal");
        x.connect(1, ExecStream::Stdout).unwrap();
        assert!(x.connect(1, ExecStream::Stdout).is_err(), "a second stdout");
        assert!(x.connect(2, ExecStream::Stdout).is_err(), "a terminal has no stdout");
        assert!(matches!(
            x.connect(2, ExecStream::Tty).unwrap(),
            Relay::Tty { input: Some(_), .. }
        ));
        assert!(x.guest(&exited(1, 0)).is_err(), "exited before started");
        x.guest(&started(1)).unwrap();
        assert!(x.guest(&started(1)).is_err(), "started twice");
        let failed = GuestMessage::ExecFailed(ExecFailed::new(1, None, "x"));
        assert!(x.guest(&failed).is_err(), "failed after started");
        assert!(x.guest(&started(7)).is_err());
    }

    #[test]
    fn failures_limits_and_the_end() {
        let (x, tx, rx) = execs();
        for id in 1..=MAX_EXECS as u32 {
            x.add(id, false, streams(false), tx.clone()).unwrap();
        }
        assert!(x.add(99, false, streams(false), tx.clone()).is_err(), "over the limit");
        x.guest(&GuestMessage::ExecFailed(ExecFailed::new(1, Some(2), "not found")))
            .unwrap();
        assert_eq!(rx.try_recv().unwrap(), (1, ExecEvent::Failed("not found".into())));
        x.add(99, false, streams(false), tx.clone()).unwrap();
        x.guest(&started(2)).unwrap();
        let _ = rx.try_recv();
        x.end();
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.contains(&(2, ExecEvent::Exited(EXEC_KILLED))));
        assert!(events.contains(&(3, ExecEvent::Failed("the VM ended before the exec started".into()))));
        assert_eq!(events.len(), MAX_EXECS);
        assert!(x.add(100, false, streams(false), tx).is_err(), "after the end");
    }
}
