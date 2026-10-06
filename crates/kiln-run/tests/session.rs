//! The host's side of T9 without KVM (spec §9.5, §11.5): the production session
//! against a fake VM whose "guest" is a thread on the host's vsock ports. Covers
//! the M3a carry-over D-1 to D-6.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kiln_proto::{
    Config, EXIT_INFRA, ExitMethod, Exited, GuestMessage, Hello, HostMessage, InitFailed, Process, Scratch, Shutdown,
    Stage, WindowSize, port, read_message, write_message,
};
use kiln_run::{EXIT_KILLED, Session, SessionOptions, Streams};
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

/// A sink the test can read while the session writes it.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

struct Io {
    out: Shared,
    err: Shared,
}

fn streams(stdin: Option<&'static [u8]>) -> (Streams, Io) {
    let (out, err) = (Shared::default(), Shared::default());
    let s = Streams {
        stdin: stdin.map(|b| Box::new(b) as Box<dyn Read + Send>),
        stdout: Box::new(out.clone()),
        stderr: Box::new(err.clone()),
    };
    (s, Io { out, err })
}

fn opts() -> SessionOptions {
    SessionOptions {
        boot_timeout: Duration::from_secs(10),
        end_timeout: Duration::from_secs(5),
        drain_timeout: Duration::from_secs(2),
        ..SessionOptions::default()
    }
}

/// A VM without KVM: `start` runs the guest on a thread that reaches the host's
/// ports as a real guest's vsock does (`<socket>_<port>`). The VM ends when the
/// guest returns, or when it is killed.
struct FakeVm {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    guest: Option<Guest>,
    thread: Option<JoinHandle<()>>,
    killed: Arc<AtomicBool>,
    /// A VMM that survives its kill (stuck in the kernel, say).
    unkillable: bool,
}

type Guest = Box<dyn FnOnce(PathBuf, Arc<AtomicBool>) + Send>;

fn fake(guest: impl FnOnce(PathBuf, Arc<AtomicBool>) + Send + 'static) -> Box<dyn Vm> {
    fake_vm(guest, false)
}

fn fake_vm(guest: impl FnOnce(PathBuf, Arc<AtomicBool>) + Send + 'static, unkillable: bool) -> Box<dyn Vm> {
    let dir = tempfile::tempdir().unwrap();
    Box::new(FakeVm {
        socket: dir.path().join("vsock.sock"),
        _dir: dir,
        guest: Some(Box::new(guest)),
        thread: None,
        killed: Arc::new(AtomicBool::new(false)),
        unkillable,
    })
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
        if !self.unkillable {
            self.killed.store(true, Ordering::SeqCst);
        }
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
            if self.thread.as_ref().is_some_and(|t| t.is_finished()) {
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
        unreachable!("the session does not ask")
    }

    fn vsock_socket(&self) -> Option<&Path> {
        Some(&self.socket)
    }
}

fn connect(socket: &Path, p: u32) -> UnixStream {
    let path = format!("{}_{p}", socket.display());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(&path) {
            Ok(s) => return s,
            Err(e) => {
                assert!(Instant::now() < deadline, "connect {path}: {e}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn send(s: &mut UnixStream, m: GuestMessage) {
    let _ = write_message(s, &m);
}

/// Hello, then Config back.
fn handshake(c: &mut UnixStream) {
    send(c, GuestMessage::Hello(Hello { protocol: 1 }));
    let reply = read_message::<_, HostMessage>(c).unwrap();
    assert!(matches!(reply, Some(HostMessage::Config(_))), "{reply:?}");
}

/// Stages 3 to 6, Running, Stage 7.
fn boot(c: &mut UnixStream) {
    for n in 3..=6 {
        send(c, GuestMessage::Stage(Stage { n }));
    }
    send(c, GuestMessage::Running);
    send(c, GuestMessage::Stage(Stage { n: 7 }));
}

fn exited(c: &mut UnixStream, code: i32) {
    send(c, GuestMessage::Exited(Exited { signaled: false, code }));
}

fn wait_killed(killed: &AtomicBool) {
    while !killed.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }
}

const LIMIT: Option<Duration> = Some(Duration::from_secs(30));

#[test]
fn a_normal_run_streams_output_and_reports_the_exit_code() {
    let big: Vec<u8> = (0..8u32 << 20)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let want = big.clone();
    let vm = fake(move |sock, _| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        let (mut out, mut err) = (connect(&sock, port::STDOUT), connect(&sock, port::STDERR));
        out.write_all(&big).unwrap();
        err.write_all(b"done\n").unwrap();
        drop((out, err));
        exited(&mut c, 3);
    });
    let (st, io) = streams(None);
    let o = Session::start(vm, config(), st, opts()).unwrap().finish_within(LIMIT);
    assert_eq!(o.violation, None, "{o:?}");
    assert_eq!((o.exit_code, o.stages.as_slice()), (3, &[3, 4, 5, 6, 7][..]));
    assert!(o.running && o.hello && o.running_after.is_some());
    assert!(
        io.out.bytes() == want,
        "stdout differs ({} bytes)",
        io.out.bytes().len()
    );
    assert_eq!(io.err.bytes(), b"done\n");
}

/// `kiln run img | head -1`: a sink that fails is dropped, but the guest's output
/// is still read to EOF, so the guest never blocks and its exit code is kept.
#[test]
fn a_failing_sink_does_not_block_the_guest() {
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let vm = fake(|sock, _| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        let (mut out, err) = (connect(&sock, port::STDOUT), connect(&sock, port::STDERR));
        out.write_all(&vec![b'x'; 8 << 20]).unwrap();
        drop((out, err));
        exited(&mut c, 3);
    });
    let (mut st, _) = streams(None);
    st.stdout = Box::new(Closed);
    let o = Session::start(vm, config(), st, opts()).unwrap().finish_within(LIMIT);
    assert_eq!((o.exit_code, o.violation.as_deref()), (3, None), "{o:?}");
}

#[test]
fn stdin_is_relayed_with_eof() {
    let (tx, rx) = std::sync::mpsc::channel();
    let vm = fake(move |sock, _| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        let mut input = Vec::new();
        connect(&sock, port::STDIN).read_to_end(&mut input).unwrap();
        tx.send(input).unwrap();
        drop((connect(&sock, port::STDOUT), connect(&sock, port::STDERR)));
        exited(&mut c, 0);
    });
    let mut cfg = config();
    cfg.interactive = true;
    let (st, _io) = streams(Some(b"hi\n\x00binary"));
    let o = Session::start(vm, cfg, st, opts()).unwrap().finish_within(LIMIT);
    assert_eq!(o.exit_code, 0, "{o:?}");
    assert_eq!(rx.recv().unwrap(), b"hi\n\x00binary");
}

/// D-1: a guest cannot flood the host with valid messages.
#[test]
fn a_stage_flood_and_messages_after_the_end_are_violations() {
    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        for _ in 0..100_000 {
            if killed.load(Ordering::SeqCst) {
                return;
            }
            send(&mut c, GuestMessage::Stage(Stage { n: 3 }));
        }
        wait_killed(&killed);
    });
    let (st, _) = streams(None);
    let o = Session::start(vm, config(), st, opts()).unwrap().finish_within(LIMIT);
    assert_eq!(o.violation.as_deref(), Some("Stage 3 after stage 3"), "{o:?}");
    assert_eq!((o.exit_code, o.end.reason), (EXIT_INFRA, EndReason::Killed));

    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        exited(&mut c, 0);
        exited(&mut c, 1);
        wait_killed(&killed);
    });
    let (st, _) = streams(None);
    let o = Session::start(vm, config(), st, opts()).unwrap().finish_within(LIMIT);
    assert_eq!(o.violation.as_deref(), Some("Exited after the guest's final message"));
    assert_eq!(o.exit_code, EXIT_INFRA);
}

/// D-5: a second connection on any served port.
#[test]
fn a_second_connection_on_any_port_is_a_violation() {
    for p in [port::CONTROL, port::STDOUT] {
        let vm = fake(move |sock, killed| {
            let mut c = connect(&sock, port::CONTROL);
            handshake(&mut c);
            let _first = connect(&sock, p);
            let _second = connect(&sock, p);
            wait_killed(&killed);
            drop(c);
        });
        let (st, _) = streams(None);
        let o = Session::start(vm, config(), st, opts()).unwrap().finish_within(LIMIT);
        assert_eq!(
            o.violation.as_deref(),
            Some(format!("a second connection on vsock port {p}").as_str()),
            "{o:?}"
        );
    }
}

/// D-2: a guest that stops reading cannot block the host, and the kill still works.
#[test]
fn a_guest_that_does_not_read_its_control_channel_is_killed() {
    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        wait_killed(&killed);
        drop(c);
    });
    let (st, _) = streams(None);
    let mut s = Session::start(vm, config(), st, opts()).unwrap();
    assert!(s.wait_running());
    let h = s.handle();
    let started = Instant::now();
    for _ in 0..200_000 {
        h.signal(1);
    }
    assert!(started.elapsed() < Duration::from_secs(5), "sending blocked");
    let o = s.finish_within(LIMIT);
    assert_eq!(
        o.violation.as_deref(),
        Some("the guest is not reading its control channel"),
        "{o:?}"
    );
    assert_eq!((o.exit_code, o.end.reason), (EXIT_INFRA, EndReason::Killed));
}

/// D-3: the VM must end soon after the guest's final message, or the control closing.
#[test]
fn a_vm_that_lingers_after_its_final_message_is_killed() {
    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        drop((connect(&sock, port::STDOUT), connect(&sock, port::STDERR)));
        exited(&mut c, 4);
        wait_killed(&killed);
    });
    let (st, _) = streams(None);
    let mut o = opts();
    o.end_timeout = Duration::from_millis(300);
    let started = Instant::now();
    let out = Session::start(vm, config(), st, o).unwrap().finish_within(LIMIT);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!((out.exit_code, out.violation.as_deref()), (4, None), "{out:?}");
    assert_eq!(out.end.reason, EndReason::Killed);
    assert!(out.warnings[0].contains("did not end"), "{:?}", out.warnings);

    // The control connection closing without a final message: 125, bounded by the
    // end timeout, not the boot timeout (10 s here), and not a violation.
    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        drop(c);
        wait_killed(&killed);
    });
    let (st, _) = streams(None);
    let mut o = opts();
    o.end_timeout = Duration::from_millis(300);
    let started = Instant::now();
    let out = Session::start(vm, config(), st, o).unwrap().finish_within(LIMIT);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!((out.exit_code, out.violation.as_deref()), (EXIT_INFRA, None), "{out:?}");
    assert_eq!(out.end.reason, EndReason::Killed);
    assert_eq!(
        out.warnings,
        ["the control connection closed, and the VM did not end within 0.3s; it was killed"]
    );
}

/// D-3: `Exited` itself never kills the VM: a guest that powers off within the
/// end bound ends on its own.
#[test]
fn a_guest_ending_its_vm_within_the_bound_is_not_killed() {
    let vm = fake(|sock, _| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        drop((connect(&sock, port::STDOUT), connect(&sock, port::STDERR)));
        exited(&mut c, 5);
        // Syncing the scratch disk.
        std::thread::sleep(Duration::from_millis(500));
    });
    let (st, _) = streams(None);
    let out = Session::start(vm, config(), st, opts()).unwrap().finish_within(LIMIT);
    assert_eq!((out.exit_code, out.violation.as_deref()), (5, None), "{out:?}");
    assert_eq!(out.end.reason, EndReason::Exited, "{out:?}");
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
}

/// A stop request ends with the guest's final message: init's grace kill makes
/// the app exit, the guest reports it and then syncs for longer than the host's
/// margin. The host does not kill it at grace + margin or warn that it did not
/// stop; only the end bound applies.
#[test]
fn a_stop_request_ends_at_the_guests_final_message() {
    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        drop((connect(&sock, port::STDOUT), connect(&sock, port::STDERR)));
        let shutdown = read_message::<_, HostMessage>(&mut c).unwrap();
        assert!(matches!(shutdown, Some(HostMessage::Shutdown(_))), "{shutdown:?}");
        // The app ignores its stop signal; init kills it when the grace ends.
        std::thread::sleep(Duration::from_secs(1));
        send(
            &mut c,
            GuestMessage::Exited(Exited {
                signaled: true,
                code: 9,
            }),
        );
        wait_killed(&killed);
    });
    let (st, _) = streams(None);
    let mut o = opts();
    (o.stop_timeout, o.stop_margin) = (1, Duration::from_secs(1));
    o.end_timeout = Duration::from_secs(3);
    let mut s = Session::start(vm, config(), st, o).unwrap();
    assert!(s.wait_running());
    s.handle().terminate();
    let started = Instant::now();
    let out = s.finish_within(LIMIT);
    assert!(started.elapsed() >= Duration::from_secs(3), "{:?}", started.elapsed());
    assert_eq!((out.exit_code, out.violation.as_deref()), (137, None), "{out:?}");
    assert_eq!(out.end.reason, EndReason::Killed);
    assert_eq!(
        out.warnings,
        ["the VM did not end within 3.0s of the guest's final message; it was killed"]
    );
}

#[test]
fn a_guest_not_running_within_the_boot_timeout_is_killed() {
    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        wait_killed(&killed);
    });
    let (st, _) = streams(None);
    let mut o = opts();
    o.boot_timeout = Duration::from_millis(400);
    let mut s = Session::start(vm, config(), st, o).unwrap();
    assert!(!s.wait_running());
    let out = s.finish_within(LIMIT);
    assert!(out.violation.as_deref().unwrap().starts_with("boot timeout"), "{out:?}");
    assert_eq!(out.exit_code, EXIT_INFRA);
}

/// First interrupt: `Shutdown` with the stop timeout; second: the VM is killed (137).
#[test]
fn interrupts_ask_then_kill() {
    let (tx, rx) = std::sync::mpsc::channel();
    let vm = fake(move |sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        tx.send(read_message::<_, HostMessage>(&mut c).unwrap()).unwrap();
        wait_killed(&killed);
    });
    let (st, _) = streams(None);
    let mut o = opts();
    o.stop_timeout = 7;
    let mut s = Session::start(vm, config(), st, o).unwrap();
    assert!(s.wait_running());
    s.handle().interrupt();
    let until = Instant::now() + Duration::from_secs(10);
    let got = loop {
        s.pump(Duration::from_millis(50), |_| false);
        if let Ok(m) = rx.try_recv() {
            break m;
        }
        assert!(Instant::now() < until, "no Shutdown reached the guest");
    };
    assert_eq!(got, Some(HostMessage::Shutdown(Shutdown { grace_secs: 7 })));
    s.handle().interrupt();
    let out = s.finish_within(LIMIT);
    assert_eq!(
        (out.exit_code, out.killed, out.violation.as_deref()),
        (EXIT_KILLED, true, None)
    );
}

/// `Ctrl-]` `k` is the user's kill: 137, as `docker kill`, not 125.
#[test]
fn the_kill_key_kills_the_vm_with_137() {
    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        let tty = connect(&sock, port::TTY);
        wait_killed(&killed);
        drop((tty, c));
    });
    let mut cfg = config();
    cfg.tty = Some(WindowSize { rows: 24, cols: 80 });
    cfg.interactive = true;
    let (st, _) = streams(Some(b"\x1dk"));
    let mut o = opts();
    o.escape_keys = true;
    let out = Session::start(vm, cfg, st, o).unwrap().finish_within(LIMIT);
    assert_eq!(
        (out.exit_code, out.killed, out.violation.as_deref()),
        (EXIT_KILLED, true, None),
        "{out:?}"
    );
}

/// A guest that ignores `Shutdown` is killed after the grace period and a margin.
#[test]
fn a_guest_that_ignores_shutdown_is_killed_after_the_grace() {
    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        wait_killed(&killed);
        drop(c);
    });
    let (st, _) = streams(None);
    let mut o = opts();
    (o.stop_timeout, o.stop_margin) = (0, Duration::from_millis(300));
    let mut s = Session::start(vm, config(), st, o).unwrap();
    assert!(s.wait_running());
    s.handle().terminate();
    let out = s.finish_within(LIMIT);
    assert_eq!(out.exit_code, EXIT_INFRA, "{out:?}");
    assert!(out.warnings[0].contains("did not stop"), "{:?}", out.warnings);
}

/// Before `Hello` nothing can stop gracefully: an interrupt kills.
#[test]
fn an_interrupt_before_hello_kills() {
    let vm = fake(|_, killed| wait_killed(&killed));
    let (st, _) = streams(None);
    let s = Session::start(vm, config(), st, opts()).unwrap();
    s.handle().interrupt();
    let out = s.finish_within(LIMIT);
    assert_eq!((out.exit_code, out.killed), (EXIT_KILLED, true));
}

/// Host messages queued before `Config` go out after it, in order.
#[test]
fn messages_queued_before_config_follow_it() {
    let (tx, rx) = std::sync::mpsc::channel();
    let (go, wait_go) = std::sync::mpsc::channel::<()>();
    let vm = fake(move |sock, _| {
        let mut c = connect(&sock, port::CONTROL);
        wait_go.recv().unwrap();
        handshake(&mut c);
        tx.send(read_message::<_, HostMessage>(&mut c).unwrap()).unwrap();
        boot(&mut c);
        drop((connect(&sock, port::STDOUT), connect(&sock, port::STDERR)));
        exited(&mut c, 0);
    });
    let (st, _) = streams(None);
    let s = Session::start(vm, config(), st, opts()).unwrap();
    s.handle().window_size(WindowSize { rows: 1, cols: 2 });
    go.send(()).unwrap();
    let out = s.finish_within(LIMIT);
    assert_eq!(out.exit_code, 0);
    assert_eq!(
        rx.recv().unwrap(),
        Some(HostMessage::WindowSize(WindowSize { rows: 1, cols: 2 }))
    );
}

/// `-t`: the terminal's input goes to port 1028, `Ctrl-]` `q` becomes `Shutdown`.
#[test]
fn the_tty_relays_input_and_escape_keys() {
    let (tx, rx) = std::sync::mpsc::channel();
    let vm = fake(move |sock, _| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        let mut tty = connect(&sock, port::TTY);
        let mut got = [0u8; 2];
        tty.read_exact(&mut got).unwrap();
        let msg = read_message::<_, HostMessage>(&mut c).unwrap();
        tty.write_all(b"bye\r\n").unwrap();
        drop(tty);
        tx.send((got, msg)).unwrap();
        exited(&mut c, 0);
    });
    let mut cfg = config();
    cfg.tty = Some(WindowSize { rows: 24, cols: 80 });
    cfg.interactive = true;
    let (st, io) = streams(Some(b"ab\x1dq"));
    let mut o = opts();
    o.escape_keys = true;
    let out = Session::start(vm, cfg, st, o).unwrap().finish_within(LIMIT);
    assert_eq!(out.exit_code, 0, "{out:?}");
    let (got, msg) = rx.recv().unwrap();
    assert_eq!(&got, b"ab");
    assert!(matches!(msg, Some(HostMessage::Shutdown(_))), "{msg:?}");
    assert_eq!(io.out.bytes(), b"bye\r\n");
}

/// The guest's last messages may arrive just as its VM ends: they still count.
#[test]
fn messages_read_as_the_vm_ends_are_kept() {
    let vm = fake(|sock, _| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        exited(&mut c, 3);
    });
    let (st, _) = streams(None);
    let o = Session::start(vm, config(), st, opts()).unwrap().finish_within(LIMIT);
    assert_eq!((o.exit_code, o.violation.as_deref()), (3, None), "{o:?}");
}

/// Guest protocol errors: framing, payloads and versions.
#[test]
fn bad_frames_are_violations() {
    const OVERSIZE: [u8; 4] = 70_000u32.to_le_bytes();
    let cases: [(&[u8], &str); 3] = [
        (&OVERSIZE, "exceeds"),
        (&[5, 0, 0, 0, 5, b'{', b'x', b':', b'}'], "invalid"),
        (&[3, 0, 0, 0, 2, b'{', b'}'], "not valid in this direction"),
    ];
    for (raw, needle) in cases {
        let vm = fake(move |sock, killed| {
            let mut c = connect(&sock, port::CONTROL);
            if needle != "exceeds" {
                handshake(&mut c);
            }
            c.write_all(raw).unwrap();
            wait_killed(&killed);
        });
        let (st, _) = streams(None);
        let o = Session::start(vm, config(), st, opts()).unwrap().finish_within(LIMIT);
        assert!(
            o.violation.as_deref().unwrap_or_default().contains(needle),
            "{needle}: {o:?}"
        );
    }
    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        send(&mut c, GuestMessage::Hello(Hello { protocol: 2 }));
        wait_killed(&killed);
    });
    let (st, _) = streams(None);
    let o = Session::start(vm, config(), st, opts()).unwrap().finish_within(LIMIT);
    assert_eq!(o.violation.as_deref(), Some("unsupported guest protocol 2"));
}

/// D-6: output still flowing when the VM ends reaches a slow sink in full: the
/// drain bound restarts whenever bytes move.
#[test]
fn a_slow_sink_gets_all_of_the_output() {
    /// Takes 64 KiB a second.
    #[derive(Clone, Default)]
    struct Slow(Shared);
    impl Write for Slow {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let n = b.len().min(4096);
            std::thread::sleep(Duration::from_millis(62));
            self.0.write(&b[..n])
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let data: Vec<u8> = (0..512u32 << 10).map(|i| (i % 251) as u8).collect();
    let want = data.clone();
    let sink = Slow::default();
    let arrived = sink.0.clone();
    let vm = fake(move |sock, _| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        let (mut out, err) = (connect(&sock, port::STDOUT), connect(&sock, port::STDERR));
        boot(&mut c);
        // The output is still in flight when the VM ends.
        std::thread::spawn(move || {
            let _ = out.write_all(&data);
        });
        drop(err);
        // Once the host is streaming it (as kiln-init's stdout is, long before its exit).
        let until = Instant::now() + Duration::from_secs(10);
        while arrived.bytes().is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        exited(&mut c, 0);
    });
    let (mut st, _) = streams(None);
    st.stdout = Box::new(sink.clone());
    let mut o = opts();
    o.drain_timeout = Duration::from_secs(1);
    let out = Session::start(vm, config(), st, o).unwrap().finish_within(LIMIT);
    assert_eq!((out.exit_code, out.violation.as_deref()), (0, None), "{out:?}");
    let got = sink.0.bytes();
    assert!(got == want, "{} of {} bytes arrived", got.len(), want.len());
}

/// `Ctrl-]` `k` works while the guest is not reading its terminal: the keys act
/// before the bytes are written, and a stalled write gives up after the send
/// timeout instead of holding up the input.
#[test]
fn escape_keys_work_while_the_guest_does_not_read_its_terminal() {
    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        let tty = connect(&sock, port::TTY);
        wait_killed(&killed);
        drop((tty, c));
    });
    let mut cfg = config();
    cfg.tty = Some(WindowSize { rows: 24, cols: 80 });
    cfg.interactive = true;
    let mut input = vec![b'a'; 4 << 20];
    input.extend_from_slice(b"\x1dk");
    let input: &'static [u8] = input.leak();
    let (st, _) = streams(Some(input));
    let mut o = opts();
    o.escape_keys = true;
    let out = Session::start(vm, cfg, st, o).unwrap().finish_within(LIMIT);
    assert_eq!(
        (out.exit_code, out.killed, out.violation.as_deref()),
        (EXIT_KILLED, true, None),
        "{out:?}"
    );
}

/// D-6: a VMM that survives its kill does not hold kiln forever: after the kill
/// bound the run ends, with a warning.
#[test]
fn a_kill_that_does_not_take_is_bounded() {
    let vm = fake_vm(
        |sock, _| {
            let mut c = connect(&sock, port::CONTROL);
            handshake(&mut c);
            boot(&mut c);
            std::thread::sleep(Duration::from_secs(20));
        },
        true,
    );
    let (st, _) = streams(None);
    let mut o = opts();
    o.kill_wait = Duration::from_millis(300);
    let mut s = Session::start(vm, config(), st, o).unwrap();
    assert!(s.wait_running());
    s.handle().kill();
    let started = Instant::now();
    let out = s.finish_within(LIMIT);
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert_eq!(
        (out.exit_code, out.killed, out.violation.as_deref()),
        (EXIT_KILLED, true, None)
    );
    assert_eq!(out.end.reason, EndReason::Killed);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("did not exit within 0.3s of being killed")),
        "{:?}",
        out.warnings
    );
}

/// D-2: resizes are dropped, never a violation, when the queue is full.
#[test]
fn window_sizes_are_dropped_when_the_queue_is_full() {
    let (go, wait_go) = std::sync::mpsc::channel::<()>();
    let (tx, rx) = std::sync::mpsc::channel();
    let vm = fake(move |sock, _| {
        let mut c = connect(&sock, port::CONTROL);
        wait_go.recv().unwrap();
        handshake(&mut c);
        // What was queued, read until the host has nothing more to send.
        c.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut sizes = 0;
        while let Ok(Some(HostMessage::WindowSize(_))) = read_message::<_, HostMessage>(&mut c) {
            sizes += 1;
        }
        tx.send(sizes).unwrap();
        boot(&mut c);
        drop((connect(&sock, port::STDOUT), connect(&sock, port::STDERR)));
        exited(&mut c, 0);
    });
    let (st, _) = streams(None);
    let s = Session::start(vm, config(), st, opts()).unwrap();
    // Before Config nothing is sent: the queue fills.
    for i in 0..10_000u16 {
        s.handle().window_size(WindowSize { rows: i, cols: 80 });
    }
    go.send(()).unwrap();
    let out = s.finish_within(LIMIT);
    assert_eq!((out.exit_code, out.violation.as_deref()), (0, None), "{out:?}");
    let sizes = rx.recv().unwrap();
    assert!((1..=64).contains(&sizes), "{sizes} resizes arrived");
}

/// A user's kill after the app exited keeps the app's exit code (spec §9.5).
#[test]
fn a_user_kill_after_exited_keeps_the_apps_code() {
    let (told, exited_sent) = std::sync::mpsc::channel::<()>();
    let vm = fake(move |sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        drop((connect(&sock, port::STDOUT), connect(&sock, port::STDERR)));
        exited(&mut c, 3);
        told.send(()).unwrap();
        wait_killed(&killed);
    });
    let (st, _) = streams(None);
    let mut s = Session::start(vm, config(), st, opts()).unwrap();
    exited_sent.recv_timeout(Duration::from_secs(10)).unwrap();
    s.pump(Duration::from_millis(300), |_| false);
    s.handle().kill();
    let out = s.finish_within(LIMIT);
    assert_eq!(
        (out.exit_code, out.killed, out.violation.as_deref()),
        (3, true, None),
        "{out:?}"
    );
    assert_eq!(out.end.reason, EndReason::Killed);
}

/// An exec failure at stage 6 (ENOENT) is 127, and the end bound follows it.
#[test]
fn an_entrypoint_not_found_is_127_and_bounded() {
    let vm = fake(|sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        for n in 3..=6 {
            send(&mut c, GuestMessage::Stage(Stage { n }));
        }
        send(
            &mut c,
            GuestMessage::InitFailed(InitFailed::new(6, Some(2), "exec /nope: No such file or directory")),
        );
        wait_killed(&killed);
    });
    let (st, _) = streams(None);
    let mut o = opts();
    o.end_timeout = Duration::from_millis(300);
    let out = Session::start(vm, config(), st, o).unwrap().finish_within(LIMIT);
    assert_eq!((out.exit_code, out.violation.as_deref()), (127, None), "{out:?}");
    assert!(!out.running);
    assert_eq!(out.end.reason, EndReason::Killed);
    assert!(
        out.warnings.iter().any(|w| w.contains("did not end within")),
        "{:?}",
        out.warnings
    );
}

/// A session dropped before its end (a failure in kiln after the start) kills its VM.
#[test]
fn dropping_an_unfinished_session_kills_its_vm() {
    let (tx, rx) = std::sync::mpsc::channel();
    let vm = fake(move |sock, killed| {
        let mut c = connect(&sock, port::CONTROL);
        handshake(&mut c);
        boot(&mut c);
        let until = Instant::now() + Duration::from_secs(20);
        while !killed.load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = tx.send(killed.load(Ordering::SeqCst));
    });
    let (st, _) = streams(None);
    let mut s = Session::start(vm, config(), st, opts()).unwrap();
    assert!(s.wait_running());
    drop(s);
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true));
}

/// The main loop knows of `Hello` before the guest gets `Config`: a `Ctrl-]` `q`
/// typed the moment the guest's terminal connects is a stop request, never a kill
/// "before Hello". (Racy before the fix; repeated to make a loss likely.)
#[test]
fn an_escape_key_right_after_config_is_a_stop_request() {
    for _ in 0..50 {
        let vm = fake(|sock, _| {
            let mut c = connect(&sock, port::CONTROL);
            handshake(&mut c);
            let tty = connect(&sock, port::TTY);
            c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let msg = read_message::<_, HostMessage>(&mut c).unwrap();
            assert!(matches!(msg, Some(HostMessage::Shutdown(_))), "{msg:?}");
            boot(&mut c);
            drop(tty);
            send(
                &mut c,
                GuestMessage::Exited(Exited {
                    signaled: true,
                    code: 15,
                }),
            );
        });
        let mut cfg = config();
        cfg.tty = Some(WindowSize { rows: 24, cols: 80 });
        cfg.interactive = true;
        let (st, _) = streams(Some(b"\x1dq"));
        let mut o = opts();
        o.escape_keys = true;
        let out = Session::start(vm, cfg, st, o).unwrap().finish_within(LIMIT);
        assert_eq!(
            (out.exit_code, out.killed, out.violation.as_deref()),
            (143, false, None),
            "{out:?}"
        );
    }
}
