//! A minimal host side of the control protocol (spec §9.5), for the boot tests.
//! It is written so that `kiln run` (M3b) can lift it: listeners for every port
//! are bound before the guest starts; the first `Hello` on the first control
//! connection gets `Config`, once; stdio is relayed; and any protocol violation
//! (a second control connection or `Hello`, a message before `Hello`, a bad
//! frame) kills the VM (T9).

use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kiln_proto::{
    Config, EXIT_INFRA, Exited, GuestMessage, HostMessage, InitFailed, PROTOCOL_VERSION, port, read_message,
    write_message,
};
use vmkit::{Vm, VmEnd};

/// What the host answers the first `Hello` with.
#[derive(Debug, Clone)]
pub enum Reply {
    Config,
    /// Raw bytes instead of a `Config` frame, to test the guest's side of T9.
    Raw(Vec<u8>),
}

/// What happened on the control connection.
#[derive(Debug)]
pub enum Event {
    Guest(GuestMessage),
    Violation(String),
    Closed,
}

/// Handles one control connection: answers the first `Hello`, forwards every
/// message, and reports the first violation. Returns when the guest closes the
/// connection or breaks the protocol.
pub fn serve_control(stream: UnixStream, config: &Config, reply: &Reply, events: &Sender<Event>) {
    let mut reader = match stream.try_clone() {
        Ok(r) => r,
        Err(e) => {
            let _ = events.send(Event::Violation(format!("control socket: {e}")));
            return;
        }
    };
    let mut writer = stream;
    let mut configured = false;
    loop {
        let event = match read_message::<_, GuestMessage>(&mut reader) {
            Ok(Some(GuestMessage::Hello(_))) if configured => Event::Violation("a second Hello".into()),
            Ok(Some(GuestMessage::Hello(h))) if h.protocol != PROTOCOL_VERSION => {
                Event::Violation(format!("unsupported guest protocol {}", h.protocol))
            }
            Ok(Some(msg @ GuestMessage::Hello(_))) => {
                configured = true;
                let sent = match reply {
                    Reply::Config => write_message(&mut writer, &HostMessage::Config(Box::new(config.clone())))
                        .map_err(|e| e.to_string()),
                    Reply::Raw(bytes) => writer.write_all(bytes).map_err(|e| e.to_string()),
                };
                match sent {
                    Ok(()) => Event::Guest(msg),
                    Err(e) => Event::Violation(format!("send Config: {e}")),
                }
            }
            Ok(Some(_)) if !configured => Event::Violation("a message before Hello".into()),
            Ok(Some(msg)) => Event::Guest(msg),
            Ok(None) => Event::Closed,
            Err(e) => Event::Violation(format!("guest protocol error: {e}")),
        };
        let last = !matches!(event, Event::Guest(_));
        if events.send(event).is_err() || last {
            return;
        }
    }
}

/// Accepts one connection, polling so the thread can give up when `stop` is set.
fn accept(listener: &UnixListener, stop: &AtomicBool) -> Option<UnixStream> {
    listener.set_nonblocking(true).ok()?;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).ok()?;
                return Some(stream);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if stop.load(Ordering::SeqCst) {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// Bytes read from a guest stream so far, filled by a thread until EOF.
#[derive(Clone, Default)]
pub struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    pub fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }
}

/// How the run ended.
#[derive(Debug)]
pub struct Outcome {
    pub messages: Vec<GuestMessage>,
    /// Why the host killed the VM, if it did.
    pub violation: Option<String>,
    pub end: VmEnd,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub tty: Vec<u8>,
}

impl Outcome {
    pub fn exited(&self) -> Option<Exited> {
        self.messages.iter().find_map(|m| match m {
            GuestMessage::Exited(e) => Some(*e),
            _ => None,
        })
    }

    pub fn init_failed(&self) -> Option<&InitFailed> {
        self.messages.iter().find_map(|m| match m {
            GuestMessage::InitFailed(f) => Some(f),
            _ => None,
        })
    }

    pub fn stages(&self) -> Vec<u8> {
        self.messages
            .iter()
            .filter_map(|m| match m {
                GuestMessage::Stage(s) => Some(s.n),
                _ => None,
            })
            .collect()
    }

    pub fn running(&self) -> bool {
        self.messages.contains(&GuestMessage::Running)
    }

    /// The exit code `kiln run` reports (spec §9.5).
    pub fn exit_code(&self) -> i32 {
        if self.violation.is_none()
            && let Some(e) = self.exited()
        {
            return e.exit_code();
        }
        match (self.violation.is_none(), self.init_failed()) {
            (true, Some(f)) => f.exit_code(),
            _ => EXIT_INFRA,
        }
    }

    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    pub fn tty(&self) -> String {
        String::from_utf8_lossy(&self.tty).into_owned()
    }
}

/// One running VM and the host side of its protocol.
pub struct Session {
    vm: Box<dyn Vm>,
    events: Receiver<Event>,
    control: Slot,
    tty_in: Slot,
    stdout: Captured,
    stderr: Captured,
    tty: Captured,
    threads: Vec<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    messages: Vec<GuestMessage>,
    violation: Option<String>,
    end: Option<VmEnd>,
}

impl Session {
    /// Binds the ports on `vm`'s vsock socket, then starts the guest. `stdin` is
    /// written to port 1025 and half-closed (only when `config.interactive`).
    pub fn start(mut vm: Box<dyn Vm>, config: Config, reply: Reply, stdin: Vec<u8>) -> Self {
        let base = vm.vsock_socket().expect("the VM has vsock").to_path_buf();
        let bind = |port: u32| {
            // A socket file left by an earlier VM in the same run directory.
            let path = format!("{}_{port}", base.display());
            let _ = std::fs::remove_file(&path);
            UnixListener::bind(path).expect("bind a vsock port")
        };
        let (tx, events) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let control = Arc::new(Mutex::new(None));
        let tty_in = Arc::new(Mutex::new(None));
        let (stdout, stderr, tty) = (Captured::default(), Captured::default(), Captured::default());
        let mut threads = vec![spawn_control(
            bind(port::CONTROL),
            config.clone(),
            reply,
            tx,
            &stop,
            &control,
        )];
        if config.tty.is_some() {
            threads.push(spawn_tty(bind(port::TTY), &stop, &tty_in, &tty));
        } else {
            if config.interactive {
                threads.push(spawn_stdin(bind(port::STDIN), stdin, &stop));
            }
            threads.push(spawn_output(bind(port::STDOUT), &stdout, &stop));
            threads.push(spawn_output(bind(port::STDERR), &stderr, &stop));
        }
        vm.start().expect("start the VM");
        Session {
            vm,
            events,
            control,
            tty_in,
            stdout,
            stderr,
            tty,
            threads,
            stop,
            messages: Vec::new(),
            violation: None,
            end: None,
        }
    }

    /// Handles events for up to `timeout`; stops early once `done` holds.
    fn pump(&mut self, timeout: Duration, done: impl Fn(&Self) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if done(self) {
                return true;
            }
            if self.end.is_none()
                && let Some(end) = self.vm.wait_timeout(Duration::ZERO).expect("wait for the VM")
            {
                self.end = Some(end);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return done(self);
            }
            match self.events.recv_timeout(left.min(Duration::from_millis(50))) {
                Ok(Event::Guest(msg)) => self.messages.push(msg),
                Ok(Event::Violation(why)) => {
                    if self.violation.is_none() {
                        self.violation = Some(why);
                        self.vm.kill().expect("kill the VM");
                    }
                }
                Ok(Event::Closed) | Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
            }
        }
    }

    /// Waits for `Running`; false if the VM ended or failed first.
    pub fn wait_running(&mut self, timeout: Duration) -> bool {
        self.pump(timeout, |s| {
            s.messages.contains(&GuestMessage::Running) || s.end.is_some() || s.violation.is_some()
        }) && self.messages.contains(&GuestMessage::Running)
    }

    /// Waits until the captured stdout (or tty output) contains `needle`.
    pub fn wait_output(&mut self, needle: &str, timeout: Duration) -> bool {
        self.pump(timeout, |s| {
            s.stdout.text().contains(needle) || s.tty.text().contains(needle) || s.end.is_some()
        });
        self.stdout.text().contains(needle) || self.tty.text().contains(needle)
    }

    pub fn send(&self, msg: &HostMessage) {
        let mut slot = self.control.lock().unwrap();
        write_message(slot.as_mut().expect("the guest connected"), msg).expect("send to the guest");
    }

    pub fn send_raw(&self, bytes: &[u8]) {
        let mut slot = self.control.lock().unwrap();
        slot.as_mut()
            .expect("the guest connected")
            .write_all(bytes)
            .expect("send to the guest");
    }

    pub fn tty_write(&self, bytes: &[u8]) {
        let mut slot = self.tty_in.lock().unwrap();
        slot.as_mut()
            .expect("the guest connected the tty")
            .write_all(bytes)
            .expect("write to the tty");
    }

    /// Runs until the VM ends (killing it after `timeout`), then collects the output.
    pub fn finish(mut self, timeout: Duration) -> Outcome {
        let ended = self.pump(timeout, |s| s.end.is_some());
        if !ended {
            self.vm.kill().expect("kill the VM");
            self.violation.get_or_insert_with(|| "timeout".into());
            self.end = self.vm.wait_timeout(Duration::from_secs(10)).expect("wait for the VM");
        }
        // Drain what the guest sent before it ended.
        self.pump(Duration::from_millis(200), |_| false);
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        Outcome {
            messages: std::mem::take(&mut self.messages),
            violation: self.violation.take(),
            end: self.end.expect("the VM ended"),
            stdout: self.stdout.bytes(),
            stderr: self.stderr.bytes(),
            tty: self.tty.bytes(),
        }
    }
}

fn copy_into(mut stream: UnixStream, into: &Captured) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => into.0.lock().unwrap().extend_from_slice(&buf[..n]),
        }
    }
}

type Slot = Arc<Mutex<Option<UnixStream>>>;

/// Serves the first control connection; a second one is a violation.
fn spawn_control(
    listener: UnixListener,
    config: Config,
    reply: Reply,
    events: Sender<Event>,
    stop: &Arc<AtomicBool>,
    slot: &Slot,
) -> JoinHandle<()> {
    let (stop, slot) = (stop.clone(), slot.clone());
    std::thread::spawn(move || {
        let Some(stream) = accept(&listener, &stop) else { return };
        *slot.lock().unwrap() = Some(stream.try_clone().expect("clone the control socket"));
        let (watch_events, watch_stop) = (events.clone(), stop.clone());
        std::thread::spawn(move || {
            if accept(&listener, &watch_stop).is_some() {
                let _ = watch_events.send(Event::Violation("a second control connection".into()));
            }
        });
        serve_control(stream, &config, &reply, &events);
    })
}

fn spawn_stdin(listener: UnixListener, bytes: Vec<u8>, stop: &Arc<AtomicBool>) -> JoinHandle<()> {
    let stop = stop.clone();
    std::thread::spawn(move || {
        if let Some(mut stream) = accept(&listener, &stop) {
            let _ = stream.write_all(&bytes);
            let _ = stream.shutdown(Shutdown::Write);
        }
    })
}

fn spawn_output(listener: UnixListener, into: &Captured, stop: &Arc<AtomicBool>) -> JoinHandle<()> {
    let (stop, into) = (stop.clone(), into.clone());
    std::thread::spawn(move || {
        if let Some(stream) = accept(&listener, &stop) {
            copy_into(stream, &into);
        }
    })
}

fn spawn_tty(listener: UnixListener, stop: &Arc<AtomicBool>, slot: &Slot, into: &Captured) -> JoinHandle<()> {
    let (stop, slot, into) = (stop.clone(), slot.clone(), into.clone());
    std::thread::spawn(move || {
        if let Some(stream) = accept(&listener, &stop) {
            *slot.lock().unwrap() = Some(stream.try_clone().expect("clone the tty socket"));
            copy_into(stream, &into);
        }
    })
}
