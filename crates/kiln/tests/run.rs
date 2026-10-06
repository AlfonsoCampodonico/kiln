//! `kiln run` end to end (spec §9.7, §11.4): the CLI booting busybox images on
//! Linux with KVM. Needs what the boot suite needs (`scripts/boot-tests.sh` runs
//! both): KILN_TEST_KERNEL, KILN_TEST_INIT, KILN_TEST_HOSTILE, a static busybox
//! (KILN_TEST_BUSYBOX, default /bin/busybox), the VMMs and VMKIT_SANDBOX.
//! KILN_TEST_VMM picks the VMM (default firecracker). Without the kernel and init
//! every test is skipped, unless KILN_REQUIRE_KVM_TESTS=1.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
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

const APPLETS: [&str; 16] = [
    "cat", "echo", "env", "grep", "hostname", "id", "kill", "ls", "nc", "sh", "sleep", "stty", "test", "true", "tty",
    "wc",
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
}

/// A store holding `busybox`, converted by kiln.
fn fixture() -> Option<Fixture> {
    let env = env()?;
    let (src, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let f = Fixture {
        store: home.path().to_path_buf(),
        _dirs: vec![src, home],
        env,
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
    let out = f
        .kiln()
        .args(["convert", "--tag", "busybox"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Some(f)
}

impl Fixture {
    fn kiln(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_kiln"));
        c.arg("--store").arg(&self.store);
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
        self.run(args).stdin(Stdio::null()).output().unwrap()
    }
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
struct Captured(Arc<Mutex<Vec<u8>>>);

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
        });
        c
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

fn wait(child: &mut Child, timeout: Duration) -> i32 {
    let until = Instant::now() + timeout;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s.code().unwrap_or(-1);
        }
        assert!(Instant::now() < until, "kiln did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
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
            .output()
            .unwrap()
    };
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let o = f.output(&["busybox", "--", "/nonexistent"]);
    assert_eq!(code(&o), 127);
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
    // The image's cmd when none is given; env from --env.
    let o = f.output(&["--env", "A=b c", "busybox", "--", "sh", "-c", "echo $A; cat /marker"]);
    assert_eq!(text(&o.stdout), "b c\none");
}

#[test]
fn stdin_is_relayed_only_with_i() {
    let Some(f) = fixture() else { return };
    // echo hi | kiln run -i busybox -- cat
    let mut child = f
        .run(&["-i", "busybox", "--", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input: Vec<u8> = (0..1u32 << 20)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let mut stdin = child.stdin.take().unwrap();
    let data = input.clone();
    std::thread::spawn(move || {
        stdin.write_all(&data).unwrap();
    });
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 0);
    assert!(o.stdout == input, "binary stdin came back as {} bytes", o.stdout.len());
    // Without -i the guest's stdin is at EOF at once, even with kiln's stdin open.
    let mut child = f
        .run(&["busybox", "--", "sh", "-c", "cat; echo eof"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
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
        .spawn()
        .unwrap();
    let out = Captured::spawn(child.stdout.take().unwrap());
    out.wait_for("up\n", BOOT);
    signal(&child, Signal::TERM);
    assert_eq!(wait(&mut child, Duration::from_secs(30)), 7);
    out.wait_for("TERM\n", Duration::from_secs(5));

    let script = "trap '' INT TERM; echo up; while :; do sleep 0.1; done";
    let mut child = f
        .run(&["--stop-timeout", "100", "busybox", "--", "sh", "-c", script])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
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
        .spawn()
        .unwrap();
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
        .spawn()
        .unwrap();
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
    use rustix::termios::{OptionalActions, Winsize, tcgetattr, tcsetwinsize};
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
        .spawn()
        .unwrap();
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
    let _ = OptionalActions::Now;
}

#[test]
fn the_boot_timeout_ends_a_stuck_guest() {
    let Some(f) = fixture() else { return };
    let Some(hostile) = &f.env.hostile else { return };
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
    let o = c.stdin(Stdio::null()).output().unwrap();
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("boot timeout"), "{}", text(&o.stderr));
}

#[test]
fn a_provisional_image_needs_the_custom_kernel_flags() {
    let Some(f) = fixture() else { return };
    let o = f.kiln().args(["run", "busybox"]).output().unwrap();
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("has no kernel layer"), "{}", text(&o.stderr));
    let o = f
        .kiln()
        .args(["run", "--kernel"])
        .arg(&f.env.kernel)
        .arg("busybox")
        .output()
        .unwrap();
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("--allow-custom-kernel"), "{}", text(&o.stderr));
    let o = f
        .kiln()
        .args(["run", "--kernel"])
        .arg(&f.env.kernel)
        .args(["--allow-custom-kernel", "busybox"])
        .output()
        .unwrap();
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

/// An image with boot layers (spec §8.1, §11.5): a `base` kernel is refused while
/// kiln pins no kernel, a `custom` one boots with --allow-custom-kernel, and the
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
            .output()
            .unwrap()
    };
    let o = run(&["--allow-custom-kernel", "pinned"]);
    assert_eq!(code(&o), 125);
    assert!(
        text(&o.stderr).contains("kernel base 6.18.54") && text(&o.stderr).contains("cannot verify"),
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
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let o = f
        .kiln()
        .args(["run", "--vmm", "cloud-hypervisor", "--kernel"])
        .arg(&f.env.kernel)
        .args(["--allow-custom-kernel", "--init"])
        .arg(&f.env.init)
        .args(["--allow-custom-init", "deep", "--", "true"])
        .output()
        .unwrap();
    assert_eq!(code(&o), 125);
    assert!(text(&o.stderr).contains("--max-layers 27"), "{}", text(&o.stderr));
}
