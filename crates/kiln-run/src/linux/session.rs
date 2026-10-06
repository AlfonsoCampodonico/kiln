//! The host side of one guest session (spec §9.5, §9.7, T9).
//!
//! Threads, and what bounds them:
//! - **listener**: one `poll(2)` over every served vsock port and a wake pipe (no
//!   timed polling). The first connection on a port is served; a second one on
//!   any port is a violation (D-5).
//! - **control reader**: reads guest frames through the sequence state machine
//!   ([`Protocol`], D-1) and stops at the first violation, so at most nine guest
//!   messages are ever queued for the main loop. It answers the first `Hello`
//!   with `Config`, once. Like the output streams, it is drained to EOF after
//!   the VM ends: the guest's last messages may still be in flight then.
//! - **control writer**: the only thread that writes host messages, from a
//!   bounded queue, with a send timeout. A full queue or a stalled send is a
//!   violation, and no other thread ever waits on the control socket (D-2).
//! - **relays**: stdout, stderr and the terminal are streamed to their sinks as
//!   they arrive, never buffered whole (D-6); stdin is copied to the guest with
//!   EOF as a half-close.
//! - **main loop** ([`Session::pump`]): handles events, polls the VMM's end, and
//!   enforces every deadline: the boot timeout, the bound after a requested stop,
//!   and the bound for the VM to end after the guest's final message or after the
//!   control connection closes (D-3). Killing the VMM never depends on control
//!   I/O, and a VMM that is already gone is not an error (D-6).

use std::collections::BTreeSet;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kiln_proto::{
    Config, EXIT_INFRA, Exited, GuestMessage, HostMessage, InitFailed, Shutdown, Signal, WindowSize, port,
    read_message, write_message,
};
use rustix::event::{PollFd, PollFlags, poll};
use vmkit::{EndReason, Vm, VmEnd};

use crate::error::{Error, Result};
use crate::escape::{Escape, Key};
use crate::protocol::{Accepted, Protocol};

/// How many host messages may wait for the guest to read them.
const QUEUE: usize = 64;
/// How long one control write may block before the guest counts as not reading.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the main loop looks for the VMM's end (vmkit has no pollable handle).
const VM_POLL: Duration = Duration::from_millis(20);
/// The exit code of a VM the user killed (second SIGINT, `Ctrl-]` `k`), as `docker kill`.
pub const EXIT_KILLED: i32 = 137;

/// Where the guest's stdio goes and comes from.
pub struct Streams {
    /// With `interactive`: relayed to the guest's stdin (or terminal).
    pub stdin: Option<Box<dyn Read + Send>>,
    /// The guest's stdout, or its terminal with `tty`.
    pub stdout: Box<dyn Write + Send>,
    pub stderr: Box<dyn Write + Send>,
}

/// Timeouts and switches.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// From starting the VM to `Running` (spec §9.7).
    pub boot_timeout: Duration,
    /// The grace period sent with `Shutdown`.
    pub stop_timeout: u32,
    /// How long past the grace period the host waits before killing the VM itself.
    pub stop_margin: Duration,
    /// After `Exited`, `InitFailed` or the control connection closing: how long the
    /// guest has to end the VM (it syncs and remounts the scratch disk first, D-3).
    pub end_timeout: Duration,
    /// After the VM ended: how long its output streams have to reach EOF.
    pub drain_timeout: Duration,
    /// Interpret `Ctrl-]` sequences on the terminal's input (`-t`).
    pub escape_keys: bool,
    /// Raw bytes to answer `Hello` with instead of `Config` (tests of the guest's T9).
    #[doc(hidden)]
    pub reply: Option<Vec<u8>>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            boot_timeout: crate::options::DEFAULT_BOOT_TIMEOUT,
            stop_timeout: crate::options::DEFAULT_STOP_TIMEOUT,
            stop_margin: Duration::from_secs(5),
            end_timeout: Duration::from_secs(10),
            drain_timeout: Duration::from_secs(2),
            escape_keys: false,
            reply: None,
        }
    }
}

/// What the writer sends.
enum Out {
    Msg(HostMessage),
    Raw(Vec<u8>),
    Stop,
}

enum Event {
    Guest(GuestMessage),
    Violation(String),
    ControlClosed,
    Connected(u32),
    Eof(u32),
    Interrupt,
    Terminate,
    Shutdown,
    Kill,
}

/// Controls a running session from other threads (signals, the terminal).
#[derive(Clone)]
pub struct Handle {
    events: Sender<Event>,
    out: SyncSender<Out>,
}

impl Handle {
    /// SIGINT: the first asks the guest to stop, the second kills the VM.
    pub fn interrupt(&self) {
        let _ = self.events.send(Event::Interrupt);
    }

    /// SIGTERM: asks the guest to stop.
    pub fn terminate(&self) {
        let _ = self.events.send(Event::Terminate);
    }

    /// `Ctrl-]` `q`: asks the guest to stop.
    pub fn shutdown(&self) {
        let _ = self.events.send(Event::Shutdown);
    }

    /// `Ctrl-]` `k`: kills the VM.
    pub fn kill(&self) {
        let _ = self.events.send(Event::Kill);
    }

    /// Forwards a signal (1–31) to the guest's main process.
    pub fn signal(&self, sig: i32) {
        self.queue(Out::Msg(HostMessage::Signal(Signal { sig })), true);
    }

    /// The terminal was resized. Dropped when the queue is full: a later resize follows.
    pub fn window_size(&self, size: WindowSize) {
        self.queue(Out::Msg(HostMessage::WindowSize(size)), false);
    }

    /// Any host message (tests).
    #[doc(hidden)]
    pub fn send(&self, msg: HostMessage) {
        self.queue(Out::Msg(msg), true);
    }

    /// Raw bytes on the control connection, after `Config` (tests of the guest's T9).
    #[doc(hidden)]
    pub fn send_raw(&self, bytes: Vec<u8>) {
        self.queue(Out::Raw(bytes), true);
    }

    fn queue(&self, out: Out, must: bool) {
        match self.out.try_send(out) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) if must => {
                let _ = self
                    .events
                    .send(Event::Violation("the guest is not reading its control channel".into()));
            }
            Err(TrySendError::Full(_)) => {}
        }
    }
}

/// How a session ended.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// What `kiln run` exits with (spec §9.5).
    pub exit_code: i32,
    pub exited: Option<Exited>,
    pub init_failed: Option<InitFailed>,
    /// The stages the guest reported, in order.
    pub stages: Vec<u8>,
    pub hello: bool,
    pub running: bool,
    /// Why the host killed the VM, when the guest broke the protocol or a timeout.
    pub violation: Option<String>,
    /// The user killed the VM.
    pub killed: bool,
    pub end: VmEnd,
    /// Things the user should know (a forced kill after the final message, ...).
    pub warnings: Vec<String>,
    /// From starting the VM to `Hello` and to `Running`.
    pub hello_after: Option<Duration>,
    pub running_after: Option<Duration>,
    /// From starting the VM to the VMM's exit, and to the end of its output.
    pub ended_after: Option<Duration>,
    pub drained_after: Duration,
}

/// One running guest and the host side of its protocol.
pub struct Session {
    vm: Box<dyn Vm>,
    events: Receiver<Event>,
    handle: Handle,
    wake: OwnedFd,
    listener: Option<JoinHandle<()>>,
    opts: SessionOptions,
    proto: Protocol,
    stages: Vec<u8>,
    violation: Option<String>,
    killed: bool,
    warnings: Vec<String>,
    end: Option<VmEnd>,
    ended_at: Option<Instant>,
    started: Instant,
    hello_at: Option<Instant>,
    running_at: Option<Instant>,
    boot_deadline: Option<Instant>,
    stop_deadline: Option<Instant>,
    end_deadline: Option<Instant>,
    interrupts: u32,
    open: BTreeSet<u32>,
}

/// The Unix socket path the VMM forwards guest port `p` to.
fn port_path(base: &Path, p: u32) -> PathBuf {
    PathBuf::from(format!("{}_{p}", base.display()))
}

fn bind(base: &Path, p: u32) -> Result<UnixListener> {
    use std::os::unix::fs::FileTypeExt;
    let path = port_path(base, p);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(&path)?,
        Ok(_) => {
            return Err(Error::refused(format!("{} exists and is not a socket", path.display())));
        }
        Err(_) => {}
    }
    let l = UnixListener::bind(&path)?;
    l.set_nonblocking(true)?;
    Ok(l)
}

impl Session {
    /// Listens on the ports `config` needs, then starts the guest.
    pub fn start(vm: Box<dyn Vm>, config: Config, streams: Streams, opts: SessionOptions) -> Result<Self> {
        let base = vm
            .vsock_socket()
            .ok_or_else(|| Error::refused("the VM has no vsock device"))?
            .to_path_buf();
        let mut ports = vec![port::CONTROL];
        if config.tty.is_some() {
            ports.push(port::TTY);
        } else {
            if config.interactive {
                ports.push(port::STDIN);
            }
            ports.extend([port::STDOUT, port::STDERR]);
        }
        let listeners = ports
            .iter()
            .map(|&p| Ok((p, bind(&base, p)?)))
            .collect::<Result<Vec<_>>>()?;
        let (tx, events) = channel();
        let (out_tx, out_rx) = sync_channel(QUEUE);
        let (stream_tx, stream_rx) = channel();
        let (wake_r, wake) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(std::io::Error::from)?;
        spawn("control-writer", {
            let tx = tx.clone();
            move || writer(&stream_rx, &out_rx, &tx)
        })?;
        let serve = Serve {
            config,
            reply: opts.reply.clone(),
            events: tx.clone(),
            writer: stream_tx,
            stdin: streams.stdin,
            stdout: Some(streams.stdout),
            stderr: Some(streams.stderr),
            escape_keys: opts.escape_keys,
        };
        let listener = spawn("vsock-listener", move || listen(listeners, &wake_r, serve))?;
        let handle = Handle {
            events: tx,
            out: out_tx,
        };
        let now = Instant::now();
        let mut s = Session {
            vm,
            events,
            handle,
            wake,
            listener: Some(listener),
            opts,
            proto: Protocol::default(),
            stages: Vec::new(),
            violation: None,
            killed: false,
            warnings: Vec::new(),
            end: None,
            ended_at: None,
            started: now,
            hello_at: None,
            running_at: None,
            boot_deadline: None,
            stop_deadline: None,
            end_deadline: None,
            interrupts: 0,
            open: BTreeSet::new(),
        };
        s.vm.start()?;
        s.started = Instant::now();
        s.boot_deadline = Some(s.started + s.opts.boot_timeout);
        Ok(s)
    }

    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// The guest has started its main process.
    pub fn running(&self) -> bool {
        self.proto.running()
    }

    /// The VM has ended.
    pub fn ended(&self) -> bool {
        self.end.is_some()
    }

    /// Runs the session until `done()` holds, the VM ends, or `timeout` passes;
    /// returns `done()`.
    pub fn pump(&mut self, timeout: Duration, mut done: impl FnMut(&Self) -> bool) -> bool {
        let until = Instant::now() + timeout;
        loop {
            if done(self) {
                return true;
            }
            if self.end.is_some() || Instant::now() >= until {
                return done(self);
            }
            self.step(Some(until));
        }
    }

    /// Waits for `Running`; false if the VM ended (or was killed for a boot timeout) first.
    pub fn wait_running(&mut self) -> bool {
        self.pump(Duration::from_secs(365 * 24 * 3600), |s| s.proto.running());
        self.proto.running()
    }

    /// Runs to the end of the VM and its output.
    pub fn finish(self) -> Outcome {
        self.finish_within(None)
    }

    /// Like [`Session::finish`], killing the VM after `limit` (a test's overall bound).
    pub fn finish_within(mut self, limit: Option<Duration>) -> Outcome {
        let limit = limit.map(|l| Instant::now() + l);
        while !self.done() {
            if limit.is_some_and(|l| Instant::now() >= l) && self.end.is_none() {
                self.violate("timed out (the test's limit)".into());
                if let Ok(end) = self.vm.wait_timeout(Duration::from_secs(10)) {
                    self.set_end(end);
                }
                if self.end.is_none() {
                    self.set_end(Some(lost()));
                }
            }
            self.step(limit);
        }
        // Late events (the control thread may have read a final message just now).
        while let Ok(e) = self.events.try_recv() {
            self.handle_event(e);
        }
        self.outcome()
    }

    fn done(&self) -> bool {
        match self.ended_at {
            None => false,
            Some(at) => self.open.is_empty() || at.elapsed() >= self.opts.drain_timeout,
        }
    }

    /// One turn of the main loop: deadlines, the VM's state, at most one event.
    fn step(&mut self, until: Option<Instant>) {
        let now = Instant::now();
        if self.end.is_none() {
            if self.boot_deadline.is_some_and(|d| now >= d) {
                self.boot_deadline = None;
                self.violate(format!(
                    "boot timeout: the guest was not running {}s after the VM started",
                    self.opts.boot_timeout.as_secs()
                ));
            }
            if self.stop_deadline.is_some_and(|d| now >= d) {
                self.stop_deadline = None;
                self.warnings.push(format!(
                    "the guest did not stop within {}s of the request; the VM was killed",
                    self.opts.stop_timeout
                ));
                self.kill_vm();
            }
            if self.end_deadline.is_some_and(|d| now >= d) {
                self.end_deadline = None;
                self.warnings.push(format!(
                    "the VM did not end within {}s of the guest's final message; it was killed",
                    self.opts.end_timeout.as_secs()
                ));
                self.kill_vm();
            }
            match self.vm.wait_timeout(Duration::ZERO) {
                Ok(end) => self.set_end(end),
                Err(e) => {
                    self.warnings.push(format!("waiting for the VMM: {e}"));
                    self.set_end(Some(lost()));
                }
            }
        }
        if self.done() {
            return;
        }
        let mut wait = match self.ended_at {
            None => VM_POLL,
            Some(at) => (at + self.opts.drain_timeout).saturating_duration_since(now),
        };
        if let Some(u) = until {
            wait = wait.min(u.saturating_duration_since(now));
        }
        match self.events.recv_timeout(wait) {
            Ok(e) => self.handle_event(e),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
    }

    fn set_end(&mut self, end: Option<VmEnd>) {
        if self.end.is_none() && end.is_some() {
            self.end = end;
            self.ended_at = Some(Instant::now());
        }
    }

    fn handle_event(&mut self, e: Event) {
        let now = Instant::now();
        match e {
            Event::Guest(msg) => {
                match self.proto.accept(&msg) {
                    Ok(Accepted::Hello | Accepted::Recorded) => {}
                    Ok(Accepted::Ignored) => return,
                    Err(v) => {
                        // The reader checked it already; this cannot differ.
                        self.violate(v.0);
                        return;
                    }
                }
                match msg {
                    GuestMessage::Hello(_) => self.hello_at = Some(now),
                    GuestMessage::Stage(s) => self.stages.push(s.n),
                    GuestMessage::Running => {
                        self.running_at = Some(now);
                        self.boot_deadline = None;
                    }
                    GuestMessage::Exited(_) | GuestMessage::InitFailed(_) => self.expect_end(now),
                }
            }
            Event::Violation(why) => self.violate(why),
            Event::ControlClosed => self.expect_end(now),
            // The control connection and the output streams are drained to EOF after
            // the VM ends, bounded by the drain timeout; stdin need not be.
            Event::Connected(p) if [port::CONTROL, port::STDOUT, port::STDERR, port::TTY].contains(&p) => {
                self.open.insert(p);
            }
            Event::Connected(_) => {}
            Event::Eof(p) => {
                self.open.remove(&p);
            }
            Event::Interrupt => {
                self.interrupts += 1;
                if self.interrupts >= 2 {
                    self.user_kill();
                } else {
                    self.request_stop();
                }
            }
            Event::Terminate | Event::Shutdown => self.request_stop(),
            Event::Kill => self.user_kill(),
        }
    }

    /// The guest is done talking: it must end the VM soon (D-3).
    fn expect_end(&mut self, now: Instant) {
        self.boot_deadline = None;
        if self.end_deadline.is_none() {
            self.end_deadline = Some(now + self.opts.end_timeout);
        }
    }

    fn request_stop(&mut self) {
        if self.end.is_some() {
            return;
        }
        if !self.proto.hello() {
            // Nothing runs yet that could stop gracefully.
            self.user_kill();
            return;
        }
        let grace = self.opts.stop_timeout;
        self.handle
            .queue(Out::Msg(HostMessage::Shutdown(Shutdown { grace_secs: grace })), true);
        let at = Instant::now() + Duration::from_secs(u64::from(grace)) + self.opts.stop_margin;
        self.stop_deadline = Some(self.stop_deadline.map_or(at, |d| d.min(at)));
    }

    fn user_kill(&mut self) {
        if self.end.is_none() {
            self.killed = true;
            self.kill_vm();
        }
    }

    /// Records the first violation and kills the VM (T9).
    fn violate(&mut self, why: String) {
        if self.violation.is_none() && !self.killed {
            self.violation = Some(why);
        }
        self.kill_vm();
    }

    fn kill_vm(&mut self) {
        if self.end.is_some() {
            return;
        }
        if let Err(e) = self.vm.kill() {
            self.warnings.push(format!("killing the VMM: {e}"));
        }
    }

    fn outcome(&mut self) -> Outcome {
        let exit_code = if self.violation.is_some() {
            EXIT_INFRA
        } else if self.killed && self.proto.exited().is_none() {
            EXIT_KILLED
        } else {
            self.proto.exit_code()
        };
        Outcome {
            exit_code,
            exited: self.proto.exited(),
            init_failed: self.proto.init_failed().cloned(),
            stages: self.stages.clone(),
            hello: self.proto.hello(),
            running: self.proto.running(),
            violation: self.violation.clone(),
            killed: self.killed,
            end: self.end.unwrap_or_else(lost),
            warnings: std::mem::take(&mut self.warnings),
            hello_after: self.hello_at.map(|t| t - self.started),
            running_after: self.running_at.map(|t| t - self.started),
            ended_after: self.ended_at.map(|t| t - self.started),
            drained_after: self.started.elapsed(),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = rustix::io::write(&self.wake, b"x");
        let _ = self.handle.out.try_send(Out::Stop);
        if let Some(l) = self.listener.take() {
            let _ = l.join();
        }
    }
}

/// The end reported when vmkit could not tell.
fn lost() -> VmEnd {
    VmEnd {
        reason: EndReason::Killed,
        code: None,
        signal: None,
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>> {
    Ok(std::thread::Builder::new().name(name.into()).spawn(f)?)
}

/// What the listener hands each first connection.
struct Serve {
    config: Config,
    reply: Option<Vec<u8>>,
    events: Sender<Event>,
    writer: Sender<UnixStream>,
    stdin: Option<Box<dyn Read + Send>>,
    stdout: Option<Box<dyn Write + Send>>,
    stderr: Option<Box<dyn Write + Send>>,
    escape_keys: bool,
}

/// Accepts on every port until woken; a second connection on a port is a violation.
fn listen(listeners: Vec<(u32, UnixListener)>, wake: &OwnedFd, mut serve: Serve) {
    let mut seen = BTreeSet::new();
    loop {
        let mut fds: Vec<PollFd<'_>> = listeners
            .iter()
            .map(|(_, l)| PollFd::new(l, PollFlags::IN))
            .chain([PollFd::new(wake, PollFlags::IN)])
            .collect();
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                let _ = serve.events.send(Event::Violation(format!("vsock listener: {e}")));
                return;
            }
        }
        let ready: Vec<bool> = fds.iter().map(|f| !f.revents().is_empty()).collect();
        drop(fds);
        if ready[listeners.len()] {
            return;
        }
        for (i, (p, l)) in listeners.iter().enumerate() {
            if !ready[i] {
                continue;
            }
            let stream = match l.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                Err(e) => {
                    let _ = serve
                        .events
                        .send(Event::Violation(format!("accept on vsock port {p}: {e}")));
                    return;
                }
            };
            if !seen.insert(*p) {
                let _ = serve
                    .events
                    .send(Event::Violation(format!("a second connection on vsock port {p}")));
                continue;
            }
            if stream.set_nonblocking(false).is_err() {
                continue;
            }
            let _ = serve.events.send(Event::Connected(*p));
            if let Err(e) = serve.dispatch(*p, stream) {
                let _ = serve.events.send(Event::Violation(format!("vsock port {p}: {e}")));
            }
        }
    }
}

impl Serve {
    fn dispatch(&mut self, p: u32, stream: UnixStream) -> Result<()> {
        let events = self.events.clone();
        match p {
            port::CONTROL => {
                let (config, reply, writer) = (self.config.clone(), self.reply.take(), self.writer.clone());
                spawn("control-reader", move || {
                    control(stream, &config, reply, &events, &writer)
                })?;
            }
            port::STDIN => {
                if let Some(input) = self.stdin.take() {
                    spawn("stdin", move || copy_in(input, stream))?;
                }
            }
            port::STDOUT | port::STDERR | port::TTY => {
                let sink = if p == port::STDERR {
                    self.stderr.take()
                } else {
                    self.stdout.take()
                };
                if p == port::TTY
                    && let Some(input) = self.stdin.take()
                {
                    let (to, escape) = (stream.try_clone()?, self.escape_keys);
                    let events = events.clone();
                    spawn("tty-in", move || tty_in(input, to, escape, &events))?;
                }
                if let Some(sink) = sink {
                    spawn("output", move || copy_out(stream, sink, p, &events))?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Reads guest messages through the state machine; answers the first `Hello`.
/// Reports the end of the control connection as `Eof`, however it ended.
fn control(
    stream: UnixStream,
    config: &Config,
    reply: Option<Vec<u8>>,
    events: &Sender<Event>,
    writer: &Sender<UnixStream>,
) {
    read_control(stream, config, reply, events, writer);
    let _ = events.send(Event::Eof(port::CONTROL));
}

fn read_control(
    stream: UnixStream,
    config: &Config,
    reply: Option<Vec<u8>>,
    events: &Sender<Event>,
    writer: &Sender<UnixStream>,
) {
    let violation = |why: String| {
        let _ = events.send(Event::Violation(why));
    };
    let _ = rustix::net::sockopt::set_socket_timeout(&stream, rustix::net::sockopt::Timeout::Send, Some(SEND_TIMEOUT));
    let (mut reader, mut w) = match stream.try_clone() {
        Ok(r) => (r, stream),
        Err(e) => return violation(format!("control socket: {e}")),
    };
    let mut proto = Protocol::default();
    loop {
        let msg = match read_message::<_, GuestMessage>(&mut reader) {
            Ok(Some(m)) => m,
            Ok(None) => {
                let _ = events.send(Event::ControlClosed);
                return;
            }
            Err(e) => return violation(format!("guest protocol error: {e}")),
        };
        match proto.accept(&msg) {
            // After InitFailed: not the main loop's concern.
            Ok(Accepted::Ignored) => continue,
            Ok(Accepted::Hello) => {
                let sent = match &reply {
                    None => {
                        write_message(&mut w, &HostMessage::Config(Box::new(config.clone()))).map_err(|e| e.to_string())
                    }
                    Some(raw) => w.write_all(raw).map_err(|e| e.to_string()),
                };
                if let Err(e) = sent {
                    return violation(format!("sending Config: {e}"));
                }
                match w.try_clone() {
                    Ok(c) => {
                        let _ = writer.send(c);
                    }
                    Err(e) => return violation(format!("control socket: {e}")),
                }
            }
            Ok(Accepted::Recorded) => {}
            Err(v) => return violation(v.0),
        }
        if events.send(Event::Guest(msg)).is_err() {
            return;
        }
    }
}

/// Writes queued host messages once `Config` has gone out.
fn writer(stream: &Receiver<UnixStream>, out: &Receiver<Out>, events: &Sender<Event>) {
    let Ok(mut s) = stream.recv() else { return };
    for item in out {
        let r = match item {
            Out::Stop => return,
            Out::Msg(m) => write_message(&mut s, &m).map_err(|e| match e {
                kiln_proto::ProtoError::Io(io) => io,
                other => std::io::Error::other(other.to_string()),
            }),
            Out::Raw(b) => s.write_all(&b),
        };
        match r {
            Ok(()) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                let _ = events.send(Event::Violation("the guest is not reading its control channel".into()));
                return;
            }
            // The VM is gone, or going.
            Err(_) => return,
        }
    }
}

/// Streams guest output to `sink`. A sink that fails (a closed pipe) is dropped,
/// but the guest's output is still read to EOF, so the guest never blocks on it.
fn copy_out(mut from: UnixStream, mut sink: Box<dyn Write + Send>, p: u32, events: &Sender<Event>) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut ok = true;
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if ok && sink.write_all(&buf[..n]).and_then(|()| sink.flush()).is_err() {
                    ok = false;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = events.send(Event::Eof(p));
}

/// Copies stdin to the guest; EOF is a half-close.
fn copy_in(mut from: Box<dyn Read + Send>, mut to: UnixStream) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = to.shutdown(std::net::Shutdown::Write);
}

/// Copies the terminal's input to the guest, acting on escape sequences.
fn tty_in(mut from: Box<dyn Read + Send>, mut to: UnixStream, escape: bool, events: &Sender<Event>) {
    let mut keys = Escape::default();
    let mut buf = vec![0u8; 4096];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let parts = if escape {
            keys.feed(&buf[..n])
        } else {
            vec![Key::Data(buf[..n].to_vec())]
        };
        for k in parts {
            match k {
                Key::Data(d) => {
                    if to.write_all(&d).is_err() {
                        return;
                    }
                }
                Key::Shutdown => {
                    let _ = events.send(Event::Shutdown);
                }
                Key::Kill => {
                    let _ = events.send(Event::Kill);
                }
            }
        }
    }
}
