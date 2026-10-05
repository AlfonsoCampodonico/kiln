# M2a: vmkit core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create `vmkit`, a new Rust crate and repository that boots microVMs on Firecracker and Cloud Hypervisor behind one VMM-neutral API, and ships the `base` kernel profile they boot.

**Architecture:**
- **`Vmm` and `Vm` traits** with `Capabilities`; callers decide from capabilities, never from the backend's name.
- **Two drivers**, each spawning its VMM and driving its REST API over a Unix socket with a small HTTP client.
- **Process handling** appends the guest serial to a console log and reports how the VM ended.
- **Cloud Hypervisor reset backstop:** a watcher kills the VMM when the guest resets, so a workload never runs twice.
- **Kernel profile:** Firecracker's CI guest config plus vmkit fragments, built by a pinned, checksummed script.
- **Contract suite:** a busybox test guest runs one parametrised suite against both backends on KVM.

**Tech Stack:**
- Rust 2024; runtime dependencies `serde_json` and `thiserror`; dev dependency `tempfile`.
- Firecracker 1.17.0, Cloud Hypervisor 53.0, Linux 6.18.54 LTS, busybox.
- GitHub Actions on x86_64 KVM; a Lima template for aarch64.

**Spec:** `/Users/alfonso/Github/Personal/playground/kiln/docs/superpowers/specs/2026-09-30-kiln-design.md` (rev 2.2). This plan implements milestone M2 minus the sandbox and networking:
- §4.1: the `Vmm` trait with snapshot/restore shapes only, both drivers, `Capabilities`, binary discovery and the `kernels/` pipeline;
- §8.2: kernel profiles;
- §11.3: the contract suite, minus the sandbox-contents check;
- §11.7: CI hosts;
- §14: verification items 3–5, plus item 1 for the plain namespace setup (results below).

Plan **M2b** (separate) adds `sandbox` (§9.2), `net` (§9.3), the sandbox-contents contract test and the hostile-guest network tests.

## Global Constraints

- **Repository:** a new repository at `/Users/alfonso/Github/Personal/playground/vmkit`, Apache-2.0, default branch `main`. Do not create or push a GitHub remote; the controller asks the user.
- **Commits:** plain messages with no `Co-Authored-By`, `Claude-Session` or other attribution lines.
- **Crate:**
  - `vmkit`, with `#![forbid(unsafe_code)]`.
  - It must build and pass its unit tests on macOS and Linux; kiln's non-run commands depend on it.
  - Everything that creates VMs needs Linux and KVM.
- **Pinned versions:**
  - Firecracker 1.17.0 and Cloud Hypervisor 53.0 are the `MIN_VERSION`s and what `scripts/install-vmms.sh` installs.
  - The Linux kernel is 6.18.54 (`kernels/VERSION`), from Firecracker's `microvm-kernel-ci-<arch>-6.18.config` at tag v1.17.0.
  - Every download is checked against a pinned SHA-256.
- **Binary discovery:** `$VMKIT_FIRECRACKER` and `$VMKIT_CLOUD_HYPERVISOR`, else `PATH`. Versions are checked when a backend is discovered.
- **Capabilities (measured during planning):**

  | Backend | `max_virtio_devices` | Implicit devices | `guest_exit` | Console |
  |---|---|---|---|---|
  | Firecracker x86_64 | 17 | 0 | `Reboot` | `ttyS0` |
  | Firecracker aarch64 | 92 | 0 | `Reboot` | `ttyS0` |
  | Cloud Hypervisor | 31 | 1 (RNG) | `Poweroff` | `ttyAMA0` on aarch64, `ttyS0` on x86_64 |

- **Kernel arguments (kiln spec §8.3):** only the caller's `cmdline` plus the console and backend parameters the driver appends. On x86_64 Firecracker that is `reboot=k i8042.noaux i8042.nomux i8042.dumbkbd`.
- **vsock:** guest CID from `VsockSpec`. The host socket is `<run_dir>/vsock.sock`, and a guest-initiated connection to port `P` arrives on `<run_dir>/vsock.sock_P` on both backends.
- **The kernel profile must contain** `EROFS_FS`, `OVERLAY_FS`, `EXT4_FS`, `VIRTIO_VSOCKETS`, `SERIAL_AMBA_PL011` (aarch64), `PVH` (x86_64), `DEVTMPFS`, `UNIX98_PTYS`, `POSIX_MQUEUE` and cgroup v2 controllers, and must not contain `NETFILTER`. The build fails if any fragment option is missing from the final config.
- **Formatting:** `rustfmt.toml` sets `max_width = 120`. The plan's code is already `cargo fmt`-clean.
- **Validation:** this plan was replayed task by task on an empty repository inside the `vmkit` Lima VM (Ubuntu 26.04, 8 vCPUs, nested KVM on an M4 Pro). After every task the replay ran fmt, clippy (`-D warnings`) and the tests, plus macOS clippy, and the KVM steps ran for real. Test counts come from that replay.

## Review Focus

These are inputs no task's main tests target that will bite a real user, most likely first. Each has a pinned test in the task named.

1. **A second VM in the same run directory.** Leftover sockets and a leftover Cloud Hypervisor `events.json` with a `rebooting` event must not break or kill the new VM. Pinned in Task 4 (`a_panic_ends_the_vm` runs two VMs in one directory; it failed before the fix).
2. **A guest that resets instead of powering off on Cloud Hypervisor**, which is what a panicking kiln-init does. The VM must end exactly once and the workload must not run twice. Pinned in Task 4 (`a_guest_reset_ends_the_vm_once` counts `VMKIT-GUEST-UP`).
3. **More disks than the backend can take.** This must be refused before any VMM process starts, and exactly the budget must boot. Pinned in Task 3 (`device_budget_is_enforced_before_any_vmm_starts`).
4. **The console after a guest reset.** It must keep everything printed before the reset, which is what kiln shows on failure. Pinned in Task 3 (`console_is_appended_not_truncated`) and Task 4 (serial in `Tty` mode, never `file=`).
5. **A VMM that rejects its arguments or exits at startup.** The error must carry the VMM's own message, not a timeout. Pinned in Task 3 (`early_exit_is_reported_with_the_vmm_log`).

## Results of spec §14 for M2 (checked during planning)

- **Item 1 (VMMs in an unprivileged user/mount/PID/net namespace sandbox, reaching `/dev/kvm` through the `kvm` group): true.**
  - Ubuntu 23.10 and later, including 26.04, sets `kernel.apparmor_restrict_unprivileged_userns=1`. An unconfined binary then cannot write its `uid_map`.
  - With an AppArmor profile granting `userns`, both VMMs boot inside `unshare -Urnmp`. A tmpfs root and bind-mounting `/dev/kvm` work, as does Cloud Hypervisor's `--seccomp true --landlock`.
  - Consequence for M2b: the sandbox lives in a small helper binary with an installable AppArmor profile, as bubblewrap does. Fedora, Arch and Debian need no profile.
- **Item 2 (tap and nftables in a user-namespace-owned net namespace, with `pasta` attached): true**, with the same AppArmor condition.
  - A tap device, `ip_forward` and nftables tables and chains all work.
  - `pasta --netns … --userns …` attaches and provides egress.
  - It is wired up in M2b.
- **Item 3 (Cloud Hypervisor vsock uses Firecracker's `<uds>_<port>` scheme for guest-initiated connections): true.** Pinned in the contract suite.
- **Item 4 (Cloud Hypervisor exits on guest power-off): true on aarch64** (PSCI `SYSTEM_OFF`). x86_64 is checked by the CI contract job (Task 5).
- **Item 5 (Cloud Hypervisor aarch64 under Lima nested virtualization): true.**

## Design decisions (rulings made while planning)

- **No async runtime and no hyper.** A 60-line HTTP/1.1 client over `UnixStream` is enough for one small JSON request per connection.
- **Cloud Hypervisor events go to a regular file that vmkit tails, not a FIFO.** `mknod` isn't available on macOS, where the crate must build; the file is in the private run directory.
- **The `Vmm::create` + `Vm::start` split is kept on both backends.**
  - Firecracker: configure over the API, then `InstanceStart`.
  - Cloud Hypervisor: `vm.create`, then `vm.boot`.
  - This is also why Landlock rules go in the `vm.create` body.
- **`VmSpec::check` is public**, so callers such as kiln can validate a device budget before building disks.
- **Known upstream issue:** Cloud Hypervisor 53 on aarch64 can leave a guest stuck after pause/resume when the host is saturated.
  - Reproduced with plain `ch-remote` and 12 busy VMs under nested virtualization; Firecracker passed 25 of 25 rounds under the same load.
  - Pause/resume is a separate test binary that runs alone (2 of 2 passes in each of 5 runs). The contract suite runs with `--test-threads=4`: 23 of 24 runs passed, and the one failure was a stalled Cloud Hypervisor guest.
  - Kiln never pauses VMs; snapshot work (project #2) must re-check this on bare metal.
- **The x86_64 kernel and the x86_64 drivers are first booted by CI** (Task 5). It cross-builds fine, but booting it needs x86 KVM, which this Mac does not have.

## File Structure

```
vmkit/
  Cargo.toml, rustfmt.toml, .gitignore, LICENSE, README.md
  src/lib.rs            Backend, re-exports
  src/error.rs          Error
  src/spec.rs           VmSpec, Disk, VsockSpec, NetSpec, Capabilities, GuestExit, VmEnd, EndReason, snapshot shapes
  src/vmm.rs            Vmm and Vm traits
  src/http.rs           HTTP/1.1 over a Unix socket
  src/binary.rs         binary discovery and version checks
  src/process.rs        the VMM child process: spawn, readiness, kill, wait
  src/firecracker.rs    Firecracker driver
  src/cloud_hypervisor.rs  Cloud Hypervisor driver
  src/events.rs         Cloud Hypervisor event stream and reset detection
  tests/common/mod.rs   Case: a VM under test, shared by the KVM test binaries
  tests/contract.rs     the contract suite (kiln spec §11.3)
  tests/pause.rs        pause/resume, run alone
  kernels/              VERSION, SHA256SUMS, build.sh, fragments/, Firecracker CI configs
  testguest/            init, vsock-hello.c, build-initramfs.sh, smoke.sh
  scripts/install-vmms.sh
  .github/workflows/ci.yml, kernels.yml
  lima/vmkit.yaml
```

---


### Task 1: Repository, crate skeleton and the VM spec types

**Files:**
- Create: `Cargo.toml`, `rustfmt.toml`, `.gitignore`, `LICENSE`, `src/lib.rs`, `src/error.rs`, `src/spec.rs`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: nothing (new repository).
- Produces:
  - `vmkit::{Error, Result}`. `Error` variants: `Io`, `BinaryNotFound { binary, env }`, `VersionTooOld { binary, found, min }`, `VersionUnknown { binary, output }`, `Api { backend, method, path, status, body }`, `Http(String)`, `TooManyDevices { requested, available }`, `InvalidSpec(String)`, `Unsupported(&'static str)`, `EarlyExit(String)`, `Timeout(&'static str)`.
  - `VmSpec { kernel, initramfs, cmdline: Vec<String>, disks: Vec<Disk>, vcpus: u8, memory_mib: u32, vsock: Option<VsockSpec>, net: Option<NetSpec>, console_log, run_dir }` with `devices_needed()` and `check(&Capabilities) -> Result<()>` (device budget, vCPU/memory floor, one word per kernel argument).
  - `Disk { path, read_only }`, `VsockSpec { guest_cid }`, `NetSpec { tap, guest_mac }`.
  - `Capabilities { max_virtio_devices, implicit_devices, supports_diff_snapshot, supports_balloon, supports_drive_remap, guest_exit: GuestExit, console: &'static str }` with `available_devices()`; `GuestExit { Reboot, Poweroff }`.
  - `VmEnd { reason: EndReason, code: Option<i32>, signal: Option<i32> }`, `EndReason { Exited, Killed, ResetStopped }`.
  - `SnapshotBundle { dir }`, `RestoreSpec { disks, console_log, run_dir }` (shapes only; project #2 implements them).

- [ ] **Step 1: Create the repository**

The repository is `/Users/alfonso/Github/Personal/playground/vmkit` (sibling of `kiln`). It is a new repo: do not create a GitHub remote; the controller asks the user about publishing.
```bash
mkdir -p /Users/alfonso/Github/Personal/playground/vmkit && cd /Users/alfonso/Github/Personal/playground/vmkit
git init -b main
curl -sfL https://www.apache.org/licenses/LICENSE-2.0.txt -o LICENSE && head -3 LICENSE
```
Expected: the first lines include `Apache License` and `Version 2.0, January 2004`.

`Cargo.toml`:
```toml
[package]
name = "vmkit"
version = "0.1.0"
edition = "2024"
license = "Apache-2.0"
description = "VMM-neutral microVM lifecycle for Firecracker and Cloud Hypervisor"

[dependencies]
thiserror = "2.0.21"

[dev-dependencies]
tempfile = "3.27.0"
```

`rustfmt.toml`:
```toml
max_width = 120
```

`.gitignore`:
```
/target
```

- [ ] **Step 2: Write the error type**

`src/error.rs`:
```rust
use thiserror::Error;

/// Everything `vmkit` can fail with.
#[derive(Debug, Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{binary} not found: set {env} or put it on PATH")]
    BinaryNotFound { binary: &'static str, env: &'static str },
    #[error("{binary} {found} is older than the minimum supported {min}")]
    VersionTooOld {
        binary: &'static str,
        found: String,
        min: String,
    },
    #[error("cannot read the version of {binary} from {output:?}")]
    VersionUnknown { binary: &'static str, output: String },
    #[error("{backend} API {method} {path} failed with HTTP {status}: {body}")]
    Api {
        backend: &'static str,
        method: &'static str,
        path: String,
        status: u16,
        body: String,
    },
    #[error("malformed API response: {0}")]
    Http(String),
    #[error("{requested} virtio devices requested but only {available} are available")]
    TooManyDevices { requested: u32, available: u32 },
    #[error("invalid VM spec: {0}")]
    InvalidSpec(String),
    #[error("{0} is not supported yet")]
    Unsupported(&'static str),
    #[error("the VMM exited before it was ready ({0})")]
    EarlyExit(String),
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;
```

- [ ] **Step 3: Write the spec types, with their unit tests**

`devices_needed` counts disks, vsock and network. `Capabilities::implicit_devices` covers devices a backend always adds itself: Cloud Hypervisor's built-in RNG takes one of its 31 slots. These budgets were measured during planning: Firecracker aarch64 boots 91 disks plus vsock and refuses 92 plus vsock; Cloud Hypervisor boots 29 disks plus vsock plus its RNG and refuses 30.

`src/spec.rs`:
```rust
//! What a VM is made of, what backends can do, and how a VM ends (kiln spec §4.1).

use std::path::PathBuf;

/// One block device, in boot order (`vda`, `vdb`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    pub path: PathBuf,
    pub read_only: bool,
}

/// A vsock device. Guest-initiated connections to host port `P` arrive on the Unix
/// socket `<Vm::vsock_socket()>_P` (both backends use this hybrid scheme).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsockSpec {
    pub guest_cid: u32,
}

/// A network interface backed by an existing tap device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetSpec {
    pub tap: String,
    pub guest_mac: Option<String>,
}

/// A VM to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmSpec {
    pub kernel: PathBuf,
    pub initramfs: Option<PathBuf>,
    /// Kernel arguments from the caller. The driver appends the console and its
    /// backend's own parameters; nothing else contributes (kiln spec §8.3).
    pub cmdline: Vec<String>,
    pub disks: Vec<Disk>,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub vsock: Option<VsockSpec>,
    pub net: Option<NetSpec>,
    /// Guest serial output is appended here.
    pub console_log: PathBuf,
    /// A private (0700) directory for this VM's sockets and logs; it must exist.
    pub run_dir: PathBuf,
}

/// How the guest must end itself so that the VMM process exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestExit {
    /// `reboot(2)`; Firecracker exits on reboot on both arches.
    Reboot,
    /// Power off; Cloud Hypervisor exits on power-off and rebuilds the VM on reset.
    Poweroff,
}

/// What a backend supports. Callers decide from these, never from the backend's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Virtio devices the machine model allows in total.
    pub max_virtio_devices: u32,
    /// Devices the backend always adds itself (Cloud Hypervisor's RNG).
    pub implicit_devices: u32,
    pub supports_diff_snapshot: bool,
    pub supports_balloon: bool,
    pub supports_drive_remap: bool,
    pub guest_exit: GuestExit,
    /// The guest console device, e.g. `ttyS0`.
    pub console: &'static str,
}

impl Capabilities {
    /// Devices left for the caller's disks, vsock and network.
    pub fn available_devices(&self) -> u32 {
        self.max_virtio_devices - self.implicit_devices
    }
}

/// Why a VM ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The VMM exited by itself (the guest ended, or the VMM failed).
    Exited,
    /// `Vm::kill` was called.
    Killed,
    /// The guest reset and `vmkit` stopped the VMM rather than let it reboot.
    ResetStopped,
}

/// How a VM ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmEnd {
    pub reason: EndReason,
    /// The VMM's exit code, if it exited normally.
    pub code: Option<i32>,
    /// The signal that ended the VMM, if any.
    pub signal: Option<i32>,
}

/// A snapshot on disk (implemented by project #2; the shape is fixed now).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotBundle {
    pub dir: PathBuf,
}

/// Where a restored VM's resources live (implemented by project #2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreSpec {
    pub disks: Vec<Disk>,
    pub console_log: PathBuf,
    pub run_dir: PathBuf,
}

impl VmSpec {
    /// The virtio devices this VM needs: its disks, vsock and network.
    pub fn devices_needed(&self) -> u32 {
        self.disks.len() as u32 + u32::from(self.vsock.is_some()) + u32::from(self.net.is_some())
    }

    /// Rejects a spec the backend cannot run; drivers call this before starting anything.
    pub fn check(&self, caps: &Capabilities) -> crate::Result<()> {
        let requested = self.devices_needed();
        if requested > caps.available_devices() {
            return Err(crate::Error::TooManyDevices {
                requested,
                available: caps.available_devices(),
            });
        }
        if self.vcpus == 0 || self.memory_mib < 64 {
            return Err(crate::Error::InvalidSpec(
                "at least 1 vCPU and 64 MiB of memory are required".into(),
            ));
        }
        if self
            .cmdline
            .iter()
            .any(|a| a.is_empty() || a.contains(char::is_whitespace))
        {
            return Err(crate::Error::InvalidSpec(
                "kernel arguments must be single non-empty words".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(disks: usize) -> VmSpec {
        VmSpec {
            kernel: "k".into(),
            initramfs: None,
            cmdline: vec!["quiet".into()],
            disks: (0..disks)
                .map(|i| Disk {
                    path: format!("d{i}").into(),
                    read_only: true,
                })
                .collect(),
            vcpus: 1,
            memory_mib: 128,
            vsock: Some(VsockSpec { guest_cid: 3 }),
            net: None,
            console_log: "c".into(),
            run_dir: "r".into(),
        }
    }

    const CAPS: Capabilities = Capabilities {
        max_virtio_devices: 31,
        implicit_devices: 1,
        supports_diff_snapshot: false,
        supports_balloon: true,
        supports_drive_remap: false,
        guest_exit: GuestExit::Poweroff,
        console: "ttyAMA0",
    };

    #[test]
    fn device_budget_counts_disks_vsock_net_and_implicit_devices() {
        assert!(spec(29).check(&CAPS).is_ok(), "29 disks + vsock + rng = 31");
        let err = spec(30).check(&CAPS).unwrap_err();
        assert!(
            matches!(
                err,
                crate::Error::TooManyDevices {
                    requested: 31,
                    available: 30,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn kernel_arguments_are_single_words() {
        let mut s = spec(0);
        s.cmdline.push("a b".into());
        assert!(matches!(s.check(&CAPS), Err(crate::Error::InvalidSpec(_))));
    }
}
```

`src/lib.rs`:
```rust
//! VMM-neutral microVM lifecycle for Firecracker and Cloud Hypervisor (kiln spec §4.1).
#![forbid(unsafe_code)]

mod error;
mod spec;

pub use error::{Error, Result};
pub use spec::{
    Capabilities, Disk, EndReason, GuestExit, NetSpec, RestoreSpec, SnapshotBundle, VmEnd, VmSpec, VsockSpec,
};
```

- [ ] **Step 4: Add CI**

Plain checks on Linux and macOS: the library must keep building on macOS, where kiln uses it for its non-run commands.

`.github/workflows/ci.yml`:
```yaml
name: ci
on: [push, pull_request]
jobs:
  test:
    strategy:
      matrix:
        os: [ubuntu-24.04, macos-15]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - run: cargo fmt --all --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test --all
```

- [ ] **Step 5: Run the unit tests**

Run: `cargo test -q`
Expected: all pass (2 tests).

- [ ] **Step 6: Format, lint and test**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 7: Commit**

```bash
git add .gitignore Cargo.toml Cargo.lock LICENSE rustfmt.toml src .github
git commit -m 'feat: vmkit crate skeleton and VM spec types'
```


### Task 2: Kernel profile, test guest and pinned VMMs

**Files:**
- Create: `kernels/VERSION`, `kernels/SHA256SUMS`, `kernels/build.sh`, `kernels/fragments/base.config`, `kernels/fragments/base-aarch64.config`, `kernels/fragments/base-x86_64.config`, `kernels/microvm-kernel-ci-aarch64-6.18.config`, `kernels/microvm-kernel-ci-x86_64-6.18.config`, `testguest/init`, `testguest/vsock-hello.c`, `testguest/build-initramfs.sh`, `testguest/smoke.sh`, `scripts/install-vmms.sh`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `kernels/build.sh <aarch64|x86_64> <out-dir>` → `<out-dir>/vmlinux-6.18.54-<arch>` and `config-6.18.54-<arch>`, failing if any fragment option did not survive `olddefconfig`.
  - `testguest/build-initramfs.sh <out.cpio.gz>` → a reproducible busybox initramfs whose `/init` reads `vmkit.test=<up|reboot|panic|exit|idle|vsock|disks>` and `vmkit.exit=<poweroff|reboot>`, and prints `VMKIT-GUEST-UP`, `VMKIT-GUEST-TICK`, `VMKIT-DISK <name> <sectors>`, `VMKIT-VSOCK-REPLY <line>`.
  - `scripts/install-vmms.sh [dir]` → Firecracker 1.17.0 and Cloud Hypervisor 53.0 for the host arch, SHA-256 checked.
  - `testguest/smoke.sh <kernel> <initramfs>` → boots the guest once on each VMM without vmkit.

- [ ] **Step 1: Pin the kernel and fetch Firecracker's CI guest configs**

The profile starts from Firecracker's CI guest config for the 6.18 LTS series (kiln spec §8.2). The two configs are about 3,800 lines each, so fetch them from Firecracker's v1.17.0 tag; `kernels/SHA256SUMS` pins them and the kernel tarball:
```bash
mkdir -p kernels/fragments testguest scripts
for arch in aarch64 x86_64; do
  curl -sfL -o kernels/microvm-kernel-ci-$arch-6.18.config \
    https://raw.githubusercontent.com/firecracker-microvm/firecracker/v1.17.0/resources/guest_configs/microvm-kernel-ci-$arch-6.18.config
done
```

`kernels/VERSION`:
```
6.18.54
```

`kernels/SHA256SUMS`:
```
9df30b02dd8102bbd0be52556288ef6889ddbe7f1ddb96fbf847d0becf3eacac  linux-6.18.54.tar.xz
35cce8e8b754a84523ca20dc068fc3f8f03a15523be36a8623bad54e11972e2a  microvm-kernel-ci-aarch64-6.18.config
ba22401a0c7292a4c024ebcd10a562d4a1f1bfd2faed671406d3b159c0cf5215  microvm-kernel-ci-x86_64-6.18.config
```

```bash
(cd kernels && sha256sum -c SHA256SUMS --ignore-missing)
```
Expected: both configs `OK` (the tarball is checked by `build.sh` when it downloads it).

- [ ] **Step 2: Write the vmkit fragments and the build script**

The fragments add what kiln's guests need on top of Firecracker's config: erofs and overlayfs for the image layers, ext4 for scratch, vsock, devtmpfs, cgroup v2 controllers, the PL011 console Cloud Hypervisor uses on aarch64, PVH on x86_64 (one ELF `vmlinux` boots on both VMMs), and no netfilter (the egress policy lives outside the guest). The build pins `KBUILD_BUILD_*` so the same tree and toolchain give the same binary.

`kernels/fragments/base.config`:
```
# vmkit "base" profile (kiln spec §8.2), merged over Firecracker's CI guest config.
# Root filesystems: erofs layers under an overlay, ext4 scratch with online resize.
CONFIG_EROFS_FS=y
CONFIG_EROFS_FS_XATTR=y
CONFIG_EROFS_FS_POSIX_ACL=y
CONFIG_EROFS_FS_SECURITY=y
CONFIG_OVERLAY_FS=y
CONFIG_EXT4_FS=y
CONFIG_EXT4_FS_POSIX_ACL=y
CONFIG_EXT4_FS_SECURITY=y
# Control channel and devices.
CONFIG_VSOCKETS=y
CONFIG_VIRTIO_VSOCKETS=y
CONFIG_VIRTIO_BLK=y
CONFIG_VIRTIO_NET=y
CONFIG_VIRTIO_MMIO=y
CONFIG_VIRTIO_PCI=y
# Userspace expectations.
CONFIG_DEVTMPFS=y
CONFIG_DEVTMPFS_MOUNT=y
CONFIG_UNIX98_PTYS=y
CONFIG_POSIX_MQUEUE=y
CONFIG_CGROUPS=y
CONFIG_MEMCG=y
CONFIG_CGROUP_PIDS=y
CONFIG_CGROUP_SCHED=y
CONFIG_CPUSETS=y
CONFIG_BLK_CGROUP=y
CONFIG_TMPFS=y
CONFIG_TMPFS_POSIX_ACL=y
# The test guest boots a busybox initramfs.
CONFIG_BLK_DEV_INITRD=y
CONFIG_RD_GZIP=y
# Netfilter-free guest: egress policy lives outside the VM (§9.3).
# CONFIG_NETFILTER is not set
```

`kernels/fragments/base-aarch64.config`:
```
# Cloud Hypervisor aarch64 console (ttyAMA0).
CONFIG_SERIAL_AMBA_PL011=y
CONFIG_SERIAL_AMBA_PL011_CONSOLE=y
```

`kernels/fragments/base-x86_64.config`:
```
# One ELF vmlinux boots on both VMMs through PVH.
CONFIG_PVH=y
```

`kernels/build.sh` (mode 0755):
```bash
#!/usr/bin/env bash
# Builds the vmkit kernel for one arch: Firecracker's CI guest config for the series
# plus vmkit's fragments (kiln spec §8.2). The source tarball is checked against SHA256SUMS.
# Usage: kernels/build.sh <aarch64|x86_64> <out-dir>
set -euo pipefail
arch=$1; out=$(realpath -m "$2")
here=$(cd "$(dirname "$0")" && pwd)
version=$(cat "$here/VERSION")
series=$(echo "$version" | cut -d. -f1-2)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -sfL -o "$work/linux-$version.tar.xz" "https://cdn.kernel.org/pub/linux/kernel/v${version%%.*}.x/linux-$version.tar.xz"
(cd "$work" && grep " linux-$version.tar.xz\$" "$here/SHA256SUMS" | sha256sum -c --quiet -)
(cd "$here" && grep " microvm-kernel-ci-$arch-$series.config\$" SHA256SUMS | sha256sum -c --quiet -)
tar -xJf "$work/linux-$version.tar.xz" -C "$work"
src="$work/linux-$version"
case $arch in
  aarch64) karch=arm64; image=arch/arm64/boot/Image; cross=aarch64-linux-gnu- ;;
  x86_64) karch=x86; image=vmlinux; cross=x86_64-linux-gnu- ;;
esac
[ "$(uname -m)" = "$arch" ] && cross=
# Fixed build metadata, so the same tree and toolchain give the same binary.
export KBUILD_BUILD_TIMESTAMP="1970-01-01 00:00:00 UTC" KBUILD_BUILD_USER=vmkit KBUILD_BUILD_HOST=vmkit
cp "$here/microvm-kernel-ci-$arch-$series.config" "$src/.config"
(cd "$src" && ARCH=$karch scripts/kconfig/merge_config.sh -m .config "$here/fragments/base.config" "$here/fragments/base-$arch.config" >/dev/null \
  && make -s ARCH=$karch CROSS_COMPILE=$cross olddefconfig \
  && make -s ARCH=$karch CROSS_COMPILE=$cross -j"$(nproc)" "$(basename $image)")
mkdir -p "$out"
cp "$src/$image" "$out/vmlinux-$version-$arch"
cp "$src/.config" "$out/config-$version-$arch"
# Every fragment line must have survived olddefconfig.
missing=0
for f in "$here/fragments/base.config" "$here/fragments/base-$arch.config"; do
  while IFS= read -r line; do
    case $line in CONFIG_*=*) grep -qx "$line" "$src/.config" || { echo "missing: $line"; missing=1; } ;; esac
  done < "$f"
done
exit $missing
```

- [ ] **Step 3: Write the test guest**

A busybox initramfs plus a 30-line static vsock client (busybox has none). `init` is PID 1: an `exit` action makes the kernel panic, which `panic=-1` turns into the end of the VM. The archive is built with fixed mtimes, owners and `cpio --reproducible`, so its digest is stable.

`testguest/init` (mode 0755):
```bash
#!/bin/busybox sh
# vmkit contract-suite guest. Kernel arguments:
#   vmkit.test=<action>  up | reboot | panic | exit | idle | vsock | disks
#   vmkit.exit=<method>  poweroff | reboot  (how "up", "vsock" and "disks" end)
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sys /sys
/bin/busybox mount -t devtmpfs dev /dev
action=up
method=poweroff
for arg in $(/bin/busybox cat /proc/cmdline); do
  case $arg in
    vmkit.test=*) action=${arg#vmkit.test=} ;;
    vmkit.exit=*) method=${arg#vmkit.exit=} ;;
  esac
done
end() {
  case $method in
    reboot) /bin/busybox reboot -f ;;
    *) /bin/busybox poweroff -f ;;
  esac
}
echo "VMKIT-GUEST-UP action=$action"
case $action in
  up) end ;;
  reboot) /bin/busybox reboot -f ;;
  panic) echo c > /proc/sysrq-trigger ;;
  exit) exit 0 ;;  # PID 1 exiting panics the kernel
  idle) while :; do echo "VMKIT-GUEST-TICK"; /bin/busybox sleep 0.2; done ;;
  vsock) /bin/vsock-hello 1234; end ;;
  disks)
    for d in /sys/block/vd*; do echo "VMKIT-DISK $(/bin/busybox basename "$d") $(/bin/busybox cat "$d/size")"; done
    end ;;
esac
/bin/busybox sleep 5
echo "VMKIT-GUEST-FELL-THROUGH"
```

`testguest/vsock-hello.c`:
```c
/* vsock-hello PORT: connects to the host (CID 2) on PORT, sends one line, prints the reply. */
#include <linux/vm_sockets.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    int s = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (s < 0) { perror("socket"); return 1; }
    struct sockaddr_vm addr = {.svm_family = AF_VSOCK, .svm_cid = VMADDR_CID_HOST, .svm_port = (unsigned)atoi(argv[1])};
    if (connect(s, (struct sockaddr *)&addr, sizeof addr) < 0) { perror("connect"); return 1; }
    const char *msg = "VMKIT-VSOCK-HELLO\n";
    if (write(s, msg, strlen(msg)) < 0) { perror("write"); return 1; }
    char buf[128];
    ssize_t n = read(s, buf, sizeof buf - 1);
    if (n > 0) { buf[n] = 0; printf("VMKIT-VSOCK-REPLY %s", buf); }
    return 0;
}
```

`testguest/build-initramfs.sh` (mode 0755):
```bash
#!/usr/bin/env bash
# Builds the contract-suite guest: a busybox initramfs for the host's arch.
# Needs a static busybox (`busybox-static` on Debian/Ubuntu).
# Usage: testguest/build-initramfs.sh <out.cpio.gz>
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=$(realpath -m "$1")
mkdir -p "$(dirname "$out")"
busybox=$(command -v busybox)
file -L "$busybox" | grep -q "statically linked" || { echo "busybox must be static" >&2; exit 1; }
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir -p "$root"/{bin,proc,sys,dev}
cp "$busybox" "$root/bin/busybox"
cp "$here/init" "$root/init"
"${CC:-cc}" -static -O2 -o "$root/bin/vsock-hello" "$here/vsock-hello.c"
chmod 0755 "$root/init" "$root/bin/busybox" "$root/bin/vsock-hello"
# Fixed ownership and mtimes, sorted entries: the image is reproducible.
(cd "$root" && find . -print0 | LC_ALL=C sort -z | xargs -0 touch -h -d @0 \
  && find . -print0 | LC_ALL=C sort -z | cpio --null -o -H newc -R 0:0 --reproducible --quiet) | gzip -n -9 > "$out"
```

`testguest/smoke.sh` (mode 0755):
```bash
#!/usr/bin/env bash
# Boots the test guest once on each VMM, without vmkit: checks a kernel and
# initramfs before the Rust drivers exist. Needs KVM and the VMMs on PATH.
# Usage: testguest/smoke.sh <kernel> <initramfs.cpio.gz>
set -euo pipefail
kernel=$1; initramfs=$2
case $(uname -m) in aarch64) ch_console=ttyAMA0 ;; *) ch_console=ttyS0 ;; esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
args="panic=-1 rdinit=/init vmkit.test=up"
cat > "$work/fc.json" <<JSON
{"boot-source": {"kernel_image_path": "$kernel", "initrd_path": "$initramfs",
  "boot_args": "console=ttyS0 reboot=k $args vmkit.exit=reboot"},
 "drives": [], "machine-config": {"vcpu_count": 1, "mem_size_mib": 256}}
JSON
timeout 60 firecracker --no-api --config-file "$work/fc.json" > "$work/fc.log" 2>&1
grep -q VMKIT-GUEST-UP "$work/fc.log" && echo "firecracker: guest booted and exited"
timeout 60 cloud-hypervisor --kernel "$kernel" --initramfs "$initramfs" --cmdline "console=$ch_console $args vmkit.exit=poweroff" \
  --cpus boot=1 --memory size=256M --serial tty --console off > "$work/ch.log" 2>&1
grep -q VMKIT-GUEST-UP "$work/ch.log" && echo "cloud-hypervisor: guest booted and exited"
```

- [ ] **Step 4: Write the pinned VMM installer**

The versions here are the ones the drivers' `MIN_VERSION` constants (Tasks 3 and 4) and the contract suite are tested with.

`scripts/install-vmms.sh` (mode 0755):
```bash
#!/usr/bin/env bash
# Installs the pinned Firecracker and Cloud Hypervisor releases for this machine's
# arch into DIR (default ~/.local/bin), verifying each download's SHA-256.
# These are the versions vmkit's MIN_VERSION constants and contract suite are tested with.
set -euo pipefail
dir=${1:-$HOME/.local/bin}
arch=$(uname -m)
fc_version=1.17.0
ch_version=53.0
case $arch in
  aarch64)
    fc_sha=e351ebe4f7a16b5873bbd51005d2e6767103cff4d5ebc829df2d3f95a93e2256
    ch_asset=cloud-hypervisor-static-aarch64
    ch_sha=f192b510eea1c710cbc439d716bb0573c223fc463dbe3e6523788a2b7ef62850 ;;
  x86_64)
    fc_sha=06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558
    ch_asset=cloud-hypervisor-static
    ch_sha=448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc ;;
  *) echo "unsupported arch $arch" >&2; exit 1 ;;
esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fetch() { # url sha out
  curl -sfL -o "$3" "$1"
  echo "$2  $3" | sha256sum -c --quiet -
}
fetch "https://github.com/firecracker-microvm/firecracker/releases/download/v$fc_version/firecracker-v$fc_version-$arch.tgz" "$fc_sha" "$work/fc.tgz"
tar -xzf "$work/fc.tgz" -C "$work"
fetch "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v$ch_version/$ch_asset" "$ch_sha" "$work/cloud-hypervisor"
mkdir -p "$dir"
install -m 0755 "$work/release-v$fc_version-$arch/firecracker-v$fc_version-$arch" "$dir/firecracker"
install -m 0755 "$work/cloud-hypervisor" "$dir/cloud-hypervisor"
"$dir/firecracker" --version | head -1
"$dir/cloud-hypervisor" --version | head -1
```

- [ ] **Step 5: Build and boot on KVM**

This needs Linux with KVM: on the Mac, the `vmkit` Lima VM (nested virtualization; see Task 5's template), with the repo on a shared mount. Install the VMMs, build the aarch64 kernel (about 3 minutes on 8 vCPUs) and the guest, then boot it once on each VMM:

Run (Linux with KVM: the Lima VM): `scripts/install-vmms.sh $HOME/bin && kernels/build.sh aarch64 out && testguest/build-initramfs.sh out/initramfs-aarch64.cpio.gz && testguest/smoke.sh out/vmlinux-6.18.54-aarch64 out/initramfs-aarch64.cpio.gz`
Expected: `Firecracker v1.17.0`, `cloud-hypervisor v53.0`, then `firecracker: guest booted and exited` and `cloud-hypervisor: guest booted and exited`.

Run (Linux with KVM: the Lima VM): `testguest/build-initramfs.sh /tmp/again.cpio.gz && cmp out/initramfs-aarch64.cpio.gz /tmp/again.cpio.gz && echo reproducible`
Expected: `reproducible`.

Cross-building x86_64 also works (`kernels/build.sh x86_64 out-x86` with `gcc-x86-64-linux-gnu`); booting it needs x86 KVM, which CI provides (Task 5).

- [ ] **Step 6: Commit**

`out/` is build output: add it to `.gitignore` first.
```bash
echo '/out' >> .gitignore
git add .gitignore kernels testguest scripts
git commit -m 'feat: base kernel profile, test guest and pinned VMM installer'
```


### Task 3: The `Vmm` trait, the Firecracker driver and the contract suite

**Files:**
- Create: `src/http.rs`, `src/binary.rs`, `src/process.rs`, `src/vmm.rs`, `src/firecracker.rs`, `tests/common/mod.rs`, `tests/contract.rs`, `tests/pause.rs`
- Modify: `Cargo.toml`, `src/lib.rs`

**Interfaces:**
- Consumes: Task 1's types (`VmSpec::check`, `Capabilities`, `VmEnd`, `Error`); Task 2's kernel, guest and VMMs (for the contract suite).
- Produces:
  - `trait Vmm: Send + Sync { name, capabilities, create(&VmSpec) -> Result<Box<dyn Vm>>, restore(&SnapshotBundle, &RestoreSpec) }` and `trait Vm: Send { start, pause, resume, kill, wait -> Result<VmEnd>, wait_timeout(Duration) -> Result<Option<VmEnd>>, snapshot(&Path), capabilities, vsock_socket() -> Option<&Path> }`. Dropping a `Vm` kills its VMM.
  - `Firecracker::discover()` (`$VMKIT_FIRECRACKER`, else `PATH`; at least `firecracker::MIN_VERSION` = 1.17.0), `Backend { Firecracker }` with `ALL` and `discover() -> Result<Box<dyn Vmm>>`.
  - Crate-internal: `http::request(socket, method, path, Option<&Value>) -> io::Result<Response { status, body }>`; `binary::{find, parse_version, check_version}`; `process::{spawn(binary, args, console_log, log) -> Result<Proc>, clear_socket}` and `Proc { wait_for_socket, kill, try_end, wait(Option<Duration>) }` (`Clone`, shared with Task 4's backstop).
  - The run directory holds `firecracker.sock`, `firecracker.log` and `vsock.sock`; the guest console is appended to `VmSpec::console_log`.
  - `tests/common/mod.rs`: `Case` (a VM under test with its run directory: `new(Backend) -> Option<Case>`, `spec(action)`, `run(&VmSpec) -> (Box<dyn Vm>, VmEnd)`, `console()`, `tail()`, `await_console(needle, n)`) and `END`.
  - `tests/contract.rs`: the `contract!` macro that instantiates every contract test per backend; `tests/pause.rs`: pause/resume, one test per backend, run alone (Firecracker only until Task 4).

- [ ] **Step 1: Add the JSON dependency**

In `Cargo.toml`, replace:
```toml
[dependencies]
thiserror = "2.0.21"
```
with:
```toml
[dependencies]
serde_json = "1.0.151"
thiserror = "2.0.21"
```

- [ ] **Step 2: Write the HTTP client, with its unit tests**

Both VMMs serve a small REST API on a Unix socket, one request per connection. This client is about 60 lines instead of hyper and an async runtime; it caps response bodies at 1 MiB and fails on truncation.

`src/http.rs`:
```rust
//! A minimal HTTP/1.1 client over a Unix socket: enough for the VMM REST APIs,
//! which take small JSON bodies and answer one request per connection.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Largest response body accepted (API errors are a few hundred bytes).
const MAX_BODY: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Response {
    pub status: u16,
    pub body: String,
}

/// Sends one request with an optional JSON body and reads the response.
pub(crate) fn request(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> io::Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\n");
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(req.as_bytes())?;
    read_response(BufReader::new(stream))
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn read_response(mut r: impl BufRead) -> io::Result<Response> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| bad(format!("bad status line {line:?}")))?;
    let mut length: Option<u64> = None;
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Err(bad("connection closed in headers"));
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(
                value
                    .trim()
                    .parse()
                    .map_err(|_| bad(format!("bad Content-Length {value:?}")))?,
            );
        }
    }
    let length = length.unwrap_or(0);
    if length > MAX_BODY {
        return Err(bad(format!("response body of {length} bytes")));
    }
    let mut body = Vec::with_capacity(length as usize);
    r.take(length).read_to_end(&mut body)?;
    if body.len() as u64 != length {
        return Err(bad("connection closed in body"));
    }
    Ok(Response {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn parses_status_headers_and_body() {
        let raw = b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\ncontent-length: 17\r\n\r\n{\"fault\":\"nope\"}\n";
        let r = read_response(&raw[..]).unwrap();
        assert_eq!((r.status, r.body.as_str()), (400, "{\"fault\":\"nope\"}\n"));
        let no_content = read_response(&b"HTTP/1.1 204 No Content\r\n\r\n"[..]).unwrap();
        assert_eq!((no_content.status, no_content.body.as_str()), (204, ""));
    }

    #[test]
    fn rejects_truncated_and_oversized_responses() {
        assert!(read_response(&b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort"[..]).is_err());
        assert!(read_response(&b"HTTP/1.1 200 OK\r\nContent-Length: 99999999\r\n\r\n"[..]).is_err());
        assert!(read_response(&b"garbage\r\n\r\n"[..]).is_err());
    }

    #[test]
    fn sends_the_request_over_a_unix_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut req = vec![0u8; 4096];
            let n = s.read(&mut req).unwrap();
            s.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
            String::from_utf8_lossy(&req[..n]).into_owned()
        });
        let r = request(
            &sock,
            "PUT",
            "/actions",
            Some(&serde_json::json!({"action_type": "InstanceStart"})),
        )
        .unwrap();
        assert_eq!(r.status, 204);
        let req = server.join().unwrap();
        assert!(req.starts_with("PUT /actions HTTP/1.1\r\n"), "{req}");
        assert!(req.ends_with("\r\n\r\n{\"action_type\":\"InstanceStart\"}"), "{req}");
        assert!(req.contains("Content-Length: 31\r\n"), "{req}");
    }
}
```

- [ ] **Step 3: Write binary discovery and version checks, with their unit tests**

`$VMKIT_FIRECRACKER` / `$VMKIT_CLOUD_HYPERVISOR` override `PATH` (kiln spec §4.1). Selecting a binary through the environment confers nothing beyond what the user already has, since nothing runs privileged.

`src/binary.rs`:
```rust
//! Finding the VMM binaries and checking their versions (kiln spec §4.1).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// A `major.minor.patch` version; missing parts are 0.
pub(crate) type Version = (u32, u32, u32);

/// `$env` if set, else the first executable `name` on `PATH`.
pub(crate) fn find(name: &'static str, env: &'static str) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(env).filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return if is_executable(&p) {
            Ok(p)
        } else {
            Err(Error::BinaryNotFound { binary: name, env })
        };
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
        .ok_or(Error::BinaryNotFound { binary: name, env })
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The first `v<digits>[.<digits>...]` token of `text`.
pub(crate) fn parse_version(text: &str) -> Option<Version> {
    let token = text.split_whitespace().find_map(|t| t.strip_prefix('v'))?;
    let mut parts = token.split('.').map(|p| {
        p.split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|d| d.parse().ok())
    });
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

fn show(v: Version) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// Runs `<binary> --version` and refuses anything older than `min`.
pub(crate) fn check_version(path: &Path, name: &'static str, min: Version) -> Result<Version> {
    let out = Command::new(path).arg("--version").output()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let found = parse_version(&text).ok_or_else(|| Error::VersionUnknown {
        binary: name,
        output: text.clone(),
    })?;
    if found < min {
        return Err(Error::VersionTooOld {
            binary: name,
            found: show(found),
            min: show(min),
        });
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_vmm_version_formats() {
        assert_eq!(
            parse_version("Firecracker v1.17.0\n\nSupported snapshot data format versions: v8.0.0"),
            Some((1, 17, 0))
        );
        assert_eq!(
            parse_version("cloud-hypervisor v53.0\nMigration Protocol Versions: 0"),
            Some((53, 0, 0))
        );
        assert_eq!(parse_version("cloud-hypervisor v54.1-dirty"), Some((54, 1, 0)));
        assert_eq!(parse_version("no version here"), None);
    }

    #[test]
    fn env_override_must_be_executable() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fc");
        std::fs::write(&fake, "#!/bin/sh\necho 'Firecracker v1.2.0'\n").unwrap();
        let err = check_version(&fake, "firecracker", (1, 17, 0));
        assert!(err.is_err(), "not executable yet");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = check_version(&fake, "firecracker", (1, 17, 0)).unwrap_err();
        assert!(matches!(err, Error::VersionTooOld { .. }), "{err}");
        assert_eq!(check_version(&fake, "firecracker", (1, 2, 0)).unwrap(), (1, 2, 0));
    }
}
```

- [ ] **Step 4: Write the VMM process handling, with its unit tests**

The guest serial is the VMM's stdout, appended (never truncated) to the console log. `wait_for_socket` fails early with the VMM's last log lines if it exits before its API is up. `clear_socket` removes a socket a previous VMM left in the run directory, since both VMMs refuse to bind over one.

`src/process.rs`:
```rust
//! The VMM child process: spawning, readiness, kill and wait.

use std::fs::{File, OpenOptions};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::spec::{EndReason, VmEnd};

/// How long a VMM may take to open its API socket.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);

/// Removes a socket a previous VMM left in the run directory; VMMs refuse to bind over it.
pub(crate) fn clear_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => Ok(std::fs::remove_file(path)?),
        Ok(_) => Err(Error::InvalidSpec(format!(
            "{} exists and is not a socket",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// A running VMM. Shared with the Cloud Hypervisor reset backstop, which may kill it.
#[derive(Clone)]
pub(crate) struct Proc {
    child: Arc<Mutex<Child>>,
    killed: Arc<AtomicBool>,
    reset_stopped: Arc<AtomicBool>,
    end: Arc<Mutex<Option<VmEnd>>>,
    log: PathBuf,
}

/// Starts `binary args...` with the guest serial (the VMM's stdout) appended to
/// `console_log` and the VMM's own messages in `log`.
pub(crate) fn spawn(binary: &Path, args: &[String], console_log: &Path, log: &Path) -> Result<Proc> {
    let console = OpenOptions::new().create(true).append(true).open(console_log)?;
    let log_file = File::create(log)?;
    let child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(console)
        .stderr(log_file)
        .spawn()?;
    Ok(Proc {
        child: Arc::new(Mutex::new(child)),
        killed: Arc::new(AtomicBool::new(false)),
        reset_stopped: Arc::new(AtomicBool::new(false)),
        end: Arc::new(Mutex::new(None)),
        log: log.to_path_buf(),
    })
}

impl Proc {
    /// Waits until `socket` accepts connections, failing early if the VMM exits.
    pub(crate) fn wait_for_socket(&self, socket: &Path) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(end) = self.try_end()? {
                let tail = std::fs::read_to_string(&self.log).unwrap_or_default();
                let tail: String = tail
                    .lines()
                    .rev()
                    .take(5)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(" | ");
                return Err(Error::EarlyExit(format!("{end:?}: {tail}")));
            }
            if UnixStream::connect(socket).is_ok() {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(Error::Timeout("the VMM API socket"));
            }
            std::thread::sleep(POLL);
        }
    }

    /// Kills the VMM (idempotent).
    pub(crate) fn kill(&self) -> Result<()> {
        self.killed.store(true, Ordering::SeqCst);
        self.signal_kill()
    }

    fn signal_kill(&self) -> Result<()> {
        let mut child = self.child.lock().expect("not poisoned");
        match child.kill() {
            Ok(()) => Ok(()),
            // Already reaped: nothing to kill.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn end_from(&self, status: ExitStatus) -> VmEnd {
        let reason = if self.reset_stopped.load(Ordering::SeqCst) {
            EndReason::ResetStopped
        } else if self.killed.load(Ordering::SeqCst) {
            EndReason::Killed
        } else {
            EndReason::Exited
        };
        VmEnd {
            reason,
            code: status.code(),
            signal: status.signal(),
        }
    }

    /// The end, if the VMM has exited (non-blocking).
    pub(crate) fn try_end(&self) -> Result<Option<VmEnd>> {
        let mut end = self.end.lock().expect("not poisoned");
        if end.is_none()
            && let Some(status) = self.child.lock().expect("not poisoned").try_wait()?
        {
            *end = Some(self.end_from(status));
        }
        Ok(*end)
    }

    /// Waits up to `timeout` (forever when `None`) for the VMM to exit.
    pub(crate) fn wait(&self, timeout: Option<Duration>) -> Result<Option<VmEnd>> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if let Some(end) = self.try_end()? {
                return Ok(Some(end));
            }
            if deadline.is_some_and(|d| Instant::now() > d) {
                return Ok(None);
            }
            std::thread::sleep(POLL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, dir: &Path) -> Proc {
        spawn(
            Path::new("/bin/sh"),
            &["-c".into(), script.into()],
            &dir.join("console"),
            &dir.join("log"),
        )
        .unwrap()
    }

    #[test]
    fn reports_how_the_process_ended() {
        let dir = tempfile::tempdir().unwrap();
        let p = sh("echo serial; exit 3", dir.path());
        let end = p.wait(Some(Duration::from_secs(5))).unwrap().unwrap();
        assert_eq!((end.reason, end.code), (EndReason::Exited, Some(3)));
        assert_eq!(std::fs::read_to_string(dir.path().join("console")).unwrap(), "serial\n");

        let p = sh("sleep 30", dir.path());
        assert_eq!(p.wait(Some(Duration::from_millis(50))).unwrap(), None);
        p.kill().unwrap();
        let end = p.wait(None).unwrap().unwrap();
        assert_eq!((end.reason, end.signal), (EndReason::Killed, Some(9)));
        p.kill().unwrap();
    }

    #[test]
    fn console_is_appended_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        for word in ["one", "two"] {
            sh(&format!("echo {word}"), dir.path()).wait(None).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("console")).unwrap(),
            "one\ntwo\n"
        );
    }

    #[test]
    fn early_exit_is_reported_with_the_vmm_log() {
        let dir = tempfile::tempdir().unwrap();
        let p = sh("echo 'bad flag' >&2; exit 1", dir.path());
        let err = p.wait_for_socket(&dir.path().join("never.sock")).unwrap_err();
        assert!(
            matches!(&err, Error::EarlyExit(msg) if msg.contains("bad flag")),
            "{err}"
        );
    }
}
```

- [ ] **Step 5: Write the traits and the Firecracker driver**

Firecracker replaces its default kernel arguments when given `boot_args`, so the driver appends the console and, on x86_64, `reboot=k` and the i8042 options; on aarch64 Firecracker itself adds `pci=off earlycon=…`. Drives attach in the order given, so the first disk is `vda`. `is_root_device` is false for all of them: kiln passes `root=/dev/vda` itself (spec §8.3). Firecracker exits on a guest reboot on both arches, so `guest_exit` is `Reboot`.

`src/vmm.rs`:
```rust
//! The backend-neutral VM lifecycle (kiln spec §4.1).

use std::path::Path;
use std::time::Duration;

use crate::error::Result;
use crate::spec::{Capabilities, RestoreSpec, SnapshotBundle, VmEnd, VmSpec};

/// A VMM backend. Create VMs from it; decide on `capabilities`, never on `name`.
pub trait Vmm: Send + Sync {
    /// For logs and diagnostics only.
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// Starts a VMM process and configures `spec` in it; the guest has not started.
    fn create(&self, spec: &VmSpec) -> Result<Box<dyn Vm>>;
    /// Restores a snapshot into a fresh VMM process (implemented by project #2).
    fn restore(&self, bundle: &SnapshotBundle, spec: &RestoreSpec) -> Result<Box<dyn Vm>>;
}

/// One VM. Dropping it kills the VMM if it is still running.
pub trait Vm: Send {
    /// Starts the guest.
    fn start(&mut self) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn resume(&mut self) -> Result<()>;
    /// Ends the VM now. Graceful shutdown goes through the guest instead.
    fn kill(&mut self) -> Result<()>;
    /// Blocks until the VMM exits.
    fn wait(&mut self) -> Result<VmEnd>;
    /// Like `wait`, but gives up after `timeout`.
    fn wait_timeout(&mut self, timeout: Duration) -> Result<Option<VmEnd>>;
    /// Writes a snapshot to `dest` (implemented by project #2).
    fn snapshot(&mut self, dest: &Path) -> Result<SnapshotBundle>;
    fn capabilities(&self) -> Capabilities;
    /// The host side of the vsock device, if the spec had one.
    fn vsock_socket(&self) -> Option<&Path>;
}
```

`src/firecracker.rs`:
```rust
//! The Firecracker driver.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::binary::{self, Version};
use crate::error::{Error, Result};
use crate::http;
use crate::process::{self, Proc};
use crate::spec::{Capabilities, GuestExit, RestoreSpec, SnapshotBundle, VmEnd, VmSpec};
use crate::vmm::{Vm, Vmm};

pub const MIN_VERSION: Version = (1, 17, 0);
const NAME: &str = "firecracker";

pub struct Firecracker {
    binary: PathBuf,
    arch: &'static str,
}

impl Firecracker {
    /// Finds the binary (`$VMKIT_FIRECRACKER`, else `PATH`) and checks its version.
    pub fn discover() -> Result<Self> {
        let binary = binary::find("firecracker", "VMKIT_FIRECRACKER")?;
        binary::check_version(&binary, NAME, MIN_VERSION)?;
        Ok(Self {
            binary,
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
        let api = spec.run_dir.join("firecracker.sock");
        let args = vec![
            "--api-sock".into(),
            api.display().to_string(),
            "--id".into(),
            "vmkit".into(),
        ];
        process::clear_socket(&api)?;
        let proc = process::spawn(
            &self.binary,
            &args,
            &spec.console_log,
            &spec.run_dir.join("firecracker.log"),
        )?;
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
        let r = http::request(&self.api, method, path, Some(&body))?;
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
            "kernel_image_path": spec.kernel,
            "boot_args": spec.cmdline.iter().chain(backend_args).cloned().collect::<Vec<_>>().join(" "),
        });
        if let Some(initrd) = &spec.initramfs {
            boot["initrd_path"] = json!(initrd);
        }
        self.call("PUT", "/boot-source", boot)?;
        // Drives attach in this order: vda, vdb, ...
        for (i, d) in spec.disks.iter().enumerate() {
            let id = format!("disk{i}");
            self.call(
                "PUT",
                &format!("/drives/{id}"),
                json!({"drive_id": id, "path_on_host": d.path, "is_root_device": false, "is_read_only": d.read_only}),
            )?;
        }
        if let Some(v) = spec.vsock {
            let uds = spec.run_dir.join("vsock.sock");
            process::clear_socket(&uds)?;
            self.call("PUT", "/vsock", json!({"guest_cid": v.guest_cid, "uds_path": uds}))?;
            self.vsock = Some(uds);
        }
        if let Some(n) = &spec.net {
            let mut iface = json!({"iface_id": "eth0", "host_dev_name": n.tap});
            if let Some(mac) = &n.guest_mac {
                iface["guest_mac"] = json!(mac);
            }
            self.call("PUT", "/network-interfaces/eth0", iface)?;
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

`src/lib.rs`:
```rust
//! VMM-neutral microVM lifecycle for Firecracker and Cloud Hypervisor (kiln spec §4.1).
#![forbid(unsafe_code)]

mod binary;
mod error;
mod firecracker;
mod http;
mod process;
mod spec;
mod vmm;

pub use error::{Error, Result};
pub use firecracker::Firecracker;
pub use spec::{
    Capabilities, Disk, EndReason, GuestExit, NetSpec, RestoreSpec, SnapshotBundle, VmEnd, VmSpec, VsockSpec,
};
pub use vmm::{Vm, Vmm};

/// The backends `vmkit` drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Firecracker,
}

impl Backend {
    pub const ALL: [Backend; 1] = [Backend::Firecracker];

    /// Finds and version-checks the backend's binary.
    pub fn discover(self) -> Result<Box<dyn Vmm>> {
        Ok(match self {
            Backend::Firecracker => Box::new(Firecracker::discover()?),
        })
    }
}
```

- [ ] **Step 6: Write the contract suite**

Kiln spec §11.3. Every test is written once and instantiated per backend by `contract!`. Without `VMKIT_TEST_KERNEL`/`VMKIT_TEST_INITRAMFS` each test returns early (CI sets `VMKIT_REQUIRE_KVM_TESTS=1`, which turns that into a failure). Pause and resume are a separate test binary that runs alone, with `--test-threads=1` (Task 4 explains why). `sandbox contents` and network tests belong to plan M2b, which adds the sandbox.

`tests/common/mod.rs`:
```rust
//! Shared by the KVM test binaries: the test kernel and guest, and a VM under test.
// Each test binary compiles this module and uses a different part of it.
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vmkit::{Backend, GuestExit, Vm, VmEnd, VmSpec, Vmm};

/// How long any guest may take to end by itself.
pub const END: Duration = Duration::from_secs(60);

pub struct Env {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

pub fn env() -> Option<Env> {
    let get = |k: &str| std::env::var_os(k).map(PathBuf::from);
    match (get("VMKIT_TEST_KERNEL"), get("VMKIT_TEST_INITRAMFS")) {
        (Some(kernel), Some(initramfs)) => Some(Env { kernel, initramfs }),
        _ => {
            assert!(
                std::env::var_os("VMKIT_REQUIRE_KVM_TESTS").is_none(),
                "VMKIT_REQUIRE_KVM_TESTS is set but VMKIT_TEST_KERNEL/VMKIT_TEST_INITRAMFS are not"
            );
            None
        }
    }
}

/// A VM under test with its private run directory.
pub struct Case {
    pub dir: tempfile::TempDir,
    pub vmm: Box<dyn Vmm>,
    pub env: Env,
}

impl Case {
    pub fn new(backend: Backend) -> Option<Self> {
        let env = env()?;
        let vmm = backend.discover().expect("VMM binary");
        Some(Self {
            dir: tempfile::tempdir().unwrap(),
            vmm,
            env,
        })
    }

    pub fn exit_arg(&self) -> &'static str {
        match self.vmm.capabilities().guest_exit {
            GuestExit::Reboot => "vmkit.exit=reboot",
            GuestExit::Poweroff => "vmkit.exit=poweroff",
        }
    }

    pub fn spec(&self, action: &str) -> VmSpec {
        VmSpec {
            kernel: self.env.kernel.clone(),
            initramfs: Some(self.env.initramfs.clone()),
            cmdline: [
                "panic=-1",
                "rdinit=/init",
                &format!("vmkit.test={action}"),
                self.exit_arg(),
            ]
            .map(String::from)
            .to_vec(),
            disks: Vec::new(),
            vcpus: 1,
            memory_mib: 256,
            vsock: None,
            net: None,
            console_log: self.dir.path().join("console.log"),
            run_dir: self.dir.path().to_path_buf(),
        }
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

    pub fn run(&self, spec: &VmSpec) -> (Box<dyn Vm>, VmEnd) {
        let mut vm = self.vmm.create(spec).expect("create");
        vm.start().expect("start");
        let end = vm
            .wait_timeout(END)
            .unwrap()
            .unwrap_or_else(|| panic!("VM did not end; console tail:\n{}", self.tail()));
        (vm, end)
    }

    /// Waits until the console has `n` occurrences of `needle`.
    pub fn await_console(&self, needle: &str, n: usize) {
        let deadline = Instant::now() + END;
        while self.console().matches(needle).count() < n {
            assert!(
                Instant::now() < deadline,
                "no {needle:?} x{n}; console tail:\n{}",
                self.tail()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
```

`tests/contract.rs`:
```rust
//! The vmkit contract suite (kiln spec §11.3): every test runs against both backends.
//!
//! Needs KVM, the VMM binaries, and a test kernel and guest:
//!   VMKIT_TEST_KERNEL=<vmlinux or Image>  VMKIT_TEST_INITRAMFS=<initramfs.cpio.gz>
//! Without them each test is skipped, unless VMKIT_REQUIRE_KVM_TESTS=1 makes that a failure.
//! Pause and resume live in `tests/pause.rs`, which runs alone.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::time::Duration;

use common::{Case, END};
use vmkit::{Backend, Disk, EndReason, Error, GuestExit, VsockSpec};

fn boots_and_ends_with_the_exit_method(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let (_vm, end) = c.run(&c.spec("up"));
    assert_eq!(end.reason, EndReason::Exited, "{end:?}");
    assert_eq!(c.console().matches("VMKIT-GUEST-UP").count(), 1);
}

fn a_guest_reset_ends_the_vm_once(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let (_vm, end) = c.run(&c.spec("reboot"));
    let expected = match c.vmm.capabilities().guest_exit {
        GuestExit::Reboot => EndReason::Exited,
        GuestExit::Poweroff => EndReason::ResetStopped,
    };
    assert_eq!(end.reason, expected, "{end:?}");
    // The workload must never run twice (kiln spec §4.1 backstop).
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(c.console().matches("VMKIT-GUEST-UP").count(), 1, "{}", c.console());
}

fn a_panic_ends_the_vm(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    for action in ["panic", "exit"] {
        let (_vm, end) = c.run(&c.spec(action));
        assert_ne!(end.reason, EndReason::Killed, "{action}: {end:?}");
    }
    assert_eq!(c.console().matches("VMKIT-GUEST-UP").count(), 2, "{}", c.console());
}

fn kill_ends_the_vmm_and_its_api(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let mut vm = c.vmm.create(&c.spec("idle")).unwrap();
    vm.start().unwrap();
    c.await_console("VMKIT-GUEST-TICK", 1);
    vm.kill().unwrap();
    let end = vm
        .wait_timeout(Duration::from_secs(10))
        .unwrap()
        .expect("killed VMM is reaped");
    assert_eq!((end.reason, end.signal), (EndReason::Killed, Some(9)));
    let api = std::fs::read_dir(c.dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "sock") && !p.ends_with("vsock.sock"))
        .expect("API socket path");
    assert!(
        std::os::unix::net::UnixStream::connect(api).is_err(),
        "nobody serves the API any more"
    );
    vm.kill().unwrap();
    drop(vm);
}

fn disks_attach_in_order(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let mut spec = c.spec("disks");
    for (i, sectors) in [8u64, 16, 24].into_iter().enumerate() {
        let path = c.dir.path().join(format!("disk{i}.img"));
        std::fs::File::create(&path).unwrap().set_len(sectors * 512).unwrap();
        spec.disks.push(Disk {
            path,
            read_only: i != 1,
        });
    }
    let (_vm, end) = c.run(&spec);
    assert_eq!(end.reason, EndReason::Exited);
    let disks: Vec<String> = c
        .console()
        .lines()
        .filter_map(|l| l.trim().strip_prefix("VMKIT-DISK ").map(String::from))
        .collect();
    assert_eq!(disks, ["vda 8", "vdb 16", "vdc 24"]);
}

fn device_budget_is_enforced_before_any_vmm_starts(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let available = c.vmm.capabilities().available_devices();
    let mut spec = c.spec("disks");
    spec.vsock = Some(VsockSpec { guest_cid: 3 });
    let disk = c.dir.path().join("disk.img");
    std::fs::File::create(&disk).unwrap().set_len(4096).unwrap();
    // vsock takes one device; fill the rest with disks, then one too many.
    spec.disks = (0..available)
        .map(|_| Disk {
            path: disk.clone(),
            read_only: true,
        })
        .collect();
    let err = c.vmm.create(&spec).err().expect("over budget");
    assert!(matches!(err, Error::TooManyDevices { .. }), "{err}");
    assert!(!c.dir.path().join("console.log").exists(), "no VMM was started");
    spec.disks.pop();
    let (_vm, end) = c.run(&spec);
    assert_eq!(end.reason, EndReason::Exited, "exactly the budget boots");
    assert_eq!(c.console().matches("VMKIT-DISK ").count(), available as usize - 1);
}

fn guest_vsock_connections_reach_the_host_socket(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let mut spec = c.spec("vsock");
    spec.vsock = Some(VsockSpec { guest_cid: 3 });
    let mut vm = c.vmm.create(&spec).unwrap();
    let host = vm.vsock_socket().expect("vsock socket").to_path_buf();
    // Guest-initiated connections to port P arrive on `<socket>_P` on both backends.
    let listener = UnixListener::bind(format!("{}_1234", host.display())).unwrap();
    vm.start().unwrap();
    let (stream, _) = listener.accept().unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    assert_eq!(line, "VMKIT-VSOCK-HELLO\n");
    (&stream).write_all(b"HOST-ACK\n").unwrap();
    let end = vm.wait_timeout(END).unwrap().expect("guest ends after the exchange");
    assert_eq!(end.reason, EndReason::Exited);
    assert!(c.console().contains("VMKIT-VSOCK-REPLY HOST-ACK"), "{}", c.console());
}

macro_rules! contract {
    ($($name:ident),* $(,)?) => {
        mod firecracker {
            $( #[test] fn $name() { super::$name(vmkit::Backend::Firecracker) } )*
        }
    };
}

contract!(
    boots_and_ends_with_the_exit_method,
    a_guest_reset_ends_the_vm_once,
    a_panic_ends_the_vm,
    kill_ends_the_vmm_and_its_api,
    disks_attach_in_order,
    device_budget_is_enforced_before_any_vmm_starts,
    guest_vsock_connections_reach_the_host_socket,
);
```

`tests/pause.rs`:
```rust
//! Pause and resume on both backends, run on an otherwise idle host.
//!
//! Cloud Hypervisor 53 on aarch64 can leave a guest stuck after a resume when other
//! VMs load the host (reproduced with plain `ch-remote` under nested virtualization;
//! Firecracker is unaffected), so this binary runs after the contract suite with
//! `--test-threads=1`. Same environment variables as `tests/contract.rs`.

mod common;

use std::time::Duration;

use common::Case;
use vmkit::Backend;

fn pause_stops_the_guest_and_resume_continues_it(backend: Backend) {
    let Some(c) = Case::new(backend) else { return };
    let mut vm = c.vmm.create(&c.spec("idle")).unwrap();
    vm.start().unwrap();
    c.await_console("VMKIT-GUEST-TICK", 3);
    vm.pause().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let paused = c.console().matches("VMKIT-GUEST-TICK").count();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        c.console().matches("VMKIT-GUEST-TICK").count(),
        paused,
        "ticks while paused"
    );
    vm.resume().unwrap();
    c.await_console("VMKIT-GUEST-TICK", paused + 3);
    vm.kill().unwrap();
}

#[test]
fn firecracker() {
    pause_stops_the_guest_and_resume_continues_it(Backend::Firecracker)
}
```

- [ ] **Step 7: Run the unit tests**

Run: `cargo test -q --lib`
Expected: all pass (10 tests).

- [ ] **Step 8: Format, lint and test**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 9: Run the contract suite on KVM**

In the Lima VM, with Task 2's kernel and guest in `out/`. Four test threads: each test boots its own VM (see Task 4 for why more parallelism is a problem on Cloud Hypervisor).

Run (Linux with KVM: the Lima VM): `export VMKIT_TEST_KERNEL=out/vmlinux-6.18.54-aarch64 VMKIT_TEST_INITRAMFS=out/initramfs-aarch64.cpio.gz VMKIT_REQUIRE_KVM_TESTS=1 && cargo test --test contract -- --test-threads=4 && cargo test --test pause -- --test-threads=1`
Expected: 7 contract tests pass (`firecracker::*`), then 1 pause test.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock src tests
git commit -m 'feat: Vmm trait, Firecracker driver and contract suite'
```


### Task 4: The Cloud Hypervisor driver and its reset backstop

**Files:**
- Create: `src/events.rs`, `src/cloud_hypervisor.rs`
- Modify: `src/process.rs`, `src/lib.rs`, `tests/contract.rs`, `tests/pause.rs`

**Interfaces:**
- Consumes: Task 3's `Vmm`/`Vm` traits, `http`, `binary`, `process` (`Proc` is `Clone`).
- Produces:
  - `CloudHypervisor::discover()` (`$VMKIT_CLOUD_HYPERVISOR`, else `PATH`; at least `cloud_hypervisor::MIN_VERSION` = 53.0); `Backend::CloudHypervisor`, and `Backend::ALL` lists both.
  - `Proc::stop_on_reset()`; `EndReason::ResetStopped` when the backstop stopped the VMM.
  - The run directory holds `cloud-hypervisor.sock`, `cloud-hypervisor.log`, `events.json` and `vsock.sock`.
  - The contract suite runs every test on both backends.

- [ ] **Step 1: Add the reset backstop to the process**

In `src/process.rs`, replace:
```rust
    fn signal_kill(&self) -> Result<()> {
```
with:
```rust
    /// Kills the VMM because the guest reset (Cloud Hypervisor backstop).
    pub(crate) fn stop_on_reset(&self) -> Result<()> {
        self.reset_stopped.store(true, Ordering::SeqCst);
        self.signal_kill()
    }

    fn signal_kill(&self) -> Result<()> {
```

In `src/process.rs`, replace:
```rust
        p.kill().unwrap();
    }
```
with:
```rust
        p.kill().unwrap();

        let p = sh("sleep 30", dir.path());
        p.stop_on_reset().unwrap();
        assert_eq!(p.wait(None).unwrap().unwrap().reason, EndReason::ResetStopped);
    }
```

- [ ] **Step 2: Write the event-stream watcher, with its unit test**

Cloud Hypervisor reboots the VM when the guest resets; kiln must never run a workload twice (spec §4.1), so vmkit kills the VMM on the `vm`/`rebooting` event. The event monitor writes pretty-printed JSON objects one after another (the test uses a stream captured from v53); `serde_json`'s stream deserializer reads them. The monitor writes to a regular file in the run directory that `Tail` follows until the VMM exits: a FIFO would need `mknod`, which is not portable to macOS, where the crate must still build.

`src/events.rs`:
```rust
//! Cloud Hypervisor's `--event-monitor` stream: a sequence of JSON objects.

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::process::Proc;

/// Reads a file the VMM is still writing, like `tail -f`, until the VMM exits.
pub(crate) struct Tail {
    path: PathBuf,
    file: Option<File>,
    proc: Proc,
}

impl Tail {
    pub(crate) fn new(path: PathBuf, proc: Proc) -> Self {
        Self { path, file: None, proc }
    }
}

impl Read for Tail {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.file.is_none() {
                self.file = File::open(&self.path).ok();
            }
            if let Some(f) = &mut self.file {
                let n = f.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
            }
            // Nothing new: stop once the VMM has exited and everything was read.
            if self.proc.try_end().map_err(io::Error::other)?.is_some() {
                return match &mut self.file {
                    Some(f) => f.read(buf),
                    None => Ok(0),
                };
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The event a guest reset produces; the VMM would otherwise reboot the VM.
pub(crate) fn is_reset(event: &Value) -> bool {
    event["source"] == "vm" && event["event"] == "rebooting"
}

/// Reads events until the stream ends, calling `on_reset` on the first reset.
pub(crate) fn watch(stream: impl Read, mut on_reset: impl FnMut()) {
    for event in serde_json::Deserializer::from_reader(stream).into_iter::<Value>() {
        match event {
            Ok(e) if is_reset(&e) => return on_reset(),
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from Cloud Hypervisor v53 for a guest `reboot -f`.
    const STREAM: &str = r#"{
  "timestamp": {"secs": 0, "nanos": 150720},
  "source": "vmm",
  "event": "starting",
  "properties": null
}

{
  "timestamp": {"secs": 2, "nanos": 676003268},
  "source": "virtio-device",
  "event": "reset",
  "properties": {"id": "__rng"}
}

{
  "timestamp": {"secs": 2, "nanos": 682140329},
  "source": "vm",
  "event": "rebooting",
  "properties": null
}
"#;

    #[test]
    fn stops_at_the_vm_reboot_event_not_a_device_reset() {
        let mut resets = 0;
        watch(STREAM.as_bytes(), || resets += 1);
        assert_eq!(resets, 1);
        let mut resets = 0;
        watch(
            &STREAM.as_bytes()[..STREAM
                .find("\n\n{\n  \"timestamp\": {\"secs\": 2, \"nanos\": 682")
                .unwrap()],
            || resets += 1,
        );
        assert_eq!(resets, 0, "a virtio device reset alone is not a VM reset");
    }
}
```

- [ ] **Step 3: Write the Cloud Hypervisor driver**

Decisions this file encodes, each checked against Cloud Hypervisor 53 during planning:
- **Configure through `vm.create`, boot with `vm.boot`.** Landlock rules are VM configuration: given on the command line (`--landlock-rules`), Cloud Hypervisor also demands `--kernel` there and boots at once. In the `vm.create` body (`landlock_enable`, `landlock_rules`) they limit it to the kernel, initramfs, disks and the run directory.
- **Serial in `Tty` mode**, i.e. on the VMM's stdout, which vmkit appends to the console log. A `file=` serial is truncated when the guest resets, which would lose the console tail kiln reports on failure.
- **A stale `events.json` is removed before spawning.** Otherwise a previous VM's `rebooting` event, read by the new backstop, kills the new VM at once (this happened in validation).
- **`guest_exit` is `Poweroff`:** power-off ends the VMM; reset, panic and PID 1 exiting reboot it, which the backstop turns into `ResetStopped`.
- **`max_virtio_devices` 31 with 1 implicit** (the RNG). Console `ttyAMA0` on aarch64, `ttyS0` on x86_64.

`src/cloud_hypervisor.rs`:
```rust
//! The Cloud Hypervisor driver.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::binary::{self, Version};
use crate::error::{Error, Result};
use crate::events;
use crate::http;
use crate::process::{self, Proc};
use crate::spec::{Capabilities, GuestExit, RestoreSpec, SnapshotBundle, VmEnd, VmSpec};
use crate::vmm::{Vm, Vmm};

pub const MIN_VERSION: Version = (53, 0, 0);
const NAME: &str = "cloud-hypervisor";

pub struct CloudHypervisor {
    binary: PathBuf,
    arch: &'static str,
}

impl CloudHypervisor {
    /// Finds the binary (`$VMKIT_CLOUD_HYPERVISOR`, else `PATH`) and checks its version.
    pub fn discover() -> Result<Self> {
        let binary = binary::find("cloud-hypervisor", "VMKIT_CLOUD_HYPERVISOR")?;
        binary::check_version(&binary, NAME, MIN_VERSION)?;
        Ok(Self {
            binary,
            arch: std::env::consts::ARCH,
        })
    }
}

/// Landlock rules: Cloud Hypervisor may touch only the VM's own files (kiln spec §9.2).
fn landlock_rules(spec: &VmSpec) -> Vec<Value> {
    let rule = |path: &Path, access: &str| json!({"path": path, "access": access});
    let mut rules = vec![rule(&spec.kernel, "r")];
    if let Some(i) = &spec.initramfs {
        rules.push(rule(i, "r"));
    }
    for d in &spec.disks {
        rules.push(rule(&d.path, if d.read_only { "r" } else { "rw" }));
    }
    rules.push(rule(&spec.run_dir, "rw"));
    rules
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
        let api = spec.run_dir.join("cloud-hypervisor.sock");
        let events = spec.run_dir.join("events.json");
        // A previous VM's events (say, its reset) must not reach this VM's backstop.
        match std::fs::remove_file(&events) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let args = vec![
            "--api-socket".into(),
            format!("path={}", api.display()),
            "--event-monitor".into(),
            format!("path={}", events.display()),
            "--seccomp".into(),
            "true".into(),
        ];
        crate::process::clear_socket(&api)?;
        let proc = process::spawn(
            &self.binary,
            &args,
            &spec.console_log,
            &spec.run_dir.join("cloud-hypervisor.log"),
        )?;
        // The backstop: a guest reset must end the VM, never reboot it (kiln spec §4.1).
        let watcher = proc.clone();
        std::thread::spawn(move || {
            events::watch(events::Tail::new(events, watcher.clone()), || {
                let _ = watcher.stop_on_reset();
            });
        });
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
        let r = http::request(&self.api, "PUT", &path, body.as_ref())?;
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
        let mut payload = json!({"kernel": spec.kernel, "cmdline": cmdline.join(" ")});
        if let Some(i) = &spec.initramfs {
            payload["initramfs"] = json!(i);
        }
        let mut config = json!({
            "payload": payload,
            "cpus": {"boot_vcpus": spec.vcpus, "max_vcpus": spec.vcpus},
            "memory": {"size": u64::from(spec.memory_mib) << 20},
            "disks": spec.disks.iter().map(|d| json!({"path": d.path, "readonly": d.read_only})).collect::<Vec<_>>(),
            // Guest serial on the VMM's stdout, which vmkit appends to the console log;
            // a `file=` serial would be truncated when the guest resets.
            "serial": {"mode": "Tty"},
            "console": {"mode": "Off"},
            "landlock_enable": true,
            "landlock_rules": landlock_rules(spec),
        });
        if let Some(v) = spec.vsock {
            let socket = spec.run_dir.join("vsock.sock");
            crate::process::clear_socket(&socket)?;
            config["vsock"] = json!({"cid": v.guest_cid, "socket": socket});
            self.vsock = Some(socket);
        }
        if let Some(n) = &spec.net {
            let mut net = json!({"tap": n.tap});
            if let Some(mac) = &n.guest_mac {
                net["mac"] = json!(mac);
            }
            config["net"] = json!([net]);
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
```

In `src/lib.rs`, replace:
```rust
mod binary;
mod error;
mod firecracker;
```
with:
```rust
mod binary;
mod cloud_hypervisor;
mod error;
mod events;
mod firecracker;
```

In `src/lib.rs`, replace:
```rust
pub use error::{Error, Result};
```
with:
```rust
pub use cloud_hypervisor::CloudHypervisor;
pub use error::{Error, Result};
```

In `src/lib.rs`, replace:
```rust
    Firecracker,
}
```
with:
```rust
    Firecracker,
    CloudHypervisor,
}
```

In `src/lib.rs`, replace:
```rust
impl Backend {
    pub const ALL: [Backend; 1] = [Backend::Firecracker];
```
with:
```rust
impl Backend {
    pub const ALL: [Backend; 2] = [Backend::Firecracker, Backend::CloudHypervisor];
```

In `src/lib.rs`, replace:
```rust
            Backend::Firecracker => Box::new(Firecracker::discover()?),
        })
```
with:
```rust
            Backend::Firecracker => Box::new(Firecracker::discover()?),
            Backend::CloudHypervisor => Box::new(CloudHypervisor::discover()?),
        })
```

- [ ] **Step 4: Run every contract test on both backends**

In `tests/contract.rs`, replace:
```rust
        }
    };
```
with:
```rust
        }
        mod cloud_hypervisor {
            $( #[test] fn $name() { super::$name(vmkit::Backend::CloudHypervisor) } )*
        }
    };
```

In `tests/pause.rs`, replace:
```rust
    pause_stops_the_guest_and_resume_continues_it(Backend::Firecracker)
}
```
with:
```rust
    pause_stops_the_guest_and_resume_continues_it(Backend::Firecracker)
}

#[test]
fn cloud_hypervisor() {
    pause_stops_the_guest_and_resume_continues_it(Backend::CloudHypervisor)
}
```

- [ ] **Step 5: Run the unit tests**

Run: `cargo test -q --lib`
Expected: all pass (11 tests).

- [ ] **Step 6: Format, lint and test**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 7: Run the contract suite on KVM**

Cloud Hypervisor 53 on aarch64 can leave a guest stuck after `resume` while other VMs load the host. That was reproduced during planning with plain `ch-remote` and 12 busy VMs. The stuck VM reports `Running`, but no timer or serial interrupt arrives again, and a second pause/resume does not revive it. So it is not vmkit's doing, and Firecracker passed 25 of 25 rounds under the same load. Hence `tests/pause.rs` runs alone, after the contract suite. Over 24 planning runs of the contract suite at four threads, one failed with a stalled Cloud Hypervisor guest; a failing test prints the end of the console to tell these cases apart. `kiln run` never pauses a VM; the README records the issue.

Run (Linux with KVM: the Lima VM): `export VMKIT_TEST_KERNEL=out/vmlinux-6.18.54-aarch64 VMKIT_TEST_INITRAMFS=out/initramfs-aarch64.cpio.gz VMKIT_REQUIRE_KVM_TESTS=1 && cargo test --test contract -- --test-threads=4 && cargo test --test pause -- --test-threads=1`
Expected: 14 contract tests pass (`firecracker::*` and `cloud_hypervisor::*`), then 2 pause tests.

- [ ] **Step 8: Commit**

```bash
git add src tests
git commit -m 'feat: Cloud Hypervisor driver with the reset backstop'
```


### Task 5: CI on KVM, the kernel release pipeline, the Lima template and the README

**Files:**
- Create: `.github/workflows/kernels.yml`, `lima/vmkit.yaml`, `README.md`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: everything above.
- Produces: the `contract-x86_64` CI job (kernel cached by `hashFiles('kernels/**')`), `.github/workflows/kernels.yml` (on a `kernels-<version>` tag: build both arches, boot-test x86_64 on both VMMs, publish release assets with `SHA256SUMS` and `ghcr.io/<owner>/vmkit-kernels/base:<version>-<arch>` OCI artifacts), `lima/vmkit.yaml`, `README.md`.

- [ ] **Step 1: Run the contract suite on x86_64 KVM in CI**

Hosted x86_64 runners have `/dev/kvm` once a udev rule opens it; hosted arm64 runners have none, so aarch64 runs on the Lima template (kiln spec §11.7). This job is also the first boot of the x86_64 kernel: it can only be checked in CI.

In `.github/workflows/ci.yml`, replace:
```yaml
      - run: cargo test --all
```
with:
```yaml
      - run: cargo test --all
  # The contract suite on x86_64 KVM (hosted arm64 runners have no /dev/kvm; aarch64
  # runs on the Lima template, kiln spec §11.7).
  contract-x86_64:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Allow KVM for the runner user
        run: |
          echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' | sudo tee /etc/udev/rules.d/99-kvm4all.rules
          sudo udevadm control --reload-rules
          sudo udevadm trigger --name-match=kvm
      - run: sudo apt-get update && sudo apt-get install -y busybox-static flex bison bc libelf-dev libssl-dev
      - run: scripts/install-vmms.sh "$HOME/.local/bin"
      - uses: actions/cache@v4
        with:
          path: out/vmlinux-*-x86_64
          key: kernel-x86_64-${{ hashFiles('kernels/**') }}
      - name: Build the kernel (cached by kernels/)
        run: ls out/vmlinux-*-x86_64 2>/dev/null || kernels/build.sh x86_64 out
      - run: testguest/build-initramfs.sh out/initramfs-x86_64.cpio.gz
      - name: Contract suite
        # Four threads: the suite boots one VM per test, and hosted runners have four cores.
        # Pause/resume runs alone afterwards (see tests/pause.rs).
        run: |
          export PATH="$HOME/.local/bin:$PATH"
          export VMKIT_TEST_KERNEL=$(ls out/vmlinux-*-x86_64) VMKIT_TEST_INITRAMFS=out/initramfs-x86_64.cpio.gz VMKIT_REQUIRE_KVM_TESTS=1
          cargo test --test contract -- --test-threads=4
          cargo test --test pause -- --test-threads=1
```

- [ ] **Step 2: Write the kernel release workflow**

Publishing is outward-facing and happens only when someone pushes a `kernels-*` tag; it is not run as part of this plan. `<project-org>` in kiln spec §8.2 is the repository owner. aarch64 is boot-tested on the Lima template before tagging.

`.github/workflows/kernels.yml`:
```yaml
# Builds, boot-tests and publishes the `base` kernel profile (kiln spec §8.2).
# Runs on a `kernels-<version>` tag (e.g. kernels-6.18.54) or by hand; publishing
# needs the tag. aarch64 has no hosted KVM, so its boot test runs on the Lima template
# before tagging.
name: kernels
on:
  push:
    tags: ["kernels-*"]
  workflow_dispatch:
permissions:
  contents: write
  packages: write
jobs:
  build:
    strategy:
      matrix:
        include:
          - arch: x86_64
            runner: ubuntu-24.04
          - arch: aarch64
            runner: ubuntu-24.04-arm
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4
      - run: sudo apt-get update && sudo apt-get install -y busybox-static flex bison bc libelf-dev libssl-dev
      - run: kernels/build.sh ${{ matrix.arch }} out
      - name: Boot test on both VMMs (x86_64)
        if: matrix.arch == 'x86_64'
        run: |
          echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' | sudo tee /etc/udev/rules.d/99-kvm4all.rules
          sudo udevadm control --reload-rules && sudo udevadm trigger --name-match=kvm
          scripts/install-vmms.sh "$HOME/.local/bin"
          testguest/build-initramfs.sh out/initramfs-x86_64.cpio.gz
          export PATH="$HOME/.local/bin:$PATH"
          export VMKIT_TEST_KERNEL=$(ls out/vmlinux-*-x86_64) VMKIT_TEST_INITRAMFS=out/initramfs-x86_64.cpio.gz VMKIT_REQUIRE_KVM_TESTS=1
          cargo test --test contract -- --test-threads=4
          cargo test --test pause -- --test-threads=1
      - uses: actions/upload-artifact@v4
        with:
          name: kernel-${{ matrix.arch }}
          path: out/*-${{ matrix.arch }}
  publish:
    if: startsWith(github.ref, 'refs/tags/kernels-')
    needs: build
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: actions/download-artifact@v4
        with:
          path: out
          merge-multiple: true
      - name: Check the tag matches kernels/VERSION
        run: test "${GITHUB_REF_NAME#kernels-}" = "$(cat kernels/VERSION)"
      - run: cd out && sha256sum vmlinux-* config-* > SHA256SUMS && cat SHA256SUMS
      - name: GitHub release
        env:
          GH_TOKEN: ${{ github.token }}
        run: gh release create "$GITHUB_REF_NAME" out/vmlinux-* out/config-* out/SHA256SUMS --title "base kernel $(cat kernels/VERSION)" --notes "vmkit base kernel profile, Linux $(cat kernels/VERSION)."
      - uses: oras-project/setup-oras@v1
      - name: OCI artifacts on ghcr.io
        run: |
          echo "${{ github.token }}" | oras login ghcr.io -u "${{ github.actor }}" --password-stdin
          version=$(cat kernels/VERSION)
          for arch in x86_64 aarch64; do
            (cd out && oras push "ghcr.io/${{ github.repository_owner }}/vmkit-kernels/base:$version-$arch" \
              --artifact-type application/vnd.vmkit.kernel.v1 \
              "vmlinux-$version-$arch:application/vnd.vmkit.kernel.v1.binary" \
              "config-$version-$arch:application/vnd.vmkit.kernel.v1.config")
          done
```

- [ ] **Step 3: Write the Lima template**

Kiln spec §10. The macOS home is mounted read-only; build output goes to guest paths. The VMMs come from `scripts/install-vmms.sh`, so their pins live in one place.

`lima/vmkit.yaml`:
```yaml
# vmkit development VM: Firecracker and Cloud Hypervisor on nested KVM (kiln spec §10).
# Needs an Apple M3 or later and macOS 15 or later.
#   limactl start --name vmkit lima/vmkit.yaml
#   limactl shell vmkit -- <repo>/scripts/install-vmms.sh
# The macOS home is mounted read-only: build into guest paths
# (CARGO_TARGET_DIR=~/target, kernels/build.sh aarch64 ~/out).
base: template:ubuntu-26.04
vmType: vz
nestedVirtualization: true
cpus: 8
memory: 16GiB
disk: 80GiB
mounts:
  - location: "~"
    writable: false
provision:
  - mode: system
    script: |
      #!/bin/bash
      set -eux
      export DEBIAN_FRONTEND=noninteractive
      apt-get update
      apt-get install -y build-essential flex bison bc libelf-dev libssl-dev busybox-static cpio file curl \
        gcc-x86-64-linux-gnu passt nftables iproute2 uidmap socat
      usermod -aG kvm "{{.User}}"
  - mode: user
    script: |
      #!/bin/bash
      set -eux
      command -v rustup || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c clippy,rustfmt
```

Run: `limactl validate lima/vmkit.yaml`
Expected: `... lima/vmkit.yaml: OK` (run on the Mac).

- [ ] **Step 4: Write the README**

`README.md`:
````markdown
# vmkit

VMM-neutral microVM lifecycle for [Firecracker](https://github.com/firecracker-microvm/firecracker) and [Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor), and the kernel they boot. It is the VM layer of [kiln](https://github.com/AlfonsoCampodonico/kiln) (design: kiln's `docs/superpowers/specs/2026-09-30-kiln-design.md`, §4.1, §8.2, §9 and §11.3).

```rust
use vmkit::{Backend, Disk, VmSpec, VsockSpec};

let vmm = Backend::Firecracker.discover()?;          // $VMKIT_FIRECRACKER, else PATH
let mut vm = vmm.create(&VmSpec {
    kernel: "vmlinux".into(),
    initramfs: None,
    cmdline: vec!["root=/dev/vda".into(), "ro".into(), "panic=-1".into()],
    disks: vec![Disk { path: "rootfs.erofs".into(), read_only: true }],
    vcpus: 2,
    memory_mib: 512,
    vsock: Some(VsockSpec { guest_cid: 3 }),
    net: None,
    console_log: "run/console.log".into(),
    run_dir: "run".into(),
})?;
vm.start()?;
let end = vm.wait()?;
```

- **Backends:** `Backend::Firecracker` and `Backend::CloudHypervisor`. Decide from `Vmm::capabilities()` (device budget, how the guest must exit, console device), never from the backend's name.
- **Guest exit:** the guest ends the VM with `capabilities().guest_exit`: `reboot` on Firecracker, `poweroff` on Cloud Hypervisor. A Cloud Hypervisor guest reset is stopped, not rebooted (`EndReason::ResetStopped`), so a workload never runs twice.
- **Kernel arguments:** the caller's `cmdline` plus the console and backend parameters vmkit appends. Nothing else contributes.
- **vsock:** guest-initiated connections to host port `P` arrive on the Unix socket `<vm.vsock_socket()>_P` on both backends.
- **Snapshots:** `Vm::snapshot` and `Vmm::restore` have their final shape but return `Error::Unsupported` until the snapshot work lands.

## Requirements

Linux with KVM (`/dev/kvm`, user in the `kvm` group) and the pinned VMMs:

```bash
scripts/install-vmms.sh ~/.local/bin    # Firecracker 1.17.0 and Cloud Hypervisor 53.0, SHA-256 checked
```

The library also builds on macOS (for kiln's non-run commands); creating VMs needs Linux. On a Mac, use the Lima template (Apple M3 or later, macOS 15 or later):

```bash
limactl start --name vmkit lima/vmkit.yaml
limactl shell vmkit -- "$PWD/scripts/install-vmms.sh"
```

## Kernel

`kernels/` holds the `base` profile: Firecracker's CI guest config for the pinned LTS (`kernels/VERSION`) plus vmkit's fragments (erofs, overlayfs, vsock, PL011 console, PVH, no netfilter). One binary per arch boots on both VMMs.

```bash
kernels/build.sh aarch64 out     # or x86_64 (cross-compiles when needed)
```

Tagging `kernels-<version>` builds both arches, boot-tests x86_64 on both VMMs, and publishes release assets and `ghcr.io/<owner>/vmkit-kernels/base:<version>-<arch>` (`.github/workflows/kernels.yml`). Boot-test aarch64 on the Lima template before tagging: hosted arm64 runners have no KVM.

## Tests

```bash
cargo test                                   # unit tests, any platform
testguest/build-initramfs.sh out/initramfs.cpio.gz
export VMKIT_TEST_KERNEL=out/vmlinux-6.18.54-aarch64 VMKIT_TEST_INITRAMFS=out/initramfs.cpio.gz VMKIT_REQUIRE_KVM_TESTS=1
cargo test --test contract -- --test-threads=4
cargo test --test pause -- --test-threads=1
```

The contract suite (`tests/contract.rs`) runs every test against both backends with a busybox guest: boot and exit method, reset backstop, panic, kill, disk order, device budget and guest-initiated vsock. Pause and resume (`tests/pause.rs`) run alone: Cloud Hypervisor 53 on aarch64 can leave a guest stuck after a resume while other VMs load the host. That was reproduced with plain `ch-remote` under nested virtualization; Firecracker is unaffected. Keep the contract suite's `--test-threads` at about half the CPUs; under heavier load, Cloud Hypervisor guests also stalled occasionally on the same nested setup. A failing test prints the end of the guest console.

License: Apache-2.0.
````

- [ ] **Step 5: Format, lint and test**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test -q`
Expected: no clippy warnings; every test passes.

- [ ] **Step 6: Commit**

```bash
git add .github lima README.md
git commit -m 'ci: contract suite on x86_64 KVM, kernel release pipeline, Lima template and README'
```


## Spec coverage (M2a)

| Spec item | Task |
|---|---|
| §4.1 `Vmm` trait: `create`, `restore` (shape), `start`, `pause`, `resume`, `kill`, `wait`, `snapshot` (shape), `capabilities` | 3 |
| §4.1 `VmSpec`: kernel and cmdline, ordered block devices, vCPUs, memory, network, vsock, console log | 1, 3 |
| §4.1 Firecracker driver: console, `reboot` exit, re-added `boot_args` defaults | 3 |
| §4.1 Cloud Hypervisor driver: console, `poweroff` exit, `--event-monitor` reset backstop | 4 |
| §4.1 `Capabilities`: `max_virtio_devices` (measured), `supports_diff_snapshot`, `supports_balloon`, `supports_drive_remap` | 1, 3, 4 |
| §4.1 Binary discovery and pinned minimum versions | 3, 4 |
| §4.1 `kernels/`: config fragments, pinned LTS, build, boot-test and publish pipeline | 2, 5 |
| §8.2 Kernel profile `base`: one binary per arch, PVH on x86_64, the listed options, no netfilter | 2 |
| §9.2 (part): Cloud Hypervisor `--seccomp true` and Landlock limited to the VM's files | 4 |
| §10 Lima template | 5 |
| §11.3 Contract suite: boot, exit method, reset, panic, pause/resume, kill, device budget (plus disk order and vsock) | 3, 4 |
| §11.7 CI hosts: x86_64 KVM per commit, aarch64 on Lima | 5 |
| §14 items 3, 4, 5; item 1 without the sandbox code | 3, 4 (and the results above) |

**Left to plan M2b:**
- `vmkit::sandbox` (§9.2): user, mount, PID and net namespaces through a helper binary with an AppArmor profile; a tmpfs root with only `/dev/kvm`, `/dev/null`, `/dev/urandom`, `/dev/net/tun`, the VMM binary and `/vm/...`; `O_NOFOLLOW` file attachment; `no_new_privs`; fd and process rlimits; cgroup v2 limits.
- `vmkit::net` (§9.3): the per-VM namespace, `tap0`, the nftables policy and the egress modes, `pasta` with DNS forwarding, and port forwarding.
- The sandbox-contents contract test and the hostile-guest network tests (§11.5).
- The launcher seam: drivers keep spawning through `process::spawn`, and M2b routes that through the sandbox and maps resources to the fixed `/vm/...` paths.
