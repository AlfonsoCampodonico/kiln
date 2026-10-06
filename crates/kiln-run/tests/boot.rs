//! Boots real kiln guests on both VMMs (spec §9, §11.4, §11.5). See `support` for
//! what the suite needs; without it every test is skipped.
#![cfg(target_os = "linux")]

mod support;

use std::time::Duration;

use kiln_proto::{
    EXIT_CANNOT_INVOKE, EXIT_INFRA, EXIT_NOT_FOUND, HostMessage, MAX_FRAME, Network, Shutdown, Signal, WindowSize,
};
use support::driver::{Outcome, Reply};
use support::{BOOT, Case, DEEP_LAYERS, END, STACK_LAYERS, fixtures};
use vmkit::{Backend, EndReason, GuestExit, NetSpec};

/// `sh -c <script>` on the base image.
fn sh(c: &Case, script: &str) -> kiln_proto::Config {
    c.config(&fixtures().base, &["/bin/sh", "-c", script])
}

/// The guest ended the VM itself, with the configured exit method.
fn ended_cleanly(c: &Case, o: &Outcome) {
    assert_eq!(o.violation, None, "{o:?}\nconsole:\n{}", c.tail());
    assert_eq!(o.end.reason, EndReason::Exited, "{o:?}\nconsole:\n{}", c.tail());
}

fn exit_codes_and_stages(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let o = c.run(&fixtures().base, sh(&c, "exit 0"));
    ended_cleanly(&c, &o);
    assert_eq!(o.exit_code(), 0);
    assert_eq!(o.stages(), [3, 4, 5, 6, 7]);
    assert!(o.run.hello, "{o:?}");
    assert!(o.running());
    let o = c.run(&fixtures().base, sh(&c, "exit 3"));
    ended_cleanly(&c, &o);
    assert_eq!(o.exit_code(), 3);
    let o = c.run(&fixtures().base, sh(&c, "kill -9 $$"));
    ended_cleanly(&c, &o);
    assert_eq!(o.exited().map(|e| (e.signaled, e.code)), Some((true, 9)));
    assert_eq!(o.exit_code(), 137);
}

fn stdio_is_binary_safe_and_eof_propagates(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let input: Vec<u8> = (0..1 << 20)
        .map(|i: u32| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let mut config = sh(&c, "cat; echo done >&2");
    config.interactive = true;
    let o = c.run_with_stdin(&fixtures().base, config, input.clone());
    ended_cleanly(&c, &o);
    assert_eq!(o.exit_code(), 0);
    assert_eq!(o.stdout.len(), input.len());
    assert!(o.stdout == input, "stdout differs from stdin");
    assert_eq!(o.stderr(), "done\n");
    // Without -i, stdin is at EOF at once.
    let o = c.run(&fixtures().base, sh(&c, "cat; echo eof"));
    ended_cleanly(&c, &o);
    assert_eq!(o.stdout(), "eof\n");
}

fn output_is_drained_and_background_processes_do_not_hang_the_exit(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let o = c.run(
        &fixtures().base,
        sh(&c, "sleep 1000 & seq 1 20000; echo last >&2; exit 4"),
    );
    ended_cleanly(&c, &o);
    assert_eq!(o.exit_code(), 4);
    let expected: String = (1..=20000).map(|i| format!("{i}\n")).collect();
    assert!(o.stdout() == expected, "stdout is {} bytes", o.stdout.len());
    assert_eq!(o.stderr(), "last\n");
}

fn tty_mode_with_window_size_and_ctrl_c(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let mut config = sh(
        &c,
        "stty size; [ -t 0 ] && echo isatty > /dev/stdout; read line; stty size; echo got:$line; \
         trap 'echo INT; exit 9' INT; while :; do sleep 0.1; done",
    );
    config.tty = Some(WindowSize { rows: 24, cols: 80 });
    config.interactive = true;
    // As a non-root user, who must be able to reopen the terminal (/dev/stdout).
    config.process.user = Some("app".into());
    let mut s = c.start(&fixtures().base, config, Reply::Config, Vec::new());
    assert!(s.wait_running(BOOT), "console:\n{}", c.tail());
    assert!(s.wait_output("isatty", BOOT), "console:\n{}", c.tail());
    s.send(&HostMessage::WindowSize(WindowSize { rows: 50, cols: 132 }))
        .expect("send to the guest");
    std::thread::sleep(Duration::from_millis(300));
    s.tty_write(b"hello\r").expect("write to the tty");
    assert!(s.wait_output("got:hello", BOOT));
    std::thread::sleep(Duration::from_millis(300));
    s.tty_write(b"\x03").expect("write to the tty");
    let o = s.finish(END);
    ended_cleanly(&c, &o);
    let tty = o.tty();
    assert!(tty.contains("24 80\r\n"), "{tty:?}");
    assert!(tty.contains("50 132\r\n"), "{tty:?}");
    assert!(tty.contains("INT"), "Ctrl-C reached the app: {tty:?}");
    assert_eq!(o.exit_code(), 9, "{tty:?}");
}

fn shutdown_sends_the_stop_signal_then_kills_after_the_grace(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    // The default stop signal is SIGTERM, and the app handles it.
    let config = sh(
        &c,
        "trap 'echo TERM; exit 7' TERM; echo up; while :; do sleep 0.1; done",
    );
    let mut s = c.start(&fixtures().base, config, Reply::Config, Vec::new());
    assert!(s.wait_output("up", BOOT), "console:\n{}", c.tail());
    s.send(&HostMessage::Shutdown(Shutdown { grace_secs: 30 }))
        .expect("send to the guest");
    let o = s.finish(END);
    ended_cleanly(&c, &o);
    assert_eq!((o.exit_code(), o.stdout()), (7, "up\nTERM\n".into()));

    // A custom stop signal (SIGUSR1).
    let mut config = sh(
        &c,
        "trap 'echo USR1; exit 8' USR1; echo up; while :; do sleep 0.1; done",
    );
    config.stop_signal = 10;
    let mut s = c.start(&fixtures().base, config, Reply::Config, Vec::new());
    assert!(s.wait_output("up", BOOT));
    s.send(&HostMessage::Shutdown(Shutdown { grace_secs: 30 }))
        .expect("send to the guest");
    let o = s.finish(END);
    ended_cleanly(&c, &o);
    assert_eq!((o.exit_code(), o.stdout()), (8, "up\nUSR1\n".into()));

    // An app that ignores the stop signal is killed when the grace period ends.
    let config = sh(&c, "trap '' TERM; echo up; while :; do sleep 0.1; done");
    let mut s = c.start(&fixtures().base, config, Reply::Config, Vec::new());
    assert!(s.wait_output("up", BOOT));
    let sent = std::time::Instant::now();
    s.send(&HostMessage::Shutdown(Shutdown { grace_secs: 2 }))
        .expect("send to the guest");
    let o = s.finish(END);
    ended_cleanly(&c, &o);
    assert_eq!(o.exit_code(), 137);
    assert!(sent.elapsed() >= Duration::from_secs(2), "{:?}", sent.elapsed());
}

fn signals_are_forwarded(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let config = sh(
        &c,
        "trap 'echo HUP' HUP; trap 'exit 5' USR2; echo up; while :; do sleep 0.1; done",
    );
    let mut s = c.start(&fixtures().base, config, Reply::Config, Vec::new());
    assert!(s.wait_output("up", BOOT), "console:\n{}", c.tail());
    s.send(&HostMessage::Signal(Signal { sig: 1 }))
        .expect("send to the guest");
    assert!(s.wait_output("HUP", BOOT));
    s.send(&HostMessage::Signal(Signal { sig: 12 }))
        .expect("send to the guest");
    let o = s.finish(END);
    ended_cleanly(&c, &o);
    assert_eq!(o.exit_code(), 5);
}

fn missing_and_non_executable_entrypoints(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    for (argv, code) in [
        (&["/nonexistent"][..], EXIT_NOT_FOUND),
        (&["no-such-command"][..], EXIT_NOT_FOUND),
        (&["/bin/noexec"][..], EXIT_CANNOT_INVOKE),
    ] {
        let o = c.run(&fixtures().base, c.config(&fixtures().base, argv));
        ended_cleanly(&c, &o);
        let f = o
            .init_failed()
            .unwrap_or_else(|| panic!("{argv:?}: no InitFailed: {o:?}"));
        assert_eq!(f.stage, 6);
        assert_eq!(o.exit_code(), code, "{argv:?}: {f:?}");
        assert!(!o.running());
        assert!(
            c.console().contains(&format!("kiln-init: process: exec {}", argv[0])),
            "{}",
            c.tail()
        );
    }
}

fn init_failures_are_sanitised_on_the_host(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let o = c.run(
        &fixtures().base,
        c.config(&fixtures().base, &["/bin/\x1b[2J\u{9b}31mevil"]),
    );
    ended_cleanly(&c, &o);
    let f = o.init_failed().expect("InitFailed");
    assert!(f.message.contains('\x1b'), "the guest sends raw text: {f:?}");
    let line = f.describe();
    assert!(!line.chars().any(|ch| ch.is_control()), "{line:?}");
    assert!(
        line.starts_with("guest init failed at process: exec /bin/[2J31mevil"),
        "{line:?}"
    );
    // A failure before the workload (stage 3: a layer disk that is not there) is 125.
    let mut config = sh(&c, "true");
    config.layers = 2;
    let o = c.run(&fixtures().base, config);
    ended_cleanly(&c, &o);
    assert_eq!(o.init_failed().map(|f| f.stage), Some(3), "{o:?}");
    assert_eq!(o.exit_code(), EXIT_INFRA);
    assert!(
        c.console().contains("kiln-init: storage: mount layer /dev/vdd"),
        "{}",
        c.tail()
    );
}

fn user_groups_env_and_workdir(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    // The workload's stdio belongs to its user, so it can reopen /dev/stdout and /dev/stderr.
    let script = "id; echo home=$HOME host=$HOSTNAME path=$PATH foo=$FOO; pwd; ls -ld .; \
                  echo reopened-out > /dev/stdout && echo reopened-err > /dev/stderr";
    let mut config = sh(&c, script);
    config.process.user = Some("app".into());
    config.process.env = vec!["FOO=bar".into(), "FOO=baz".into()];
    config.process.working_dir = Some("/work/dir".into());
    let o = c.run(&fixtures().base, config);
    ended_cleanly(&c, &o);
    assert_eq!(o.exit_code(), 0, "{o:?}");
    let out = o.stdout();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[0], "uid=1000(app) gid=1000(app) groups=1000(app),2000(extra),3000(more)",
        "{out}"
    );
    assert_eq!(
        lines[1],
        format!(
            "home=/home/app host=kiln-guest path={} foo=baz",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        )
    );
    assert_eq!(lines[2], "/work/dir");
    assert!(
        lines[3].starts_with("drwxr-xr-x") && lines[3].contains(" root "),
        "{out}"
    );
    assert_eq!(lines[4..], ["reopened-out"], "{out}");
    assert_eq!(o.stderr(), "reopened-err\n");

    let mut config = sh(&c, "id; echo home=$HOME");
    config.process.user = Some("1234:5678".into());
    let o = c.run(&fixtures().base, config);
    ended_cleanly(&c, &o);
    assert_eq!(o.stdout(), "uid=1234 gid=5678 groups=5678\nhome=/\n");

    // An unknown user fails stage 6 before exec: 125, as Docker reports it. The
    // guest must end the run itself (a timeout would also be 125).
    let mut config = sh(&c, "true");
    config.process.user = Some("nobody".into());
    let o = c.run(&fixtures().base, config);
    ended_cleanly(&c, &o);
    let f = o
        .init_failed()
        .unwrap_or_else(|| panic!("no InitFailed: {o:?}\n{}", c.tail()));
    assert_eq!(f.stage, 6, "{f:?}");
    assert!(!o.running());
    assert_eq!(o.exit_code(), EXIT_INFRA, "{o:?}");
    assert!(
        c.console()
            .contains("kiln-init: process: unable to find user nobody: no matching entries in passwd file"),
        "{}",
        c.tail()
    );
}

fn layers_and_whiteouts_through_the_overlay(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let stack = &fixtures().stack;
    let script = "ls /data; ls /opq; ls /layers | wc -l; cat /proc/mounts | grep ' / '";
    let o = c.run(stack, c.config(stack, &["/bin/sh", "-c", script]));
    ended_cleanly(&c, &o);
    let out = o.stdout();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[..3], ["b", "y", &(STACK_LAYERS - 3).to_string()], "{out}");
    assert!(lines[3].starts_with("overlay / overlay "), "{out}");
    let top = format!(
        "lowerdir=/kiln/layers/{}:/kiln/layers/{}:",
        STACK_LAYERS - 1,
        STACK_LAYERS - 2
    );
    assert!(lines[3].contains(&top), "{out}");
    assert!(
        lines[3].contains(",upperdir=/kiln/rw/upper,workdir=/kiln/rw/work,"),
        "{out}"
    );
    // The kernel lists options only when they differ from its defaults (here: off).
    assert!(lines[3].contains("xino=on"), "{out}");
    for opt in ["redirect_dir=on", "index=on", "metacopy=on"] {
        assert!(!lines[3].contains(opt), "{opt}: {out}");
    }
}

/// Runs where the VMM has room for 83 devices: Firecracker on aarch64. Cloud
/// Hypervisor allows 31 virtio devices (one is its RNG) and Firecracker on x86_64 17,
/// so there the case is skipped with a message;
/// `layers_and_whiteouts_through_the_overlay` covers 14 layers on every VMM and arch.
fn many_layers_fit_one_mount_data_page(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let deep = &fixtures().deep;
    // init, scratch and vsock take three devices.
    let (available, needed) = (c.vmm.capabilities().available_devices(), deep.len() as u32 + 3);
    if available < needed {
        eprintln!("skipped: {backend:?} has room for {available} devices, {DEEP_LAYERS} layers need {needed}");
        return;
    }
    let o = c.run(
        deep,
        c.config(deep, &["/bin/sh", "-c", "ls /layers | wc -l; cat /layers/79"]),
    );
    ended_cleanly(&c, &o);
    assert_eq!(o.stdout(), format!("{}\n79\n", DEEP_LAYERS - 1));
}

/// Spec §14: the template grows online to 64 GiB. Cloud Hypervisor 53 cannot grow it
/// that far (resizing hangs on its WRITE_ZEROES support for the disk), so there the
/// scratch disk is 8 GiB, which is expected to work without nesting (unverified on bare
/// metal); kiln's default is 4 GiB.
fn the_scratch_disk_grows_online(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let size: u64 = match backend {
        Backend::Firecracker => 64 << 30,
        Backend::CloudHypervisor => 8 << 30,
    };
    let mut config = sh(
        &c,
        "df -k / | tail -n 1; dd if=/dev/zero of=/big bs=1M count=64 2>/dev/null; ls -l /big",
    );
    config.scratch.size_bytes = size;
    let o = c.run(&fixtures().base, config);
    ended_cleanly(&c, &o);
    let out = o.stdout();
    let total_kib: u64 = out.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect(&out);
    // ext4's own metadata takes a few percent.
    assert!(
        total_kib > size / 1024 / 16 * 15,
        "df says {total_kib} KiB of {size} bytes: {out}"
    );
    assert!(out.contains(" 67108864 "), "the upper layer is writable: {out}");
}

fn hostname_hosts_and_resolv_conf_replace_symlinks(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let script = "hostname; for f in hostname hosts resolv.conf; do stat -c '%A %n' /etc/$f; done; \
                  cat /etc/hostname /etc/hosts /etc/resolv.conf";
    let o = c.run(&fixtures().base, sh(&c, script));
    ended_cleanly(&c, &o);
    let out = o.stdout();
    assert!(
        out.starts_with("kiln-guest\n-rw-r--r-- /etc/hostname\n-rw-r--r-- /etc/hosts\n"),
        "{out}"
    );
    assert!(
        out.contains("-rw-r--r-- /etc/resolv.conf\nkiln-guest\n127.0.0.1\tlocalhost\n"),
        "{out}"
    );
    assert!(
        out.contains("127.0.1.1\tkiln-guest\n# kiln: this VM has no network\n"),
        "{out}"
    );
    assert!(!out.contains("stale"), "{out}");
}

fn networking_from_config(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    if std::env::var_os("KILN_TEST_NET").is_none_or(|v| v != "1") {
        assert!(
            !support::require(),
            "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_NET is not (needs pasta and nft)"
        );
        return;
    }
    use vmkit::net::{GATEWAY, GUEST, PREFIX, PortForward, Protocol};
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let script = "ip -4 addr show eth0 | grep inet; ip route | grep default; cat /etc/resolv.conf; \
                  echo hello-from-guest | nc -l -p 8080";
    let mut config = sh(&c, script);
    config.network = Some(Network {
        address: GUEST,
        prefix_len: PREFIX,
        gateway: GATEWAY,
        dns: vec![GATEWAY],
    });
    let net = NetSpec {
        forwards: vec![PortForward {
            protocol: Protocol::Tcp,
            host: port,
            guest: 8080,
        }],
        ..NetSpec::default()
    };
    let spec = c.spec(&fixtures().base, &config, Some(net));
    let mut s = c.start_with(spec, config, Reply::Config, Vec::new());
    assert!(s.wait_output("nameserver", BOOT), "console:\n{}", c.tail());
    let deadline = std::time::Instant::now() + BOOT;
    let reply = loop {
        use std::io::Read;
        let mut got = String::new();
        if let Ok(mut conn) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let _ = conn.read_to_string(&mut got);
            if !got.is_empty() {
                break got;
            }
        }
        assert!(std::time::Instant::now() < deadline, "the forward never answered");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(reply, "hello-from-guest\n");
    let o = s.finish(END);
    ended_cleanly(&c, &o);
    let out = o.stdout();
    assert!(out.contains(&format!("inet {GUEST}/{PREFIX} ")), "{out}");
    assert!(out.contains(&format!("default via {GATEWAY} dev eth0")), "{out}");
    assert!(out.contains(&format!("nameserver {GATEWAY}")), "{out}");
}

fn a_second_hello_makes_the_host_kill_the_vm(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let Some(image) = &fixtures().hostile else {
        assert!(
            !support::require(),
            "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_HOSTILE is unset"
        );
        return;
    };
    let o = c.run(image, c.config(image, &["/bin/hostile"]));
    assert_eq!(o.end.reason, EndReason::Killed, "{o:?}");
    let why = o.violation.as_deref().unwrap_or_default();
    assert!(
        why == "a second connection on vsock port 1024" || why == "a second Hello",
        "{why}"
    );
    assert_eq!(o.exit_code(), EXIT_INFRA);
}

/// The guest's side of T9: protocol violations from the host end the VM.
fn host_protocol_violations_end_the_guest(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let reset = match c.vmm.capabilities().guest_exit {
        GuestExit::Reboot => EndReason::Exited,
        GuestExit::Poweroff => EndReason::ResetStopped,
    };
    // Instead of Config: an oversized frame, then invalid JSON. Before Config the guest reboots.
    let oversize = (MAX_FRAME + 1).to_le_bytes().to_vec();
    let mut bad_json = 7u32.to_le_bytes().to_vec();
    bad_json.push(2);
    bad_json.extend_from_slice(b"{nope}");
    for (raw, needle) in [(oversize, "exceeds"), (bad_json, "invalid Config payload")] {
        let config = sh(&c, "true");
        let o = c
            .start(&fixtures().base, config, Reply::Raw(raw), Vec::new())
            .finish(END);
        assert_eq!(o.violation, None);
        assert_eq!(o.end.reason, reset, "{o:?}\n{}", c.tail());
        let f = o
            .init_failed()
            .unwrap_or_else(|| panic!("no InitFailed: {o:?}\n{}", c.tail()));
        assert_eq!(f.stage, 2);
        assert!(f.message.contains(needle), "{f:?}");
        assert!(
            c.console().contains("kiln-init: control: protocol violation"),
            "{}",
            c.tail()
        );
    }
    // After Running: garbage on the control channel ends the workload and the VM.
    let config = sh(&c, "echo up; sleep 1000");
    let mut s = c.start(&fixtures().base, config, Reply::Config, Vec::new());
    assert!(s.wait_output("up", BOOT), "console:\n{}", c.tail());
    s.send_raw(&[3, 0, 0, 0, 250, b'{', b'}']).expect("send to the guest");
    let o = s.finish(END);
    ended_cleanly(&c, &o);
    let f = o.init_failed().expect("InitFailed");
    assert_eq!(f.stage, 7);
    assert!(f.message.contains("unknown message type 250"), "{f:?}");
    assert_eq!(o.exited(), None);
}

/// M-1: an image whose `/proc` is a symlink is refused at stage 4, as runc refuses it.
fn a_symlinked_proc_is_refused(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let layers = &fixtures().proc_link;
    let o = c.run(layers, c.config(layers, &["/bin/true"]));
    ended_cleanly(&c, &o);
    let f = o
        .init_failed()
        .unwrap_or_else(|| panic!("no InitFailed: {o:?}\n{}", c.tail()));
    assert_eq!(f.stage, 4, "{f:?}");
    assert!(f.message.contains("the image's /proc is a symlink"), "{f:?}");
    assert_eq!(o.exit_code(), EXIT_INFRA);
}

/// M-2: a host that breaks the protocol right after `Config` stops the boot at
/// the next stage, before the workload starts.
fn a_broken_host_stops_the_boot_before_the_workload(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let config = sh(&c, "echo should-not-run");
    let mut raw = Vec::new();
    kiln_proto::write_message(&mut raw, &HostMessage::Config(Box::new(config.clone()))).unwrap();
    raw.extend_from_slice(&[3, 0, 0, 0, 250, b'{', b'}']);
    let o = c
        .start(&fixtures().base, config, Reply::Raw(raw), Vec::new())
        .finish(END);
    ended_cleanly(&c, &o);
    let f = o
        .init_failed()
        .unwrap_or_else(|| panic!("no InitFailed: {o:?}\n{}", c.tail()));
    assert!((3..=6).contains(&f.stage), "{f:?}");
    assert!(f.message.contains("unknown message type 250"), "{f:?}");
    assert!(!o.running() && o.stdout.is_empty(), "{o:?}");
}

/// A Case whose PID 1 is the hostile program, misbehaving as `mode` says.
fn hostile_init(backend: Backend) -> Option<Case> {
    let mut c = Case::new(backend)?;
    match &fixtures().hostile_init {
        Some(p) => c.init = Some(p.clone()),
        None => {
            assert!(
                !support::require(),
                "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_HOSTILE is unset"
            );
            return None;
        }
    }
    Some(c)
}

/// D-1: a guest flooding valid `Stage` messages is killed at the first repeat.
fn a_guest_flooding_stages_is_killed(backend: Backend) {
    let Some(c) = hostile_init(backend) else { return };
    let o = c.run(&fixtures().base, c.config(&fixtures().base, &["stage-flood"]));
    assert_eq!(o.violation.as_deref(), Some("Stage 3 after stage 3"), "{o:?}");
    assert_eq!((o.exit_code(), o.end.reason), (EXIT_INFRA, EndReason::Killed));
}

/// D-3: a guest that reports `Exited` but keeps its VM running is killed after the bound.
fn a_guest_lingering_after_exited_is_killed(backend: Backend) {
    let Some(c) = hostile_init(backend) else { return };
    let config = c.config(&fixtures().base, &["exited-then-hang"]);
    let spec = c.spec(&fixtures().base, &config, None);
    let vm = c.vmm.create(&spec).expect("create the VM");
    let mut opts = c.options();
    opts.end_timeout = Duration::from_secs(2);
    let o = support::driver::Session::start(vm, config, Reply::Config, Vec::new(), opts).finish(END);
    assert_eq!((o.exit_code(), o.violation.as_deref()), (0, None), "{o:?}");
    assert_eq!(o.end.reason, EndReason::Killed);
    assert!(o.run.warnings.iter().any(|w| w.contains("did not end")), "{o:?}");
}

/// D-5: a second connection on a stdio port.
fn a_second_stdout_connection_is_a_violation(backend: Backend) {
    let Some(c) = hostile_init(backend) else { return };
    let o = c.run(&fixtures().base, c.config(&fixtures().base, &["second-stdout"]));
    assert_eq!(
        o.violation.as_deref(),
        Some("a second connection on vsock port 1026"),
        "{o:?}"
    );
    assert_eq!(o.exit_code(), EXIT_INFRA);
}

/// D-2: a guest that stops reading cannot block the host's sends or its kill.
fn a_guest_not_reading_control_is_killed(backend: Backend) {
    let Some(c) = hostile_init(backend) else { return };
    let mut s = c.start(
        &fixtures().base,
        c.config(&fixtures().base, &["no-read"]),
        Reply::Config,
        Vec::new(),
    );
    assert!(s.wait_running(BOOT), "console:\n{}", c.tail());
    let h = s.handle();
    let started = std::time::Instant::now();
    for _ in 0..100_000 {
        h.signal(1);
    }
    assert!(started.elapsed() < Duration::from_secs(5));
    let o = s.finish(END);
    assert_eq!(
        o.violation.as_deref(),
        Some("the guest is not reading its control channel"),
        "{o:?}"
    );
    assert_eq!(o.end.reason, EndReason::Killed);
}

/// Spec §9.7: a guest that is not running within the boot timeout is killed (125).
fn the_boot_timeout_kills_a_stuck_guest(backend: Backend) {
    let Some(c) = hostile_init(backend) else { return };
    let config = c.config(&fixtures().base, &["hang-before-running"]);
    let spec = c.spec(&fixtures().base, &config, None);
    let vm = c.vmm.create(&spec).expect("create the VM");
    let mut opts = c.options();
    opts.boot_timeout = Duration::from_secs(5);
    let o = support::driver::Session::start(vm, config, Reply::Config, Vec::new(), opts).finish(END);
    assert!(
        o.violation.as_deref().unwrap_or_default().starts_with("boot timeout"),
        "{o:?}"
    );
    // Under load the guest may not even have reached Hello in 5 s.
    assert!(!o.running() && o.run.stages.len() <= 1, "{o:?}");
    assert_eq!(o.exit_code(), EXIT_INFRA);
}

macro_rules! boot_tests {
    ($($name:ident),* $(,)?) => {
        mod firecracker {
            $( #[test] fn $name() { super::$name(vmkit::Backend::Firecracker) } )*
        }
        mod cloud_hypervisor {
            $( #[test] fn $name() { super::$name(vmkit::Backend::CloudHypervisor) } )*
        }
    };
}

boot_tests!(
    exit_codes_and_stages,
    stdio_is_binary_safe_and_eof_propagates,
    output_is_drained_and_background_processes_do_not_hang_the_exit,
    tty_mode_with_window_size_and_ctrl_c,
    shutdown_sends_the_stop_signal_then_kills_after_the_grace,
    signals_are_forwarded,
    missing_and_non_executable_entrypoints,
    init_failures_are_sanitised_on_the_host,
    user_groups_env_and_workdir,
    layers_and_whiteouts_through_the_overlay,
    many_layers_fit_one_mount_data_page,
    the_scratch_disk_grows_online,
    hostname_hosts_and_resolv_conf_replace_symlinks,
    networking_from_config,
    a_second_hello_makes_the_host_kill_the_vm,
    host_protocol_violations_end_the_guest,
    a_symlinked_proc_is_refused,
    a_broken_host_stops_the_boot_before_the_workload,
    a_guest_flooding_stages_is_killed,
    a_guest_lingering_after_exited_is_killed,
    a_second_stdout_connection_is_a_violation,
    a_guest_not_reading_control_is_killed,
    the_boot_timeout_kills_a_stuck_guest,
);

/// Boot-to-`Running` and whole-run times, sequentially (run with `--ignored --nocapture`).
#[test]
#[ignore = "a measurement, not a check"]
fn measure_boot_to_running() {
    let only = std::env::var("KILN_MEASURE_VMM").ok();
    for backend in Backend::ALL {
        if only.as_deref().is_some_and(|v| v != format!("{backend:?}")) {
            continue;
        }
        let Some(c) = Case::new(backend) else { return };
        let n: usize = std::env::var("KILN_MEASURE_RUNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let (mut hello, mut running, mut total) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..n {
            let started = std::time::Instant::now();
            let o = c.run(&fixtures().base, c.config(&fixtures().base, &["/bin/true"]));
            if o.exit_code() != 0 {
                eprintln!("{backend:?}: a run failed: {:?}", o.violation);
                continue;
            }
            total.push(started.elapsed().as_millis());
            hello.push(o.run.hello_after.unwrap().as_millis());
            running.push(o.run.running_after.unwrap().as_millis());
        }
        let stats = |v: &mut Vec<u128>| {
            v.sort();
            format!(
                "median {} ms, min {}, max {} (n={})",
                v[v.len() / 2],
                v[0],
                v[v.len() - 1],
                v.len()
            )
        };
        eprintln!(
            "{backend:?}: VM start to Hello {}; to Running {}; create+boot+run+end {}",
            stats(&mut hello),
            stats(&mut running),
            stats(&mut total)
        );
    }
}
