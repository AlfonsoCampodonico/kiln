//! `kiln run` end to end (spec §9.7, §11.4): the CLI booting busybox images on
//! Linux with KVM. Needs what the boot suite needs (`scripts/boot-tests.sh` runs
//! both): KILN_TEST_KERNEL, KILN_TEST_INIT, KILN_TEST_HOSTILE, a static busybox
//! (KILN_TEST_BUSYBOX, default /bin/busybox), the VMMs and VMKIT_SANDBOX.
//! KILN_TEST_VMM picks the VMM (default firecracker). Without the kernel and init
//! every test is skipped, unless KILN_REQUIRE_KVM_TESTS=1. KILN_TEST_KEEP=1 keeps
//! every run directory (KILN_KEEP_RUN_DIR=1). Every kiln a test starts is bounded:
//! killed when it overruns (the failure shows its guest's console) or when the
//! test fails first.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{ContainerConfig, Platform};

struct Env {
    kernel: PathBuf,
    init: PathBuf,
    hostile: Option<PathBuf>,
    busybox: PathBuf,
    vmm: String,
}

fn env() -> Option<Env> {
    let get = |k: &str| std::env::var_os(k).map(PathBuf::from);
    match (get("KILN_TEST_KERNEL"), get("KILN_TEST_INIT")) {
        (Some(kernel), Some(init)) => Some(Env {
            kernel,
            init,
            hostile: get("KILN_TEST_HOSTILE"),
            busybox: get("KILN_TEST_BUSYBOX").unwrap_or_else(|| "/bin/busybox".into()),
            vmm: std::env::var("KILN_TEST_VMM").unwrap_or_else(|_| "firecracker".into()),
        }),
        _ => {
            assert!(
                std::env::var_os("KILN_REQUIRE_KVM_TESTS").is_none_or(|v| v != "1"),
                "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_KERNEL/KILN_TEST_INIT are unset"
            );
            None
        }
    }
}

const APPLETS: [&str; 19] = [
    "cat", "dd", "echo", "env", "grep", "hostname", "httpd", "id", "kill", "ls", "nc", "nslookup", "sh", "sleep",
    "stty", "test", "true", "tty", "wc",
];

/// busybox, its applets, and an extra file `marker` with `content`.
fn busybox_layer(busybox: &Path, content: &str) -> Vec<u8> {
    let dir = Opts::default().mode(0o755);
    let mut t = TarBuilder::new();
    t.dir("bin", &dir)
        .dir("etc", &dir)
        .dir("tmp", &Opts::default().mode(0o1777))
        .file("bin/busybox", &std::fs::read(busybox).unwrap(), &dir)
        .file("etc/passwd", b"root:x:0:0:root:/root:/bin/sh\n", &Opts::default())
        .file("etc/group", b"root:x:0:\n", &Opts::default())
        .file("marker", content.as_bytes(), &Opts::default());
    for a in APPLETS {
        t.symlink(&format!("bin/{a}"), "busybox", &Opts::default());
    }
    t.finish()
}

struct Fixture {
    _dirs: Vec<tempfile::TempDir>,
    store: PathBuf,
    env: Env,
    _turn: Turn,
}

/// The disk-heavy case runs alone: under nested virtualization its guest's
/// hundreds of MB of writes slow the other cases' guests past their boot timeouts.
static ALONE: RwLock<()> = RwLock::new(());

enum Turn {
    Shared(#[allow(dead_code)] RwLockReadGuard<'static, ()>),
    Alone(#[allow(dead_code)] RwLockWriteGuard<'static, ()>),
}

impl Turn {
    fn shared() -> Self {
        Turn::Shared(ALONE.read().unwrap_or_else(|e| e.into_inner()))
    }

    fn alone() -> Self {
        Turn::Alone(ALONE.write().unwrap_or_else(|e| e.into_inner()))
    }
}

/// A store holding `busybox`, converted by kiln, in a temporary directory.
fn fixture() -> Option<Fixture> {
    fixture_in(tempfile::tempdir().unwrap(), Turn::shared)
}

/// Like [`fixture`], with the store (`$KILN_HOME`, where the scratch disks go)
/// under Cargo's target directory: on disk, as a real store is, where `/tmp` may
/// be a tmpfs. Only the case that needs it: a disk the other cases' VMs share
/// would slow their boots while it writes. It runs alone.
fn fixture_on_disk() -> Option<Fixture> {
    fixture_in(tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap(), Turn::alone)
}

fn fixture_in(home: tempfile::TempDir, turn: fn() -> Turn) -> Option<Fixture> {
    let env = env()?;
    let _turn = turn();
    let src = tempfile::tempdir().unwrap();
    let f = Fixture {
        store: home.path().to_path_buf(),
        _dirs: vec![src, home],
        env,
        _turn,
    };
    let path = f._dirs[0].path().join("busybox");
    let mut b = LayoutBuilder::new(&path);
    let cfg = ContainerConfig {
        cmd: Some(vec!["/bin/sh".into()]),
        ..Default::default()
    };
    let d = b.image(
        &Platform::host(),
        &[TestLayer::tar(busybox_layer(&f.env.busybox, "one"))],
        cfg,
    );
    b.add(d, None).finish();
    let out = f.kiln().args(["convert", "--tag", "busybox"]).arg(&path).finish();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Some(f)
}

impl Fixture {
    fn kiln(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_kiln"));
        c.arg("--store").arg(&self.store);
        if std::env::var_os("KILN_TEST_KEEP").is_some_and(|v| v == "1") {
            c.env("KILN_KEEP_RUN_DIR", "1");
        }
        c
    }

    /// `kiln run` with the test kernel and init, and `args`.
    fn run(&self, args: &[&str]) -> Command {
        let mut c = self.kiln();
        c.arg("run")
            .args(["--vmm", &self.env.vmm, "--kernel"])
            .arg(&self.env.kernel)
            .arg("--allow-custom-kernel")
            .arg("--init")
            .arg(&self.env.init)
            .arg("--allow-custom-init")
            .args(args);
        c
    }

    fn output(&self, args: &[&str]) -> Output {
        self.run(args).finish()
    }
}

/// How long any kiln a test starts may take, unless the test bounds it tighter.
const DEADLINE: Duration = Duration::from_secs(180);

/// A kiln the test started. Dropped while running (a failed assertion), it is
/// killed, and its VMM dies with it: no orphans.
struct Kiln(Child);

impl Deref for Kiln {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for Kiln {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Kiln {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

impl Kiln {
    /// Waits for the exit; past `timeout` kiln is killed and the test fails with
    /// the end of its guest's console.
    fn wait_status(&mut self, timeout: Duration) -> ExitStatus {
        let until = Instant::now() + timeout;
        loop {
            if let Some(s) = self.0.try_wait().unwrap() {
                return s;
            }
            if Instant::now() >= until {
                let pid = self.0.id();
                let _ = self.0.kill();
                let _ = self.0.wait();
                panic!(
                    "kiln (pid {pid}) did not exit within {timeout:?} and was killed; the end of its guest's \
                     console:\n{}",
                    console_of(pid)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Like `wait_with_output`, bounded.
    fn output(mut self, timeout: Duration) -> Output {
        let out = self.0.stdout.take().map(Captured::spawn);
        let err = self.0.stderr.take().map(Captured::spawn);
        let status = self.wait_status(timeout);
        Output {
            status,
            stdout: out.map(|c| c.finish()).unwrap_or_default(),
            stderr: err.map(|c| c.finish()).unwrap_or_default(),
        }
    }
}

/// Starting kiln, bounded.
trait Start {
    fn start(&mut self) -> Kiln;
    /// `output()` within [`DEADLINE`] (stdin is `/dev/null`, as with `output()`).
    fn finish(&mut self) -> Output;
}

impl Start for Command {
    fn start(&mut self) -> Kiln {
        Kiln(self.spawn().unwrap())
    }

    fn finish(&mut self) -> Output {
        self.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .start()
            .output(DEADLINE)
    }
}

/// The last lines of the console of the run kiln `pid` made, found by the pid in
/// its run.json.
fn console_of(pid: u32) -> String {
    let base = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(x) if x.is_absolute() => x.join("kiln"),
        _ => PathBuf::from(format!("/tmp/kiln-{}", rustix::process::getuid().as_raw())),
    };
    for dir in std::fs::read_dir(&base).into_iter().flatten().flatten() {
        let Ok(info) = std::fs::read(dir.path().join("run.json")) else {
            continue;
        };
        let Ok(info) = serde_json::from_slice::<serde_json::Value>(&info) else {
            continue;
        };
        if info["kiln"]["pid"] == pid {
            let log = text(&std::fs::read(dir.path().join("console.log")).unwrap_or_default());
            let lines: Vec<&str> = log.lines().collect();
            return lines[lines.len().saturating_sub(40)..].join("\n");
        }
    }
    format!("(no run directory of kiln {pid} in {})", base.display())
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// stderr without kiln's own lines: what the guest wrote.
fn guest_err(o: &Output) -> String {
    text(&o.stderr)
        .lines()
        .filter(|l| !l.starts_with("kiln: ") && !l.starts_with("  | "))
        .map(|l| format!("{l}\n"))
        .collect()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

/// Reads a child's stdout on a thread, so tests can wait for text.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>);

impl Captured {
    fn spawn(mut r: impl Read + Send + 'static) -> Self {
        let c = Captured::default();
        let into = c.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = r.read(&mut buf) {
                if n == 0 {
                    break;
                }
                into.0.lock().unwrap().extend_from_slice(&buf[..n]);
            }
            into.1.store(true, Ordering::SeqCst);
        });
        c
    }

    /// Everything, once the stream ended (bounded: a straggler holding it open).
    fn finish(&self) -> Vec<u8> {
        let until = Instant::now() + Duration::from_secs(10);
        while !self.1.load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.0.lock().unwrap().clone()
    }

    fn text(&self) -> String {
        text(&self.0.lock().unwrap())
    }

    fn wait_for(&self, needle: &str, timeout: Duration) {
        let until = Instant::now() + timeout;
        while !self.text().contains(needle) {
            assert!(Instant::now() < until, "no {needle:?} in {:?}", self.text());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn signal(child: &Child, sig: rustix::process::Signal) {
    let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    rustix::process::kill_process(pid, sig).unwrap();
}

fn wait(child: &mut Kiln, timeout: Duration) -> i32 {
    child.wait_status(timeout).code().unwrap_or(-1)
}

const BOOT: Duration = Duration::from_secs(60);

#[test]
fn exit_codes_as_docker_reports_them() {
    let Some(f) = fixture() else { return };
    let started = Instant::now();
    let o = f.output(&["busybox", "--", "true"]);
    eprintln!("cold kiln run of busybox -- true: {:?}", started.elapsed());
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let o = f.output(&["busybox", "--", "sh", "-c", "echo out; echo err >&2; exit 3"]);
    assert_eq!(
        (code(&o), text(&o.stdout), guest_err(&o)),
        (3, "out\n".into(), "err\n".into())
    );
    let o = f.output(&["busybox", "--", "sh", "-c", "kill -9 $$"]);
    assert_eq!(code(&o), 137);
    // kiln's own environment need not be UTF-8.
    let o = {
        use std::os::unix::ffi::OsStrExt;
        f.run(&["busybox", "--", "true"])
            .env("KILN_TEST_NOT_UTF8", std::ffi::OsStr::from_bytes(b"\xff"))
            .stdin(Stdio::null())
            .finish()
    };
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let o = f.output(&["busybox", "--", "/nonexistent"]);
    assert_eq!(code(&o), 127, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("guest init failed at process: exec /nonexistent"),
        "{}",
        text(&o.stderr)
    );
    assert!(
        text(&o.stderr).contains("the end of the guest's console"),
        "{}",
        text(&o.stderr)
    );
    // The environment from --env.
    let o = f.output(&["--env", "A=b c", "busybox", "--", "sh", "-c", "echo $A; cat /marker"]);
    assert_eq!(
        (code(&o), text(&o.stdout)),
        (0, "b c\none".into()),
        "{}",
        text(&o.stderr)
    );
    // Without a command, the image's cmd (/bin/sh) runs: here it reads stdin.
    let mut child = f
        .run(&["-i", "busybox"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .start();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"echo from-the-image-cmd; exit 4\n").unwrap();
    drop(stdin);
    let o = child.output(DEADLINE);
    assert_eq!(
        (code(&o), text(&o.stdout)),
        (4, "from-the-image-cmd\n".into()),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn stdin_is_relayed_only_with_i() {
    let Some(f) = fixture() else { return };
    // echo hi | kiln run -i busybox -- cat
    let mut child = f
        .run(&["-i", "busybox", "--", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .start();
    let input: Vec<u8> = (0..1u32 << 20)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let mut stdin = child.stdin.take().unwrap();
    let data = input.clone();
    std::thread::spawn(move || {
        stdin.write_all(&data).unwrap();
    });
    let o = child.output(DEADLINE);
    assert_eq!(code(&o), 0);
    assert!(o.stdout == input, "binary stdin came back as {} bytes", o.stdout.len());
    // Without -i the guest's stdin is at EOF at once, even with kiln's stdin open.
    let mut child = f
        .run(&["busybox", "--", "sh", "-c", "cat; echo eof"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .start();
    let _keep_open = child.stdin.take();
    let out = Captured::spawn(child.stdout.take().unwrap());
    assert_eq!(wait(&mut child, BOOT), 0);
    out.wait_for("eof\n", Duration::from_secs(5));
}

#[test]
fn sigterm_asks_the_guest_and_a_second_sigint_kills() {
    let Some(f) = fixture() else { return };
    use rustix::process::Signal;
    let script = "trap 'echo TERM; exit 7' TERM; echo up; while :; do sleep 0.1; done";
    let mut child = f
        .run(&["busybox", "--", "sh", "-c", script])
        .stdout(Stdio::piped())
        .start();
    let out = Captured::spawn(child.stdout.take().unwrap());
    out.wait_for("up\n", BOOT);
    signal(&child, Signal::TERM);
    assert_eq!(wait(&mut child, Duration::from_secs(30)), 7);
    out.wait_for("TERM\n", Duration::from_secs(5));

    let script = "trap '' INT TERM; echo up; while :; do sleep 0.1; done";
    let mut child = f
        .run(&["--stop-timeout", "100", "busybox", "--", "sh", "-c", script])
        .stdout(Stdio::piped())
        .start();
    let out = Captured::spawn(child.stdout.take().unwrap());
    out.wait_for("up\n", BOOT);
    signal(&child, Signal::INT);
    std::thread::sleep(Duration::from_millis(500));
    let killed = Instant::now();
    signal(&child, Signal::INT);
    assert_eq!(wait(&mut child, Duration::from_secs(30)), 137);
    assert!(killed.elapsed() < Duration::from_secs(10), "{:?}", killed.elapsed());
}

/// Ctrl-C at a terminal signals the whole foreground process group. The VMM leads
/// a process group of its own (vmkit), so only kiln gets SIGINT and asks the guest
/// to stop: the app's own exit, not 125 from a VMM killed under it.
#[test]
fn sigint_to_kilns_process_group_stops_the_guest_gracefully() {
    let Some(f) = fixture() else { return };
    use std::os::unix::process::CommandExt;
    let script = "trap 'echo TERM; exit 7' TERM; echo up; while :; do sleep 0.1; done";
    let mut child = f
        .run(&["busybox", "--", "sh", "-c", script])
        .stdout(Stdio::piped())
        .process_group(0)
        .start();
    let out = Captured::spawn(child.stdout.take().unwrap());
    out.wait_for("up\n", BOOT);
    let group = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    rustix::process::kill_process_group(group, rustix::process::Signal::INT).unwrap();
    assert_eq!(wait(&mut child, Duration::from_secs(30)), 7);
    out.wait_for("TERM\n", Duration::from_secs(5));
}

/// Whether `pid` has a handler for SIGINT (`/proc/<pid>/status`, `SigCgt`).
fn catches_sigint(pid: u32) -> bool {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("SigCgt:"))
        .and_then(|m| u64::from_str_radix(m.trim(), 16).ok())
        .is_some_and(|m| m & (1 << (2 - 1)) != 0)
}

/// SIGINT during setup (here while kiln waits to read `--init`, a FIFO) is
/// handled, not fatal: setup stops, cleans up and exits 137, as a user kill
/// before the guest ran.
#[test]
fn sigint_during_setup_aborts_with_137() {
    let Some(f) = fixture() else { return };
    let fifo = f._dirs[0].path().join("init.fifo");
    rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, rustix::fs::Mode::from_raw_mode(0o600)).unwrap();
    let mut child = f
        .kiln()
        .args(["run", "--vmm", &f.env.vmm, "--kernel"])
        .arg(&f.env.kernel)
        .args(["--allow-custom-kernel", "--init"])
        .arg(&fifo)
        .args(["--allow-custom-init", "busybox", "--", "true"])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .start();
    let until = Instant::now() + Duration::from_secs(30);
    while !catches_sigint(child.id()) {
        assert!(
            Instant::now() < until,
            "kiln never installed its SIGINT handler during setup"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    signal(&child, rustix::process::Signal::INT);
    // Let setup go on: it reads the init binary and then sees the interrupt. (The
    // FIFO is opened without blocking, so a kiln that never reads it cannot hang the test.)
    let writer = loop {
        use rustix::fs::{Mode, OFlags, open};
        match open(
            &fifo,
            OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => break fd,
            Err(e) => {
                assert!(Instant::now() < until, "kiln never opened --init: {e}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    };
    rustix::fs::fcntl_setfl(&writer, rustix::fs::OFlags::empty()).unwrap();
    std::fs::File::from(writer)
        .write_all(&std::fs::read(&f.env.init).unwrap())
        .unwrap();
    let err = Captured::spawn(child.stderr.take().unwrap());
    assert_eq!(wait(&mut child, Duration::from_secs(60)), 137, "{}", err.text());
    err.wait_for("interrupted before the guest started", Duration::from_secs(5));
}

/// `-t -i` under a pseudo-terminal: raw mode, the guest's window size and its
/// changes, Ctrl-] q, and the terminal restored afterwards.
#[test]
fn tty_mode_on_a_pty() {
    let Some(f) = fixture() else { return };
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    use rustix::termios::{Winsize, tcgetattr, tcsetwinsize};
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let name = ptsname(&master, Vec::new()).unwrap();
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name.to_str().unwrap())
        .unwrap();
    tcsetwinsize(
        &master,
        Winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    let before = tcgetattr(&slave).unwrap();
    let script = "stty size; tty; read l; echo got:$l; read l; stty size; while :; do sleep 0.1; done";
    let mut child = f
        .run(&["-t", "-i", "busybox", "--", "sh", "-c", script])
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap())
        .start();
    let mut writer = std::fs::File::from(rustix::io::dup(&master).unwrap());
    let out = Captured::spawn(std::fs::File::from(master));
    out.wait_for("30 100", BOOT);
    out.wait_for("/dev/pts/", Duration::from_secs(10));
    // Raw: kiln does not echo or translate; the guest's terminal does.
    writer.write_all(b"hello\r").unwrap();
    out.wait_for("got:hello", Duration::from_secs(10));
    tcsetwinsize(
        &slave,
        Winsize {
            ws_row: 50,
            ws_col: 132,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    signal(&child, rustix::process::Signal::WINCH);
    std::thread::sleep(Duration::from_millis(500));
    writer.write_all(b"x\r").unwrap();
    out.wait_for("50 132", Duration::from_secs(10));
    // Ctrl-] q: Shutdown, so sh gets SIGTERM (no handler: 143).
    writer.write_all(b"\x1dq").unwrap();
    assert_eq!(wait(&mut child, Duration::from_secs(30)), 143, "{}", out.text());
    let after = tcgetattr(&slave).unwrap();
    assert_eq!(
        (after.local_modes, after.input_modes, after.output_modes),
        (before.local_modes, before.input_modes, before.output_modes),
        "the terminal is restored"
    );
}

#[test]
fn the_boot_timeout_ends_a_stuck_guest() {
    let Some(f) = fixture() else { return };
    let Some(hostile) = &f.env.hostile else {
        assert!(
            std::env::var_os("KILN_REQUIRE_KVM_TESTS").is_none_or(|v| v != "1"),
            "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_HOSTILE is unset"
        );
        return;
    };
    let mut c = f.kiln();
    c.args(["run", "--vmm", &f.env.vmm, "--kernel"])
        .arg(&f.env.kernel)
        .arg("--allow-custom-kernel")
        .arg("--init")
        .arg(hostile)
        .args([
            "--allow-custom-init",
            "--boot-timeout",
            "5",
            "busybox",
            "--",
            "hang-before-running",
        ]);
    let o = c.finish();
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("boot timeout"), "{}", text(&o.stderr));
}

/// The fixture's image has the pinned kernel layer (kiln convert fetched it) and
/// no init layer: it needs --init, and --kernel needs its flag.
#[test]
fn an_image_without_a_pinned_init_needs_the_custom_flags() {
    let Some(f) = fixture() else { return };
    let o = f.kiln().args(["run", "busybox"]).finish();
    assert_eq!(code(&o), 125);
    assert!(
        text(&o.stderr).contains("no pinned kiln-init") && text(&o.stderr).contains("--allow-custom-init"),
        "{}",
        text(&o.stderr)
    );
    let o = f
        .kiln()
        .args(["run", "--kernel"])
        .arg(&f.env.kernel)
        .arg("busybox")
        .finish();
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("--allow-custom-kernel"), "{}", text(&o.stderr));
    let o = f
        .kiln()
        .args(["run", "--kernel"])
        .arg(&f.env.kernel)
        .args(["--allow-custom-kernel", "busybox"])
        .finish();
    assert_eq!(code(&o), 125);
    assert!(
        text(&o.stderr).contains("no pinned kiln-init") && text(&o.stderr).contains("--allow-custom-init"),
        "{}",
        text(&o.stderr)
    );
}

/// Tags `from` again as `tag` with boot layers: a kernel layer of `kernel` naming
/// `profile`, and an init layer around `init` (convert adds such layers once kiln
/// pins a kernel and kiln-init).
fn with_boot_layers(store: &kiln_store::Store, from: &str, tag: &str, kernel: &Path, profile: &str, init: &Path) {
    use kiln_image::types::{InitRef, KILN_CONFIG, KILN_INIT, KILN_KERNEL, KernelRef};
    use kiln_oci::Descriptor;
    let loaded = kiln_image::load(store, &kiln_image::resolve_name(store, from).unwrap()).unwrap();
    let image = &loaded.entries[0].1;
    let mut config = image.config.clone();
    config.kernel = Some(KernelRef {
        profile: profile.into(),
        version: "6.18.54".into(),
    });
    config.init = Some(InitRef {
        version: "0.0.1".into(),
    });
    let config = serde_json::to_vec(&config).unwrap();
    let mut manifest = image.manifest.clone();
    manifest.config = Descriptor::new(KILN_CONFIG, store.put_bytes(&config).unwrap(), config.len() as u64);
    let kernel = std::fs::read(kernel).unwrap();
    let init = kiln_image::init_layer(&std::fs::read(init).unwrap(), &store.tmp_dir()).unwrap();
    // In place of the boot layers `kiln convert` added (the pinned kernel).
    manifest
        .layers
        .retain(|l| l.media_type == kiln_image::types::KILN_LAYER);
    manifest.layers.splice(
        0..0,
        [
            Descriptor::new(KILN_KERNEL, store.put_bytes(&kernel).unwrap(), kernel.len() as u64),
            Descriptor::new(KILN_INIT, store.put_bytes(&init).unwrap(), init.len() as u64),
        ],
    );
    let manifest = store.put_bytes(&serde_json::to_vec(&manifest).unwrap()).unwrap();
    store.set_ref(tag, &manifest).unwrap();
}

/// An image with boot layers (spec §8.1, §11.5): a `base` kernel this kiln does
/// not pin is refused, a `custom` one boots with --allow-custom-kernel, and the
/// image's init layer never boots (here it wraps busybox, which cannot be kiln's
/// PID 1): it is replaced, with a warning.
#[test]
fn image_boot_layers_are_verified_and_init_replaced() {
    let Some(f) = fixture() else { return };
    let store = kiln_store::Store::open(&f.store).unwrap();
    for (tag, profile) in [("pinned", "base"), ("custom", "custom")] {
        with_boot_layers(&store, "busybox", tag, &f.env.kernel, profile, &f.env.busybox);
    }
    let run = |more: &[&str]| {
        f.kiln()
            .args(["run", "--vmm", &f.env.vmm, "--init"])
            .arg(&f.env.init)
            .arg("--allow-custom-init")
            .args(more)
            .args(["--", "echo", "ran"])
            .stdin(Stdio::null())
            .finish()
    };
    let o = run(&["--allow-custom-kernel", "pinned"]);
    assert_eq!(code(&o), 125);
    assert!(
        text(&o.stderr).contains("is not the pinned kernel base 6.18.54"),
        "{}",
        text(&o.stderr)
    );
    let o = run(&["custom"]);
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("boots a custom kernel"), "{}", text(&o.stderr));
    let o = run(&["--allow-custom-kernel", "custom"]);
    assert_eq!((code(&o), text(&o.stdout)), (0, "ran\n".into()), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("init layer") && text(&o.stderr).contains("replaced"),
        "{}",
        text(&o.stderr)
    );
}

/// The boot layers `convert` adds (spec §6.5), from pins for the test kernel and,
/// as the image's "kiln-init", busybox: the image runs with the custom flags, and
/// its init layer is replaced.
#[test]
fn converted_boot_layers_are_replaced_or_refused_at_run() {
    let Some(f) = fixture() else { return };
    use kiln_image::artifacts::{ArtifactKind, Pin};
    use kiln_image::{Boot, BootLayers, ConvertOptions, LocalRequest, convert_local};
    let store = kiln_store::Store::open(&f.store).unwrap();
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let put = |path: &Path, kind, name, version| {
        let bytes = std::fs::read(path).unwrap();
        let d = store.put_bytes(&bytes).unwrap();
        Pin {
            kind,
            name,
            version,
            arch: leak(Platform::host().architecture),
            repository: "127.0.0.1:1/unused",
            digest: leak(d.to_string()),
            size: bytes.len() as u64,
        }
    };
    let boot = Boot {
        kernel: Some(put(&f.env.kernel, ArtifactKind::Kernel, "base", "6.18.54")),
        init: Some(put(&f.env.busybox, ArtifactKind::Init, "kiln-init", "0.0.1")),
    };
    let layout = f._dirs[0].path().join("busybox");
    let platforms = [Platform::host()];
    let req = LocalRequest {
        source_ref: None,
        platforms: &platforms,
        tag: Some("booted"),
    };
    let opts = ConvertOptions {
        boot: BootLayers::Explicit(boot),
        ..Default::default()
    };
    convert_local(&store, &layout, &req, &opts).unwrap();
    let o = f
        .kiln()
        .args(["run", "--vmm", &f.env.vmm, "--init"])
        .arg(&f.env.init)
        .args(["--allow-custom-init", "booted", "--", "true"])
        .finish();
    assert_eq!(code(&o), 125);
    assert!(
        text(&o.stderr).contains("is not the pinned kernel base 6.18.54"),
        "{}",
        text(&o.stderr)
    );
    let o = f.output(&["booted", "--", "echo", "ran"]);
    assert_eq!((code(&o), text(&o.stdout)), (0, "ran\n".into()), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("init layer") && text(&o.stderr).contains("replaced"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn the_device_budget_is_checked_before_boot() {
    let Some(f) = fixture() else { return };
    let path = f._dirs[0].path().join("deep");
    let mut b = LayoutBuilder::new(&path);
    let mut layers = vec![TestLayer::tar(busybox_layer(&f.env.busybox, "deep"))];
    layers.extend((0..32).map(|i| {
        TestLayer::tar(
            TarBuilder::new()
                .file(&format!("f{i}"), b"x", &Opts::default())
                .finish(),
        )
    }));
    let d = b.image(&Platform::host(), &layers, ContainerConfig::default());
    b.add(d, None).finish();
    let out = f
        .kiln()
        .args(["convert", "--max-layers", "64", "--tag", "deep"])
        .arg(&path)
        .finish();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let o = f
        .kiln()
        .args(["run", "--vmm", "cloud-hypervisor", "--kernel"])
        .arg(&f.env.kernel)
        .args(["--allow-custom-kernel", "--init"])
        .arg(&f.env.init)
        .args(["--allow-custom-init", "deep", "--", "true"])
        .finish();
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("--max-layers 27"), "{}", text(&o.stderr));
}

/// Converts, as `tag`, a busybox image whose `marker` holds `content`, with `config`.
fn add_image(f: &Fixture, tag: &str, content: &str, config: ContainerConfig) {
    let path = f._dirs[0].path().join(tag);
    let mut b = LayoutBuilder::new(&path);
    let d = b.image(
        &Platform::host(),
        &[TestLayer::tar(busybox_layer(&f.env.busybox, content))],
        config,
    );
    b.add(d, None).finish();
    let out = f.kiln().args(["convert", "--tag", tag]).arg(&path).finish();
    assert!(out.status.success(), "{}", text(&out.stderr));
}

/// `--persist` (spec §9.4): the disk and its sidecar outlive the run; it is never
/// shrunk; another image with the same layers reuses it with a note (ruling 2),
/// one with other layers only with --persist-force; --persist-reset starts over;
/// two runs never share it; and no cleanup of the run's directories touches it.
#[test]
fn persisted_disks_are_reused_and_guarded() {
    let Some(f) = fixture() else { return };
    let env = ContainerConfig {
        env: Some(vec!["CHANGED=1".into()]),
        ..ContainerConfig::default()
    };
    add_image(&f, "same-layers", "one", env);
    add_image(&f, "other", "two", ContainerConfig::default());
    let disk = f._dirs[0].path().join("data.img");
    let d = disk.to_str().unwrap();
    let o = f.output(&[
        "--persist",
        d,
        "--disk",
        "128M",
        "busybox",
        "--",
        "sh",
        "-c",
        "echo kept > /keep",
    ]);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    assert_eq!(std::fs::metadata(&disk).unwrap().len(), 128 << 20);
    let sidecar: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f._dirs[0].path().join("data.img.kiln.json")).unwrap()).unwrap();
    assert_eq!(sidecar["size"], 128 << 20);
    // No scratch directory was made for the run, and none is left.
    let scratch: Vec<_> = std::fs::read_dir(f.store.join("scratch")).unwrap().flatten().collect();
    assert!(scratch.is_empty(), "{scratch:?}");
    let o = f.output(&["--persist", d, "busybox", "--", "cat", "/keep"]);
    assert_eq!((code(&o), text(&o.stdout)), (0, "kept\n".into()), "{}", text(&o.stderr));
    // Never shrunk.
    let o = f.output(&["--persist", d, "--disk", "64M", "busybox", "--", "true"]);
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("never shrunk"), "{}", text(&o.stderr));
    // The same layers under another manifest: reused, with a note.
    let o = f.output(&[
        "--persist",
        d,
        "same-layers",
        "--",
        "sh",
        "-c",
        "cat /keep; echo $CHANGED",
    ]);
    assert_eq!(
        (code(&o), text(&o.stdout)),
        (0, "kept\n1\n".into()),
        "{}",
        text(&o.stderr)
    );
    assert!(
        text(&o.stderr).contains("kiln: note:") && text(&o.stderr).contains("same layers"),
        "{}",
        text(&o.stderr)
    );
    // Other layers: refused, unless forced or reset.
    let o = f.output(&["--persist", d, "other", "--", "true"]);
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("--persist-reset"), "{}", text(&o.stderr));
    let o = f.output(&[
        "--persist",
        d,
        "--persist-force",
        "other",
        "--",
        "cat",
        "/keep",
        "/marker",
    ]);
    assert_eq!(
        (code(&o), text(&o.stdout)),
        (0, "kept\ntwo".into()),
        "{}",
        text(&o.stderr)
    );
    assert!(text(&o.stderr).contains("--persist-force"), "{}", text(&o.stderr));
    // Two runs never share it.
    let mut first = f
        .run(&[
            "--persist",
            d,
            "--persist-force",
            "busybox",
            "--",
            "sh",
            "-c",
            "echo up; sleep 60",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .start();
    let out = Captured::spawn(first.stdout.take().unwrap());
    out.wait_for("up\n", BOOT);
    let o = f.output(&["--persist", d, "busybox", "--", "true"]);
    assert_eq!(code(&o), 125);
    assert!(
        text(&o.stderr).contains("in use by another kiln run"),
        "{}",
        text(&o.stderr)
    );
    signal(&first, rustix::process::Signal::TERM);
    assert_eq!(wait(&mut first, Duration::from_secs(30)), 143);
    // A reset starts from the template, at the size asked for.
    let o = f.output(&[
        "--persist",
        d,
        "--persist-reset",
        "--disk",
        "64M",
        "busybox",
        "--",
        "cat",
        "/keep",
    ]);
    assert_eq!(code(&o), 1, "a reset disk is empty: {}", text(&o.stderr));
    assert_eq!(std::fs::metadata(&disk).unwrap().len(), 64 << 20);
}

/// `KILN_IMPORT_FROM` (spec §10, the Lima template's dev loop): a run in an empty
/// store imports the image from the other store first.
#[test]
fn images_are_imported_from_kiln_import_from() {
    let Some(f) = fixture() else { return };
    let other = tempfile::tempdir().unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_kiln"));
    c.arg("--store").arg(other.path()).env("KILN_IMPORT_FROM", &f.store);
    c.args(["run", "--vmm", &f.env.vmm, "--kernel"])
        .arg(&f.env.kernel)
        .arg("--allow-custom-kernel")
        .arg("--init")
        .arg(&f.env.init)
        .args(["--allow-custom-init", "busybox", "--", "cat", "/marker"]);
    let o = c.finish();
    assert_eq!((code(&o), text(&o.stdout)), (0, "one".into()), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("kiln: imported busybox from"),
        "{}",
        text(&o.stderr)
    );
}

fn net_enabled() -> bool {
    let on = std::env::var_os("KILN_TEST_NET").is_some_and(|v| v == "1");
    assert!(
        on || std::env::var_os("KILN_REQUIRE_KVM_TESTS").is_none_or(|v| v != "1"),
        "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_NET is not (needs pasta and nft)"
    );
    on
}

/// The host's primary address (no packet is sent).
fn host_ip() -> std::net::Ipv4Addr {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    s.connect("1.1.1.1:80").unwrap();
    match s.local_addr().unwrap().ip() {
        std::net::IpAddr::V4(a) => a,
        other => panic!("{other}"),
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// What `addr` sends before it closes, once it sends anything (retried while the
/// guest starts).
fn read_from(addr: (std::net::Ipv4Addr, u16)) -> String {
    let until = Instant::now() + BOOT;
    loop {
        let mut got = String::new();
        if let Ok(mut conn) = std::net::TcpStream::connect(addr) {
            conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let _ = conn.read_to_string(&mut got);
            if !got.is_empty() {
                return got;
            }
        }
        assert!(Instant::now() < until, "{addr:?} never answered");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `-p` (spec §9.3, rev 2.10): published on 127.0.0.1 unless an address is given,
/// so the host's other addresses refuse it; `0.0.0.0` publishes it on all of them.
/// A guest listening on its own loopback only cannot be reached: the connection
/// ends at once (the image must listen on 0.0.0.0), it does not hang.
#[test]
fn published_ports_bind_to_loopback_unless_told_otherwise() {
    let Some(f) = fixture() else { return };
    if !net_enabled() {
        return;
    }
    let (loopback, everywhere, guest_loopback) = (free_port(), free_port(), free_port());
    let script = "httpd -f -p 127.0.0.1:8080 -h / & \
                  (while :; do echo hello-from-guest | nc -l -p 8081; done) & \
                  (while :; do echo hello-everywhere | nc -l -p 8082; done) & \
                  sleep 1; echo up; sleep 120";
    let (p0, p1, p2) = (
        format!("{guest_loopback}:8080"),
        format!("{loopback}:8081"),
        format!("0.0.0.0:{everywhere}:8082"),
    );
    let mut child = f
        .run(&[
            "--net", "-p", &p0, "-p", &p1, "-p", &p2, "busybox", "--", "sh", "-c", script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .start();
    let out = Captured::spawn(child.stdout.take().unwrap());
    out.wait_for("up\n", BOOT);
    let (lo, host) = (std::net::Ipv4Addr::LOCALHOST, host_ip());
    assert_eq!(read_from((lo, loopback)), "hello-from-guest\n");
    let refused = std::net::TcpStream::connect((host, loopback)).map(|_| ()).unwrap_err();
    assert_eq!(
        refused.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "{host}: {refused}"
    );
    assert_eq!(read_from((host, everywhere)), "hello-everywhere\n");
    // The guest's 127.0.0.1-only listener: closed or reset quickly, never a hang.
    let started = Instant::now();
    let ended = match std::net::TcpStream::connect((lo, guest_loopback)) {
        Ok(mut conn) => {
            conn.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let mut buf = Vec::new();
            match conn.read_to_end(&mut buf) {
                Ok(_) => buf.is_empty(),
                Err(e) => matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ),
            }
        }
        Err(e) => e.kind() == std::io::ErrorKind::ConnectionRefused,
    };
    assert!(
        ended,
        "the connection to a loopback-only guest listener did not end cleanly"
    );
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    signal(&child, rustix::process::Signal::TERM);
    assert_eq!(wait(&mut child, Duration::from_secs(30)), 143, "{}", out.text());
}

/// T5 and `--egress` (spec §9.3): the host's own address is denied, a public
/// address is reachable by default, not under deny-all, and again with an
/// allow= exception (1.1.1.1:80, so this needs the internet). Without `--net`,
/// lookups fail at once instead of tools lacking a resolver (rev 2.10).
#[test]
fn egress_policy_and_the_resolver_without_a_network() {
    let Some(f) = fixture() else { return };
    if !net_enabled() {
        return;
    }
    let server = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let hport = server.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in server.incoming().flatten() {
            let _ = (&c).write_all(b"host\n");
        }
    });
    let probe = |addr: String| format!("nc -w 3 {addr} </dev/null >/dev/null 2>&1 && echo reached || echo blocked");
    let o = f.output(&[
        "--net",
        "busybox",
        "--",
        "sh",
        "-c",
        &probe(format!("{} {hport}", host_ip())),
    ]);
    assert_eq!(text(&o.stdout), "blocked\n", "{}", text(&o.stderr));
    let public = probe("1.1.1.1 80".into());
    for (egress, want) in [
        (&[][..], "reached\n"),
        (&["--egress", "deny-all"][..], "blocked\n"),
        (&["--egress", "deny-all", "--egress", "allow=1.1.1.1"][..], "reached\n"),
    ] {
        let mut args = vec!["--net"];
        args.extend_from_slice(egress);
        args.extend(["busybox", "--", "sh", "-c", &public]);
        let o = f.output(&args);
        assert_eq!(text(&o.stdout), want, "{egress:?}: {}", text(&o.stderr));
    }
    let o = f.output(&["--net", "--egress", "open", "busybox", "--", "true"]);
    assert!(
        text(&o.stderr).contains("warning: --egress open"),
        "{}",
        text(&o.stderr)
    );
    // No network: a resolver that refuses at once.
    let o = f.output(&[
        "busybox",
        "--",
        "sh",
        "-c",
        "grep nameserver /etc/resolv.conf; nslookup example.com >/dev/null 2>&1; echo rc=$?",
    ]);
    assert_eq!(
        (code(&o), text(&o.stdout)),
        (0, "nameserver 127.0.0.1\nrc=1\n".into()),
        "{}",
        text(&o.stderr)
    );
}

/// The scratch disk lives on the store's filesystem, in `$KILN_HOME/scratch/<id>/`
/// (spec §5.3, rev 2.9), not in the run directory, which is usually a tmpfs whose
/// pages count against the VMM's memory limit (`--memory` plus 256 MiB): a guest
/// writes more than that limit to its disk (512 MiB, with a 384 MiB limit) and
/// exits 0, and the scratch directory goes with the run. Disk writes are slow
/// under nested virtualization (about 4 MB/s in Lima), hence the longer bound.
#[test]
fn the_scratch_disk_holds_more_than_the_vmms_memory() {
    let Some(f) = fixture_on_disk() else { return };
    let o = f
        .run(&[
            "--memory",
            "128",
            "busybox",
            "--",
            "sh",
            "-c",
            "dd if=/dev/zero of=/big bs=1M count=512 2>/dev/null && wc -c </big",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .start()
        .output(Duration::from_secs(400));
    assert_eq!(
        (code(&o), text(&o.stdout).trim()),
        (0, "536870912"),
        "{}",
        text(&o.stderr)
    );
    let scratch = f.store.join("scratch");
    let left: Vec<_> = std::fs::read_dir(&scratch)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    if std::env::var_os("KILN_TEST_KEEP").is_none_or(|v| v != "1") {
        assert!(left.is_empty(), "{left:?}");
    }
}
