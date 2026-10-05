//! The host side of T9 without a VM: the boot tests' driver against a fake
//! guest on a socket pair (spec §9.5, §11.5), and its `Session` against a fake
//! VM whose "guest" is a thread connecting to the host's vsock ports.
#![cfg(target_os = "linux")]

mod support;

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kiln_proto::{
    Config, EXIT_INFRA, ExitMethod, Exited, GuestMessage, Hello, HostMessage, Process, Scratch, Signal, Stage, port,
    read_message, write_message,
};
use support::driver::{Event, Reply, SendError, Session, Slot, serve_control};
use vmkit::{Capabilities, EndReason, SnapshotBundle, Vm, VmEnd};

fn config() -> Config {
    Config {
        process: Process {
            cmd: vec!["/bin/true".into()],
            ..Process::default()
        },
        stop_signal: 15,
        tty: None,
        interactive: false,
        hostname: "h".into(),
        network: None,
        layers: 1,
        scratch: Scratch { size_bytes: 1 << 30 },
        exit_method: ExitMethod::Reboot,
        shutdown_grace_secs: 10,
    }
}

/// Runs the driver against `guest` (which writes to its end) and returns its events.
fn drive(guest: impl FnOnce(&mut UnixStream) + Send + 'static) -> Vec<Event> {
    let (host, mut fake) = UnixStream::pair().unwrap();
    let (tx, rx) = channel();
    let g = std::thread::spawn(move || {
        guest(&mut fake);
        fake
    });
    serve_control(host, &config(), &Reply::Config, &tx, &Slot::default());
    drop(g.join().unwrap());
    drop(tx);
    rx.into_iter().collect()
}

fn hello(s: &mut UnixStream) {
    write_message(s, &GuestMessage::Hello(Hello { protocol: 1 })).unwrap();
}

fn violation(events: &[Event]) -> &str {
    match events.last() {
        Some(Event::Violation(why)) => why,
        other => panic!("expected a violation, got {other:?}"),
    }
}

#[test]
fn the_first_hello_gets_config_once() {
    let events = drive(|s| {
        hello(s);
        let reply = read_message::<_, HostMessage>(s).unwrap();
        assert_eq!(reply, Some(HostMessage::Config(Box::new(config()))));
        write_message(s, &GuestMessage::Stage(Stage { n: 3 })).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
    });
    assert!(
        matches!(
            events[..],
            [
                Event::Guest(GuestMessage::Hello(_)),
                Event::Guest(GuestMessage::Stage(_)),
                Event::Closed
            ]
        ),
        "{events:?}"
    );
}

#[test]
fn a_second_hello_is_a_violation() {
    let events = drive(|s| {
        hello(s);
        hello(s);
    });
    assert_eq!(violation(&events), "a second Hello");
}

#[test]
fn messages_before_hello_and_unknown_versions_are_violations() {
    let events = drive(|s| write_message(s, &GuestMessage::Running).unwrap());
    assert_eq!(violation(&events), "a message before Hello");
    let events = drive(|s| write_message(s, &GuestMessage::Hello(Hello { protocol: 2 })).unwrap());
    assert_eq!(violation(&events), "unsupported guest protocol 2");
}

#[test]
fn oversized_frames_and_invalid_json_are_violations() {
    let events = drive(|s| s.write_all(&(70_000u32).to_le_bytes()).unwrap());
    assert!(violation(&events).contains("exceeds"), "{events:?}");
    let events = drive(|s| {
        hello(s);
        s.write_all(&[5, 0, 0, 0, 5, b'{', b'x', b':', b'}']).unwrap();
    });
    assert!(violation(&events).contains("invalid Exited payload"), "{events:?}");
    let events = drive(|s| {
        hello(s);
        s.write_all(&[3, 0, 0, 0, 2, b'{', b'}']).unwrap();
    });
    assert!(violation(&events).contains("not valid in this direction"), "{events:?}");
}

#[test]
fn host_messages_go_out_only_after_config() {
    let (host, mut guest) = UnixStream::pair().unwrap();
    let (tx, rx) = channel();
    let writer = Slot::default();
    let w = writer.clone();
    let server = std::thread::spawn(move || serve_control(host, &config(), &Reply::Config, &tx, &w));
    std::thread::sleep(Duration::from_millis(100));
    assert!(writer.lock().unwrap().is_none(), "no writer before the guest's Hello");
    hello(&mut guest);
    let reply = read_message::<_, HostMessage>(&mut guest).unwrap();
    assert_eq!(reply, Some(HostMessage::Config(Box::new(config()))));
    assert!(matches!(rx.recv().unwrap(), Event::Guest(GuestMessage::Hello(_))));
    // Config is out, so the writer is there (set before the Hello event is sent).
    let msg = HostMessage::Signal(Signal { sig: 1 });
    write_message(writer.lock().unwrap().as_mut().expect("the writer after Config"), &msg).unwrap();
    assert_eq!(read_message::<_, HostMessage>(&mut guest).unwrap(), Some(msg));
    drop(guest);
    server.join().unwrap();
}

/// A VM without KVM: `start` runs the guest on a thread, which reaches the host's
/// ports as a real guest's vsock does (`<socket>_<port>`). The VM ends when the
/// guest returns (or at once, with `ends_at_start`), or when it is killed.
struct FakeVm {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    guest: Option<Guest>,
    thread: Option<JoinHandle<()>>,
    killed: Arc<AtomicBool>,
    ends_at_start: bool,
}

/// The fake guest: the vsock socket's path and the VM's kill flag.
type Guest = Box<dyn FnOnce(PathBuf, Arc<AtomicBool>) + Send>;

impl FakeVm {
    fn boxed(ends_at_start: bool, guest: impl FnOnce(PathBuf, Arc<AtomicBool>) + Send + 'static) -> Box<dyn Vm> {
        let dir = tempfile::tempdir().unwrap();
        Box::new(FakeVm {
            socket: dir.path().join("vsock.sock"),
            _dir: dir,
            guest: Some(Box::new(guest)),
            thread: None,
            killed: Arc::new(AtomicBool::new(false)),
            ends_at_start,
        })
    }
}

impl Vm for FakeVm {
    fn start(&mut self) -> vmkit::Result<()> {
        let (guest, socket, killed) = (self.guest.take().unwrap(), self.socket.clone(), self.killed.clone());
        self.thread = Some(std::thread::spawn(move || guest(socket, killed)));
        Ok(())
    }

    fn pause(&mut self) -> vmkit::Result<()> {
        Ok(())
    }

    fn resume(&mut self) -> vmkit::Result<()> {
        Ok(())
    }

    fn kill(&mut self) -> vmkit::Result<()> {
        self.killed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn wait(&mut self) -> vmkit::Result<VmEnd> {
        loop {
            if let Some(end) = self.wait_timeout(Duration::from_secs(1))? {
                return Ok(end);
            }
        }
    }

    fn wait_timeout(&mut self, timeout: Duration) -> vmkit::Result<Option<VmEnd>> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.killed.load(Ordering::SeqCst) {
                return Ok(Some(VmEnd {
                    reason: EndReason::Killed,
                    code: None,
                    signal: Some(9),
                }));
            }
            if self.ends_at_start || self.thread.as_ref().is_some_and(|t| t.is_finished()) {
                return Ok(Some(VmEnd {
                    reason: EndReason::Exited,
                    code: Some(0),
                    signal: None,
                }));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn snapshot(&mut self, _dest: &Path) -> vmkit::Result<SnapshotBundle> {
        Err(vmkit::Error::Unsupported("snapshots of the fake VM"))
    }

    fn capabilities(&self) -> Capabilities {
        unreachable!("the driver does not ask")
    }

    fn vsock_socket(&self) -> Option<&Path> {
        Some(&self.socket)
    }
}

/// Connects the fake guest to the host's control port.
fn connect_control(socket: &Path) -> UnixStream {
    UnixStream::connect(format!("{}_{}", socket.display(), port::CONTROL)).expect("connect to the control port")
}

/// Sends `Hello` and checks that `Config` comes back.
fn handshake(control: &mut UnixStream) {
    hello(control);
    let reply = read_message::<_, HostMessage>(control).unwrap();
    assert!(matches!(reply, Some(HostMessage::Config(_))), "{reply:?}");
}

fn wait_killed(killed: &AtomicBool) {
    while !killed.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn sending_before_config_or_after_the_end_is_an_error_not_a_panic() {
    let (go, wait_go) = channel::<()>();
    let (got_tx, got) = channel();
    let closed = Arc::new(Mutex::new(false));
    let closed_flag = closed.clone();
    let vm = FakeVm::boxed(false, move |socket, killed| {
        let mut control = connect_control(&socket);
        wait_go.recv().unwrap();
        handshake(&mut control);
        write_message(&mut control, &GuestMessage::Running).unwrap();
        got_tx
            .send(read_message::<_, HostMessage>(&mut control).unwrap())
            .unwrap();
        drop(control);
        *closed_flag.lock().unwrap() = true;
        wait_killed(&killed);
    });
    let mut s = Session::start(vm, config(), Reply::Config, Vec::new());
    std::thread::sleep(Duration::from_millis(100));
    let signal = HostMessage::Signal(Signal { sig: 1 });
    assert!(matches!(s.send(&signal), Err(SendError::NotConfigured)));
    assert!(matches!(s.send_raw(b"x"), Err(SendError::NotConfigured)));
    assert!(s.tty_write(b"x").is_err(), "no tty in this config");
    go.send(()).unwrap();
    assert!(s.wait_running(Duration::from_secs(10)));
    s.send(&signal).unwrap();
    assert_eq!(got.recv_timeout(Duration::from_secs(10)).unwrap(), Some(signal.clone()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !*closed.lock().unwrap() {
        assert!(Instant::now() < deadline, "the guest did not close");
        std::thread::sleep(Duration::from_millis(5));
    }
    // The guest is gone: an error, not a panic.
    assert!(matches!(s.send(&signal), Err(SendError::Proto(_))));
    let o = s.finish(Duration::from_millis(200));
    assert_eq!(o.violation.as_deref(), Some("timeout"));
}

#[test]
fn a_guest_not_running_within_the_boot_timeout_is_killed() {
    let vm = FakeVm::boxed(false, |socket, killed| {
        let mut control = connect_control(&socket);
        handshake(&mut control);
        wait_killed(&killed);
    });
    let mut s = Session::start(vm, config(), Reply::Config, Vec::new());
    assert!(!s.wait_running(Duration::from_millis(500)));
    let o = s.finish(Duration::from_secs(10));
    assert!(
        o.violation.as_deref().unwrap_or_default().starts_with("boot timeout"),
        "{o:?}"
    );
    assert_eq!(o.end.reason, EndReason::Killed);
    assert_eq!(o.exit_code(), EXIT_INFRA);
}

#[test]
fn messages_read_after_the_vm_ended_are_kept() {
    // The VM ends at once; the guest's messages arrive after finish's short drain,
    // while it waits for the control thread to see the connection close.
    let vm = FakeVm::boxed(true, |socket, _killed| {
        let mut control = connect_control(&socket);
        std::thread::sleep(Duration::from_millis(600));
        handshake(&mut control);
        write_message(&mut control, &GuestMessage::Running).unwrap();
        let exited = Exited {
            signaled: false,
            code: 3,
        };
        write_message(&mut control, &GuestMessage::Exited(exited)).unwrap();
    });
    let s = Session::start(vm, config(), Reply::Config, Vec::new());
    let o = s.finish(Duration::from_secs(10));
    assert_eq!(o.violation, None, "{o:?}");
    assert_eq!(o.end.reason, EndReason::Exited);
    assert!(o.running(), "{o:?}");
    assert_eq!(o.exit_code(), 3, "{o:?}");
}
