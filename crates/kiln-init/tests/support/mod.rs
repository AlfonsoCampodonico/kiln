//! Boots real kiln guests under vmkit: the init layer around the static
//! `kiln-init`, a scratch disk from the template, and app layers converted by
//! kiln from small OCI images built around a static busybox.
//!
//! Needs Linux with KVM, the VMMs and vmkit's sandbox helper (as vmkit's own
//! contract suite does), and:
//!   KILN_TEST_KERNEL=<vmkit kernel>   KILN_TEST_INIT=<static kiln-init>
//!   KILN_TEST_HOSTILE=<static `hostile` from kiln-testguest>   KILN_TEST_BUSYBOX=<static busybox, default /bin/busybox>
//! Without the kernel and init each test is skipped, unless KILN_REQUIRE_KVM_TESTS=1.
//! KILN_TEST_KEEP=1 keeps every run directory (console and VMM logs); KILN_TEST_VCPUS
//! sets the guests' vCPUs (default 1).
//! `scripts/boot-tests.sh` builds everything and runs the suite.
#![allow(dead_code)]

pub mod driver;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_image::{ConvertOptions, LocalRequest, convert_local, init_layer, scratch};
use kiln_oci::testlayout::{LayoutBuilder, TestLayer};
use kiln_oci::{ContainerConfig, Platform};
use kiln_proto::{Config, ExitMethod, GUEST_CID, Process, Scratch};
use kiln_store::Store;
use vmkit::{Backend, Disk, GuestExit, NetSpec, VmSpec, Vmm, VsockSpec};

use driver::{Outcome, Reply, Session};

/// From VMM start to `Running` (nested virtualization is slow).
pub const BOOT: Duration = Duration::from_secs(60);
/// For a whole run.
pub const END: Duration = Duration::from_secs(120);
/// The default scratch disk.
pub const SCRATCH: u64 = 1 << 30;

/// The kernel command line of spec §8.3; vmkit appends the console and backend parameters.
pub const CMDLINE: [&str; 7] = [
    "root=/dev/vda",
    "ro",
    "rootfstype=erofs",
    "init=/kiln-init",
    "panic=-1",
    "quiet",
    "loglevel=3",
];

pub struct Env {
    pub kernel: PathBuf,
    pub init: PathBuf,
    pub hostile: Option<PathBuf>,
    pub busybox: PathBuf,
}

pub fn require() -> bool {
    std::env::var_os("KILN_REQUIRE_KVM_TESTS").is_some_and(|v| v == "1")
}

pub fn env() -> Option<&'static Env> {
    static ENV: OnceLock<Option<Env>> = OnceLock::new();
    ENV.get_or_init(|| {
        let get = |k: &str| std::env::var_os(k).map(PathBuf::from);
        match (get("KILN_TEST_KERNEL"), get("KILN_TEST_INIT")) {
            (Some(kernel), Some(init)) => Some(Env {
                kernel,
                init,
                hostile: get("KILN_TEST_HOSTILE"),
                busybox: get("KILN_TEST_BUSYBOX").unwrap_or_else(|| "/bin/busybox".into()),
            }),
            _ => {
                assert!(
                    !require(),
                    "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_KERNEL/KILN_TEST_INIT are unset"
                );
                None
            }
        }
    })
    .as_ref()
}

/// Disk images shared by every test in the process (read-only to the VMs).
pub struct Fixtures {
    /// `kiln-boot-<pid>` in the target's tmp directory (see `fixtures_dir`).
    pub dir: PathBuf,
    pub init: PathBuf,
    /// One layer: busybox, users, `/etc` symlinks.
    pub base: Vec<PathBuf>,
    /// `base` plus whiteout, opaque and filler layers: 14 layers, which with init, scratch and
    /// vsock fill Firecracker x86_64's 17 devices.
    pub stack: Vec<PathBuf>,
    /// `base` plus 79 filler layers, for backends with room for them.
    pub deep: Vec<PathBuf>,
    /// `base` plus the `hostile` program, when it was built.
    pub hostile: Option<Vec<PathBuf>>,
}

pub const STACK_LAYERS: usize = 14;
pub const DEEP_LAYERS: usize = 80;

pub fn fixtures() -> &'static Fixtures {
    static FIXTURES: OnceLock<Fixtures> = OnceLock::new();
    FIXTURES.get_or_init(|| {
        let env = env().expect("checked by Case::new");
        let dir = fixtures_dir();
        let init = dir.join("init.erofs");
        let bin = std::fs::read(&env.init).expect("read KILN_TEST_INIT");
        std::fs::write(&init, init_layer(&bin, &dir).unwrap()).unwrap();
        let busybox = std::fs::read(&env.busybox).expect("read the static busybox");
        let base = base_layer(&busybox);
        let store = Store::open(dir.join("store")).unwrap();
        let mut stack = vec![base.clone(), whiteouts_lower(), whiteouts_upper()];
        stack.extend((3..STACK_LAYERS).map(filler));
        let mut deep = vec![base.clone()];
        deep.extend((1..DEEP_LAYERS).map(filler));
        let hostile = env.hostile.as_ref().map(|path| {
            let bin = std::fs::read(path).expect("read KILN_TEST_HOSTILE");
            let top = TarBuilder::new()
                .file("bin/hostile", &bin, &Opts::default().mode(0o755))
                .finish();
            convert(&store, &dir, "hostile", vec![base.clone(), top])
        });
        Fixtures {
            init,
            base: convert(&store, &dir, "base", vec![base]),
            stack: convert(&store, &dir, "stack", stack),
            deep: convert(&store, &dir, "deep", deep),
            hostile,
            dir,
        }
    })
}

/// A fresh `kiln-boot-<pid>` directory for this process's fixtures. Statics are
/// never dropped, so a temporary directory would outlive every run; instead each
/// run first removes the directories of test processes that are gone, leaving at
/// most the latest run's and those of runs still going.
fn fixtures_dir() -> PathBuf {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    if let Ok(entries) = std::fs::read_dir(tmp) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(rest) = name.to_str().and_then(|n| n.strip_prefix("kiln-boot-")) else {
                continue;
            };
            // Older runs used random suffixes; a numeric one is a pid, maybe still running.
            let alive = rest
                .parse::<u32>()
                .is_ok_and(|pid| Path::new(&format!("/proc/{pid}")).exists());
            if !alive {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let dir = tmp.join(format!("kiln-boot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\napp:x:1000:1000:App:/home/app:/bin/sh\n";
const GROUP: &str = "root:x:0:\ntty:x:5:\napp:x:1000:\nextra:x:2000:app\nmore:x:3000:app\n";
const APPLETS: [&str; 30] = [
    "[", "cat", "date", "dd", "df", "echo", "env", "false", "grep", "head", "hostname", "id", "ip", "kill", "ls",
    "mount", "nc", "printf", "pwd", "readlink", "seq", "sh", "sleep", "stat", "stty", "test", "tr", "true", "tty",
    "wc",
];

fn base_layer(busybox: &[u8]) -> Vec<u8> {
    let dir = Opts::default().mode(0o755);
    let mut t = TarBuilder::new();
    t.dir("bin", &dir)
        .dir("etc", &dir)
        .dir("home", &dir)
        .dir("home/app", &dir.clone().uid(1000).gid(1000))
        .dir("root", &Opts::default().mode(0o700))
        .dir("tmp", &Opts::default().mode(0o1777))
        .file("bin/busybox", busybox, &dir)
        .file("bin/noexec", b"#!/bin/sh\necho ran\n", &Opts::default().mode(0o644))
        .file("etc/passwd", PASSWD.as_bytes(), &Opts::default())
        .file("etc/group", GROUP.as_bytes(), &Opts::default())
        .file("etc/hosts", b"10.9.8.7 stale\n", &Opts::default())
        .symlink("etc/hostname", "/nonexistent/hostname", &Opts::default())
        .symlink(
            "etc/resolv.conf",
            "../run/systemd/resolve/stub-resolv.conf",
            &Opts::default(),
        );
    for applet in APPLETS {
        t.symlink(&format!("bin/{applet}"), "busybox", &Opts::default());
    }
    t.finish()
}

fn whiteouts_lower() -> Vec<u8> {
    TarBuilder::new()
        .dir("data", &Opts::default().mode(0o755))
        .file("data/a", b"a", &Opts::default())
        .file("data/b", b"b", &Opts::default())
        .dir("opq", &Opts::default().mode(0o755))
        .file("opq/x", b"x", &Opts::default())
        .finish()
}

fn whiteouts_upper() -> Vec<u8> {
    TarBuilder::new()
        .whiteout("data/a")
        .dir("opq", &Opts::default().mode(0o755))
        .opaque("opq")
        .file("opq/y", b"y", &Opts::default())
        .finish()
}

fn filler(n: usize) -> Vec<u8> {
    TarBuilder::new()
        .file(&format!("layers/{n:02}"), format!("{n}\n").as_bytes(), &Opts::default())
        .finish()
}

/// Converts layer tars with kiln (no squash) and returns the erofs layers, lowest first.
fn convert(store: &Store, dir: &Path, name: &str, tars: Vec<Vec<u8>>) -> Vec<PathBuf> {
    let layout = dir.join(format!("{name}-layout"));
    let platform = Platform::host();
    let mut b = LayoutBuilder::new(&layout);
    let layers: Vec<TestLayer> = tars.into_iter().map(TestLayer::tar).collect();
    let desc = b.image(&platform, &layers, ContainerConfig::default());
    b.add(desc, Some(name));
    b.finish();
    let opts = ConvertOptions {
        max_layers: 200,
        ..Default::default()
    };
    let req = LocalRequest {
        source_ref: Some(name),
        platforms: std::slice::from_ref(&platform),
        tag: None,
    };
    let out = convert_local(store, &layout, &req, &opts).expect("kiln converts the fixture");
    out.images[0].layers.iter().map(|l| store.blob_path(&l.erofs)).collect()
}

/// vCPUs per guest: KILN_TEST_VCPUS, default 1 (Cloud Hypervisor 53 stalls more with 2 under nested virtualization).
fn vcpus() -> u8 {
    std::env::var("KILN_TEST_VCPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// One VM under test, with its private run directory.
pub struct Case {
    pub dir: tempfile::TempDir,
    pub vmm: Box<dyn Vmm>,
}

impl Case {
    pub fn new(backend: Backend) -> Option<Self> {
        env()?;
        // KILN_TEST_KEEP=1 keeps each run directory (console, VMM logs) for debugging.
        let keep = std::env::var_os("KILN_TEST_KEEP").is_some_and(|v| v == "1");
        Some(Self {
            dir: tempfile::Builder::new()
                .prefix("kiln-run-")
                .disable_cleanup(keep)
                .tempdir()
                .unwrap(),
            vmm: backend.discover().expect("VMM binary"),
        })
    }

    pub fn exit_method(&self) -> ExitMethod {
        match self.vmm.capabilities().guest_exit {
            GuestExit::Reboot => ExitMethod::Reboot,
            GuestExit::Poweroff => ExitMethod::Poweroff,
        }
    }

    /// A config running `argv` on `layers` layers with the defaults `kiln run` would use.
    pub fn config(&self, layers: &[PathBuf], argv: &[&str]) -> Config {
        Config {
            process: Process {
                entrypoint: Vec::new(),
                cmd: argv.iter().map(|s| s.to_string()).collect(),
                env: Vec::new(),
                working_dir: None,
                user: None,
            },
            stop_signal: kiln_proto::signal::DEFAULT_STOP,
            tty: None,
            interactive: false,
            hostname: "kiln-guest".into(),
            network: None,
            layers: layers.len() as u32,
            scratch: Scratch { size_bytes: SCRATCH },
            exit_method: self.exit_method(),
            shutdown_grace_secs: 10,
        }
    }

    /// The VM: init layer, a fresh scratch disk of `config.scratch` bytes, then `layers`.
    pub fn spec(&self, layers: &[PathBuf], config: &Config, net: Option<NetSpec>) -> VmSpec {
        let env = env().unwrap();
        let scratch_path = self.dir.path().join("scratch.img");
        let _ = std::fs::remove_file(&scratch_path);
        scratch::create_scratch(&scratch_path, config.scratch.size_bytes).unwrap();
        let mut disks = vec![
            Disk {
                path: fixtures().init.clone(),
                read_only: true,
            },
            Disk {
                path: scratch_path,
                read_only: false,
            },
        ];
        disks.extend(layers.iter().map(|p| Disk {
            path: p.clone(),
            read_only: true,
        }));
        VmSpec {
            kernel: env.kernel.clone(),
            initramfs: None,
            cmdline: CMDLINE.map(String::from).to_vec(),
            disks,
            vcpus: vcpus(),
            memory_mib: 256,
            vsock: Some(VsockSpec { guest_cid: GUEST_CID }),
            net,
            console_log: self.dir.path().join("console.log"),
            run_dir: self.dir.path().to_path_buf(),
        }
    }

    pub fn start(&self, layers: &[PathBuf], config: Config, reply: Reply, stdin: Vec<u8>) -> Session {
        self.start_with(self.spec(layers, &config, None), config, reply, stdin)
    }

    pub fn start_with(&self, spec: VmSpec, config: Config, reply: Reply, stdin: Vec<u8>) -> Session {
        let vm = self.vmm.create(&spec).expect("create the VM");
        Session::start(vm, config, reply, stdin)
    }

    /// Boots, runs to the end, and returns what happened.
    pub fn run(&self, layers: &[PathBuf], config: Config) -> Outcome {
        self.run_with_stdin(layers, config, Vec::new())
    }

    pub fn run_with_stdin(&self, layers: &[PathBuf], config: Config, stdin: Vec<u8>) -> Outcome {
        self.start(layers, config, Reply::Config, stdin).finish(END)
    }

    pub fn console(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("console.log")).unwrap_or_default()
    }

    /// The end of the console, for failure messages.
    pub fn tail(&self) -> String {
        let console = self.console();
        let lines: Vec<&str> = console.lines().collect();
        lines[lines.len().saturating_sub(40)..].join("\n")
    }
}
