//! Build mode on real VMs (spec §9.8): steps stacked and sealed onto the output
//! disk, read back and converted by kiln into an image that runs. See `support`
//! for what the suite needs; without it every test is skipped.
#![cfg(target_os = "linux")]

mod support;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kiln_erofs::testtar::{Opts, TarBuilder};
use kiln_erofs::{Image, LayerWriter, Limits};
use kiln_proto::{Command, StepSealed};
use kiln_run::build::{BuildSpec, Exit, KILL_WAIT, RunStep, STEP_GRACE, Secret, StepOutcome, Stream};
use kiln_run::{BuildVm, VmmKind};
use kiln_store::Store;
use support::{Case, fixtures};
use vmkit::Backend;

const SECRET: &[u8] = b"kiln-secret-7f3a9c";
const OUTPUT: u64 = 256 << 20;

fn kind(backend: Backend) -> VmmKind {
    match backend {
        Backend::Firecracker => VmmKind::Firecracker,
        Backend::CloudHypervisor => VmmKind::CloudHypervisor,
    }
}

/// A build VM over `layers` with one cache disk at `/root/.cache`.
fn spec(c: &Case, backend: Backend, layers: Vec<PathBuf>, root: usize) -> BuildSpec {
    let env = support::env().unwrap();
    let dir = c.dir.path();
    let output = dir.join("output.img");
    std::fs::File::create(&output).unwrap().set_len(OUTPUT).unwrap();
    let cache = dir.join("cache.img");
    kiln_image::scratch::create_scratch(&cache, 128 << 20).unwrap();
    BuildSpec {
        vmm: kind(backend),
        kernel: env.kernel.clone(),
        init: env.init.clone(),
        layers,
        root,
        cache_disks: vec![(cache, "/root/.cache".into())],
        output,
        output_size: OUTPUT,
        network: false,
        memory_mib: 512,
        cpus: 1,
        scratch_bytes: 1 << 30,
        boot_timeout: support::BOOT,
        source_date_epoch: None,
    }
}

fn store() -> Store {
    Store::open(fixtures().dir.join("store")).unwrap()
}

fn sh(id: u32, script: &str) -> RunStep {
    RunStep {
        id,
        command: Command::Shell(script.into()),
        env: BTreeMap::new(),
        workdir: "/".into(),
        uid: 0,
        gid: 0,
        lowers_added: Vec::new(),
        mounts: Vec::new(),
        secrets: Vec::new(),
        network: false,
        timeout: Duration::from_secs(60),
    }
}

/// Runs a step, collecting its output by stream.
fn run(vm: &mut BuildVm, step: RunStep) -> (StepOutcome, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let o = vm
        .run_step(step, &mut |s, b| match s {
            Stream::Stdout => out.extend_from_slice(b),
            Stream::Stderr => err.extend_from_slice(b),
        })
        .expect("the step runs");
    (
        o,
        String::from_utf8_lossy(&out).into(),
        String::from_utf8_lossy(&err).into(),
    )
}

/// The sealed range, as potter reads it: in bounds, aligned, after the
/// previous diff, hashed by the reader, zeros after the tar's end.
fn read_diff(output: &Path, s: &StepSealed, after: u64) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek, SeekFrom};
    assert!(
        s.offset.is_multiple_of(4096) && s.len.is_multiple_of(4096) && s.offset >= after,
        "{s:?}"
    );
    assert!(s.offset.checked_add(s.len).is_some_and(|end| end <= OUTPUT), "{s:?}");
    let mut f = std::fs::File::open(output).unwrap();
    f.seek(SeekFrom::Start(s.offset)).unwrap();
    let mut buf = vec![0u8; s.len as usize];
    f.read_exact(&mut buf).unwrap();
    let hex: String = Sha256::digest(&buf).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, s.sha256, "the guest's hash matches what was read");
    buf
}

/// The diff's paths (as kiln's converter reads them), whiteouts as `.wh.` names.
fn diff_paths(tar: &[u8]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let mut w = LayerWriter::new(Cursor::new(Vec::new()), dir.path(), Limits::default()).unwrap();
    w.append_tar(tar).unwrap();
    let (out, _) = w.finish(&BTreeMap::new()).unwrap();
    let mut img = Image::open(Cursor::new(out.into_inner())).unwrap();
    let mut paths = Vec::new();
    let mut queue = vec![(String::new(), img.root_nid())];
    while let Some((path, nid)) = queue.pop() {
        let info = img.inode(nid).unwrap();
        if info.is_dir() {
            let opaque = img
                .xattrs(nid)
                .unwrap()
                .into_keys()
                .any(|k| k.full_name().ends_with(b"overlay.opaque"));
            if opaque {
                paths.push(format!("{path}/.wh..wh..opq"));
            }
            for e in img.read_dir(nid).unwrap() {
                queue.push((format!("{path}/{}", String::from_utf8_lossy(&e.name)), e.nid));
            }
        }
        if info.is_whiteout() {
            let (parent, name) = path.rsplit_once('/').unwrap();
            paths.push(format!("{parent}/.wh.{name}"));
        } else if !path.is_empty() {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

fn build_steps_seal_stack_and_convert(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let f = fixtures();
    // The base image, then a layer stacked only by step 2.
    let layers = vec![f.base[0].clone(), f.stack[3].clone()];
    let spec = spec(&c, backend, layers, 1);
    let output = spec.output.clone();
    let started = Instant::now();
    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
    let booted = started.elapsed();
    let long = "n".repeat(200);

    // Step 1: files, a hard link, long names, a cache write, a secret, a FIFO,
    // stdout and stderr apart, a working directory created as root.
    let mut s1 = sh(
        1,
        &format!(
            "set -e; pwd; mkdir -p /data /opq/deep; echo a > /data/a; echo b > /data/b; ln /data/b /data/hard; \
             echo x > /opq/x; mkdir -p /data/{long}; echo long > /data/{long}/{long}; \
             cat /run/secrets/token; echo cached > /root/.cache/c; echo err >&2; touch here"
        ),
    );
    s1.workdir = "/work/sub".into();
    s1.secrets = vec![("/run/secrets/token".into(), Secret(SECRET.to_vec()))];
    s1.mounts = vec!["/root/.cache".into()];
    let t1 = Instant::now();
    let (o1, out1, err1) = run(&mut vm, s1);
    let step1 = t1.elapsed();
    assert_eq!(o1.exit, Exit::Code(0), "{out1}\n{err1}");
    assert_eq!(out1, format!("/work/sub\n{}", String::from_utf8_lossy(SECRET)));
    assert_eq!(err1, "err\n");
    let seal1 = o1.sealed.expect("sealed");
    assert_eq!((seal1.id, seal1.offset), (1, 0));

    // Step 2: stack layer 1, delete a lower file (whiteout), replace a lower
    // directory (opaque), read the cache disk back.
    let mut s2 = sh(
        2,
        "set -e; cat /layers/03; cat /root/.cache/c; rm /data/a; rm -rf /opq; mkdir /opq; echo y > /opq/y; \
         test -f /work/sub/here",
    );
    s2.lowers_added = vec![1];
    s2.mounts = vec!["/root/.cache".into()];
    let (o2, out2, err2) = run(&mut vm, s2);
    assert_eq!(o2.exit, Exit::Code(0), "{out2}\n{err2}");
    assert_eq!(out2, "3\ncached\n");
    let seal2 = o2.sealed.expect("sealed");
    assert_eq!(seal2.offset, seal1.offset + seal1.len, "diffs are packed, aligned");

    // Step 3 fails: its changes are discarded.
    let (o3, _, _) = run(&mut vm, sh(3, "echo bad > /f; exit 3"));
    assert_eq!(
        o3,
        StepOutcome {
            exit: Exit::Code(3),
            sealed: None
        }
    );

    // Step 4: as uid 1000 in a new working directory, on top of 1 and 2 only.
    // Its secret is mode 0400, owned by the step's uid and gid (ruling 4).
    let mut s4 = sh(
        4,
        "id -u; id -g; pwd; test ! -e /f && test ! -e /data/a && cat /opq/y; stat -c '%a %u %g' /run/secrets/t",
    );
    (s4.uid, s4.gid, s4.workdir) = (1000, 1000, "/home/app/w".into());
    s4.secrets = vec![("/run/secrets/t".into(), Secret(SECRET.to_vec()))];
    let (o4, out4, err4) = run(&mut vm, s4);
    assert_eq!(o4.exit, Exit::Code(0), "{out4}\n{err4}");
    assert_eq!(out4, "1000\n1000\n/home/app/w\ny\n400 1000 1000\n");
    let seal4 = o4.sealed.expect("sealed");
    // A command that does not exist: 127, with kiln-init's message on stderr.
    let mut missing = sh(5, "x");
    missing.command = Command::Argv(vec!["/nonexistent".into()]);
    let (o5, _, err5) = run(&mut vm, missing);
    assert_eq!(o5.exit, Exit::Code(127), "{err5}");
    assert!(err5.contains("exec /nonexistent"), "{err5}");
    vm.finish().expect("the guest ends the VM");

    let d1 = read_diff(&output, &seal1, 0);
    let d2 = read_diff(&output, &seal2, seal1.offset + seal1.len);
    let d4 = read_diff(&output, &seal4, seal2.offset + seal2.len);
    for (d, s) in [(&d1, &seal1), (&d2, &seal2), (&d4, &seal4)] {
        assert!(
            !d.windows(SECRET.len()).any(|w| w == SECRET),
            "a secret in diff {}",
            s.id
        );
    }
    let p1 = diff_paths(&d1);
    let want1 = [
        "/data".to_string(),
        "/data/a".into(),
        "/data/b".into(),
        "/data/hard".into(),
        format!("/data/{long}"),
        format!("/data/{long}/{long}"),
        "/opq".into(),
        "/opq/deep".into(),
        "/opq/x".into(),
        // Copied up when the cache disk's mount point was made (and removed).
        "/root".into(),
        "/work".into(),
        "/work/sub".into(),
        "/work/sub/here".into(),
    ];
    let mut want1 = want1.to_vec();
    want1.sort();
    assert_eq!(p1, want1, "no mount points, /etc files, secrets or cache content");
    assert_eq!(seal1.entries, want1.len() as u64);
    let p2 = diff_paths(&d2);
    assert_eq!(
        p2,
        ["/data", "/data/.wh.a", "/opq", "/opq/.wh..wh..opq", "/opq/y", "/root"],
        "{p2:?}"
    );
    let p4 = diff_paths(&d4);
    assert_eq!(p4, ["/home", "/home/app", "/home/app/w"], "{p4:?}");

    // kiln converts the base and the sealed diffs into an image that runs.
    let image = support::convert_tars(&format!("built-{backend:?}"), vec![support::base_tar(), d1, d2]);
    let config = c.config(
        &image,
        &[
            "/bin/sh",
            "-c",
            "cat /data/b /data/hard /opq/y; test ! -e /data/a && test ! -e /opq/x && ls /work/sub",
        ],
    );
    let o = c.run(&image, config);
    assert_eq!(o.exit_code(), 0, "{o:?}\n{}", c.tail());
    assert_eq!(o.stdout(), "b\nb\ny\nhere\n");
    eprintln!(
        "{backend:?}: build VM boot {} ms, step 1 {} ms; diffs {} + {} + {} bytes",
        booted.as_millis(),
        step1.as_millis(),
        seal1.len,
        seal2.len,
        seal4.len
    );
}

/// Ruling 1: a root step has no block device in its `/dev`, cannot mount one,
/// cannot make (or open) a node for the output disk or a layer disk in `/dev` or
/// in `/tmp`, and cannot reach init's root (and its `/dev`) through `/proc/1/root`.
fn steps_cannot_reach_the_output_disk(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    // The disks in order: init (vda), scratch (vdb), the output (vdc), then the layers.
    let disks: Vec<&str> = ["vdc", "vdd"].into_iter().take(1 + spec.layers.len().min(1)).collect();
    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
    // Each probe prints `name: <its own stderr, shell redirection errors included>`
    // (or `name: ok` if it succeeded), so the host can tell EPERM or EROFS from
    // ENOENT, a missing applet or a usage error.
    let per_disk = "test ! -e /dev/@D@ && echo no-@D@; \
                    d=$(cat /sys/block/@D@/dev); echo dev-@D@=$d; M=${d%:*}; m=${d#*:}; \
                    t mknod-/dev:@D@ \"mknod /dev/@D@ b $M $m\"; t open-/dev:@D@ 'head -c 1 /dev/@D@'; \
                    t mknod-/tmp:@D@ \"mknod /tmp/@D@ b $M $m\"; t open-/tmp:@D@ 'head -c 1 /tmp/@D@'; rm -f /tmp/@D@; \
                    t proc-root:@D@ 'head -c 1 /proc/1/root/dev/@D@'; \
                    t sys:@D@ 'echo x > /sys/block/@D@/uevent'; ";
    let script = format!(
        "t() {{ n=$1; r=$(eval \"$2\" 2>&1) && echo \"$n: ok\" || echo \"$n: $r\" | tr '\\n' ' '; echo; }}; \
         ls /dev | tr '\\n' ' '; echo; id -u; {} \
         t mount 'mount -t devtmpfs d /tmp'; \
         t core-pattern 'echo core > /proc/sys/kernel/core_pattern'; \
         t modprobe 'echo /sbin/modprobe > /proc/sys/kernel/modprobe'; \
         t sysrq 'echo h > /proc/sysrq-trigger'; \
         echo kcore-$(head -c 16 /proc/kcore 2>/dev/null | wc -c)",
        disks.iter().map(|d| per_disk.replace("@D@", d)).collect::<String>()
    );
    let (o, out, err) = run(&mut vm, sh(1, &script));
    assert_eq!(o.exit, Exit::Code(0), "{out}\n{err}");
    let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    let find = |prefix: &str| -> &str {
        lines
            .iter()
            .find_map(|l| l.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("no line starting {prefix:?} in the probe output:\n{out}"))
    };
    // `/dev` lists only the character devices; the script runs as root.
    let [listing, uid] = [lines.first(), lines.get(1)];
    assert_eq!(
        listing,
        Some(&"fd full null ptmx pts random shm stderr stdin stdout tty urandom zero "),
        "{out}"
    );
    assert_eq!(uid, Some(&"0"), "{out}");
    // Every probe failed for the reason that keeps the step out, not for a
    // missing path, applet or argument.
    const EPERM: &str = "Operation not permitted";
    const EROFS: &str = "Read-only file system";
    for d in &disks {
        // No node in `/dev`, and the disk's major:minor was really read.
        assert!(
            lines.contains(&format!("no-{d}").as_str()),
            "{d} is in the step's /dev:\n{out}"
        );
        let dev = find(&format!("dev-{d}="));
        let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        assert!(
            dev.split_once(':').is_some_and(|(a, b)| numeric(a) && numeric(b)),
            "{d}: {out}"
        );
        for (name, reason) in [
            ("mknod-/dev", EROFS),
            ("mknod-/tmp", EPERM),
            ("proc-root", "Permission denied"),
            ("sys", EROFS),
        ] {
            let line = find(&format!("{name}:{d}: "));
            assert!(
                line.contains(reason),
                "{name}:{d}: wanted {reason:?}, got {line:?}\n{out}"
            );
        }
        // Opening a node is refused too (in /dev and /tmp no node exists to open).
        for name in ["open-/dev", "open-/tmp"] {
            let line = find(&format!("{name}:{d}: "));
            assert!(!line.is_empty() && line != "ok", "{name}:{d}: {line:?}\n{out}");
        }
    }
    for (name, reason) in [
        ("mount", "permission denied"),
        ("core-pattern", EROFS),
        ("modprobe", EROFS),
        ("sysrq", EROFS),
    ] {
        let line = find(&format!("{name}: "));
        assert!(line.contains(reason), "{name}: wanted {reason:?}, got {line:?}\n{out}");
    }
    // Reading /proc/kcore yields nothing because the step lacks CAP_SYS_RAWIO
    // (not because of the mask over it).
    assert_eq!(lines.last(), Some(&"kcore-0"), "{out}");
    vm.finish().expect("the guest ends the VM");
}

/// A step whose mounts cannot be set up (a secret whose parent is a regular
/// file in the image) ends with 125 and the reason on its stderr; the VM goes on
/// and the next step runs, on the state before the failed one: its diff holds no
/// stub or copied-up entry of the failed step, and its `/proc` and `/dev` are one
/// mount each.
fn a_failed_mount_setup_fails_the_step_not_the_vm(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    let output = spec.output.clone();
    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
    let mut bad = sh(1, "echo must-not-run > /ran; echo must-not-run");
    bad.secrets = vec![("/bin/busybox/token".into(), Secret(SECRET.to_vec()))];
    let (o, out, err) = run(&mut vm, bad);
    assert_eq!(
        o,
        StepOutcome {
            exit: Exit::Code(125),
            sealed: None
        },
        "{out}\n{err}"
    );
    assert_eq!(out, "");
    assert!(
        err.contains("set up the step's mounts") && err.contains("/bin/busybox/token"),
        "{err}"
    );
    assert!(!err.contains(&String::from_utf8_lossy(SECRET).to_string()), "{err}");
    // The VM is still there, and the failed step left nothing behind. `/proc`
    // and `/dev` are each mounted once (the failed step's mounts were torn down
    // and not stacked under this one's).
    let mounts = |dir: &str| format!("grep -c '^[0-9]* [0-9]* [0-9]*:[0-9]* [^ ]* {dir} ' /proc/self/mountinfo");
    let script = format!(
        "test ! -e /ran && echo ok; echo proc=$({}); echo dev=$({})",
        mounts("/proc"),
        mounts("/dev")
    );
    let (o, out, err) = run(&mut vm, sh(2, &script));
    assert_eq!(o.exit, Exit::Code(0), "{out}\n{err}");
    assert_eq!(out, "ok\nproc=1\ndev=1\n", "{err}");
    let seal = o.sealed.expect("sealed");
    let diff = read_diff(&output, &seal, 0);
    assert_eq!(
        diff_paths(&diff),
        Vec::<String>::new(),
        "the failed step's stubs or copied-up parents reached the next diff"
    );
    vm.finish().expect("the guest ends the VM");
}

/// A layer a step adds can make a fixed mount target unresolvable: here `/dev`
/// is a regular file in it. The step ends with 125 and the reason on its stderr
/// (the target is the step's doing, though init mounts `/dev` itself); the VM
/// goes on. A stacked layer stays stacked, so the next step runs once it stacks a
/// layer that puts a directory back at `/dev`.
fn a_layer_that_breaks_a_fixed_target_fails_the_step_not_the_vm(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let dev_file = TarBuilder::new()
        .file("dev", b"not a directory", &Opts::default())
        .finish();
    let dev_dir = TarBuilder::new().dir("dev", &Opts::default().mode(0o755)).finish();
    let layers = support::convert_tars("dev-file", vec![support::base_tar(), dev_file, dev_dir]);
    let spec = spec(&c, backend, layers, 1);
    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
    let mut bad = sh(1, "echo must-not-run");
    bad.lowers_added = vec![1];
    let (o, out, err) = run(&mut vm, bad);
    assert_eq!(
        o,
        StepOutcome {
            exit: Exit::Code(125),
            sealed: None
        },
        "{out}\n{err}"
    );
    assert_eq!(out, "");
    assert!(
        err.contains("set up the step's mounts") && err.contains("/dev"),
        "the reason is on stderr: {err}"
    );
    let mut next = sh(2, "test -d /dev && echo ok");
    next.lowers_added = vec![2];
    let (o, out, err) = run(&mut vm, next);
    assert_eq!(o.exit, Exit::Code(0), "{out}\n{err}");
    assert_eq!(out, "ok\n");
    assert!(o.sealed.is_some());
    vm.finish().expect("the guest ends the VM");
}

/// A cache target that is a regular file in the image (`/etc/passwd` here, the
/// spec's cache target) fails the step with 125, not the VM.
fn a_cache_target_that_is_a_file_fails_the_step_not_the_vm(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let mut spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    spec.cache_disks[0].1 = "/etc/passwd".into();
    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
    let mut bad = sh(1, "echo must-not-run");
    bad.mounts = vec!["/etc/passwd".into()];
    let (o, out, err) = run(&mut vm, bad);
    assert_eq!(
        o,
        StepOutcome {
            exit: Exit::Code(125),
            sealed: None
        },
        "{out}\n{err}"
    );
    assert!(
        err.contains("set up the step's mounts") && err.contains("/etc/passwd"),
        "{err}"
    );
    let (o, out, err) = run(&mut vm, sh(2, "echo still-works"));
    assert_eq!(o.exit, Exit::Code(0), "{out}\n{err}");
    assert_eq!(out, "still-works\n");
    vm.finish().expect("the guest ends the VM");
}

/// Before `Running` kiln-init sets `max_user_namespaces` to 0: a step cannot
/// create a user namespace, and the steps after the attempt still run.
fn a_step_cannot_create_user_namespaces(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
    let script = "cat /proc/sys/user/max_user_namespaces; unshare -U true 2>&1; echo rc=$?";
    let (o, out, err) = run(&mut vm, sh(1, script));
    assert_eq!(o.exit, Exit::Code(0), "{out}\n{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.first(), Some(&"0"), "{out}");
    let rc = lines.last().and_then(|l| l.strip_prefix("rc=")).unwrap_or_default();
    assert!(
        !rc.is_empty() && rc != "0" && rc != "127",
        "unshare -U must fail, not be missing: {out}"
    );
    assert!(out.contains("No space left on device"), "{out}");
    let (o, out, err) = run(&mut vm, sh(2, "echo still-works"));
    assert_eq!(o.exit, Exit::Code(0), "{out}\n{err}");
    assert_eq!(out, "still-works\n");
    vm.finish().expect("the guest ends the VM");
}

/// Ruling 3: with a `SOURCE_DATE_EPOCH` every entry's mtime is at most it, a
/// parent directory copied up for a mount point included; without one the
/// guest's times are kept.
fn source_date_epoch_clamps_the_diff(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    const EPOCH: u64 = 1_000_000_000;
    let mut mtimes = Vec::new();
    for epoch in [Some(EPOCH), None] {
        // A fresh output and cache disk for each VM.
        for f in ["output.img", "cache.img"] {
            let _ = std::fs::remove_file(c.dir.path().join(f));
        }
        let mut spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
        spec.source_date_epoch = epoch;
        let output = spec.output.clone();
        let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
        let mut s = sh(1, "mkdir -p /new/dir && echo x > /new/dir/f && echo c > /root/.cache/c");
        s.mounts = vec!["/root/.cache".into()];
        let (o, out, err) = run(&mut vm, s);
        assert_eq!(o.exit, Exit::Code(0), "{out}\n{err}");
        vm.finish().expect("the guest ends the VM");
        let diff = read_diff(&output, &o.sealed.expect("sealed"), 0);
        mtimes.push(support::tar_mtimes(&diff));
    }
    let names = |m: &[(String, u64)]| m.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
    assert_eq!(names(&mtimes[0]), ["new/", "new/dir/", "new/dir/f", "root/"]);
    assert_eq!(names(&mtimes[0]), names(&mtimes[1]));
    assert!(mtimes[0].iter().all(|(_, t)| *t == EPOCH), "{:?}", mtimes[0]);
    assert!(mtimes[1].iter().all(|(_, t)| *t > EPOCH), "{:?}", mtimes[1]);
}

/// A build VM needs a version 3 `kiln-init`; an older one (`KILN_TEST_INIT_V2`,
/// built from the base of this branch) is refused at boot, naming the version.
fn a_version_2_guest_is_refused_build_mode(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let Some(v2) = support::env().unwrap().init_v2.clone() else {
        eprintln!("skipped: KILN_TEST_INIT_V2 is unset");
        return;
    };
    let mut spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    spec.init = v2;
    let err = BuildVm::start(&store(), spec).err().expect("refused").to_string();
    assert!(err.contains("build mode needs") && err.contains("version 3"), "{err}");
}

/// The VMM's identity, from the run directory's `run.json`.
fn vmm_of(vm: &BuildVm) -> kiln_run::rundir::Identity {
    let json = std::fs::read(vm.run_dir().expect("a run directory").join("run.json")).expect("read run.json");
    let info: kiln_run::rundir::RunInfo = serde_json::from_slice(&json).expect("parse run.json");
    info.vmm.expect("the VMM's identity is recorded once it runs")
}

fn a_step_past_its_timeout_is_killed(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
    let vmm = vmm_of(&vm);
    assert!(kiln_run::rundir::alive(&vmm), "the VMM runs before the step: {vmm:?}");
    let mut s = sh(1, "echo started; sleep 600");
    s.timeout = Duration::from_secs(5);
    let t = Instant::now();
    let (o, out, _) = run(&mut vm, s);
    let took = t.elapsed();
    assert_eq!(
        o,
        StepOutcome {
            exit: Exit::TimedOut,
            sealed: None
        }
    );
    assert_eq!(out, "started\n");
    assert!(took < Duration::from_secs(5) + STEP_GRACE + KILL_WAIT, "{took:?}");
    // kiln killed the VM: its VMM process is gone, not only refused more steps.
    assert!(
        !kiln_run::rundir::alive(&vmm),
        "the VMM outlived the timed-out step: {vmm:?}"
    );
    let err = vm.run_step(sh(2, "true"), &mut |_, _| {}).unwrap_err().to_string();
    assert!(err.contains("timed out"), "{err}");
    // A timed-out step ends the VM, so Finish reports it too.
    assert!(vm.finish().is_err());
    eprintln!("{backend:?}: a 5 s timeout returned after {} ms", took.as_millis());
}

/// A hostile guest seals its step far outside the output disk: kiln refuses the
/// claim itself (`BuildProtocol`), `run_step` fails naming the check, and the VM
/// is ended.
fn a_seal_outside_the_disk_is_refused_by_kiln(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let Some(hostile) = support::env().unwrap().hostile.clone() else {
        assert!(
            !support::require(),
            "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_HOSTILE is unset"
        );
        return;
    };
    let mut spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    // The init layer is built from this binary: the hostile program is PID 1.
    spec.init = hostile;
    let mut vm = BuildVm::start(&store(), spec).expect("boot the hostile build VM");
    let err = vm
        .run_step(sh(1, "true"), &mut |_, _| {})
        .expect_err("kiln refuses the seal")
        .to_string();
    assert!(
        err.contains("StepSealed for step 1") && err.contains("is refused") && err.contains("does not start where"),
        "{err}"
    );
    // The VM is gone: no more steps.
    let err = vm.run_step(sh(2, "true"), &mut |_, _| {}).unwrap_err().to_string();
    assert!(err.contains("ended") || err.contains("StepSealed"), "{err}");
    assert!(vm.finish().is_err());
}

/// Egress is per step in a VM with a network: off, on, off again, through
/// vmkit's `Vm::set_egress` (needs pasta, nft and the internet: KILN_TEST_NET=1).
/// Each step probes a new TCP connection to 1.1.1.1:80 (printing `nc`'s own exit
/// status and the first bytes of the reply) and one `nslookup` through each of two
/// resolvers (the gateway, which is the guest's resolver, and 1.1.1.1 directly),
/// each under its own time limit. Open steps must connect (the open probe is
/// retried up to 3 times, as the internet may blink; the closed probes never are).
/// Closed steps must neither connect nor resolve. A closed probe fails by its own
/// exit status, not because its applet is missing: `nc` and `nslookup` must not
/// return 127. A closed `nslookup` is expected to return 143 (`timeout`'s SIGTERM,
/// 128 + 15): the gate drops its queries, it keeps retrying and `timeout` ends it,
/// so 143 counts as blocked.
///
/// Whether an open step's DNS answers depends on the host's resolver and is only
/// reported, not asserted, so a closed step's DNS failure has no positive control
/// where open-step DNS does not work (as in Lima): there it shows only that the
/// lookup did not succeed, and TCP (`nc`) is what proves the gate. Not checked: a
/// connection an earlier step opened, since the guest kills every process when a
/// step's command exits, so no straggler survives into the next step to be cut.
fn egress_follows_each_step(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    if std::env::var_os("KILN_TEST_NET").is_none_or(|v| v != "1") {
        assert!(!support::require(), "KILN_REQUIRE_KVM_TESTS=1 but KILN_TEST_NET is not");
        return;
    }
    let mut spec = spec(&c, backend, vec![fixtures().base[0].clone()], 1);
    spec.network = true;
    let mut vm = BuildVm::start(&store(), spec).expect("boot the build VM");
    let gateway = kiln_run::config::GATEWAY;
    // `nc` ends the pipeline, so `$?` is its own exit status (or `timeout`'s 143
    // if it hangs). DNS goes to the guest's resolver (the gateway) and straight to
    // 1.1.1.1. Every probe is bounded (10 + 8 + 8 s, well under the step's 60 s), so
    // a probe that hangs fails that probe instead of timing out the step, which
    // would kill the VM before the open probe could be retried. `t=` is each
    // probe's time in seconds, for diagnosis.
    let probe = format!(
        "s=$(date +%s); printf 'HEAD / HTTP/1.0\\r\\n\\r\\n' | timeout 10 nc -w 5 1.1.1.1 80 > /dev/shm/rep 2>/dev/null; rc=$?; \
         echo \"nc={{$rc}} head=[$(head -c 8 /dev/shm/rep)] t=$(($(date +%s) - s))\"; \
         s=$(date +%s); timeout 8 nslookup example.com {gateway} >/dev/null 2>&1; \
         echo \"dns={{$?}} t=$(($(date +%s) - s))\"; \
         s=$(date +%s); timeout 8 nslookup example.com 1.1.1.1 >/dev/null 2>&1; \
         echo \"dnsx={{$?}} t=$(($(date +%s) - s))\""
    );
    let mut next_id = 1;
    // (label, network) in order; the open step may take up to 3 attempts.
    let mut report = Vec::new();
    for (index, open) in [(1, false), (2, true), (3, false)] {
        let attempts = if open { 3 } else { 1 };
        for attempt in 1..=attempts {
            let mut s = sh(next_id, &probe);
            next_id += 1;
            s.network = open;
            let (o, out, err) = run(&mut vm, s);
            assert_eq!(o.exit, Exit::Code(0), "step {index}: {out}\n{err}");
            let field = |k: &str| {
                out.split_once(&format!("{k}={{"))
                    .and_then(|(_, r)| r.split_once('}'))
                    .map(|(v, _)| v.to_string())
                    .unwrap_or_default()
            };
            let (nc, dns, dnsx) = (field("nc"), field("dns"), field("dnsx"));
            let connected = nc == "0" && out.contains("head=[HTTP/1.");
            let context = format!(
                "step {index} (attempt {attempt}/{attempts}, id {}), network expected {}: nc rc {nc:?}, dns rc {dns:?}, dns to 1.1.1.1 rc {dnsx:?}, \
                 stdout {out:?}, stderr {err:?}",
                next_id - 1,
                if open { "open" } else { "closed" }
            );
            report.push(context.clone());
            if open {
                if connected {
                    break;
                }
                assert!(attempt < attempts, "egress did not open: {context}");
            } else {
                assert!(!connected, "egress did not close: {context}");
                // 127 is a missing applet, not a blocked network.
                assert!(
                    nc != "0" && nc != "127" && !nc.is_empty(),
                    "a closed step's nc must fail by itself: {context}"
                );
                for (what, rc) in [("DNS", &dns), ("DNS to 1.1.1.1", &dnsx)] {
                    assert!(
                        rc != "0" && rc != "127" && !rc.is_empty(),
                        "a closed step's {what} must fail, and not by a missing applet: {context}"
                    );
                }
            }
        }
    }
    eprintln!("{backend:?}: egress per step:\n{}", report.join("\n"));
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
    build_steps_seal_stack_and_convert,
    steps_cannot_reach_the_output_disk,
    a_failed_mount_setup_fails_the_step_not_the_vm,
    a_layer_that_breaks_a_fixed_target_fails_the_step_not_the_vm,
    a_cache_target_that_is_a_file_fails_the_step_not_the_vm,
    a_step_cannot_create_user_namespaces,
    source_date_epoch_clamps_the_diff,
    a_version_2_guest_is_refused_build_mode,
    a_step_past_its_timeout_is_killed,
    a_seal_outside_the_disk_is_refused_by_kiln,
    egress_follows_each_step,
);
