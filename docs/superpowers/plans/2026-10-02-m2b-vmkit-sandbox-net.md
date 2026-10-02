# M2b: vmkit sandbox and networking Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run every VMM that `vmkit` starts unprivileged inside its own user, PID, mount and network namespaces, with a minimal root, and give guests a policed network: a tap and an nftables policy in the VM's own namespace, and `pasta` for egress and port forwards.

**Architecture:**
- **Helper binary.** A small helper, `vmkit-sandbox`, does the namespace work, as bubblewrap does, because Ubuntu 23.10 and later let only AppArmor-profiled binaries create user namespaces.
- **Plan file.** The library writes a JSON plan into the run directory and spawns `vmkit-sandbox run <plan>`, inside a systemd user scope with memory, CPU and task limits when one is available.
- **Outer helper.** It unshares the user and PID namespaces. It passes `SYS_ADMIN` and `NET_ADMIN` to its child as ambient capabilities, attaches `pasta` to the VM's network namespace, then mirrors how the VMM ended.
- **Child (`init`, PID 1).** It unshares the mount and network namespaces, creates the tap and loads the nftables policy, and builds a read-only tmpfs root from descriptor-attached binds. It then drops everything and execs the VMM.
- **Drivers.** They address every resource by fixed in-sandbox paths (`/vm/kernel`, `/vm/disk/<n>`, `/vm/sock/...`).

**Tech Stack:**
- Rust 2024; new runtime dependencies `serde` (derive) and `rustix` 1.1 (Linux only).
- `passt`/`pasta` 2024-02-20 or later, `nftables`, `iproute2`, systemd user scopes.
- AppArmor profiles (Ubuntu); Firecracker 1.17.0 and Cloud Hypervisor 53.0 as in M2a; a busybox test guest.

**Spec:** `/Users/alfonso/Github/Personal/playground/kiln/docs/superpowers/specs/2026-09-30-kiln-design.md` (rev 2.4). This plan implements the rest of milestone M2:
- §9.2: the sandbox;
- §9.3: networking;
- §4.1: `$VMKIT_PASTA` and the `net` and `sandbox` modules;
- §11.3: the sandbox-contents contract test;
- §11.5: the hostile-guest network tests;
- §14: items 1 and 2, now wired up.

It builds on plan M2a (`kiln` repo, `docs/superpowers/plans/2026-10-02-m2a-vmkit-core.md`), which this repository's history implements.

## Global Constraints

- **Repository:**
  - `/Users/alfonso/Github/Personal/playground/vmkit` (GitHub `AlfonsoCampodonico/vmkit`, private).
  - Work on a new branch `m2b-sandbox-net` created from `m2a-vmkit-core` (PR #1, not yet merged), so the result stacks on PR #1.
  - Do not push; the controller asks the user.
- **Commits:** plain messages with no `Co-Authored-By`, `Claude-Session` or other attribution lines.
- **Unsafe code:**
  - The library keeps `#![forbid(unsafe_code)]`.
  - The helper binary has `#![deny(unsafe_code)]` with exactly two audited `#[allow(unsafe_code)]` blocks: `unshare` (rustix marks it unsafe) and borrowing inherited descriptor numbers to mark them close-on-exec.
- **Platforms:**
  - The crate must build and pass its unit tests on macOS and Linux.
  - The helper is a stub that exits 2 on other platforms.
  - `rustix` is a Linux-only dependency.
- **Fixed in-sandbox layout (kiln spec §9.2):**
  - `/dev/kvm`, `/dev/null`, `/dev/urandom`, and `/dev/net/tun` with a network;
  - `/vmm` (the VMM binary);
  - `/vm/kernel`, `/vm/initramfs`, `/vm/disk/<n>` (0-based, boot order);
  - `/vm/sock/`, which is `<run_dir>/sock` on the host.
  - Everything else in the run directory (the plan, `vmm.pid`, the console and stderr logs) is out of the VMM's reach.
- **Guest network:**
  - The guest is `eth0` at `172.30.0.2/30` with MAC `06:00:ac:1e:00:02`.
  - Gateway and DNS are `172.30.0.1` on `tap0`.
  - pasta's interface is `egress0`, and its DNS forward address is `169.254.1.53`.
- **`Egress::Restricted` denies:**
  - `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10`, `127.0.0.0/8`, `169.254.0.0/16`, `172.16.0.0/12`, `192.0.0.0/24`, `192.168.0.0/16`, `198.18.0.0/15`, `224.0.0.0/3`;
  - every local address of the host (from `/proc/net/fib_trie`).
- **Binary discovery:**
  - `$VMKIT_SANDBOX`, else `vmkit-sandbox` next to the running program, else `PATH`.
  - `$VMKIT_PASTA`, else `PATH`.
  - `ip` and `nft` come from `PATH`, then `/usr/sbin`, `/sbin`, `/usr/bin`, `/bin`.
- **Formatting:** `rustfmt.toml` sets `max_width = 120`. The plan's code is already `cargo fmt`-clean.
- **Validation:**
  - This plan was replayed task by task on a clone of `m2a-vmkit-core` inside the `vmkit` Lima VM (Ubuntu 26.04, 8 vCPUs, nested KVM on an M4 Pro).
  - After every task the replay ran fmt, clippy (`-D warnings`) and the tests, plus macOS clippy, and the KVM steps ran for real.
  - Test counts come from that replay.

## Review Focus

These are inputs no task's main tests target that will bite a real user, most likely first. Each has a pinned test in the task named.

1. **Disks and kernels on a `nosuid,nodev` filesystem**, such as `/tmp` on most distributions, attached read-only.
   - Inside a user namespace a read-only remount must keep the source mount's locked flags, or it fails with `EPERM`. That happened in validation.
   - Pinned in Task 1 (`read_only_binds_stay_read_only_and_writable_ones_reach_the_host` binds from a temporary directory) and Task 2 (the contract suite's disks live in `/tmp`).
2. **A symlink where a disk or kernel should be.** It is never followed, and the error names it.
   - Pinned in Task 1 (`a_symlink_is_never_followed`) and Task 2 (`a_symlinked_disk_is_refused`).
   - Before the fix, the symlink was mounted and the error was a confusing `EINVAL`.
3. **`Vm::kill` followed by `wait`:** when `wait` returns, the VMM is gone and nobody answers its API.
   - Killing only the helper leaves the VMM alive until its death signal arrives, which validation caught.
   - Pinned in Task 2 (`kill_ends_the_vmm_and_its_api`, and `the_vmm_sees_only_its_own_files_and_holds_no_privileges` checks `/proc/<pid>` is gone).
4. **A run directory whose path has a comma** (kiln's runtime directory is user-controlled). It used to be refused for Cloud Hypervisor; now the VMM sees only `/vm/sock`, so it must boot.
   - Pinned in Task 2 (`a_run_dir_with_a_comma_boots`).
5. **A forwarded host port that is already in use.** Creating the VM fails with pasta's own message, not a hang, and no pasta process outlives its VM.
   - Pinned in Task 3 (`a_host_port_in_use_fails_with_pastas_message`, and the pasta check at the end of `port_forwards_reach_the_guest_but_other_vms_do_not`).

## Results of spec §14 for M2 (checked during planning)

- **Item 1 (VMMs in an unprivileged user, mount, PID and net namespace sandbox): true, and implemented here.**
  - Both VMMs run as the invoking user, PID 1 of their own PID namespace, with no capabilities (`CapInh`, `CapPrm`, `CapEff` and `CapAmb` all zero), `NoNewPrivs` 1, and rlimits.
  - Their root holds exactly the fixed layout.
  - Firecracker keeps its own seccomp filter. Cloud Hypervisor runs with `--seccomp true` and Landlock limited to `/vm` and `/dev/net/tun`.
  - Ubuntu needs `scripts/install-apparmor.sh`; Fedora, Arch and Debian need nothing.
- **Item 2 (a tap and nftables in a net namespace owned by a user namespace, with `pasta` attached): true, and implemented here.**
  - Verified with pasta 2024-02-20 (Ubuntu 24.04), 2025-05-03 (Debian 13) and 2026-01-20 (Ubuntu 26.04).

## Design decisions (rulings made while planning)

- **One helper, two processes, a socket handshake.**
  - The outer helper (`run`) stays in the host network namespace so `pasta` can use host sockets.
  - Its child (`init`) becomes PID 1 of the new PID namespace and, after exec, the VMM. Capabilities cross exec only as ambient capabilities: an identity-mapped, non-root user loses everything else.
  - `init` reports `ready` on a socket on its stdin and waits for `go`, which the outer helper sends after attaching `pasta`.
  - End-of-file in place of `go` means the outer helper died. Reading `go` also proves it outlived `init`'s death-signal setup, which closes that race.
- **Killing kills the VMM itself, through a pidfd.**
  - The PID comes from `<run_dir>/vmm.pid` and is used only if its parent is the helper. The helper then reaps the VMM and dies of the same signal.
  - So when `Vm::wait` returns, the VMM is gone. Killing the helper alone left the VMM serving its API for a moment, because the death signal is asynchronous.
- **DNS:** the guest asks `172.30.0.1:53`, which nftables DNATs to pasta's `--dns-forward` address.
  - Spec §9.3's input rule ("allow only DNS from the guest to `.1`") therefore becomes: DNAT DNS in prerouting, and drop every new connection from `tap0` on input.
  - pasta queries the host's resolver from the host namespace, so loopback stubs such as systemd-resolved's `127.0.0.53` work without reading `/run/systemd/resolve/resolv.conf`. This was verified on Ubuntu 26.04.
- **Port forwards:** `pasta --tcp-ports H` (or `--udp-ports`) plus DNAT to `172.30.0.2:G` on both of pasta's paths.
  - A connection through pasta's interface is handled in prerouting.
  - A connection pasta splices from host loopback arrives as a local connection to the namespace's own address. That is handled in nat output on `fib daddr type local`, with `route_localnet` on `tap0`.
  - pasta's `--no-splice` would avoid the second path, but it exists only in 2026 builds.
- **The host's own addresses are denied by default:** pasta reaches them through host sockets.
  - The address pasta copies from the host's template interface is local to the namespace, so traffic to it hits the input chain and is dropped anyway.
- **pasta is not version-checked.**
  - Distribution builds print `pasta unknown version` (Ubuntu 24.04) or nothing (Debian 13).
  - The README states the floor (2024-02-20), and a failing pasta is reported with its own message.
- **`deny-all` takes `allow` exceptions too.** The spec lists `allow` with `restricted`; nothing argues against honouring it with `deny-all`, and it is what a user would expect.
- **cgroups** go through `systemd-run --user --scope --collect`, which execs the helper in place (about 10 ms) with:
  - `MemoryMax` = guest memory + 256 MiB (VMM and pasta overhead);
  - `CPUQuota` = (vCPUs + 1) × 100%;
  - `TasksMax` = 64 + vCPUs + 2 × devices.

  `vmkit::cgroups_available()` tells kiln when to warn. Hosts without a user session (CI runners) boot without a scope.
- **No `/proc` or `/sys` in the sandbox.**
  - Without `/sys`, Cloud Hypervisor skips only its multiqueue tap check (`net_util/src/open_tap.rs` at v53.0).
  - With `/sys` present, Landlock would also have to allow `/sys/class/net/tap0`.
- **Cloud Hypervisor 53 with virtio-net on aarch64 under nested virtualization can stall a guest.**
  - The guest hangs in its first network call with vCPU 0 at 100%. That was measured at 5 of 30 VMs without vmkit's sandbox when two run at once, and at a similar rate inside it.
  - Firecracker is unaffected. Cloud Hypervisor idle guests did not stall (0 of 48, sandboxed or not).
  - On the Lima VM, the network suite can therefore fail a Cloud Hypervisor test; x86_64 CI is the gate. Same family as M2a's pause/resume issue; not reported upstream (user's decision).
- **UDP forwards are checked at the ruleset level only:** the test guest's busybox `nc` has no UDP mode.
- **`NetSpec` changes shape:** `{ tap, guest_mac }` becomes `{ egress, allow, forwards }`. vmkit creates the tap; nothing depends on the old shape yet.

## File Structure

```
vmkit/
  src/bin/vmkit-sandbox.rs  the helper: `run` (outer) and `init` (PID 1, execs the VMM)        Task 1
  src/sandbox.rs            Plan types (Task 1); the in-sandbox layout, helper discovery,
                            cgroups and `spawn` (Task 2)
  src/net.rs                guest addressing constants (Task 1); Cidr, Egress, NetSpec, the
                            nftables ruleset, pasta options, host addresses (Task 2)
  src/{binary,error,process,spec,firecracker,cloud_hypervisor,lib}.rs   modified in Task 2
  tests/sandbox.rs          the helper alone, with busybox as the VMM (no KVM)              Task 1
  tests/contract.rs         + sandbox contents, symlinked disk, comma run dir                Task 2
  tests/network.rs          the hostile-guest network suite                                  Task 3
  testguest/init            + `net` action: probes, DNS, spoofing, an HTTP server            Task 3
  testguest/net-fixture.sh  fixture addresses on the host's loopback                         Task 3
  scripts/install-apparmor.sh                                                                Task 1
  .github/workflows/ci.yml, README.md                                                        Task 4
```

---


### Task 1: The sandbox helper

**Files:**
- Create: `src/bin/vmkit-sandbox.rs`, `src/sandbox.rs`, `src/net.rs`, `tests/sandbox.rs`, `scripts/install-apparmor.sh`
- Modify: `Cargo.toml`, `src/lib.rs`

**Interfaces:**
- Consumes: the M2a crate (nothing of its API).
- Produces:
  - `vmkit::sandbox::{Plan, Bind, Limits, NetPlan}` (serde, `#[doc(hidden)]`):
    - `Plan { vmm, args, binds: Vec<Bind>, limits: Limits, root, pid_file, net: Option<NetPlan> }`;
    - `Bind { source, target, writable }`;
    - `Limits { open_files, processes }`;
    - `NetPlan { ip, nft, ruleset, pasta, pasta_args }`.
  - `vmkit::net::{TAP, GATEWAY, GUEST, PREFIX, GUEST_MAC}`: `"tap0"`, `172.30.0.1`, `172.30.0.2`, `30`, `"06:00:ac:1e:00:02"`.
  - The `vmkit-sandbox` binary.
    - `vmkit-sandbox run <plan.json>` runs `plan.vmm` as `/vmm` with `plan.args`, in a root holding only `plan.binds`, and writes the VMM's host PID to `plan.pid_file`.
    - With `plan.net` it creates `tap0` (owned by the invoking user), loads `plan.net.ruleset` with `nft -f -`, and attaches `pasta` (`plan.net.pasta_args` plus `--netns /proc/<pid>/ns/net --netns-only`).
    - It exits like the VMM: same code, or same signal. On its own failures it exits 125 with `vmkit-sandbox: <what>: <why>` on stderr.
  - `scripts/install-apparmor.sh <absolute path or glob>`: loads a profile granting `userns` where AppArmor restricts user namespaces; elsewhere it does nothing.

- [ ] **Step 1: Add the dependencies**

`serde` for the plan the library hands the helper; `rustix` (Linux only, safe wrappers) for namespaces, mounts, capabilities and pidfds.

In `Cargo.toml`, replace:
```toml
[dependencies]
serde_json = "1.0.151"
```
with:
```toml
[dependencies]
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
```

In `Cargo.toml`, replace:
```toml
tempfile = "3.27.0"
```
with:
```toml
tempfile = "3.27.0"

[target.'cfg(target_os = "linux")'.dependencies]
rustix = { version = "1.1.5", features = ["thread", "mount", "process", "fs"] }
```

- [ ] **Step 2: Write the plan types and the guest addressing constants**

The plan is the whole contract between the library and the helper: the helper trusts nothing else. The addressing constants are public because kiln delivers them to its guest (spec §9.5).

`src/sandbox.rs`:
```rust
//! What the sandbox helper builds (kiln spec §9.2). The library writes a [`Plan`] and
//! runs `vmkit-sandbox run <plan.json>`; the helper does the namespace work.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One file or directory the VMM may see, attached by file descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bind {
    /// Host path; opened with `O_NOFOLLOW` semantics (a symlink is refused).
    pub source: PathBuf,
    /// Absolute path inside the sandbox root.
    pub target: PathBuf,
    pub writable: bool,
}

/// Resource limits applied to the VMM process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub open_files: u64,
    pub processes: u64,
}

/// The VM's network (kiln spec §9.3): set up in its namespace before the VMM starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetPlan {
    /// `ip` and `nft`, run inside the namespace before the root is replaced.
    pub ip: PathBuf,
    pub nft: PathBuf,
    /// The nftables ruleset, loaded with `nft -f -`.
    pub ruleset: String,
    /// `pasta` and its options; the helper adds the namespace to attach to.
    pub pasta: PathBuf,
    pub pasta_args: Vec<String>,
}

/// Everything the helper needs to start one VMM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The VMM binary on the host; it appears at `/vmm` inside.
    pub vmm: PathBuf,
    /// Arguments, already in terms of in-sandbox paths.
    pub args: Vec<String>,
    /// Files and directories under `/vm` (and the device nodes under `/dev`).
    pub binds: Vec<Bind>,
    pub limits: Limits,
    /// Host directory the tmpfs root is mounted on while it is built.
    pub root: PathBuf,
    /// The helper writes the VMM's host PID here.
    pub pid_file: PathBuf,
    pub net: Option<NetPlan>,
}
```

`src/net.rs`:
```rust
//! Guest networking (kiln spec §9.3): the VM's own net namespace with a tap, an
//! nftables policy and `pasta` for unprivileged egress through host sockets.

use std::net::Ipv4Addr;

/// The tap device in the VM's namespace.
pub const TAP: &str = "tap0";
/// The namespace's address on the tap: the guest's gateway and DNS server.
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(172, 30, 0, 1);
/// The guest's address. Every VM has its own namespace, so addresses never collide.
pub const GUEST: Ipv4Addr = Ipv4Addr::new(172, 30, 0, 2);
/// The prefix length of the tap network.
pub const PREFIX: u8 = 30;
/// The guest's MAC address (one VM per namespace, so it never collides either).
pub const GUEST_MAC: &str = "06:00:ac:1e:00:02";
```

In `src/lib.rs`, replace:
```rust
mod http;
mod process;
mod spec;
```
with:
```rust
mod http;
pub mod net;
mod process;
#[doc(hidden)]
pub mod sandbox;
mod spec;
```

- [ ] **Step 3: Write the helper**

Decisions this file encodes, each found or checked while validating the plan:
- **Two processes.** `run` unshares the user and PID namespaces and stays in the host network namespace, where `pasta` needs host sockets. Its first child, `init`, is PID 1 of the new PID namespace and becomes the VMM by exec.
- **Ambient capabilities.** With an identity mapping the user is not root inside, so exec clears every capability. `run` raises `SYS_ADMIN` and `NET_ADMIN` as inheritable and ambient, which carries them across exec to `init` (and `pasta`). `init` clears the ambient and inheritable sets before the final exec. The VMM ends with zero in all five sets.
- **Handshake on stdin.** `init` writes `ready` once its namespaces and the network exist, then waits for `go`, which `run` sends after `pasta` is attached. End-of-file means `run` died. Since `init` sets its death signal before it says `ready`, `go` also proves `run` outlived that setup.
- **`pasta` without `--userns`.** `run` is already in the user namespace and holds the ambient capabilities, so `--netns-only` suffices. `pasta` daemonizes once its interface is configured; its daemon lives in the PID namespace and dies with the VMM.
- **Binds by descriptor.**
  - `open_tree(…, AT_SYMLINK_NOFOLLOW)` then `move_mount`.
  - The opened object is `fstat`ed, so a symlink is refused and a path swapped after the check changes nothing.
  - A read-only bind is remounted with the source mount's locked flags (`nosuid`, `nodev`, `noexec`, atime). Inside a user namespace, dropping them fails with `EPERM`, which `/tmp` on a tmpfs triggers.
- **Unsafe code** is two audited blocks: `unshare` (rustix marks it unsafe because of `CLONE_FILES`, which is never passed) and `BorrowedFd::borrow_raw` for the inherited descriptor numbers listed in `/proc/self/fd`, which are marked close-on-exec.
- **Exit mirroring.** `run` exits with the VMM's code, or kills itself with the VMM's signal (128 + signal if that signal is ignored, as Rust ignores `SIGPIPE`).
- **The AppArmor message.** On Ubuntu 23.10 and later an unprofiled helper cannot write its `uid_map`; the error says to run `scripts/install-apparmor.sh`.

`src/bin/vmkit-sandbox.rs`:
```rust
//! `vmkit-sandbox`: runs one VMM inside unprivileged user, PID, mount and net
//! namespaces with a minimal root (kiln spec §9.2). Started by the vmkit library.
//!
//!   vmkit-sandbox run <plan.json>    outer: user + PID namespaces, attaches pasta, waits for the VMM
//!   vmkit-sandbox init <plan.json>   inner (PID 1): mount + net namespaces, then execs the VMM
//!
//! The outer helper and `init` talk over a socket on `init`'s stdin: `init` sends
//! `ready` once its namespaces exist, and waits for `go`, which the outer helper sends
//! after attaching `pasta`. If the outer helper dies first, `init` sees end-of-file.
#![deny(unsafe_code)]

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("vmkit-sandbox: only Linux is supported");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::Path;
    use std::process::{Command, ExitStatus, Stdio};

    use rustix::fs::{CWD, FileType, StatVfsMountFlags, fstat, statvfs};
    use rustix::io::{FdFlags, fcntl_setfd};
    use rustix::mount::{
        MountFlags, MountPropagationFlags, MoveMountFlags, OpenTreeFlags, UnmountFlags, mount, mount_change,
        mount_remount, move_mount, open_tree, unmount,
    };
    use rustix::process::{
        Resource, Rlimit, Signal, getpid, kill_process, pivot_root, set_parent_process_death_signal, setrlimit,
    };
    use rustix::thread::{
        CapabilitySet, UnshareFlags, capabilities, clear_ambient_capability_set, configure_capability_in_ambient_set,
        set_capabilities, set_no_new_privs,
    };
    use vmkit::net::{GATEWAY, PREFIX, TAP};
    use vmkit::sandbox::{NetPlan, Plan};

    /// The capabilities `init` needs inside the user namespace: mounts and pivot_root,
    /// then the net namespace's tap and nftables. They reach `init` (and `pasta`) across
    /// exec as ambient capabilities; an identity-mapped, non-root user loses everything else.
    const SETUP: [CapabilitySet; 2] = [CapabilitySet::SYS_ADMIN, CapabilitySet::NET_ADMIN];

    fn fail(what: &str, e: impl std::fmt::Display) -> ! {
        eprintln!("vmkit-sandbox: {what}: {e}");
        std::process::exit(125);
    }

    fn read_plan(path: &str) -> Plan {
        let bytes = fs::read(path).unwrap_or_else(|e| fail("reading the plan", e));
        serde_json::from_slice(&bytes).unwrap_or_else(|e| fail("parsing the plan", e))
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        match (args.get(1).map(String::as_str), args.get(2)) {
            (Some("run"), Some(plan)) => run(plan),
            (Some("init"), Some(plan)) => init(plan),
            _ => fail("usage", "vmkit-sandbox run|init <plan.json>"),
        }
    }

    /// Unshares namespaces. This process is single-threaded and does not share its
    /// file-descriptor table, so the `unshare` safety requirement (no `FILES`) holds.
    fn unshare(flags: UnshareFlags) {
        #[allow(unsafe_code)]
        // SAFETY: `flags` never includes `UnshareFlags::FILES` (see the doc comment).
        let r = unsafe { rustix::thread::unshare_unsafe(flags) };
        r.unwrap_or_else(|e| fail("unshare", e));
    }

    /// Marks every inherited descriptor above stderr close-on-exec, so nothing the
    /// caller leaked reaches the VMM.
    fn close_inherited_fds() {
        let fds: Vec<i32> = fs::read_dir("/proc/self/fd")
            .unwrap_or_else(|e| fail("listing descriptors", e))
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .filter(|&fd| fd > 2)
            .collect();
        for fd in fds {
            #[allow(unsafe_code)]
            // SAFETY: `fd` was open when listed and nothing in this single-threaded process
            // closes descriptors meanwhile; the one `read_dir` used for the listing is
            // already closed, so setting a flag on that number fails harmlessly.
            let fd = unsafe { rustix::fd::BorrowedFd::borrow_raw(fd) };
            let _ = fcntl_setfd(fd, FdFlags::CLOEXEC);
        }
    }

    fn pass_setup_capabilities() {
        let mut caps = capabilities(None).unwrap_or_else(|e| fail("reading capabilities", e));
        caps.inheritable = SETUP[0] | SETUP[1];
        set_capabilities(None, caps).unwrap_or_else(|e| fail("setting inheritable capabilities", e));
        for cap in SETUP {
            configure_capability_in_ambient_set(cap, true).unwrap_or_else(|e| fail("raising an ambient capability", e));
        }
    }

    /// Ends this process the way the VMM ended: the same exit code, or the same signal.
    fn mirror(status: ExitStatus) -> ! {
        if let Some(code) = status.code() {
            std::process::exit(code);
        }
        if let Some(sig) = status.signal().and_then(Signal::from_named_raw) {
            let _ = kill_process(getpid(), sig);
        }
        // An ignored signal (Rust ignores SIGPIPE) cannot be re-raised.
        std::process::exit(128 + status.signal().unwrap_or(0));
    }

    fn run(plan_path: &str) {
        close_inherited_fds();
        let plan = read_plan(plan_path);
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        unshare(UnshareFlags::NEWUSER | UnshareFlags::NEWPID);
        // Identity-map the invoking user (no root inside the namespace).
        let write = |file: &str, data: String| {
            fs::write(file, data).unwrap_or_else(|e| {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    fail(
                        &format!("writing {file}"),
                        format!(
                            "{e} (AppArmor restricts unprivileged user namespaces here: \
                             run vmkit's scripts/install-apparmor.sh for this binary)"
                        ),
                    )
                }
                fail(&format!("writing {file}"), e)
            })
        };
        write("/proc/self/setgroups", "deny".into());
        write("/proc/self/uid_map", format!("{uid} {uid} 1"));
        write("/proc/self/gid_map", format!("{gid} {gid} 1"));
        pass_setup_capabilities();
        let (ours, theirs) = UnixStream::pair().unwrap_or_else(|e| fail("socketpair", e));
        let me = std::env::current_exe().unwrap_or_else(|e| fail("finding myself", e));
        // The first child is PID 1 of the new PID namespace: the VMM after `init` execs it.
        let mut child = Command::new(me)
            .args(["init", plan_path])
            .stdin(std::os::fd::OwnedFd::from(theirs))
            .spawn()
            .unwrap_or_else(|e| fail("starting the init", e));
        let pid = child.id();
        fs::write(&plan.pid_file, format!("{pid}\n")).unwrap_or_else(|e| fail("writing the pid file", e));
        let mut lines = BufReader::new(&ours);
        let mut line = String::new();
        if lines.read_line(&mut line).is_ok() && line == "ready\n" {
            if let Some(net) = &plan.net {
                attach_pasta(net, pid);
            }
            let _ = (&ours).write_all(b"go\n");
        }
        drop(lines);
        drop(ours);
        mirror(child.wait().unwrap_or_else(|e| fail("waiting for the VMM", e)));
    }

    /// Starts `pasta` on the VM's namespace. It returns once its interface is configured
    /// and keeps running in the PID namespace, so it ends with the VMM.
    fn attach_pasta(net: &NetPlan, pid: u32) {
        let status = Command::new(&net.pasta)
            .args(&net.pasta_args)
            .args(["--netns", &format!("/proc/{pid}/ns/net"), "--netns-only"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .unwrap_or_else(|e| fail(&format!("starting {}", net.pasta.display()), e));
        if !status.success() {
            let _ = kill_process(
                rustix::process::Pid::from_raw(pid as i32).expect("child pid"),
                Signal::KILL,
            );
            fail("pasta", format!("{} exited with {status}", net.pasta.display()));
        }
    }

    /// Runs a setup tool inside the namespace; failures end the sandbox with its message.
    fn tool(program: &Path, args: &[&str], input: Option<&str>) {
        let mut child = Command::new(program)
            .args(args)
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| fail(&format!("starting {}", program.display()), e));
        if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin
                .write_all(text.as_bytes())
                .unwrap_or_else(|e| fail("writing the ruleset", e));
        }
        let status = child.wait().unwrap_or_else(|e| fail("waiting for a setup tool", e));
        if !status.success() {
            fail(&format!("{} {}", program.display(), args.join(" ")), status);
        }
    }

    /// The tap the VMM opens (owned by the invoking user), routing, and the nftables policy.
    fn setup_net(net: &NetPlan) {
        let uid = rustix::process::getuid().as_raw().to_string();
        let gid = rustix::process::getgid().as_raw().to_string();
        let gateway = format!("{GATEWAY}/{PREFIX}");
        tool(&net.ip, &["link", "set", "lo", "up"], None);
        tool(
            &net.ip,
            &["tuntap", "add", TAP, "mode", "tap", "user", &uid, "group", &gid],
            None,
        );
        tool(&net.ip, &["addr", "add", &gateway, "dev", TAP], None);
        tool(&net.ip, &["link", "set", TAP, "up"], None);
        // Both apply to this namespace only. `route_localnet` lets a forwarded connection that
        // pasta spliced from host loopback leave through the tap after DNAT.
        let sysctl = |path: &str| fs::write(path, "1").unwrap_or_else(|e| fail(&format!("writing {path}"), e));
        sysctl("/proc/sys/net/ipv4/ip_forward");
        sysctl(&format!("/proc/sys/net/ipv4/conf/{TAP}/route_localnet"));
        tool(&net.nft, &["-f", "-"], Some(&net.ruleset));
    }

    /// A mount of `source` (opened without following a final symlink) onto `target`.
    fn attach(source: &Path, target: &Path, writable: bool) {
        let fd = open_tree(
            CWD,
            source,
            OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC | OpenTreeFlags::AT_SYMLINK_NOFOLLOW,
        )
        .unwrap_or_else(|e| fail(&format!("opening {}", source.display()), e));
        // The type of what was opened, not of whatever the path names now.
        let kind = FileType::from_raw_mode(
            fstat(&fd)
                .unwrap_or_else(|e| fail(&format!("stat {}", source.display()), e))
                .st_mode,
        );
        if kind == FileType::Symlink {
            fail(&source.display().to_string(), "is a symlink, which vmkit never follows");
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).unwrap_or_else(|e| fail("mkdir", e));
        }
        if kind == FileType::Directory {
            fs::create_dir_all(target).unwrap_or_else(|e| fail("mkdir", e));
        } else {
            fs::File::create(target).unwrap_or_else(|e| fail(&format!("creating {}", target.display()), e));
        }
        move_mount(&fd, "", CWD, target, MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH)
            .unwrap_or_else(|e| fail(&format!("attaching {}", target.display()), e));
        if !writable {
            // A user namespace cannot clear the source mount's locked flags, so keep them.
            let st = statvfs(target).unwrap_or_else(|e| fail(&format!("statvfs {}", target.display()), e));
            let keep = StatVfsMountFlags::NOSUID
                | StatVfsMountFlags::NODEV
                | StatVfsMountFlags::NOEXEC
                | StatVfsMountFlags::NOATIME
                | StatVfsMountFlags::NODIRATIME
                | StatVfsMountFlags::RELATIME;
            let locked = MountFlags::from_bits_retain((st.f_flag & keep).bits() as u32);
            mount_remount(
                target,
                MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | locked,
                "",
            )
            .unwrap_or_else(|e| fail(&format!("making {} read-only", target.display()), e));
        }
    }

    fn init(plan_path: &str) {
        // If the outer helper dies, so does everything in this PID namespace.
        set_parent_process_death_signal(Some(Signal::KILL)).unwrap_or_else(|e| fail("pdeathsig", e));
        let plan = read_plan(plan_path);
        unshare(UnshareFlags::NEWNS | UnshareFlags::NEWNET);
        if let Some(net) = &plan.net {
            setup_net(net);
        }
        // `go` also proves the outer helper outlived the death-signal setup above.
        let control = std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .unwrap_or_else(|e| fail("dup stdin", e));
        let control = UnixStream::from(control);
        (&control)
            .write_all(b"ready\n")
            .unwrap_or_else(|e| fail("signalling ready", e));
        let mut line = String::new();
        BufReader::new(&control)
            .read_line(&mut line)
            .unwrap_or_else(|e| fail("waiting for go", e));
        if line != "go\n" {
            fail("waiting for go", "the outer helper went away");
        }
        drop(control);
        mount_change("/", MountPropagationFlags::PRIVATE | MountPropagationFlags::REC)
            .unwrap_or_else(|e| fail("making mounts private", e));
        let root = &plan.root;
        fs::create_dir_all(root).unwrap_or_else(|e| fail("creating the root", e));
        mount(
            "tmpfs",
            root,
            "tmpfs",
            MountFlags::NOSUID | MountFlags::NODEV,
            Some(c"mode=0755,size=1m"),
        )
        .unwrap_or_else(|e| fail("mounting the root tmpfs", e));
        let inside = |p: &Path| root.join(p.strip_prefix("/").unwrap_or(p));
        attach(&plan.vmm, &inside(Path::new("/vmm")), false);
        for b in &plan.binds {
            attach(&b.source, &inside(&b.target), b.writable);
        }
        let old = root.join("old-root");
        fs::create_dir_all(&old).unwrap_or_else(|e| fail("mkdir old-root", e));
        pivot_root(root, &old).unwrap_or_else(|e| fail("pivot_root", e));
        std::env::set_current_dir("/").unwrap_or_else(|e| fail("chdir /", e));
        unmount("/old-root", UnmountFlags::DETACH).unwrap_or_else(|e| fail("detaching the old root", e));
        fs::remove_dir("/old-root").unwrap_or_else(|e| fail("removing old-root", e));
        mount_remount("/", MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV, "")
            .unwrap_or_else(|e| fail("making the root read-only", e));
        let lim = |r: Resource, n: u64| {
            setrlimit(
                r,
                Rlimit {
                    current: Some(n),
                    maximum: Some(n),
                },
            )
            .unwrap_or_else(|e| fail("setrlimit", e))
        };
        lim(Resource::Nofile, plan.limits.open_files);
        lim(Resource::Nproc, plan.limits.processes);
        set_no_new_privs(true).unwrap_or_else(|e| fail("no_new_privs", e));
        // The VMM gets no capabilities: no ambient set, and exec as a non-root user.
        clear_ambient_capability_set().unwrap_or_else(|e| fail("clearing ambient capabilities", e));
        let mut caps = capabilities(None).unwrap_or_else(|e| fail("reading capabilities", e));
        caps.inheritable = CapabilitySet::empty();
        set_capabilities(None, caps).unwrap_or_else(|e| fail("clearing inheritable capabilities", e));
        let name = plan
            .vmm
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "vmm".into());
        let err = Command::new("/vmm")
            .arg0(name)
            .args(&plan.args)
            .stdin(Stdio::null())
            .exec();
        fail("exec", err);
    }
}
```

- [ ] **Step 4: Write the AppArmor installer**

The profile is `flags=(unconfined)` plus `userns`, which is what Ubuntu's own profiles for bubblewrap-style tools use. It attaches by path, so any binary at that path gets the permission: production installs use a root-owned path, and development may use a glob. On hosts without the restriction the script does nothing.

`scripts/install-apparmor.sh` (mode 0755):
```bash
#!/usr/bin/env bash
# Lets the vmkit-sandbox helper at PATH create user namespaces on hosts that restrict
# them through AppArmor (Ubuntu 23.10 and later: kernel.apparmor_restrict_unprivileged_userns=1).
# Elsewhere it does nothing. Needs sudo. PATH may be an AppArmor glob, for example
# '/home/*/target*/debug/vmkit-sandbox' for development builds; give a root-owned
# path in production, because any binary at PATH gets the permission.
set -euo pipefail
helper=${1:?usage: install-apparmor.sh <path to vmkit-sandbox>}
case $helper in
  /*) ;;
  *) echo "install-apparmor.sh: the path must be absolute" >&2; exit 1 ;;
esac
if [ "$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0)" != 1 ]; then
  echo "user namespaces are not restricted here; no profile needed"
  exit 0
fi
name=vmkit-sandbox-$(printf %s "$helper" | sha256sum | cut -c1-12)
profile=/etc/apparmor.d/$name
sudo tee "$profile" >/dev/null <<PROFILE
# vmkit-sandbox ($helper): may create the user namespace it runs a VMM in.
abi <abi/4.0>,
include <tunables/global>
profile $name "$helper" flags=(unconfined) {
  userns,
}
PROFILE
sudo apparmor_parser -r "$profile"
echo "loaded $profile"
```

- [ ] **Step 5: Write the helper's tests**

These run the helper with a static busybox as the "VMM". They need no KVM, only Linux, user namespaces (with the AppArmor profile on Ubuntu), and for the network test `ip`, `nft` and `pasta`. They are skipped without `VMKIT_SANDBOX` unless `VMKIT_REQUIRE_KVM_TESTS=1`. `inherited_descriptors_do_not_reach_the_vmm` fails if `close_inherited_fds` is removed: that was checked during planning. The VMM's `comm` is `vmm`, from the `/vmm` path; its `argv[0]` is the binary's own name, because busybox picks its applet from it.

`tests/sandbox.rs`:
```rust
//! The sandbox helper on its own (kiln spec §9.2), with a static busybox as the "VMM".
//!
//! Needs Linux, `VMKIT_SANDBOX` naming a built `vmkit-sandbox` that may create user
//! namespaces (see `scripts/install-apparmor.sh`), and a static busybox at
//! `/usr/bin/busybox` (`busybox-static`); the network test also needs `ip`, `nft` and `pasta`.
//! Without `VMKIT_SANDBOX` each test is skipped, unless VMKIT_REQUIRE_KVM_TESTS=1 makes that a failure.
#![cfg(target_os = "linux")]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use vmkit::sandbox::{Bind, Limits, NetPlan, Plan};

const BUSYBOX: &str = "/usr/bin/busybox";

struct Sandbox {
    helper: PathBuf,
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Option<Self> {
        let Some(helper) = std::env::var_os("VMKIT_SANDBOX").map(PathBuf::from) else {
            assert!(
                std::env::var_os("VMKIT_REQUIRE_KVM_TESTS").is_none_or(|v| v != "1"),
                "VMKIT_REQUIRE_KVM_TESTS is set but VMKIT_SANDBOX is not"
            );
            return None;
        };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data"), "ro").unwrap();
        std::fs::create_dir(dir.path().join("sock")).unwrap();
        Some(Self { helper, dir })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Busybox running `applet args...` with `/dev/null`, a read-only `/vm/data` and a writable `/vm/sock`.
    fn plan(&self, args: &[&str]) -> Plan {
        let bind = |source: PathBuf, target: &str, writable| Bind {
            source,
            target: target.into(),
            writable,
        };
        Plan {
            vmm: BUSYBOX.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            binds: vec![
                bind("/dev/null".into(), "/dev/null", true),
                bind(self.path("data"), "/vm/data", false),
                bind(self.path("sock"), "/vm/sock", true),
            ],
            limits: Limits {
                open_files: 64,
                processes: 32,
            },
            root: self.path("root"),
            pid_file: self.path("vmm.pid"),
            net: None,
        }
    }

    fn command(&self, plan: &Plan) -> Command {
        let file = self.path("plan.json");
        std::fs::write(&file, serde_json::to_vec(plan).unwrap()).unwrap();
        let mut cmd = Command::new(&self.helper);
        cmd.arg("run").arg(file).stdin(Stdio::null());
        cmd
    }

    fn output(&self, plan: &Plan) -> Output {
        self.command(plan).output().unwrap()
    }

    fn spawn(&self, plan: &Plan) -> Child {
        self.command(plan).stdout(Stdio::null()).spawn().unwrap()
    }

    /// The VMM's `/proc` directory, once the helper has written its PID.
    fn vmm_proc(&self) -> PathBuf {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(pid) = std::fs::read_to_string(self.path("vmm.pid")) {
                let dir = PathBuf::from(format!("/proc/{}", pid.trim()));
                // Wait for the exec: `init` becomes `/vmm`.
                if std::fs::read_to_string(dir.join("comm")).is_ok_and(|c| c.trim() == "vmm") {
                    return dir;
                }
            }
            assert!(Instant::now() < deadline, "the sandboxed process never started");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn field(status: &str, name: &str) -> String {
    status
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}:")))
        .unwrap_or_else(|| panic!("no {name}"))
        .trim()
        .to_string()
}

fn gone(dir: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while dir.exists() {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

#[test]
fn the_root_holds_only_the_binds() {
    let Some(s) = Sandbox::new() else { return };
    let o = s.output(&s.plan(&["find", "/"]));
    assert!(o.status.success(), "{o:?}");
    let mut seen: Vec<String> = stdout(&o).lines().map(String::from).collect();
    seen.sort();
    assert_eq!(seen, ["/", "/dev", "/dev/null", "/vm", "/vm/data", "/vm/sock", "/vmm"]);
}

#[test]
fn read_only_binds_stay_read_only_and_writable_ones_reach_the_host() {
    let Some(s) = Sandbox::new() else { return };
    // The temporary directory is on a nosuid,nodev tmpfs on most hosts: its locked flags must be kept.
    let o = s.output(&s.plan(&["sh", "-c", "echo x > /vm/data"]));
    assert!(!o.status.success(), "wrote to a read-only bind");
    assert_eq!(std::fs::read_to_string(s.path("data")).unwrap(), "ro");
    let o = s.output(&s.plan(&["sh", "-c", "echo out > /vm/sock/out && echo x > /new"]));
    assert!(!o.status.success(), "the root is read-only");
    assert_eq!(std::fs::read_to_string(s.path("sock/out")).unwrap(), "out\n");
}

#[test]
fn exit_codes_and_signals_are_mirrored() {
    let Some(s) = Sandbox::new() else { return };
    assert_eq!(s.output(&s.plan(&["sh", "-c", "exit 7"])).status.code(), Some(7));
    let mut child = s.spawn(&s.plan(&["sleep", "30"]));
    let vmm = s.vmm_proc();
    let pid = vmm.file_name().unwrap().to_str().unwrap().to_string();
    // As PID 1 of its namespace the VMM takes only SIGKILL from outside it.
    assert!(Command::new("kill").args(["-KILL", &pid]).status().unwrap().success());
    assert_eq!(child.wait().unwrap().signal(), Some(9));
}

#[test]
fn the_vmm_has_no_privileges_and_dies_with_the_helper() {
    let Some(s) = Sandbox::new() else { return };
    let mut child = s.spawn(&s.plan(&["sleep", "30"]));
    let vmm = s.vmm_proc();
    let status = std::fs::read_to_string(vmm.join("status")).unwrap();
    for caps in ["CapInh", "CapPrm", "CapEff", "CapAmb"] {
        assert_eq!(field(&status, caps), "0000000000000000", "{caps}");
    }
    assert_eq!(field(&status, "NoNewPrivs"), "1");
    assert!(field(&status, "NSpid").ends_with("\t1"));
    let me = std::fs::read_to_string("/proc/self/status").unwrap();
    assert_eq!(
        field(&status, "Uid"),
        field(&me, "Uid").replace(char::is_whitespace, "\t")
    );
    let limits = std::fs::read_to_string(vmm.join("limits")).unwrap();
    assert!(
        limits
            .lines()
            .any(|l| l.starts_with("Max open files") && l.contains(" 64 ")),
        "{limits}"
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(gone(&vmm), "the VMM outlived the helper");
}

#[test]
fn inherited_descriptors_do_not_reach_the_vmm() {
    use std::os::fd::AsRawFd;
    let Some(s) = Sandbox::new() else { return };
    let leaked = std::fs::File::open("/dev/null").unwrap();
    rustix::io::fcntl_setfd(&leaked, rustix::io::FdFlags::empty()).unwrap();
    let mut child = s.spawn(&s.plan(&["sleep", "30"]));
    let vmm = s.vmm_proc();
    let mut fds: Vec<String> = std::fs::read_dir(vmm.join("fd"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    fds.sort();
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(fds, ["0", "1", "2"], "descriptor {} leaked", leaked.as_raw_fd());
}

#[test]
fn a_symlink_is_never_followed() {
    let Some(s) = Sandbox::new() else { return };
    std::os::unix::fs::symlink(s.path("data"), s.path("link")).unwrap();
    let mut plan = s.plan(&["true"]);
    plan.binds[1].source = s.path("link");
    let o = s.output(&plan);
    assert_eq!(o.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&o.stderr).contains("is a symlink"), "{o:?}");
}

#[test]
fn the_network_namespace_gets_the_tap_and_pasta() {
    let Some(s) = Sandbox::new() else { return };
    let find = |name: &str| {
        ["/usr/sbin", "/sbin", "/usr/bin", "/bin"]
            .iter()
            .map(|d| Path::new(d).join(name))
            .find(|p| p.exists())
            .unwrap_or_else(|| panic!("{name} not installed"))
    };
    let mut plan = s.plan(&["ip", "-4", "-o", "addr"]);
    plan.net = Some(NetPlan {
        ip: find("ip"),
        nft: find("nft"),
        ruleset: "table inet vmkit { }\n".into(),
        pasta: std::env::var_os("VMKIT_PASTA")
            .map(PathBuf::from)
            .unwrap_or_else(|| find("pasta")),
        pasta_args: [
            "--config-net",
            "--ns-ifname",
            "egress0",
            "--ipv4-only",
            "--quiet",
            "--tcp-ports",
            "none",
            "--udp-ports",
            "none",
            "--tcp-ns",
            "none",
            "--udp-ns",
            "none",
        ]
        .map(String::from)
        .to_vec(),
    });
    let o = s.output(&plan);
    assert!(o.status.success(), "{o:?}");
    let out = stdout(&o);
    assert!(out.contains("tap0") && out.contains("172.30.0.1/30"), "{out}");
    assert!(out.contains("egress0"), "pasta configured its interface:\n{out}");
}
```

- [ ] **Step 6: Format, lint and test**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -q
```
Expected: no clippy warnings; every test passes.

- [ ] **Step 7: Run the helper's tests on Linux**

In the Lima VM (or any Linux host with user namespaces), with `busybox-static`, `passt`, `nftables` and `iproute2` installed. The installer needs sudo once.

Run (Linux: the Lima VM):
```bash
cargo build -q --bin vmkit-sandbox && scripts/install-apparmor.sh "${CARGO_TARGET_DIR:-$PWD/target}/debug/vmkit-sandbox" && export VMKIT_SANDBOX="${CARGO_TARGET_DIR:-$PWD/target}/debug/vmkit-sandbox" && VMKIT_REQUIRE_KVM_TESTS=1 cargo test --test sandbox
```
Expected: 7 tests pass.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src tests/sandbox.rs scripts/install-apparmor.sh
git commit -m 'feat: vmkit-sandbox helper: namespaces, minimal root, tap and pasta'
```


### Task 2: The network policy and every VMM in the sandbox

**Files:**
- Modify: `src/net.rs`, `src/sandbox.rs`, `src/binary.rs`, `src/error.rs`, `src/process.rs`, `src/spec.rs`, `src/firecracker.rs`, `src/cloud_hypervisor.rs`, `src/lib.rs`, `tests/contract.rs`

**Interfaces:**
- Consumes: Task 1's `Plan` types, net constants and helper; M2a's drivers, `process::spawn` and `Proc`.
- Produces:
  - `vmkit::net`:
    - `Cidr` (`FromStr`, `Display`; host bits cleared, a bare address is `/32`) and `Egress { Restricted (default), DenyAll, Open }`;
    - `Protocol { Tcp, Udp }`, `PortForward { protocol, host, guest }` and `NetSpec { egress, allow: Vec<Cidr>, forwards: Vec<PortForward> }` (also `vmkit::NetSpec`, replacing M2a's `{ tap, guest_mac }`).
    - Crate-internal: `ruleset(&NetSpec, host: &[Ipv4Addr]) -> String`, `pasta_args(&NetSpec)`, `host_addresses(fib_trie: &str)`, `NetSpec::check()`.
  - `vmkit::cgroups_available() -> bool` and `vmkit::sandbox::pid_file(run_dir) -> PathBuf` (`<run_dir>/vmm.pid`).
  - Crate-internal:
    - `sandbox::{KERNEL, INITRAMFS, SOCK, TUN, disk(n), inside(name), host(spec, name), find_helper(), spawn(helper, vmm, args, spec, stderr_log) -> Result<Proc>}`;
    - `binary::find_system(name)`, `Error::ToolNotFound(&'static str)`, `Proc::with_vmm_pid_file(path)`.
  - Both drivers find the helper at discovery and run their VMM through it, addressing everything by in-sandbox paths. The VMM's sockets and own logs are in `<run_dir>/sock/`. `Vm::vsock_socket()` returns `<run_dir>/sock/vsock.sock`.
  - The Cloud Hypervisor `run_dir` comma check is gone.

- [ ] **Step 1: Write the network policy, with its unit tests**

The nftables table lives in the VM's own namespace, so it can be strict without touching the host.
- **Forward chain.** It drops IPv6 and every source but the guest's before anything is accepted. It then accepts replies and DNAT'd traffic (DNS and port forwards), then `allow`, then drops `deny`, then accepts the rest.
- **Sets.** The interval sets need `auto-merge`, because a host address can fall inside a denied range; without it `nft` refuses the set ("conflicting intervals"), as validation found.
- **Port forwards** need DNAT on both of pasta's paths (see Design decisions). The output rule matches `fib daddr type local` because a spliced connection targets the namespace's own address, not `127.0.0.1`.
- **Host addresses** come from the local routes in `/proc/net/fib_trie`, which a plain user can read.

`src/net.rs`:
```rust
//! Guest networking (kiln spec §9.3): the VM's own net namespace with a tap, an
//! nftables policy and `pasta` for unprivileged egress through host sockets.

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

/// The tap device in the VM's namespace.
pub const TAP: &str = "tap0";
/// The namespace's address on the tap: the guest's gateway and DNS server.
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(172, 30, 0, 1);
/// The guest's address. Every VM has its own namespace, so addresses never collide.
pub const GUEST: Ipv4Addr = Ipv4Addr::new(172, 30, 0, 2);
/// The prefix length of the tap network.
pub const PREFIX: u8 = 30;
/// The guest's MAC address (one VM per namespace, so it never collides either).
pub const GUEST_MAC: &str = "06:00:ac:1e:00:02";
/// `pasta`'s interface in the VM's namespace.
pub(crate) const EGRESS: &str = "egress0";
/// Where `pasta` answers DNS in the namespace; guest queries to the gateway are sent here.
pub(crate) const DNS_FORWARD: Ipv4Addr = Ipv4Addr::new(169, 254, 1, 53);

/// Destinations the default `restricted` egress denies, besides the host's own addresses.
const RESTRICTED: [&str; 10] = [
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "224.0.0.0/3",
];

/// An IPv4 network such as `10.0.0.0/8`; a bare address is a `/32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    addr: Ipv4Addr,
    prefix: u8,
}

impl Cidr {
    /// The network containing `addr` (host bits are cleared).
    pub fn new(addr: Ipv4Addr, prefix: u8) -> Option<Self> {
        if prefix > 32 {
            return None;
        }
        let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
        Some(Self {
            addr: Ipv4Addr::from(u32::from(addr) & mask),
            prefix,
        })
    }
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("{s:?} is not an IPv4 address or CIDR");
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, p.parse::<u8>().map_err(|_| bad())?),
            None => (s, 32),
        };
        Cidr::new(addr.parse().map_err(|_| bad())?, prefix).ok_or_else(bad)
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// What the guest may reach (`--egress`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Egress {
    /// Everything except link-local (cloud metadata), CGNAT, RFC 1918, `0/8`,
    /// loopback, multicast and reserved ranges, and the host's own addresses.
    #[default]
    Restricted,
    /// Nothing but DNS.
    DenyAll,
    /// Everything. Callers should warn.
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    fn nft(self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }
}

/// Host port `host` reaches guest port `guest` (`-p HOST:GUEST/proto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortForward {
    pub protocol: Protocol,
    pub host: u16,
    pub guest: u16,
}

/// A network interface for the guest, `eth0` at [`GUEST`]/[`PREFIX`] via [`GATEWAY`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetSpec {
    pub egress: Egress,
    /// Exceptions to `Restricted` and `DenyAll`.
    pub allow: Vec<Cidr>,
    pub forwards: Vec<PortForward>,
}

impl NetSpec {
    /// Rejects forwards the namespace cannot hold: port 0, or a host port used twice.
    pub(crate) fn check(&self) -> Result<(), String> {
        for (i, f) in self.forwards.iter().enumerate() {
            if f.host == 0 || f.guest == 0 {
                return Err(format!("port forward {}:{} uses port 0", f.host, f.guest));
            }
            if self.forwards[..i]
                .iter()
                .any(|g| g.protocol == f.protocol && g.host == f.host)
            {
                return Err(format!("host port {} is forwarded twice", f.host));
            }
        }
        Ok(())
    }
}

/// The nftables ruleset for the VM's namespace. `host` lists the host's own addresses,
/// which `Restricted` denies (pasta would otherwise reach them through host sockets).
pub(crate) fn ruleset(spec: &NetSpec, host: &[Ipv4Addr]) -> String {
    let set = |items: Vec<String>| {
        if items.is_empty() {
            String::new()
        } else {
            format!(" elements = {{ {} }}", items.join(", "))
        }
    };
    let deny: Vec<String> = match spec.egress {
        Egress::Restricted => RESTRICTED
            .iter()
            .map(|s| s.to_string())
            .chain(host.iter().map(|a| a.to_string()))
            .collect(),
        Egress::DenyAll => vec!["0.0.0.0/0".into()],
        Egress::Open => Vec::new(),
    };
    let allow: Vec<String> = match spec.egress {
        Egress::Open => Vec::new(),
        _ => spec.allow.iter().map(Cidr::to_string).collect(),
    };
    let mut prerouting = String::new();
    let mut output = String::new();
    for proto in ["udp", "tcp"] {
        prerouting += &format!("    iifname \"{TAP}\" ip daddr {GATEWAY} {proto} dport 53 dnat ip to {DNS_FORWARD}\n");
    }
    for f in &spec.forwards {
        let (p, h, g) = (f.protocol.nft(), f.host, f.guest);
        // pasta delivers a forwarded connection on its interface, or (when it splices a
        // connection from host loopback) as a local connection to the namespace's address.
        prerouting += &format!("    iifname != \"{TAP}\" fib daddr type local {p} dport {h} dnat ip to {GUEST}:{g}\n");
        output += &format!("    fib daddr type local {p} dport {h} dnat ip to {GUEST}:{g}\n");
    }
    format!(
        "table inet vmkit {{
  set deny {{ type ipv4_addr; flags interval; auto-merge;{deny} }}
  set allow {{ type ipv4_addr; flags interval; auto-merge;{allow} }}
  chain prerouting {{
    type nat hook prerouting priority dstnat; policy accept;
{prerouting}  }}
  chain output {{
    type nat hook output priority dstnat; policy accept;
{output}  }}
  chain postrouting {{
    type nat hook postrouting priority srcnat; policy accept;
    oifname \"{EGRESS}\" masquerade
    oifname \"{TAP}\" ip saddr 127.0.0.0/8 masquerade
  }}
  chain forward {{
    type filter hook forward priority filter; policy drop;
    iifname \"{TAP}\" meta nfproto ipv6 drop
    iifname \"{TAP}\" ip saddr != {GUEST} drop
    ct state established,related accept
    ct status dnat accept
    iifname \"{TAP}\" oifname \"{EGRESS}\" ip daddr @allow accept
    iifname \"{TAP}\" oifname \"{EGRESS}\" ip daddr @deny drop
    iifname \"{TAP}\" oifname \"{EGRESS}\" accept
  }}
  chain input {{
    type filter hook input priority filter; policy accept;
    iifname \"{TAP}\" ct state established,related accept
    iifname \"{TAP}\" drop
  }}
}}
",
        deny = set(deny),
        allow = set(allow),
    )
}

/// `pasta` options, without the namespace to attach to.
pub(crate) fn pasta_args(spec: &NetSpec) -> Vec<String> {
    let ports = |p: Protocol| {
        let list: Vec<String> = spec
            .forwards
            .iter()
            .filter(|f| f.protocol == p)
            .map(|f| f.host.to_string())
            .collect();
        if list.is_empty() {
            "none".to_string()
        } else {
            list.join(",")
        }
    };
    [
        "--config-net",
        "--ns-ifname",
        EGRESS,
        "--ipv4-only",
        // Host loopback services stay unreachable from the namespace.
        "--no-map-gw",
        "--tcp-ns",
        "none",
        "--udp-ns",
        "none",
        "--dns-forward",
        &DNS_FORWARD.to_string(),
        "--quiet",
        "--tcp-ports",
        &ports(Protocol::Tcp),
        "--udp-ports",
        &ports(Protocol::Udp),
    ]
    .map(String::from)
    .to_vec()
}

/// The host's local IPv4 addresses, from `/proc/net/fib_trie`.
pub(crate) fn host_addresses(fib_trie: &str) -> Vec<Ipv4Addr> {
    let mut found = Vec::new();
    let mut last: Option<Ipv4Addr> = None;
    for line in fib_trie.lines() {
        let line = line.trim();
        if let Some(addr) = line.strip_prefix("|-- ") {
            last = addr.parse().ok();
        } else if line == "/32 host LOCAL" {
            if let Some(a) = last.filter(|a| !found.contains(a)) {
                found.push(a);
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidrs_parse_and_normalise() {
        assert_eq!("10.1.2.3/8".parse::<Cidr>().unwrap().to_string(), "10.0.0.0/8");
        assert_eq!("1.2.3.4".parse::<Cidr>().unwrap().to_string(), "1.2.3.4/32");
        assert_eq!("0.0.0.0/0".parse::<Cidr>().unwrap().to_string(), "0.0.0.0/0");
        for bad in ["1.2.3.4/33", "1.2.3/8", "x", "1.2.3.4/", "::1/128"] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad}");
        }
    }

    #[test]
    fn restricted_denies_the_private_ranges_and_the_host() {
        let r = ruleset(&NetSpec::default(), &["192.168.5.15".parse().unwrap()]);
        for range in RESTRICTED {
            assert!(r.contains(range), "{range} missing:\n{r}");
        }
        assert!(r.contains("192.168.5.15"), "{r}");
        assert!(
            r.contains("set allow { type ipv4_addr; flags interval; auto-merge; }"),
            "{r}"
        );
    }

    #[test]
    fn allow_adds_exceptions_but_open_needs_none() {
        let mut spec = NetSpec {
            allow: vec!["10.9.0.0/16".parse().unwrap()],
            ..NetSpec::default()
        };
        assert!(ruleset(&spec, &[]).contains("elements = { 10.9.0.0/16 }"));
        spec.egress = Egress::DenyAll;
        let r = ruleset(&spec, &[]);
        assert!(
            r.contains("elements = { 0.0.0.0/0 }") && r.contains("elements = { 10.9.0.0/16 }"),
            "{r}"
        );
        spec.egress = Egress::Open;
        let r = ruleset(&spec, &["192.168.5.15".parse().unwrap()]);
        assert!(!r.contains("elements"), "open has no deny or allow entries:\n{r}");
    }

    #[test]
    fn spoofed_and_ipv6_traffic_is_dropped_before_anything_is_accepted() {
        let r = ruleset(&NetSpec::default(), &[]);
        let at = |needle: &str| r.find(needle).unwrap_or_else(|| panic!("{needle} missing:\n{r}"));
        assert!(at("meta nfproto ipv6 drop") < at("ct state established,related accept"));
        assert!(at("ip saddr != 172.30.0.2 drop") < at("ct state established,related accept"));
        assert!(at("ip daddr @allow accept") < at("ip daddr @deny drop"));
    }

    #[test]
    fn forwards_reach_the_guest_on_both_pasta_paths() {
        let spec = NetSpec {
            forwards: vec![
                PortForward {
                    protocol: Protocol::Tcp,
                    host: 8080,
                    guest: 80,
                },
                PortForward {
                    protocol: Protocol::Udp,
                    host: 5353,
                    guest: 53,
                },
            ],
            ..NetSpec::default()
        };
        let r = ruleset(&spec, &[]);
        assert!(
            r.contains("iifname != \"tap0\" fib daddr type local tcp dport 8080 dnat ip to 172.30.0.2:80"),
            "{r}"
        );
        assert!(
            r.contains("    fib daddr type local udp dport 5353 dnat ip to 172.30.0.2:53"),
            "{r}"
        );
        let args = pasta_args(&spec);
        let after = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(
            (after("--tcp-ports"), after("--udp-ports")),
            ("8080".into(), "5353".into())
        );
        assert_eq!(after("--tcp-ns"), "none");
        let none = pasta_args(&NetSpec::default());
        assert_eq!(none[none.iter().position(|a| a == "--tcp-ports").unwrap() + 1], "none");
    }

    #[test]
    fn duplicate_or_zero_ports_are_refused() {
        let fwd = |host, guest| PortForward {
            protocol: Protocol::Tcp,
            host,
            guest,
        };
        let spec = |forwards| NetSpec {
            forwards,
            ..NetSpec::default()
        };
        assert!(spec(vec![fwd(80, 80), fwd(81, 80)]).check().is_ok());
        assert!(spec(vec![fwd(80, 80), fwd(80, 81)]).check().is_err());
        assert!(spec(vec![fwd(0, 80)]).check().is_err());
        let mut udp = fwd(80, 80);
        udp.protocol = Protocol::Udp;
        assert!(
            spec(vec![fwd(80, 80), udp]).check().is_ok(),
            "tcp and udp ports are separate"
        );
    }

    #[test]
    fn host_addresses_come_from_the_local_routes() {
        let trie = "Main:\n  +-- 0.0.0.0/0 3 0 4\n     |-- 0.0.0.0\n        /0 universe UNICAST\n     \
                    +-- 127.0.0.0/8 2 0 2\n        |-- 127.0.0.1\n           /32 host LOCAL\n        \
                    |-- 127.255.255.255\n           /32 link BROADCAST\n     |-- 192.168.5.15\n           \
                    /32 host LOCAL\nLocal:\n     |-- 192.168.5.15\n           /32 host LOCAL\n";
        assert_eq!(
            host_addresses(trie),
            [
                "127.0.0.1".parse::<Ipv4Addr>().unwrap(),
                "192.168.5.15".parse().unwrap()
            ]
        );
    }
}
```

- [ ] **Step 2: Find system tools**

A user's `PATH` often lacks `/usr/sbin`, where `ip` and `nft` live.

In `src/binary.rs`, replace:
```rust
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
```
with:
```rust
/// A system tool such as `ip` or `nft`: the first on `PATH`, else in the usual system
/// directories (a user's `PATH` often lacks `/usr/sbin`).
pub(crate) fn find_system(name: &'static str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/sbin", "/usr/bin", "/bin"].map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
        .ok_or(Error::ToolNotFound(name))
}

pub(crate) fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
```

In `src/error.rs`, replace:
```rust
    BinaryNotFound { binary: &'static str, env: &'static str },
    #[error("{binary} {found} is older than the minimum supported {min}")]
```
with:
```rust
    BinaryNotFound { binary: &'static str, env: &'static str },
    #[error("{0} not found on PATH or in /usr/sbin, /sbin, /usr/bin or /bin")]
    ToolNotFound(&'static str),
    #[error("{binary} {found} is older than the minimum supported {min}")]
```

- [ ] **Step 3: Write the sandbox launcher, with its unit tests**

`spawn` writes `<run_dir>/sandbox.json` and starts the helper. When `systemd-run --user --scope` works, the helper runs under it with memory, CPU and task limits; `--scope` execs in place, so the child is still the helper. Everything the VMM can write is `<run_dir>/sock`: the plan, `vmm.pid`, the console and the stderr log stay out of its reach.

`src/sandbox.rs`:
```rust
//! The VMM sandbox (kiln spec §9.2). The library writes a [`Plan`] and runs
//! `vmkit-sandbox run <plan.json>`; the helper does the namespace work.
//!
//! Inside, the VMM sees a read-only tmpfs root with only its devices, `/vmm` (itself),
//! `/vm/kernel`, `/vm/initramfs`, `/vm/disk/<n>` and `/vm/sock/`, which is
//! `<run_dir>/sock` on the host. Its paths never depend on where files live on the host.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::binary;
use crate::error::{Error, Result};
use crate::net;
use crate::process::{self, Proc};
use crate::spec::VmSpec;

/// One file or directory the VMM may see, attached by file descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bind {
    /// Host path; opened with `O_NOFOLLOW` semantics (a symlink is refused).
    pub source: PathBuf,
    /// Absolute path inside the sandbox root.
    pub target: PathBuf,
    pub writable: bool,
}

/// Resource limits applied to the VMM process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub open_files: u64,
    pub processes: u64,
}

/// The VM's network (kiln spec §9.3): set up in its namespace before the VMM starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetPlan {
    /// `ip` and `nft`, run inside the namespace before the root is replaced.
    pub ip: PathBuf,
    pub nft: PathBuf,
    /// The nftables ruleset, loaded with `nft -f -`.
    pub ruleset: String,
    /// `pasta` and its options; the helper adds the namespace to attach to.
    pub pasta: PathBuf,
    pub pasta_args: Vec<String>,
}

/// Everything the helper needs to start one VMM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The VMM binary on the host; it appears at `/vmm` inside.
    pub vmm: PathBuf,
    /// Arguments, already in terms of in-sandbox paths.
    pub args: Vec<String>,
    /// Files and directories under `/vm` (and the device nodes under `/dev`).
    pub binds: Vec<Bind>,
    pub limits: Limits,
    /// Host directory the tmpfs root is mounted on while it is built.
    pub root: PathBuf,
    /// The helper writes the VMM's host PID here.
    pub pid_file: PathBuf,
    pub net: Option<NetPlan>,
}

/// The kernel inside the sandbox.
pub(crate) const KERNEL: &str = "/vm/kernel";
/// The initramfs inside the sandbox.
pub(crate) const INITRAMFS: &str = "/vm/initramfs";
/// The VMM's own directory inside the sandbox: sockets and the logs it writes itself.
pub(crate) const SOCK: &str = "/vm/sock";
/// The tap's device, inside the sandbox.
pub(crate) const TUN: &str = "/dev/net/tun";

/// Disk `n` (0-based) inside the sandbox.
pub(crate) fn disk(n: usize) -> String {
    format!("/vm/disk/{n}")
}

/// `name` in the VMM's directory, inside the sandbox.
pub(crate) fn inside(name: &str) -> String {
    format!("{SOCK}/{name}")
}

/// `name` in the VMM's directory, on the host (`<run_dir>/sock/<name>`).
pub(crate) fn host(spec: &VmSpec, name: &str) -> PathBuf {
    spec.run_dir.join("sock").join(name)
}

/// The file holding the VMM's host PID once it runs: `<run_dir>/vmm.pid`.
pub fn pid_file(run_dir: &Path) -> PathBuf {
    run_dir.join("vmm.pid")
}

/// The sandbox helper (`$VMKIT_SANDBOX`, else `vmkit-sandbox` next to the running
/// program, else on `PATH`).
pub(crate) fn find_helper() -> Result<PathBuf> {
    const NAME: &str = "vmkit-sandbox";
    if std::env::var_os("VMKIT_SANDBOX").is_none() {
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.join(NAME)));
        if let Some(p) = sibling.filter(|p| binary::is_executable(p)) {
            return Ok(p);
        }
    }
    binary::find(NAME, "VMKIT_SANDBOX")
}

/// Whether VMMs go into a cgroup with memory, CPU and task limits: true when the
/// systemd user session can create a scope (kiln spec §9.2). Callers warn when false.
pub fn cgroups_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// The bind mounts for `spec`: its devices, kernel, initramfs, disks and the VMM's directory.
fn binds(spec: &VmSpec) -> Vec<Bind> {
    let bind = |source: &Path, target: &str, writable: bool| Bind {
        source: source.to_path_buf(),
        target: target.into(),
        writable,
    };
    let mut binds = vec![
        bind(Path::new("/dev/kvm"), "/dev/kvm", true),
        bind(Path::new("/dev/null"), "/dev/null", true),
        bind(Path::new("/dev/urandom"), "/dev/urandom", false),
    ];
    if spec.net.is_some() {
        binds.push(bind(Path::new(TUN), TUN, true));
    }
    binds.push(bind(&spec.kernel, KERNEL, false));
    if let Some(i) = &spec.initramfs {
        binds.push(bind(i, INITRAMFS, false));
    }
    for (n, d) in spec.disks.iter().enumerate() {
        binds.push(bind(&d.path, &disk(n), !d.read_only));
    }
    binds.push(bind(&spec.run_dir.join("sock"), SOCK, true));
    binds
}

/// The network plan for `spec`, with the host's addresses denied by default egress.
fn net_plan(net: &net::NetSpec) -> Result<NetPlan> {
    net.check().map_err(Error::InvalidSpec)?;
    let host: Vec<Ipv4Addr> = net::host_addresses(&std::fs::read_to_string("/proc/net/fib_trie")?);
    Ok(NetPlan {
        ip: binary::find_system("ip")?,
        nft: binary::find_system("nft")?,
        ruleset: net::ruleset(net, &host),
        pasta: binary::find("pasta", "VMKIT_PASTA")?,
        pasta_args: net::pasta_args(net),
    })
}

/// The cgroup limits for `spec`: guest memory plus the VMM's and pasta's overhead, one CPU
/// per vCPU plus one for the VMM's own threads, and tasks for its device threads.
fn scope_properties(spec: &VmSpec) -> Vec<String> {
    let memory = u64::from(spec.memory_mib) + 256;
    let cpu = (u32::from(spec.vcpus) + 1) * 100;
    let tasks = 64 + u32::from(spec.vcpus) + 2 * spec.devices_needed();
    vec![
        "-p".into(),
        format!("MemoryMax={memory}M"),
        "-p".into(),
        format!("CPUQuota={cpu}%"),
        "-p".into(),
        format!("TasksMax={tasks}"),
    ]
}

/// Starts `vmm` with in-sandbox `args` under the sandbox `helper` for `spec`. Guest serial is
/// appended to `spec.console_log`; the VMM's and the helper's stderr go to `stderr_log`.
pub(crate) fn spawn(helper: &Path, vmm: &Path, args: &[String], spec: &VmSpec, stderr_log: &Path) -> Result<Proc> {
    let net = spec.net.as_ref().map(net_plan).transpose()?;
    let root = spec.run_dir.join("sandbox-root");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(spec.run_dir.join("sock"))?;
    let pid_file = pid_file(&spec.run_dir);
    let _ = std::fs::remove_file(&pid_file);
    let plan = Plan {
        vmm: vmm.to_path_buf(),
        args: args.to_vec(),
        binds: binds(spec),
        limits: Limits {
            open_files: 1024,
            processes: 256,
        },
        root,
        pid_file,
        net,
    };
    let pid_file = plan.pid_file.clone();
    let plan_path = spec.run_dir.join("sandbox.json");
    std::fs::write(
        &plan_path,
        serde_json::to_vec_pretty(&plan).map_err(|e| Error::InvalidSpec(e.to_string()))?,
    )?;
    let run = vec![
        helper.display().to_string(),
        "run".into(),
        plan_path.display().to_string(),
    ];
    if cgroups_available() {
        let mut args: Vec<String> = ["--user", "--scope", "--quiet", "--collect"].map(String::from).to_vec();
        args.extend(scope_properties(spec));
        args.push("--".into());
        args.extend(run);
        process::spawn(Path::new("systemd-run"), &args, &spec.console_log, stderr_log)
    } else {
        process::spawn(helper, &run[1..], &spec.console_log, stderr_log)
    }
    .map(|p| p.with_vmm_pid_file(&pid_file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Disk;

    fn spec() -> VmSpec {
        VmSpec {
            kernel: "/k/vmlinux".into(),
            initramfs: Some("/k/initramfs".into()),
            cmdline: Vec::new(),
            disks: vec![
                Disk {
                    path: "/d/ro.img".into(),
                    read_only: true,
                },
                Disk {
                    path: "/d/rw.img".into(),
                    read_only: false,
                },
            ],
            vcpus: 2,
            memory_mib: 512,
            vsock: None,
            net: None,
            console_log: "/run/vm/console.log".into(),
            run_dir: "/run/vm".into(),
        }
    }

    fn targets(binds: &[Bind]) -> Vec<(String, String, bool)> {
        binds
            .iter()
            .map(|b| {
                (
                    b.source.display().to_string(),
                    b.target.display().to_string(),
                    b.writable,
                )
            })
            .collect()
    }

    #[test]
    fn the_vmm_sees_only_its_devices_kernel_disks_and_directory() {
        let t = targets(&binds(&spec()));
        let expected: Vec<(&str, &str, bool)> = vec![
            ("/dev/kvm", "/dev/kvm", true),
            ("/dev/null", "/dev/null", true),
            ("/dev/urandom", "/dev/urandom", false),
            ("/k/vmlinux", "/vm/kernel", false),
            ("/k/initramfs", "/vm/initramfs", false),
            ("/d/ro.img", "/vm/disk/0", false),
            ("/d/rw.img", "/vm/disk/1", true),
            ("/run/vm/sock", "/vm/sock", true),
        ];
        let expected: Vec<(String, String, bool)> =
            expected.into_iter().map(|(s, d, w)| (s.into(), d.into(), w)).collect();
        assert_eq!(t, expected);
    }

    #[test]
    fn a_network_adds_the_tun_device() {
        let mut s = spec();
        s.net = Some(net::NetSpec::default());
        assert!(targets(&binds(&s)).contains(&(TUN.into(), TUN.into(), true)));
    }

    #[test]
    fn the_scope_limits_follow_the_vm_size() {
        assert_eq!(
            scope_properties(&spec()),
            ["-p", "MemoryMax=768M", "-p", "CPUQuota=300%", "-p", "TasksMax=70"]
        );
    }

    #[test]
    fn the_plan_round_trips_as_json() {
        let plan = Plan {
            vmm: "/bin/vmm".into(),
            args: vec!["--x".into()],
            binds: binds(&spec()),
            limits: Limits {
                open_files: 1,
                processes: 2,
            },
            root: "/r".into(),
            pid_file: "/p".into(),
            net: Some(NetPlan {
                ip: "/sbin/ip".into(),
                nft: "/sbin/nft".into(),
                ruleset: "table inet vmkit {}".into(),
                pasta: "/bin/pasta".into(),
                pasta_args: vec!["--quiet".into()],
            }),
        };
        let back: Plan = serde_json::from_slice(&serde_json::to_vec(&plan).unwrap()).unwrap();
        assert_eq!(back, plan);
    }
}
```

- [ ] **Step 4: Kill the VMM itself**

Killing the helper alone returned from `wait` while the VMM, killed only by its death signal a moment later, still answered its API (the M2a `kill` contract test caught this). So a sandboxed `Proc` kills the VMM through a pidfd, after checking that its parent is still the helper, so a stale or reused PID is never signalled. The helper then reaps it and dies of the same signal. If the PID file is missing or stale, it falls back to killing the helper.

In `src/process.rs`, replace:
```rust
/// A running VMM. Shared with the Cloud Hypervisor reset backstop, which may kill it.
```
with:
```rust
/// Kills the sandboxed VMM whose PID is in `file`, if it is still the child of the helper
/// `helper` (a stale file or a reused PID is ignored). True if the signal was sent.
#[cfg(target_os = "linux")]
fn kill_vmm(file: &Path, helper: u32) -> bool {
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
    let Some(pid) = std::fs::read_to_string(file)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
    else {
        return false;
    };
    let Some(fd) = Pid::from_raw(pid).and_then(|p| pidfd_open(p, PidfdFlags::empty()).ok()) else {
        return false;
    };
    // Checked after opening the pidfd, so the signal goes to the process that was checked.
    let parent = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PPid:"))
                .and_then(|v| v.trim().parse::<u32>().ok())
        });
    parent == Some(helper) && pidfd_send_signal(&fd, Signal::KILL).is_ok()
}

#[cfg(not(target_os = "linux"))]
fn kill_vmm(_file: &Path, _helper: u32) -> bool {
    false
}

/// A running VMM. Shared with the Cloud Hypervisor reset backstop, which may kill it.
```

In `src/process.rs`, replace:
```rust
    logs: Vec<PathBuf>,
}
```
with:
```rust
    logs: Vec<PathBuf>,
    /// For a sandboxed VMM: the file with its host PID (see [`Proc::with_vmm_pid_file`]).
    vmm_pid_file: Option<PathBuf>,
}
```

In `src/process.rs`, replace:
```rust
        logs: vec![log.to_path_buf()],
    })
```
with:
```rust
        logs: vec![log.to_path_buf()],
        vmm_pid_file: None,
    })
```

In `src/process.rs`, replace:
```rust
        self.logs.push(path.to_path_buf());
        self
```
with:
```rust
        self.logs.push(path.to_path_buf());
        self
    }

    /// The child is the sandbox helper, and the VMM it runs has its host PID in `path`.
    /// Killing then kills the VMM itself: the helper reaps it and ends the same way, so
    /// when the child has ended, so has the VMM.
    pub(crate) fn with_vmm_pid_file(mut self, path: &Path) -> Self {
        self.vmm_pid_file = Some(path.to_path_buf());
        self
```

In `src/process.rs`, replace:
```rust
        let mut child = self.child.lock().expect("not poisoned");
        match child.kill() {
```
with:
```rust
        let mut child = self.child.lock().expect("not poisoned");
        if let Some(file) = &self.vmm_pid_file {
            if kill_vmm(file, child.id()) {
                return Ok(());
            }
        }
        match child.kill() {
```

- [ ] **Step 5: Switch the spec to the new `NetSpec`**

The tap name check goes: vmkit creates `tap0` itself. Port forwards are checked before anything starts.

In `src/spec.rs`, replace:
```rust
use std::path::PathBuf;
```
with:
```rust
use std::path::PathBuf;

use crate::net::NetSpec;
```

In `src/spec.rs`, replace:
```rust
/// A network interface backed by an existing tap device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetSpec {
    pub tap: String,
    pub guest_mac: Option<String>,
}

/// A VM to create.
```
with:
```rust
/// A VM to create.
```

In `src/spec.rs`, replace:
```rust
    pub vsock: Option<VsockSpec>,
    pub net: Option<NetSpec>,
```
with:
```rust
    pub vsock: Option<VsockSpec>,
    /// A NIC in the VM's own network namespace (kiln spec §9.3).
    pub net: Option<NetSpec>,
```

In `src/spec.rs`, replace:
```rust
    pub console_log: PathBuf,
    /// A private (0700) directory for this VM's sockets and logs; it must exist.
    pub run_dir: PathBuf,
```
with:
```rust
    pub console_log: PathBuf,
    /// A private (0700) directory for this VM's sandbox plan and logs; it must exist.
    /// The VMM itself sees only `<run_dir>/sock`, its sockets and own logs.
    pub run_dir: PathBuf,
```

In `src/spec.rs`, replace:
```rust
        if let Some(n) = &self.net {
            // The kernel's interface names: 1-15 bytes, no `/`, NUL or whitespace (and not `.` or `..`,
            // which would widen the Landlock rule for the tap's sysfs directory).
            let ok = (1..=15).contains(&n.tap.len())
                && !n.tap.contains(|c: char| c == '/' || c == '\0' || c.is_whitespace())
                && n.tap != "."
                && n.tap != "..";
            if !ok {
                return Err(crate::Error::InvalidSpec(format!(
                    "tap name {:?} must be 1-15 bytes with no '/', NUL or whitespace",
                    n.tap
                )));
            }
        }
```
with:
```rust
        if let Some(n) = &self.net {
            n.check().map_err(crate::Error::InvalidSpec)?;
        }
```

In `src/spec.rs`, replace:
```rust
    #[test]
    fn tap_names_are_valid_interface_names() {
        let tap = |name: &str| {
            let mut s = spec(0);
            s.net = Some(NetSpec {
                tap: name.into(),
                guest_mac: None,
            });
            s.check(&CAPS)
        };
        for good in ["t", "vmkt0", "123456789012345"] {
            assert!(tap(good).is_ok(), "{good}");
        }
        for bad in ["", "1234567890123456", "a/b", "a b", "a\tb", "a\nb", "a\0b", ".", ".."] {
            assert!(matches!(tap(bad), Err(crate::Error::InvalidSpec(_))), "{bad:?}");
        }
    }
```
with:
```rust
    #[test]
    fn invalid_port_forwards_are_refused_before_anything_starts() {
        use crate::net::{PortForward, Protocol};
        let mut s = spec(0);
        let fwd = PortForward {
            protocol: Protocol::Tcp,
            host: 8080,
            guest: 80,
        };
        s.net = Some(NetSpec {
            forwards: vec![fwd, fwd],
            ..NetSpec::default()
        });
        assert!(matches!(s.check(&CAPS), Err(crate::Error::InvalidSpec(m)) if m.contains("8080")));
    }
```

In `src/lib.rs`, replace:
```rust
pub use firecracker::Firecracker;
pub use spec::{
    Capabilities, Disk, EndReason, GuestExit, NetSpec, RestoreSpec, SnapshotBundle, VmEnd, VmSpec, VsockSpec,
};
pub use vmm::{Vm, Vmm};
```
with:
```rust
pub use firecracker::Firecracker;
pub use net::NetSpec;
pub use sandbox::cgroups_available;
pub use spec::{Capabilities, Disk, EndReason, GuestExit, RestoreSpec, SnapshotBundle, VmEnd, VmSpec, VsockSpec};
pub use vmm::{Vm, Vmm};
```

- [ ] **Step 6: Run both drivers in the sandbox**

Every path the VMM sees is an in-sandbox path; the host paths go into the plan's binds.
- **Cloud Hypervisor's Landlock rules** shrink to `/vm/...` and `/dev/net/tun`. It needs no `/sys/class/net/<tap>` rule because the sandbox has no `/sys`, and without it Cloud Hypervisor skips only its multiqueue check.
- **The comma check on `run_dir`** goes: Cloud Hypervisor's option parser now sees only `/vm/sock/...`.

`src/firecracker.rs`:
```rust
//! The Firecracker driver.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::binary::{self, Version};
use crate::error::{Error, Result};
use crate::net;
use crate::process::{self, Proc};
use crate::sandbox;
use crate::spec::{Capabilities, GuestExit, RestoreSpec, SnapshotBundle, VmEnd, VmSpec};
use crate::vmm::{Vm, Vmm};

pub const MIN_VERSION: Version = (1, 17, 0);
const NAME: &str = "firecracker";

pub struct Firecracker {
    binary: PathBuf,
    /// The sandbox helper every VMM runs under.
    sandbox: PathBuf,
    arch: &'static str,
}

impl Firecracker {
    /// Finds the binary (`$VMKIT_FIRECRACKER`, else `PATH`) and checks its version.
    pub fn discover() -> Result<Self> {
        let binary = binary::find("firecracker", "VMKIT_FIRECRACKER")?;
        binary::check_version(&binary, NAME, MIN_VERSION)?;
        Ok(Self {
            binary,
            sandbox: sandbox::find_helper()?,
            arch: std::env::consts::ARCH,
        })
    }

    /// Kernel arguments Firecracker needs, because custom `boot_args` replace its defaults.
    fn backend_args(&self) -> Vec<String> {
        let mut args = vec![format!("console={}", self.capabilities().console)];
        if self.arch == "x86_64" {
            // x86_64 Firecracker exits only on a keyboard-controller reset.
            args.extend(["reboot=k", "i8042.noaux", "i8042.nomux", "i8042.dumbkbd"].map(String::from));
        }
        args
    }
}

impl Vmm for Firecracker {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_virtio_devices: if self.arch == "aarch64" { 92 } else { 17 },
            implicit_devices: 0,
            supports_diff_snapshot: true,
            supports_balloon: true,
            supports_drive_remap: true,
            guest_exit: GuestExit::Reboot,
            console: "ttyS0",
        }
    }

    fn create(&self, spec: &VmSpec) -> Result<Box<dyn Vm>> {
        spec.check(&self.capabilities())?;
        let api = sandbox::host(spec, "firecracker.sock");
        // Without --log-path Firecracker logs to stdout, which is the guest console.
        // It does not open the file for appending, so stderr gets a file of its own.
        let log = sandbox::host(spec, "firecracker.log");
        let args = vec![
            "--api-sock".into(),
            sandbox::inside("firecracker.sock"),
            "--id".into(),
            "vmkit".into(),
            "--log-path".into(),
            sandbox::inside("firecracker.log"),
        ];
        std::fs::create_dir_all(spec.run_dir.join("sock"))?;
        process::clear_socket(&api)?;
        File::create(&log)?;
        let proc = sandbox::spawn(
            &self.sandbox,
            &self.binary,
            &args,
            spec,
            &spec.run_dir.join("firecracker.stderr"),
        )?
        .with_log(&log);
        let vm = FirecrackerVm {
            proc,
            api,
            vsock: None,
            caps: self.capabilities(),
        };
        vm.proc.wait_for_socket(&vm.api)?;
        let mut vm = vm;
        vm.configure(spec, &self.backend_args())?;
        Ok(Box::new(vm))
    }

    fn restore(&self, _bundle: &SnapshotBundle, _spec: &RestoreSpec) -> Result<Box<dyn Vm>> {
        Err(Error::Unsupported("restore"))
    }
}

struct FirecrackerVm {
    proc: Proc,
    api: PathBuf,
    vsock: Option<PathBuf>,
    caps: Capabilities,
}

impl FirecrackerVm {
    fn call(&self, method: &'static str, path: &str, body: Value) -> Result<()> {
        let r = self.proc.request(&self.api, method, path, Some(&body))?;
        if !(200..300).contains(&r.status) {
            return Err(Error::Api {
                backend: NAME,
                method,
                path: path.into(),
                status: r.status,
                body: r.body,
            });
        }
        Ok(())
    }

    fn configure(&mut self, spec: &VmSpec, backend_args: &[String]) -> Result<()> {
        self.call(
            "PUT",
            "/machine-config",
            json!({"vcpu_count": spec.vcpus, "mem_size_mib": spec.memory_mib}),
        )?;
        let mut boot = json!({
            "kernel_image_path": sandbox::KERNEL,
            "boot_args": spec.cmdline.iter().chain(backend_args).cloned().collect::<Vec<_>>().join(" "),
        });
        if spec.initramfs.is_some() {
            boot["initrd_path"] = json!(sandbox::INITRAMFS);
        }
        self.call("PUT", "/boot-source", boot)?;
        // Drives attach in this order: vda, vdb, ...
        for (i, d) in spec.disks.iter().enumerate() {
            let id = format!("disk{i}");
            self.call(
                "PUT",
                &format!("/drives/{id}"),
                json!({"drive_id": id, "path_on_host": sandbox::disk(i), "is_root_device": false, "is_read_only": d.read_only}),
            )?;
        }
        if let Some(v) = spec.vsock {
            let uds = sandbox::host(spec, "vsock.sock");
            process::clear_socket(&uds)?;
            let inside = sandbox::inside("vsock.sock");
            self.call("PUT", "/vsock", json!({"guest_cid": v.guest_cid, "uds_path": inside}))?;
            self.vsock = Some(uds);
        }
        if spec.net.is_some() {
            self.call(
                "PUT",
                "/network-interfaces/eth0",
                json!({"iface_id": "eth0", "host_dev_name": net::TAP, "guest_mac": net::GUEST_MAC}),
            )?;
        }
        Ok(())
    }
}

impl Vm for FirecrackerVm {
    fn start(&mut self) -> Result<()> {
        self.call("PUT", "/actions", json!({"action_type": "InstanceStart"}))
    }

    fn pause(&mut self) -> Result<()> {
        self.call("PATCH", "/vm", json!({"state": "Paused"}))
    }

    fn resume(&mut self) -> Result<()> {
        self.call("PATCH", "/vm", json!({"state": "Resumed"}))
    }

    fn kill(&mut self) -> Result<()> {
        self.proc.kill()
    }

    fn wait(&mut self) -> Result<VmEnd> {
        Ok(self.proc.wait(None)?.expect("an unbounded wait returns an end"))
    }

    fn wait_timeout(&mut self, timeout: Duration) -> Result<Option<VmEnd>> {
        self.proc.wait(Some(timeout))
    }

    fn snapshot(&mut self, _dest: &Path) -> Result<SnapshotBundle> {
        Err(Error::Unsupported("snapshot"))
    }

    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn vsock_socket(&self) -> Option<&Path> {
        self.vsock.as_deref()
    }
}

impl Drop for FirecrackerVm {
    fn drop(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait(Some(Duration::from_secs(5)));
    }
}
```

`src/cloud_hypervisor.rs`:
```rust
//! The Cloud Hypervisor driver.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::binary::{self, Version};
use crate::error::{Error, Result};
use crate::events;
use crate::net;
use crate::process::Proc;
use crate::sandbox;
use crate::spec::{Capabilities, GuestExit, RestoreSpec, SnapshotBundle, VmEnd, VmSpec};
use crate::vmm::{Vm, Vmm};

pub const MIN_VERSION: Version = (53, 0, 0);
const NAME: &str = "cloud-hypervisor";

pub struct CloudHypervisor {
    binary: PathBuf,
    /// The sandbox helper every VMM runs under.
    sandbox: PathBuf,
    arch: &'static str,
}

impl CloudHypervisor {
    /// Finds the binary (`$VMKIT_CLOUD_HYPERVISOR`, else `PATH`) and checks its version.
    pub fn discover() -> Result<Self> {
        let binary = binary::find("cloud-hypervisor", "VMKIT_CLOUD_HYPERVISOR")?;
        binary::check_version(&binary, NAME, MIN_VERSION)?;
        Ok(Self {
            binary,
            sandbox: sandbox::find_helper()?,
            arch: std::env::consts::ARCH,
        })
    }
}

/// Landlock rules: Cloud Hypervisor may touch only the VM's own files (kiln spec §9.2).
fn landlock_rules(spec: &VmSpec) -> Vec<Value> {
    let rule = |path: &str, access: &str| json!({"path": path, "access": access});
    let mut rules = vec![rule(sandbox::KERNEL, "r")];
    if spec.initramfs.is_some() {
        rules.push(rule(sandbox::INITRAMFS, "r"));
    }
    for (n, d) in spec.disks.iter().enumerate() {
        rules.push(rule(&sandbox::disk(n), if d.read_only { "r" } else { "rw" }));
    }
    rules.push(rule(sandbox::SOCK, "rw"));
    if spec.net.is_some() {
        rules.push(rule(sandbox::TUN, "rw"));
    }
    rules
}

/// The backstop thread: ends the VM on a guest reset, and kills the VMM if it can no longer watch.
fn run_backstop(proc: Proc, events: PathBuf, log: PathBuf) {
    let failure = match events::watch(events::Tail::new(events, proc.clone())) {
        events::Watch::Ended => return,
        events::Watch::Reset => match proc.stop_on_reset() {
            Ok(()) => return,
            Err(e) => format!("could not stop the VMM after a guest reset: {e}"),
        },
        events::Watch::Failed(e) => format!("cannot read the event stream: {e}"),
    };
    let _ = proc.fail_backstop();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(f, "vmkit: reset backstop failed: {failure}; the VMM was killed");
    }
}

impl Vmm for CloudHypervisor {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_virtio_devices: 31,
            implicit_devices: 1,
            supports_diff_snapshot: false,
            supports_balloon: true,
            supports_drive_remap: false,
            guest_exit: GuestExit::Poweroff,
            console: if self.arch == "aarch64" { "ttyAMA0" } else { "ttyS0" },
        }
    }

    fn create(&self, spec: &VmSpec) -> Result<Box<dyn Vm>> {
        spec.check(&self.capabilities())?;
        let api = sandbox::host(spec, "cloud-hypervisor.sock");
        let events = sandbox::host(spec, "events.json");
        std::fs::create_dir_all(spec.run_dir.join("sock"))?;
        // A previous VM's events (say, its reset) must not reach this VM's backstop.
        match std::fs::remove_file(&events) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let args = vec![
            "--api-socket".into(),
            format!("path={}", sandbox::inside("cloud-hypervisor.sock")),
            "--event-monitor".into(),
            format!("path={}", sandbox::inside("events.json")),
            "--seccomp".into(),
            "true".into(),
        ];
        crate::process::clear_socket(&api)?;
        let proc = sandbox::spawn(
            &self.sandbox,
            &self.binary,
            &args,
            spec,
            &spec.run_dir.join("cloud-hypervisor.log"),
        )?;
        // The backstop: a guest reset must end the VM, never reboot it (kiln spec §4.1).
        let watcher = proc.clone();
        let backstop_log = spec.run_dir.join("backstop.log");
        std::thread::spawn(move || run_backstop(watcher, events, backstop_log));
        let mut vm = ChVm {
            proc,
            api,
            vsock: None,
            caps: self.capabilities(),
        };
        vm.proc.wait_for_socket(&vm.api)?;
        vm.configure(spec, self.capabilities().console)?;
        Ok(Box::new(vm))
    }

    fn restore(&self, _bundle: &SnapshotBundle, _spec: &RestoreSpec) -> Result<Box<dyn Vm>> {
        Err(Error::Unsupported("restore"))
    }
}

struct ChVm {
    proc: Proc,
    api: PathBuf,
    vsock: Option<PathBuf>,
    caps: Capabilities,
}

impl ChVm {
    fn call(&self, path: &str, body: Option<Value>) -> Result<()> {
        let path = format!("/api/v1/{path}");
        let r = self.proc.request(&self.api, "PUT", &path, body.as_ref())?;
        if !(200..300).contains(&r.status) {
            return Err(Error::Api {
                backend: NAME,
                method: "PUT",
                path,
                status: r.status,
                body: r.body,
            });
        }
        Ok(())
    }

    fn configure(&mut self, spec: &VmSpec, console: &str) -> Result<()> {
        let mut cmdline = spec.cmdline.clone();
        cmdline.push(format!("console={console}"));
        let mut payload = json!({"kernel": sandbox::KERNEL, "cmdline": cmdline.join(" ")});
        if spec.initramfs.is_some() {
            payload["initramfs"] = json!(sandbox::INITRAMFS);
        }
        let mut config = json!({
            "payload": payload,
            "cpus": {"boot_vcpus": spec.vcpus, "max_vcpus": spec.vcpus},
            "memory": {"size": u64::from(spec.memory_mib) << 20},
            "disks": spec
                .disks
                .iter()
                .enumerate()
                .map(|(n, d)| json!({"path": sandbox::disk(n), "readonly": d.read_only}))
                .collect::<Vec<_>>(),
            // Guest serial on the VMM's stdout, which vmkit appends to the console log;
            // a `file=` serial would be truncated when the guest resets.
            "serial": {"mode": "Tty"},
            "console": {"mode": "Off"},
            "landlock_enable": true,
            "landlock_rules": landlock_rules(spec),
        });
        if let Some(v) = spec.vsock {
            let socket = sandbox::host(spec, "vsock.sock");
            crate::process::clear_socket(&socket)?;
            config["vsock"] = json!({"cid": v.guest_cid, "socket": sandbox::inside("vsock.sock")});
            self.vsock = Some(socket);
        }
        if spec.net.is_some() {
            config["net"] = json!([{"tap": net::TAP, "mac": net::GUEST_MAC}]);
        }
        self.call("vm.create", Some(config))
    }
}

impl Vm for ChVm {
    fn start(&mut self) -> Result<()> {
        self.call("vm.boot", None)
    }

    fn pause(&mut self) -> Result<()> {
        self.call("vm.pause", None)
    }

    fn resume(&mut self) -> Result<()> {
        self.call("vm.resume", None)
    }

    fn kill(&mut self) -> Result<()> {
        self.proc.kill()
    }

    fn wait(&mut self) -> Result<VmEnd> {
        Ok(self.proc.wait(None)?.expect("an unbounded wait returns an end"))
    }

    fn wait_timeout(&mut self, timeout: Duration) -> Result<Option<VmEnd>> {
        self.proc.wait(Some(timeout))
    }

    fn snapshot(&mut self, _dest: &Path) -> Result<SnapshotBundle> {
        Err(Error::Unsupported("snapshot"))
    }

    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn vsock_socket(&self) -> Option<&Path> {
        self.vsock.as_deref()
    }
}

impl Drop for ChVm {
    fn drop(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait(Some(Duration::from_secs(5)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::NetSpec;
    use crate::spec::Disk;

    fn spec() -> VmSpec {
        VmSpec {
            kernel: "/k/vmlinux".into(),
            initramfs: Some("/k/initramfs".into()),
            cmdline: Vec::new(),
            disks: vec![
                Disk {
                    path: "/d/ro.img".into(),
                    read_only: true,
                },
                Disk {
                    path: "/d/rw.img".into(),
                    read_only: false,
                },
            ],
            vcpus: 1,
            memory_mib: 128,
            vsock: None,
            net: None,
            console_log: "/run/vm/console.log".into(),
            run_dir: "/run/vm".into(),
        }
    }

    fn rules(spec: &VmSpec) -> Vec<(String, String)> {
        landlock_rules(spec)
            .iter()
            .map(|r| (r["path"].as_str().unwrap().into(), r["access"].as_str().unwrap().into()))
            .collect()
    }

    #[test]
    fn landlock_allows_only_the_vms_own_files() {
        let expected: Vec<(String, String)> = [
            ("/vm/kernel", "r"),
            ("/vm/initramfs", "r"),
            ("/vm/disk/0", "r"),
            ("/vm/disk/1", "rw"),
            ("/vm/sock", "rw"),
        ]
        .iter()
        .map(|(p, a)| (p.to_string(), a.to_string()))
        .collect();
        assert_eq!(rules(&spec()), expected);
    }

    #[test]
    fn landlock_allows_the_tun_device_when_there_is_a_nic() {
        let mut s = spec();
        s.net = Some(NetSpec::default());
        assert!(rules(&s).contains(&("/dev/net/tun".into(), "rw".into())));
    }
}
```

- [ ] **Step 7: Extend the contract suite**

The kill test finds the API socket in `<run_dir>/sock`. M2a's `a_tap_backed_nic_boots` goes: Task 3's network suite boots NICs for real.
- **`the_vmm_sees_only_its_own_files_and_holds_no_privileges`** is spec §11.3's sandbox-contents test. It walks `/proc/<vmm>/root` (the test runs as the namespace's owner, so it may), and checks the VMM's credentials, rlimits, seccomp on at least one thread (Cloud Hypervisor filters per thread, not its main thread), and the cgroup limit when a scope was made.
- **Two more pins** cover Review Focus items 2 and 4: `a_symlinked_disk_is_refused` and `a_run_dir_with_a_comma_boots`.

In `tests/contract.rs`, replace:
```rust
use common::{Case, END};
use vmkit::{Backend, Disk, EndReason, Error, GuestExit, NetSpec, VsockSpec};
```
with:
```rust
use common::{Case, END};
use vmkit::{Backend, Disk, EndReason, Error, GuestExit, VsockSpec};
```

In `tests/contract.rs`, replace:
```rust
    assert_eq!((end.reason, end.signal), (EndReason::Killed, Some(9)));
    let api = std::fs::read_dir(c.dir.path())
        .unwrap()
```
with:
```rust
    assert_eq!((end.reason, end.signal), (EndReason::Killed, Some(9)));
    let api = std::fs::read_dir(c.dir.path().join("sock"))
        .unwrap()
```

In `tests/contract.rs`, replace:
```rust
/// Needs `VMKIT_TEST_TAP` to name an existing tap the test user may open; otherwise it returns early,
/// even under VMKIT_REQUIRE_KVM_TESTS (automatic network tests arrive with M2b).
fn a_tap_backed_nic_boots(backend: Backend) {
    let Ok(tap) = std::env::var("VMKIT_TEST_TAP") else {
        return;
    };
    let Some(c) = Case::new(backend) else { return };
    let mut spec = c.spec("up");
    spec.net = Some(NetSpec { tap, guest_mac: None });
    let (_vm, end) = c.run(&spec);
    assert_eq!(end.reason, EndReason::Exited, "{end:?}\n{}", c.tail());
    assert_eq!(c.console().matches("VMKIT-GUEST-UP").count(), 1, "{}", c.console());
}
```
with:
```rust
fn a_symlinked_disk_is_refused(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let mut spec = c.spec("disks");
    let real = c.dir.path().join("real.img");
    std::fs::File::create(&real).unwrap().set_len(4096).unwrap();
    let link = c.dir.path().join("link.img");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    spec.disks.push(Disk {
        path: link,
        read_only: true,
    });
    let err = c.vmm.create(&spec).err().expect("a symlink is never followed");
    assert!(matches!(&err, Error::EarlyExit(m) if m.contains("link.img")), "{err}");
}

fn a_run_dir_with_a_comma_boots(backend: Backend) {
    let Some(mut c) = Case::new(backend) else { return };
    // The VMM sees only /vm/sock, so host paths no longer reach Cloud Hypervisor's option parser.
    c.dir = tempfile::Builder::new().prefix("vm,dir").tempdir().unwrap();
    let (_vm, end) = c.run(&c.spec("up"));
    assert_eq!(end.reason, EndReason::Exited, "{end:?}\n{}", c.tail());
}

/// Every path under `dir`, relative to it, without descending into `/vm/sock`.
fn walk(dir: &std::path::Path, rel: &str, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = format!("{rel}/{}", entry.file_name().to_string_lossy());
        out.push(path.clone());
        if entry.file_type().unwrap().is_dir() && path != "/vm/sock" {
            walk(&entry.path(), &path, out);
        }
    }
}

fn status_field(status: &str, field: &str) -> String {
    status
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{field}:")))
        .unwrap_or_else(|| panic!("no {field} in status"))
        .trim()
        .to_string()
}

fn the_vmm_sees_only_its_own_files_and_holds_no_privileges(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let mut spec = c.spec("idle");
    let disk = c.dir.path().join("disk.img");
    std::fs::File::create(&disk).unwrap().set_len(4096).unwrap();
    spec.disks.push(Disk {
        path: disk,
        read_only: true,
    });
    let mut vm = c.vmm.create(&spec).unwrap();
    vm.start().unwrap();
    c.await_console("VMKIT-GUEST-TICK", 1);
    let pid = std::fs::read_to_string(vmkit::sandbox::pid_file(c.dir.path())).unwrap();
    let proc_dir = std::path::PathBuf::from(format!("/proc/{}", pid.trim()));

    let mut seen = Vec::new();
    walk(&proc_dir.join("root"), "", &mut seen);
    seen.sort();
    let expected = [
        "/dev",
        "/dev/kvm",
        "/dev/null",
        "/dev/urandom",
        "/vm",
        "/vm/disk",
        "/vm/disk/0",
        "/vm/initramfs",
        "/vm/kernel",
        "/vm/sock",
        "/vmm",
    ];
    assert_eq!(seen, expected);

    let status = std::fs::read_to_string(proc_dir.join("status")).unwrap();
    let uid = rustix_free_uid();
    assert_eq!(status_field(&status, "Uid"), format!("{uid}\t{uid}\t{uid}\t{uid}"));
    for caps in ["CapInh", "CapPrm", "CapEff", "CapAmb"] {
        assert_eq!(status_field(&status, caps), "0000000000000000", "{caps}");
    }
    assert_eq!(status_field(&status, "NoNewPrivs"), "1");
    assert!(
        status_field(&status, "NSpid").ends_with("\t1"),
        "PID 1 of its own namespace"
    );
    let limits = std::fs::read_to_string(proc_dir.join("limits")).unwrap();
    assert!(
        limits
            .lines()
            .any(|l| l.starts_with("Max open files") && l.split_whitespace().nth(3) == Some("1024")),
        "{limits}"
    );
    // Each backend filters some of its threads (Cloud Hypervisor per thread, not the main one).
    let filtered = std::fs::read_dir(proc_dir.join("task")).unwrap().any(|t| {
        let status = std::fs::read_to_string(t.unwrap().path().join("status")).unwrap_or_default();
        status_field(&status, "Seccomp") == "2"
    });
    assert!(filtered, "no VMM thread runs under seccomp");
    if vmkit::cgroups_available() {
        let cgroup = std::fs::read_to_string(proc_dir.join("cgroup")).unwrap();
        let path = cgroup.trim().strip_prefix("0::").expect("cgroup v2");
        let max = std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.max")).unwrap();
        assert_eq!(max.trim(), ((256u64 + 256) << 20).to_string());
    }
    vm.kill().unwrap();
    vm.wait().unwrap();
    assert!(!proc_dir.exists(), "the VMM is gone once the VM ended");
}

/// The invoking user's uid, from `/proc/self/status`.
fn rustix_free_uid() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status_field(&status, "Uid")
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}
```

In `tests/contract.rs`, replace:
```rust
    guest_vsock_connections_reach_the_host_socket,
    a_tap_backed_nic_boots,
);
```
with:
```rust
    guest_vsock_connections_reach_the_host_socket,
    the_vmm_sees_only_its_own_files_and_holds_no_privileges,
    a_symlinked_disk_is_refused,
    a_run_dir_with_a_comma_boots,
);
```

- [ ] **Step 8: Run the unit tests**

Run:
```bash
cargo test -q --lib
```
Expected: all pass (38 tests).

- [ ] **Step 9: Format, lint and test**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -q
```
Expected: no clippy warnings; every test passes.

- [ ] **Step 10: Run the sandbox, contract and pause suites on KVM**

In the Lima VM, with M2a's kernel in `out/` (`kernels/build.sh aarch64 out` if it is missing) and the test guest rebuilt. Under four threads a Cloud Hypervisor guest occasionally stalls on this nested setup, as in M2a; a failing test prints the end of its console. Rerun once before investigating.

Run (Linux: the Lima VM):
```bash
cargo build -q --bin vmkit-sandbox && scripts/install-apparmor.sh "${CARGO_TARGET_DIR:-$PWD/target}/debug/vmkit-sandbox" && export VMKIT_SANDBOX="${CARGO_TARGET_DIR:-$PWD/target}/debug/vmkit-sandbox" && testguest/build-initramfs.sh out/initramfs-aarch64.cpio.gz && export VMKIT_TEST_KERNEL=out/vmlinux-6.18.54-aarch64 VMKIT_TEST_INITRAMFS=out/initramfs-aarch64.cpio.gz VMKIT_REQUIRE_KVM_TESTS=1 && cargo test --test sandbox && cargo test --test contract -- --test-threads=4 && cargo test --test pause -- --test-threads=1
```
Expected: 7 sandbox tests, 20 contract tests (`firecracker::*` and `cloud_hypervisor::*`), 2 pause tests pass.

- [ ] **Step 11: Commit**

```bash
git add src tests/contract.rs
git commit -m 'feat: network policy; every VMM runs in the sandbox'
```


### Task 3: The hostile-guest network suite

**Files:**
- Create: `tests/network.rs`, `testguest/net-fixture.sh`
- Modify: `testguest/init`

**Interfaces:**
- Consumes: Task 2's `NetSpec`, `Egress`, `Cidr`, `PortForward`, the net constants, `sandbox::pid_file`; M2a's `tests/common` `Case`.
- Produces:
  - The test guest's `net` action. It takes `vmkit.ip=<addr/len>`, `vmkit.gw=<addr>`, `vmkit.probe=<ip>:<port>` (repeatable), `vmkit.dns=<name>` (repeatable), `vmkit.spoof=<addr/len>`, `vmkit.serve=<port>` and `vmkit.hold=<seconds>`.
  - It prints `VMKIT-PROBE tcp|spoofed <ip:port> open|closed`, `VMKIT-DNS <name> ok|fail` and `VMKIT-SERVING <port>`, and serves `VMKIT-SERVED` over HTTP.
  - `testguest/net-fixture.sh`: adds `169.254.169.254`, `10.250.0.1` and `198.51.100.7` to the host's loopback (sudo, idempotent).
  - `tests/network.rs`: five tests per backend, skipped without `VMKIT_TEST_NET=1` unless `VMKIT_REQUIRE_KVM_TESTS=1`.

- [ ] **Step 1: Teach the test guest to probe the network**

`busybox nc -w 3` is a TCP connect with a timeout, so a dropped SYN reads as `closed`.
- **Spoofing** sends from an address the host did not assign: an extra address on `eth0` becomes the default route's preferred source. It must be off the guest's `/30`; `172.30.0.3` is that network's broadcast address and Linux never uses it as a source, which validation tripped over.
- **`serve`** runs busybox `httpd`, and `hold` keeps the guest up for the host's requests.

In `testguest/init`, replace:
```
# vmkit contract-suite guest. Kernel arguments:
#   vmkit.test=<action>  up | reboot | panic | exit | idle | vsock | disks
#   vmkit.exit=<method>  poweroff | reboot  (how "up", "vsock" and "disks" end)
/bin/busybox mount -t proc proc /proc
```
with:
```
# vmkit contract-suite guest. Kernel arguments:
#   vmkit.test=<action>  up | reboot | panic | exit | idle | vsock | disks | net
#   vmkit.exit=<method>  poweroff | reboot  (how "up", "vsock", "disks" and "net" end)
#   vmkit.ip=<addr/len> vmkit.gw=<addr>  configure eth0 (with vmkit.test=net)
#   vmkit.probe=<ip>:<port>  try a TCP connection (repeatable)
#   vmkit.dns=<name>         resolve a name through the gateway (repeatable)
#   vmkit.spoof=<addr/len>   then probe again from this unassigned source address
#   vmkit.serve=<port>       serve HTTP on this port ("VMKIT-SERVED"), then wait for vmkit.hold=<seconds>
/bin/busybox mount -t proc proc /proc
```

In `testguest/init`, replace:
```
method=poweroff
for arg in $(/bin/busybox cat /proc/cmdline); do
```
with:
```
method=poweroff
ip= gw= probes= names= spoof= serve= hold=0
for arg in $(/bin/busybox cat /proc/cmdline); do
```

In `testguest/init`, replace:
```
    vmkit.exit=*) method=${arg#vmkit.exit=} ;;
  esac
done
end() {
```
with:
```
    vmkit.exit=*) method=${arg#vmkit.exit=} ;;
    vmkit.ip=*) ip=${arg#vmkit.ip=} ;;
    vmkit.gw=*) gw=${arg#vmkit.gw=} ;;
    vmkit.probe=*) probes="$probes ${arg#vmkit.probe=}" ;;
    vmkit.dns=*) names="$names ${arg#vmkit.dns=}" ;;
    vmkit.spoof=*) spoof=${arg#vmkit.spoof=} ;;
    vmkit.serve=*) serve=${arg#vmkit.serve=} ;;
    vmkit.hold=*) hold=${arg#vmkit.hold=} ;;
  esac
done
probe() { # probe <label> <ip:port>
  if /bin/busybox nc -w 3 "${2%:*}" "${2##*:}" </dev/null >/dev/null 2>&1; then r=open; else r=closed; fi
  echo "VMKIT-PROBE $1 $2 $r"
}
end() {
```

In `testguest/init`, replace:
```
  vsock) /bin/vsock-hello 1234; end ;;
  disks)
```
with:
```
  vsock) /bin/vsock-hello 1234; end ;;
  net)
    /bin/busybox ip link set lo up
    /bin/busybox ip addr add "$ip" dev eth0
    /bin/busybox ip link set eth0 up
    /bin/busybox ip route add default via "$gw"
    if [ -n "$serve" ]; then
      /bin/busybox mkdir -p /srv && echo VMKIT-SERVED > /srv/index.html
      /bin/busybox httpd -p "$serve" -h /srv && echo "VMKIT-SERVING $serve"
    fi
    for p in $probes; do probe tcp "$p"; done
    for n in $names; do
      if /bin/busybox nslookup "$n" "$gw" >/dev/null 2>&1; then r=ok; else r=fail; fi
      echo "VMKIT-DNS $n $r"
    done
    if [ -n "$spoof" ]; then
      # Send from an address the host did not assign (the route's preferred source).
      /bin/busybox ip addr add "$spoof" dev eth0
      /bin/busybox ip route replace default via "$gw" src "${spoof%/*}"
      for p in $probes; do probe spoofed "$p"; done
    fi
    /bin/busybox sleep "$hold"
    end ;;
  disks)
```

- [ ] **Step 2: Write the network fixture**

The suite needs host addresses that a plain user cannot create:
- a stand-in for cloud metadata;
- a private (RFC 1918) address;
- a host address outside every denied range, which must still be unreachable because it is the host's own.

Each is a `/32` on loopback; tests bind their listeners on all addresses at ephemeral ports. The positive control: `allow=10.250.0.1/32` makes the private address reachable, which proves the path works, so every `closed` is the policy's doing.

`testguest/net-fixture.sh` (mode 0755):
```bash
#!/usr/bin/env bash
# Adds the network tests' fixture addresses to the host's loopback (needs sudo).
# They do not survive a reboot. Then: export VMKIT_TEST_NET=1
set -euo pipefail
for a in 169.254.169.254 10.250.0.1 198.51.100.7; do
  ip -4 addr show dev lo | grep -q "inet $a/" || sudo ip addr add "$a/32" dev lo
done
```

- [ ] **Step 3: Write the network suite**

Spec §11.5's hostile guest:
- metadata, the gateway's own services, private and host addresses, and spoofed sources;
- another VM through the forwarded port's host addresses.

It also covers the positive cases: allowed destinations, DNS through the gateway (`localhost`, which every resolver answers without Internet access), and port forwards on both of pasta's paths (`127.0.0.1` is spliced, `198.51.100.7` goes through its interface). Review Focus item 5 is `a_host_port_in_use_fails_with_pastas_message`, plus the check that `pasta` exits with its VM.

`tests/network.rs`:
```rust
//! Hostile-guest network tests (kiln spec §9.3, §11.5): every test runs against both backends.
//!
//! Needs what the contract suite needs, plus `pasta`, `ip` and `nft`, and the fixture
//! addresses from `testguest/net-fixture.sh` (run once per boot, with sudo):
//!   169.254.169.254  stands in for cloud metadata
//!   10.250.0.1       a private (RFC 1918) address on the host
//!   198.51.100.7     a host address outside the private ranges
//! Set VMKIT_TEST_NET=1 once they exist; VMKIT_REQUIRE_KVM_TESTS=1 makes skipping a failure.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use common::{Case, END};
use vmkit::net::{Cidr, Egress, GATEWAY, GUEST, PREFIX, PortForward, Protocol};
use vmkit::{Backend, NetSpec, Vm, VmSpec};

const METADATA: &str = "169.254.169.254";
const PRIVATE: &str = "10.250.0.1";
const HOST: &str = "198.51.100.7";
/// A source address the guest was not given.
const SPOOF: &str = "10.200.0.9/32";

fn net_case(backend: Backend) -> Option<Case> {
    if std::env::var_os("VMKIT_TEST_NET").is_none_or(|v| v != "1") {
        assert!(
            std::env::var_os("VMKIT_REQUIRE_KVM_TESTS").is_none_or(|v| v != "1"),
            "VMKIT_REQUIRE_KVM_TESTS is set but VMKIT_TEST_NET is not (run testguest/net-fixture.sh)"
        );
        return None;
    }
    Case::new(backend)
}

/// A host listener on every address that accepts and closes connections.
fn listener() -> u16 {
    let l = TcpListener::bind("0.0.0.0:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming() {
            drop(s);
        }
    });
    port
}

/// A free host port for a forward.
fn free_port() -> u16 {
    TcpListener::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port()
}

/// The `net` guest: eth0 configured, then `extra` (probes, DNS, spoof, serve).
fn net_spec(c: &Case, net: NetSpec, extra: &[String]) -> VmSpec {
    let mut spec = c.spec("net");
    spec.cmdline.push(format!("vmkit.ip={GUEST}/{PREFIX}"));
    spec.cmdline.push(format!("vmkit.gw={GATEWAY}"));
    spec.cmdline.extend(extra.iter().cloned());
    spec.net = Some(net);
    spec
}

fn probes(addrs: &[&str], port: u16) -> Vec<String> {
    addrs.iter().map(|a| format!("vmkit.probe={a}:{port}")).collect()
}

/// The guest's verdict for one probe, e.g. `("tcp", "10.250.0.1:80")` -> `"open"`.
fn verdict(c: &Case, kind: &str, target: &str) -> String {
    let prefix = format!("VMKIT-PROBE {kind} {target} ");
    c.console()
        .lines()
        .find_map(|l| l.trim().strip_prefix(&prefix).map(String::from))
        .unwrap_or_else(|| panic!("no result for {kind} {target}; console tail:\n{}", c.tail()))
}

fn dns(c: &Case, name: &str) -> String {
    let prefix = format!("VMKIT-DNS {name} ");
    c.console()
        .lines()
        .find_map(|l| l.trim().strip_prefix(&prefix).map(String::from))
        .unwrap_or_else(|| panic!("no DNS result for {name}; console tail:\n{}", c.tail()))
}

fn restricted_egress_reaches_no_metadata_private_or_host_address(backend: Backend) {
    let Some(c) = net_case(backend) else { return };
    let port = listener();
    let mut extra = probes(&[METADATA, PRIVATE, HOST, &GATEWAY.to_string()], port);
    extra.push("vmkit.dns=localhost".into());
    c.run(&net_spec(&c, NetSpec::default(), &extra));
    for target in [METADATA, PRIVATE, HOST, &GATEWAY.to_string()] {
        assert_eq!(verdict(&c, "tcp", &format!("{target}:{port}")), "closed", "{target}");
    }
    assert_eq!(dns(&c, "localhost"), "ok", "DNS through the gateway");
}

fn allowed_destinations_are_reachable_and_spoofed_sources_are_not(backend: Backend) {
    let Some(c) = net_case(backend) else { return };
    let port = listener();
    let net = NetSpec {
        allow: vec![format!("{PRIVATE}/32").parse::<Cidr>().unwrap()],
        ..NetSpec::default()
    };
    let mut extra = probes(&[PRIVATE, METADATA], port);
    extra.push(format!("vmkit.spoof={SPOOF}"));
    c.run(&net_spec(&c, net, &extra));
    // The positive control: the path works, so the other verdicts are the policy's.
    assert_eq!(verdict(&c, "tcp", &format!("{PRIVATE}:{port}")), "open");
    assert_eq!(verdict(&c, "tcp", &format!("{METADATA}:{port}")), "closed");
    assert_eq!(verdict(&c, "spoofed", &format!("{PRIVATE}:{port}")), "closed");
}

fn deny_all_leaves_only_dns_and_open_removes_the_denies(backend: Backend) {
    let Some(c) = net_case(backend) else { return };
    let port = listener();
    let mut extra = probes(&[PRIVATE, HOST], port);
    extra.push("vmkit.dns=localhost".into());
    let deny_all = NetSpec {
        egress: Egress::DenyAll,
        ..NetSpec::default()
    };
    c.run(&net_spec(&c, deny_all, &extra));
    assert_eq!(verdict(&c, "tcp", &format!("{PRIVATE}:{port}")), "closed");
    assert_eq!(dns(&c, "localhost"), "ok");

    let Some(c) = net_case(backend) else { return };
    let open = NetSpec {
        egress: Egress::Open,
        ..NetSpec::default()
    };
    c.run(&net_spec(&c, open, &probes(&[HOST, METADATA], port)));
    assert_eq!(verdict(&c, "tcp", &format!("{HOST}:{port}")), "open");
    assert_eq!(verdict(&c, "tcp", &format!("{METADATA}:{port}")), "open");
}

/// An HTTP GET of `/` from `addr`, retried until the guest serves it.
fn fetch(addr: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let attempt = TcpStream::connect(addr).and_then(|mut s| {
            s.set_read_timeout(Some(Duration::from_secs(5)))?;
            s.write_all(b"GET / HTTP/1.0\r\n\r\n")?;
            let mut body = String::new();
            s.read_to_string(&mut body)?;
            Ok(body)
        });
        match attempt {
            Ok(body) if body.contains("VMKIT-SERVED") => return body,
            other => assert!(Instant::now() < deadline, "{addr}: {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn port_forwards_reach_the_guest_but_other_vms_do_not(backend: Backend) {
    let Some(server) = net_case(backend) else { return };
    let host_port = free_port();
    let net = NetSpec {
        forwards: vec![PortForward {
            protocol: Protocol::Tcp,
            host: host_port,
            guest: 8080,
        }],
        ..NetSpec::default()
    };
    let spec = net_spec(&server, net, &["vmkit.serve=8080".into(), "vmkit.hold=60".into()]);
    let mut vm: Box<dyn Vm> = server.vmm.create(&spec).expect("create");
    vm.start().expect("start");
    server.await_console("VMKIT-SERVING", 1);
    // Both pasta paths: spliced from host loopback, and through its interface.
    fetch(&format!("127.0.0.1:{host_port}"));
    fetch(&format!("{HOST}:{host_port}"));

    // Another VM reaches the forwarded port through none of the host's addresses.
    let Some(other) = net_case(backend) else { return };
    other.run(&net_spec(
        &other,
        NetSpec::default(),
        &probes(&[HOST, PRIVATE], host_port),
    ));
    for target in [HOST, PRIVATE] {
        assert_eq!(
            verdict(&other, "tcp", &format!("{target}:{host_port}")),
            "closed",
            "{target}"
        );
    }
    let vmm_pid = std::fs::read_to_string(vmkit::sandbox::pid_file(server.dir.path())).unwrap();
    vm.kill().unwrap();
    vm.wait_timeout(END).unwrap().expect("killed");
    // pasta lived in the VM's PID namespace, so it ended with the VMM.
    let netns = format!("/proc/{}/ns/net", vmm_pid.trim());
    let deadline = Instant::now() + Duration::from_secs(5);
    while pasta_for(&netns) {
        assert!(Instant::now() < deadline, "pasta outlived its VM");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether a pasta process is attached to `netns` (as named on its command line).
fn pasta_for(netns: &str) -> bool {
    std::fs::read_dir("/proc").unwrap().filter_map(|e| e.ok()).any(|e| {
        let cmdline = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        args.first().is_some_and(|a| a.ends_with(b"pasta")) && args.contains(&netns.as_bytes())
    })
}

fn a_host_port_in_use_fails_with_pastas_message(backend: Backend) {
    let Some(c) = net_case(backend) else { return };
    let taken = TcpListener::bind("0.0.0.0:0").unwrap();
    let net = NetSpec {
        forwards: vec![PortForward {
            protocol: Protocol::Tcp,
            host: taken.local_addr().unwrap().port(),
            guest: 80,
        }],
        ..NetSpec::default()
    };
    let err = c.vmm.create(&net_spec(&c, net, &[])).err().expect("the port is taken");
    assert!(
        matches!(&err, vmkit::Error::EarlyExit(m) if m.contains("pasta")),
        "{err}"
    );
}

macro_rules! network {
    ($($name:ident),* $(,)?) => {
        mod firecracker {
            $( #[test] fn $name() { super::$name(vmkit::Backend::Firecracker) } )*
        }
        mod cloud_hypervisor {
            $( #[test] fn $name() { super::$name(vmkit::Backend::CloudHypervisor) } )*
        }
    };
}

network!(
    restricted_egress_reaches_no_metadata_private_or_host_address,
    allowed_destinations_are_reachable_and_spoofed_sources_are_not,
    deny_all_leaves_only_dns_and_open_removes_the_denies,
    port_forwards_reach_the_guest_but_other_vms_do_not,
    a_host_port_in_use_fails_with_pastas_message,
);
```

- [ ] **Step 4: Format, lint and test**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -q
```
Expected: no clippy warnings; every test passes.

- [ ] **Step 5: Run the network suite on KVM**

In the Lima VM. Cloud Hypervisor on aarch64 under nested virtualization sometimes stalls a guest in its first network call: about 1 in 6 VMs when two run at once, measured during planning with and without vmkit's sandbox. Firecracker is unaffected, and x86_64 CI is the gate. A stalled test fails after 60 s with `VM did not end`; rerun it alone before investigating.

Run (Linux: the Lima VM):
```bash
cargo build -q --bin vmkit-sandbox && scripts/install-apparmor.sh "${CARGO_TARGET_DIR:-$PWD/target}/debug/vmkit-sandbox" && export VMKIT_SANDBOX="${CARGO_TARGET_DIR:-$PWD/target}/debug/vmkit-sandbox" && testguest/build-initramfs.sh out/initramfs-aarch64.cpio.gz && testguest/net-fixture.sh && export VMKIT_TEST_KERNEL=out/vmlinux-6.18.54-aarch64 VMKIT_TEST_INITRAMFS=out/initramfs-aarch64.cpio.gz VMKIT_REQUIRE_KVM_TESTS=1 VMKIT_TEST_NET=1 && cargo test --test network -- --test-threads=4 firecracker
```
Expected: the 5 `firecracker::*` network tests pass.

Then Cloud Hypervisor, in the same shell. If a test fails with `VM did not end` and its console tail stops in the guest's first `nc`, that is the stall; rerun. In the plan's final replay, two runs stalled one Cloud Hypervisor test each and the third passed; Firecracker never stalled.

Run (Linux: the Lima VM):
```bash
cargo test --test network -- --test-threads=2 cloud_hypervisor
```
Expected: the 5 `cloud_hypervisor::*` network tests pass (on a rerun if one stalled).

- [ ] **Step 6: Commit**

```bash
git add testguest tests/network.rs
git commit -m 'test: hostile-guest network suite'
```


### Task 4: CI and the README

**Files:**
- Modify: `.github/workflows/ci.yml`, `README.md`

**Interfaces:**
- Consumes: everything above.
- Produces:
  - The `contract-x86_64` job also installs `passt` and `nftables`, builds the helper, loads its AppArmor profile and the network fixture, and runs the sandbox, contract, network and pause suites.
  - The README documents the sandbox, networking, requirements and test commands.

- [ ] **Step 1: Run every suite in CI**

GitHub's `ubuntu-24.04` runners restrict user namespaces through AppArmor like any Ubuntu 24.04, so the job loads the helper's profile. They have no systemd user session, so VMs run without a cgroup scope there; the contents test checks the scope only where one exists.

In `.github/workflows/ci.yml`, replace:
```yaml
      - run: cargo test --all
  # The contract suite on x86_64 KVM (hosted arm64 runners have no /dev/kvm; aarch64
  # runs on the Lima template, kiln spec §11.7).
```
with:
```yaml
      - run: cargo test --all
  # The sandbox, contract and network suites on x86_64 KVM (hosted arm64 runners have no /dev/kvm; aarch64
  # runs on the Lima template, kiln spec §11.7).
```

In `.github/workflows/ci.yml`, replace:
```yaml
          sudo udevadm trigger --name-match=kvm
      - run: sudo apt-get update && sudo apt-get install -y busybox-static flex bison bc libelf-dev libssl-dev cpio file
      - run: scripts/install-vmms.sh "$HOME/.local/bin"
```
with:
```yaml
          sudo udevadm trigger --name-match=kvm
      - run: sudo apt-get update && sudo apt-get install -y busybox-static flex bison bc libelf-dev libssl-dev cpio file passt nftables
      - run: scripts/install-vmms.sh "$HOME/.local/bin"
```

In `.github/workflows/ci.yml`, replace:
```yaml
      - run: testguest/build-initramfs.sh out/initramfs-x86_64.cpio.gz
      - name: Contract suite
        # About half the CPUs: the suite boots one VM per test (private-repo hosted Linux
        # runners have two vCPUs). Pause/resume runs alone afterwards (see tests/pause.rs).
```
with:
```yaml
      - run: testguest/build-initramfs.sh out/initramfs-x86_64.cpio.gz
      - name: Sandbox helper, its AppArmor profile, and the network fixture
        run: |
          cargo build --bin vmkit-sandbox
          scripts/install-apparmor.sh "$PWD/target/debug/vmkit-sandbox"
          testguest/net-fixture.sh
      - name: Sandbox, contract and network suites
        # About half the CPUs: the suites boot one VM per test (private-repo hosted Linux
        # runners have two vCPUs). Pause/resume runs alone afterwards (see tests/pause.rs).
```

In `.github/workflows/ci.yml`, replace:
```yaml
          export PATH="$HOME/.local/bin:$PATH"
          export VMKIT_TEST_KERNEL=$(ls out/vmlinux-*-x86_64) VMKIT_TEST_INITRAMFS=out/initramfs-x86_64.cpio.gz VMKIT_REQUIRE_KVM_TESTS=1
          cargo test --test contract -- --test-threads=$(( $(nproc) > 1 ? $(nproc) / 2 : 1 ))
          cargo test --test pause -- --test-threads=1
```
with:
```yaml
          export PATH="$HOME/.local/bin:$PATH"
          export VMKIT_SANDBOX=$PWD/target/debug/vmkit-sandbox VMKIT_TEST_NET=1
          export VMKIT_TEST_KERNEL=$(ls out/vmlinux-*-x86_64) VMKIT_TEST_INITRAMFS=out/initramfs-x86_64.cpio.gz VMKIT_REQUIRE_KVM_TESTS=1
          threads=$(( $(nproc) > 1 ? $(nproc) / 2 : 1 ))
          cargo test --test sandbox
          cargo test --test contract -- --test-threads=$threads
          cargo test --test network -- --test-threads=$threads
          cargo test --test pause -- --test-threads=1
```

- [ ] **Step 2: Document the sandbox and networking**

In `README.md`, replace:
```markdown
- **vsock:** guest-initiated connections to host port `P` arrive on the Unix socket `<vm.vsock_socket()>_P` on both backends.
- **Snapshots:** `Vm::snapshot` and `Vmm::restore` have their final shape but return `Error::Unsupported` until the snapshot work lands.
```
with:
```markdown
- **vsock:** guest-initiated connections to host port `P` arrive on the Unix socket `<vm.vsock_socket()>_P` on both backends.
- **Sandbox:** every VMM runs as the invoking user inside its own user, PID, mount and network namespaces, in a read-only root holding only its devices, `/vmm`, `/vm/kernel`, `/vm/initramfs`, `/vm/disk/<n>` and `/vm/sock/` (which is `<run_dir>/sock`). It has no capabilities, `no_new_privs`, rlimits, and no descriptors but stdio; files are attached by descriptor and symlinks are never followed. With a systemd user session it also runs in a cgroup scope with memory, CPU and task limits (`vmkit::cgroups_available()` says whether; warn when not). The `vmkit-sandbox` helper does the namespace work: `$VMKIT_SANDBOX`, else next to the running program, else on `PATH`.
- **Network:** `VmSpec::net` gives the guest `eth0` at `172.30.0.2/30` (gateway and DNS `172.30.0.1`, `vmkit::net`) in the VM's own namespace: a tap, an nftables policy (`Egress::Restricted` by default: no link-local or cloud metadata, private, CGNAT, loopback, multicast or host addresses; `allow` exceptions; `DenyAll` but DNS; `Open`), spoofed and IPv6 traffic dropped, and `pasta` for egress through host sockets and port forwards.
- **Snapshots:** `Vm::snapshot` and `Vmm::restore` have their final shape but return `Error::Unsupported` until the snapshot work lands.
```

In `README.md`, replace:
```markdown
Linux with KVM (`/dev/kvm`, user in the `kvm` group) and the pinned VMMs:
```
with:
```markdown
Linux with KVM (`/dev/kvm`, user in the `kvm` group), the pinned VMMs, and the sandbox helper:
```

In `README.md`, replace:
````markdown
scripts/install-vmms.sh ~/.local/bin    # Firecracker 1.17.0 and Cloud Hypervisor 53.0, SHA-256 checked
```
````
with:
````markdown
scripts/install-vmms.sh ~/.local/bin    # Firecracker 1.17.0 and Cloud Hypervisor 53.0, SHA-256 checked
cargo install --path . --bin vmkit-sandbox --root ~/.local
scripts/install-apparmor.sh ~/.local/bin/vmkit-sandbox   # only acts where AppArmor restricts user namespaces
```

Ubuntu 23.10 and later restrict unprivileged user namespaces through AppArmor; the profile lets the helper create its own. Networking also needs `pasta` (passt 2024-02-20 or later, as in Ubuntu 24.04 and Debian 13), `nft` and `ip`; `$VMKIT_PASTA` overrides the `pasta` found on `PATH`.
````

In `README.md`, replace:
```markdown
cargo test                                   # unit tests, any platform
testguest/build-initramfs.sh out/initramfs.cpio.gz
export VMKIT_TEST_KERNEL=out/vmlinux-6.18.54-aarch64 VMKIT_TEST_INITRAMFS=out/initramfs.cpio.gz VMKIT_REQUIRE_KVM_TESTS=1
cargo test --test contract -- --test-threads=4
cargo test --test pause -- --test-threads=1
```
with:
```markdown
cargo test                                   # unit tests, any platform
cargo build --bin vmkit-sandbox && scripts/install-apparmor.sh "$PWD/target/debug/vmkit-sandbox"
testguest/build-initramfs.sh out/initramfs.cpio.gz
testguest/net-fixture.sh                     # fixture addresses for the network tests (sudo, once per boot)
export VMKIT_SANDBOX=$PWD/target/debug/vmkit-sandbox VMKIT_TEST_NET=1 VMKIT_REQUIRE_KVM_TESTS=1
export VMKIT_TEST_KERNEL=out/vmlinux-6.18.54-aarch64 VMKIT_TEST_INITRAMFS=out/initramfs.cpio.gz
cargo test --test sandbox                    # the helper alone, with busybox as the VMM (no KVM)
cargo test --test contract -- --test-threads=4
cargo test --test network -- --test-threads=4
cargo test --test pause -- --test-threads=1
```

In `README.md`, replace:
```markdown
The contract suite (`tests/contract.rs`) runs every test against both backends with a busybox guest: boot and exit method, reset backstop, panic, kill, disk order, device budget and guest-initiated vsock. Set `VMKIT_TEST_TAP=<tap>` (a tap device you own, e.g. created with `sudo ip tuntap add vmkt0 mode tap user $(id -u)`) to also boot a VM with a network interface; the tap test must run alone because both backends would open the same tap: `VMKIT_TEST_TAP=vmkt0 cargo test --test contract a_tap_backed_nic_boots -- --test-threads=1`. Without `VMKIT_TEST_TAP` it returns early even under `VMKIT_REQUIRE_KVM_TESTS=1`, so CI does not cover it. Each VM's run directory also holds the VMM's own log (`firecracker.log`, `firecracker.stderr` or `cloud-hypervisor.log`) and, if Cloud Hypervisor's reset backstop ever fails, `backstop.log` saying why the VMM was killed (the VM then ends with `EndReason::BackstopFailed`). Pause and resume (`tests/pause.rs`) run alone: Cloud Hypervisor 53 on aarch64 can leave a guest stuck after a resume while other VMs load the host. That was reproduced with plain `ch-remote` under nested virtualization; Firecracker is unaffected. Keep the contract suite's `--test-threads` at about half the CPUs; under heavier load, Cloud Hypervisor guests also stalled occasionally on the same nested setup. A failing test prints the end of the guest console.
```
with:
```markdown
In the Lima VM, build into a guest path (`CARGO_TARGET_DIR=~/target`) and install the profile for `$HOME/target/debug/vmkit-sandbox`.

The contract suite (`tests/contract.rs`) runs every test against both backends with a busybox guest: boot and exit method, reset backstop, panic, kill, disk order, device budget, guest-initiated vsock, and the sandbox's contents and privileges. The network suite (`tests/network.rs`) is the hostile guest: cloud metadata, private and host addresses, the gateway, other VMs and spoofed sources must be unreachable, while allowed destinations, DNS and port forwards work. Set `VMKIT_TEST_TAP=<tap>` (a tap device you own, e.g. created with `sudo ip tuntap add vmkt0 mode tap user $(id -u)`) to also boot a VM with a network interface; the tap test must run alone because both backends would open the same tap: `VMKIT_TEST_TAP=vmkt0 cargo test --test contract a_tap_backed_nic_boots -- --test-threads=1`. Without `VMKIT_TEST_TAP` it returns early even under `VMKIT_REQUIRE_KVM_TESTS=1`, so CI does not cover it. Each VM's run directory also holds the VMM's own log (`firecracker.log`, `firecracker.stderr` or `cloud-hypervisor.log`) and, if Cloud Hypervisor's reset backstop ever fails, `backstop.log` saying why the VMM was killed (the VM then ends with `EndReason::BackstopFailed`). Pause and resume (`tests/pause.rs`) run alone: Cloud Hypervisor 53 on aarch64 can leave a guest stuck after a resume while other VMs load the host. That was reproduced with plain `ch-remote` under nested virtualization; Firecracker is unaffected. Keep the contract suite's `--test-threads` at about half the CPUs; under heavier load, Cloud Hypervisor guests also stalled occasionally on the same nested setup. A failing test prints the end of the guest console.
```

- [ ] **Step 3: Format, lint and test**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -q
```
Expected: no clippy warnings; every test passes.

- [ ] **Step 4: Commit**

```bash
git add .github README.md
git commit -m 'ci, docs: sandbox and network suites'
```

## Spec coverage (M2b)

| Spec item | Task |
|---|---|
| §3 T5: default egress cannot reach link-local, metadata, private, loopback-mapped or host addresses, or other VMs; no spoofing | 2, 3 |
| §3 T6: VMMs unprivileged in a sandbox exposing only that VM's files and devices; nothing runs as root | 1, 2 |
| §4.1 `net` and `sandbox` modules; `$VMKIT_PASTA` discovery | 1, 2 |
| §9.2 user, mount, PID and net namespaces; identity-mapped uid and gid | 1 |
| §9.2 minimal tmpfs root, the fixed `/vm` layout, `/dev/net/tun` only with a network | 1, 2 |
| §9.2 `O_NOFOLLOW` attachment by descriptor (`open_tree`/`move_mount`) | 1 |
| §9.2 `no_new_privs`, inherited descriptors closed, file and process rlimits | 1 |
| §9.2 cgroup v2 limits from memory and vCPUs, with a warning hook when unavailable | 2 |
| §9.2 Cloud Hypervisor `--seccomp true` and Landlock limited to `/vm` | 2 |
| §9.3 `tap0` at `172.30.0.1/30`, `ip_forward` in the namespace only | 1 |
| §9.3 nftables forward, input and masquerade; IPv6 and spoofed sources dropped | 2 |
| §9.3 `pasta` with `--no-map-gw` and DNS forwarding | 1, 2 |
| §9.3 egress modes `restricted`, `allow=`, `deny-all`, `open` | 2, 3 |
| §9.3 port forwarding through pasta plus DNAT | 2, 3 |
| §11.3 sandbox-contents contract test | 2 |
| §11.5 hostile guest: metadata, the gateway's host services, RFC 1918, other VMs, spoofing | 3 |
| §11.7 CI on x86_64 KVM | 4 |
| §14 items 1 and 2 | 1, 2, 3 (and the results above) |

**Left to M3 (kiln):** delivering the guest's addressing in the config message (§9.5), `kiln run`'s `--net`, `--egress` and `-p` flags, and the one-time warning when `vmkit::cgroups_available()` is false.
