# kiln-shim v1 — Implementation Design

**Date:** 2026-10-21
**Status:** Draft rev 1.
**Implements:** a first slice of milestone S3 of the [kiln-shim spec](2026-10-05-kiln-shim-design.md) (rev 1, D1–D4 confirmed 2026-10-05). Where this document narrows or changes that spec, §13 says so.
**Related:** the [kiln spec](2026-09-30-kiln-design.md) (§3 threat model, §9 boot and runtime), and vmkit's `TapAttach` network mode (vmkit PR, §6.3).

---

## 1. What this milestone delivers

`containerd-shim-kiln-v1`, a containerd shim v2, behind runtime type `io.containerd.kiln.v1` and a Kubernetes RuntimeClass `kiln`. A pod with `runtimeClassName: kiln` runs its workload container as the main process of a kiln microVM. Around it, kubelet, the CRI log files, Services, NetworkPolicy and HTTP/TCP probes keep working.

### Success criteria

1. `crictl runp` / `create` / `start` of an `alpine` container running `echo` prints its output in `crictl logs`, and `crictl inspect` reports exit code 0. A command that exits 3 reports 3.
2. A container that serves HTTP on its pod IP answers `curl http://<pod IP>:<port>/` from the node. The same check covers a kubelet HTTP probe.
3. From inside the guest, `169.254.169.254` and the node's own addresses can't be connected to.
4. `crictl stop` delivers SIGTERM to the guest's main process; a process that ignores it is killed after the timeout (exit 137).
5. Init containers run one after another, each in its own VM, before the app container.
6. `cargo test` and `cargo clippy` pass for `kiln-shim` on macOS and Linux. The end-to-end script (§11.3) passes on a Linux KVM host with containerd 2.2 or later.

### Non-goals for v1

They are listed with their reasons in §12. The main ones: exec, volumes, stats, OOM reporting, the containerd sandbox API, IPv6 inside the guest, and more than one workload container running at once.

---

## 2. Background facts this design rests on

Each was checked against upstream source on 2026-10-21.

- **The Rust shim crates.** `containerd-shim` 0.11 (sync API, no `async` feature) provides:
  - the `Shim` trait and `run()`;
  - `spawn()` with socket grouping;
  - `RemotePublisher` for events;
  - the `Task` service trait, over ttrpc 0.9 (re-exported; kiln adds no ttrpc dependency of its own).

  It builds on macOS. It does not answer `-info` and has no stdin bootstrap (containerd 2.3+), so the shim handles `-info` itself, and containerd keeps using the legacy flag protocol, which it still supports.
- **How CRI models a pod (default `podsandbox` sandboxer).**
  - The pause container is a task with annotation `io.kubernetes.cri.container-type=sandbox`.
  - Each other container has `container-type=container`.
  - All tasks of a pod share `io.kubernetes.cri.sandbox-id`.
  - `start_shim` groups by that annotation, so one shim process serves the whole pod, as runc's shim does.
  - Containers' specs name the pod netns as `/proc/<sandbox task pid>/ns/net`.
- **The erofs snapshotter (containerd ≥ 2.1, mount manager ≥ 2.2).** Its differ converts each OCI layer with `mkfs.erofs --tar=f --aufs`, which turns OCI whiteouts into overlayfs ones. A container's rootfs arrives as these mounts, in order:
  - an optional `mkfs/ext4` mount (block mode);
  - one `erofs` mount per layer, **top first** (`source` = `<snapshot>/layer.erofs`, options `ro`, `loop`, optionally `X-containerd.dmverity=…`);
  - a final `format/mkdir/overlay`.

  containerd mounts these on the host itself unless the runtime says it handles them. On 2.2–2.4 the runtime says so with the RuntimeInfo annotation `containerd.io/runtime-allow-mounts`, returned from `-info` (Kata runtime-rs does the same).
- **No CRI annotation carries the image digest**, so converting from containerd's content store would need containerd's gRPC API. The erofs snapshotter makes that unnecessary.
- **CNI networking.** CNI creates the pod netns with a veth (`eth0`). Kata's `tcfilter` model and firecracker-containerd's `tc-redirect-tap` connect the VM to it the same way:
  - a tap in the pod netns;
  - tc `mirred` redirects between the veth and the tap, in both directions;
  - the guest takes the veth's MAC and IP.

---

## 3. Components

```
kubelet ─CRI─► containerd ─ttrpc─► containerd-shim-kiln-v1 (root, one per pod)
                                     │ fork+exec                      │ fork+exec
                                     ▼                                ▼
                          `pause` (in the pod netns,       `vm` worker (per container task)
                           the sandbox task's process)       root: setns pod netns, cgroup,
                                                              bind-mount layers, then drops
                                                              to the kiln user
                                                                      │ kiln-run building blocks
                                                                      ▼
                                                          vmkit (TapAttach) ─► Firecracker /
                                                          Cloud Hypervisor in the pod netns,
                                                          unprivileged sandbox
                                                                      │ kiln-proto over vsock
                                                                      ▼
                                                                kiln-init (guest PID 1)
```

| Component | Where | Responsibility |
|---|---|---|
| `kiln-shim` crate, binary `containerd-shim-kiln-v1` | kiln, new crate | The shim v2 Task service, the pod's network attachment, spec translation, task state, and the `pause` and `vm` modes |
| vmkit `TapAttach` | vmkit PR | A VMM that stays in its caller's network namespace and uses a tap that already exists there (§6.3) |
| kiln-proto `Network.search`, `Network.options` | kiln, additive | DNS search domains and resolver options for the guest's `resolv.conf` (§6.5) |
| kiln-init on-link gateway | kiln, additive | A default route via a gateway outside the address's prefix (`/32` pod IPs) |
| `kiln-run` | unchanged | Its public building blocks: `prepare` pieces, `rundir`, `vm_spec`, `ConsoleRelay`, `Session` |

The shim composes kiln-run's public functions rather than calling `Run::start`. `Run::start` resolves an image from the kiln store and builds the guest's process from it. Under the shim, the layers come from the snapshotter and the process from the OCI spec. Extracting a shared `Run::boot(plan)` from `Run::start` is a worthwhile follow-up. It is not done here, because the IPv6 and snapshot work are changing `run.rs` at the same time.

---

## 4. Sandbox model (D2 as implemented)

- **One shim per pod**, grouped by `io.kubernetes.cri.sandbox-id`. Without CRI annotations (`ctr run`), each container is its own group.
- **The sandbox (pause) task never boots a VM.**
  - Its process is the shim binary in `pause` mode. That process joins the pod netns and waits for a signal.
  - So the sandbox task has a real PID whose `/proc/<pid>/ns/net` *is* the pod netns, which is what CRI writes into the containers' specs.
  - Killing the sandbox task signals that process. Its exit is the sandbox task's exit.
- **Each container task boots its own VM**, from that container's own image (its snapshot's erofs layers).
- **At most one container VM per pod runs at a time.**
  - `Create` of a container task while another container task of the same pod is created or running is refused (`FailedPrecondition`, "kiln runs one container per pod at a time"). An init container that has exited (stopped, or deleted) no longer counts.
  - So init containers run in sequence, each in its own VM: the "VM rebooted between images" of D2.
  - The app container follows them.
  - Sidecars, native sidecars and multi-container pods are refused at the second task `Create`, which CRI issues when that container *starts* (`StartContainer`). That fails the pod visibly, rather than silently running containers that can't share `localhost`.
- **The containerd sandbox API** (`sandboxer = "shim"`) is not used in v1. It would let the pod be the VM. It needs the multi-rootfs guest work deferred in D2, and the Rust crate does not serve that service.

---

## 5. Rootfs: containerd's erofs snapshotter

The confirmed spec (§3.1) chose the erofs snapshotter. v1 implements the attachment.

1. **Advertising raw erofs mounts.**
   - `containerd-shim-kiln-v1 -info` prints a `RuntimeInfo` protobuf with name `io.containerd.kiln.v1`, the shim's version, and the annotation `containerd.io/runtime-allow-mounts = "mkdir/*,format/*,erofs"`.
   - containerd then passes those mounts through instead of mounting them on the host.
   - containerd 2.4's `MountCapabilities` (on the stdin bootstrap) replaces the annotation once the Rust crate supports bootstrap. The annotation is deprecated but honored.
2. **Parsing `CreateTaskRequest.rootfs`** (pure, unit-tested):
   - Leading `mkfs/*` mounts are ignored: kiln has its own scratch disk.
   - Then one or more `type: "erofs"` mounts. Their `source` is a regular file. They arrive top first and are reversed into kiln's lowest-first order.
     - `X-containerd.dmverity=…` options are ignored with a log line (verity is S4).
     - A `device=` option (an fsmerged image) is refused: v1 attaches one disk per layer.
   - A trailing `overlay` or `format/…overlay` mount is ignored: kiln-init builds its own overlay (kiln §9.6).
   - Anything else is refused with `kiln needs containerd's erofs snapshotter (snapshotter = "erofs" on the kiln runtime)`. That includes:
     - an `overlay` whose `lowerdir` names host directories (the overlayfs snapshotter);
     - a `bind` mount;
     - an empty `rootfs` (a pre-mounted bundle).
3. **Device budget.** The layer count must fit `max_virtio_devices` minus init, scratch, vsock and net (kiln §6.4): 13 on Firecracker x86_64, 88 on Firecracker aarch64, 27 on Cloud Hypervisor. Over that, `Create` is refused, naming the counts. Squashing on the node (spec §3.1) is a follow-up.
4. **Kernel and init** come from the shim's configuration (§10.1), never from the image (kiln T4).
   - `init` is a static `kiln-init` binary. The worker wraps it into the init layer with `kiln_image::init_layer`, as `kiln run --init` does.
   - `kernel` is a path.
   - The node admin's config is the trust root here, as kiln's pin table is for `kiln run`.
5. **The layers as the kiln user sees them.** The layer files live under containerd's root, which is mode 0700.
   - The worker, still root, unshares a private mount namespace.
   - It bind-mounts each `layer.erofs` read-only onto `<vm dir>/layers/<n>.erofs` (§8).
   - The mounts disappear with the worker, so nothing is left to clean up.
   - The files must be readable by the kiln user: `mkfs.erofs` writes them `0644`, and the end-to-end test checks this.

---

## 6. Networking: the guest is the pod (D4, attach mode)

The user chose on 2026-10-21 to build vmkit's attach mode now, rather than an interim pasta-based network.

### 6.1 Pod attachment (the shim, as root, once per pod netns)

Before the first container VM of a pod netns starts, the shim runs this from a thread that has `setns`'d into the pod netns, using `ip` and `tc`:

1. **Find the CNI interface:** the one non-loopback interface with an IPv4 address (normally `eth0`), from `ip -j addr` and `ip -j route`. It records:
   - the address and prefix;
   - the MAC;
   - the MTU;
   - the default gateway.

   No IPv4 interface means the pod has no network: the VM gets none and a warning is logged. More than one is refused.
2. **The tap:** `ip tuntap add kiln0 mode tap user <kiln uid> group <kiln gid>`, then `ip link set kiln0 mtu <MTU> up`.
3. **Inbound, `eth0` ingress:** a `clsact` qdisc and a `matchall` filter with `mirred egress redirect dev kiln0`. Everything CNI delivers to the pod goes to the guest.
4. **Outbound, `kiln0` ingress:** a `clsact` qdisc and `flower` filters, evaluated in priority order:

   | Priority | Match | Action |
   |---|---|---|
   | 1 | IPv4 from the pod IP to each `egress_allow` CIDR from the config | redirect to `eth0` |
   | 2 | IPv4 to `169.254.0.0/16` (cloud metadata, link-local) | drop |
   | 3 | From the pod IP to each of the node's own IPv4 addresses: TCP with ACK set, ICMP echo reply, ICMP destination unreachable | redirect to `eth0` |
   | 4 | Any other IPv4 to each of the node's own addresses, whatever the protocol | drop |
   | 5 | IPv4 from the pod IP | redirect to `eth0` |
   | 6 | ARP with sender IP = pod IP | redirect to `eth0` |
   | 7 | anything else (IPv6, spoofed sources) | drop |

   - **Why stateless:** guest traffic is redirected before the pod netns's IP stack or netfilter sees it, so nftables in the pod netns can't filter it.
   - **Why rules 3 and 4 let only replies through:** the guest must still be able to *answer* kubelet, whose probes come from a node address. Passing only replies (a TCP segment with ACK set, an ICMP answer), and dropping the rest over any protocol, means the guest can open nothing on the node.
   - **What D4 delegates elsewhere:** private and cluster ranges are not denied. Routing policy belongs to the CNI and NetworkPolicy.
   - **What the filters cannot see:** they match the guest's packets *before* kube-proxy's DNAT. So a Service whose endpoints are node addresses (the API server, a hostNetwork pod) stays reachable through its Service IP, as it is for a runc pod. That is how in-cluster API access works. Enforcing the node deny list after DNAT, with node-side nftables on traffic from pod IPs, is a follow-up. The node's addresses are read when the pod is first attached; addresses added to the node later are not covered for that pod.
   - **If a step fails:** setup stops, the tap is deleted (with its filters), and the shim's redirect on the CNI interface is removed. A later container then finds no tap and sets everything up again, instead of taking a half-made attachment for a complete one.
5. **Remembering the result.** The shim keeps the attachment in memory, keyed by the netns's device and inode. One shim serves one pod, so the record lives exactly as long as the pod's shim. A second container in the same netns, such as the app after its init containers, reuses it without touching the netns again. A netns that already has `kiln0` is only read.

When the pod netns is deleted, the tap and the filters go with it. The shim removes nothing itself.

### 6.2 The guest

`Config.network` carries:
- the pod IP and prefix;
- the CNI gateway;
- the resolvers from the pod's `resolv.conf` (§6.5).

The guest NIC takes `eth0`'s MAC, so neighbours' ARP entries and CNI anti-spoofing see the same MAC as for runc. The MTU is the tap's, which the guest learns from virtio-net.

### 6.3 vmkit `TapAttach`

The pod netns is owned by the host's initial user namespace, so an unprivileged process can't `setns` into it. The worker joins it while still root and then drops privileges. vmkit must then leave the VMM there instead of creating a namespace of its own. That is a new vmkit network mode:

- **`net::TapAttach { tap: String, mac: String }`**, validated as follows:
  - `tap` is a valid interface name (1–15 bytes from `[A-Za-z0-9_.-]`);
  - `mac` is a unicast `xx:xx:xx:xx:xx:xx`.
- **`VmSpec.tap: Option<TapAttach>`.** It excludes `VmSpec.net`; a spec with both is invalid. It counts as one virtio device.
- **`sandbox::Network::Attached`.** The helper's `init` stage does not unshare `CLONE_NEWNET`. `/dev/net/tun` is bound into the root.
- **What the VMM can and can't do in the caller's netns:**
  - It opens the tap by name. That is allowed because the tap's owner is the VMM's uid.
  - It holds no capability over the netns, so it can't change addresses, routes or filters.
- **The drivers** (Firecracker `host_dev_name` and `guest_mac`; Cloud Hypervisor `tap` and `mac`) take the name and MAC from `TapAttach`.
- **vmkit does not create the tap.** Creating it needs root in the pod netns, and that is the shim's job (§6.1).

**Trade-off, recorded for the threat model.** In attach mode a compromised VMM process could open sockets in the pod netns, where today it sits in an empty one. That gives it the pod's network position, which the guest already has, and nothing more. The node deny list (§6.1) is enforced on the tap's ingress, so a VMM-originated socket bypasses it. The VMM is still seccomp-confined (Firecracker's default filter and Cloud Hypervisor's `--seccomp true`). Closing this needs a per-VM netns joined to the pod netns by a veth, which is left as a follow-up.

### 6.4 Pods without a network

- `hostNetwork: true` pods are refused at the sandbox's `Create`: a VM can't share the node's network namespace.
- A `ctr run` container has no netns path, so its VM gets no NIC (loopback only).

### 6.5 DNS

The pod's `resolv.conf` is the source of the container spec's mount whose destination is `/etc/resolv.conf`. The shim reads it as root, so it reads only the file CRI wrote for the task's own sandbox: its path must end in `sandboxes/<sandbox id>/resolv.conf`, and it must be a regular file opened without following a symlink, read up to 64 KiB. A pod's own hostPath volume at `/etc/resolv.conf` could otherwise hand the guest lines of any host file, or hang the shim on a FIFO. The shim then:
- puts its IPv4 `nameserver`s into `Config.network.dns`;
- puts its `search` domains into the new `Config.network.search`;
- puts its `options` into the new `Config.network.options`.

kiln-init writes all three. Both new fields are optional: they are omitted from the wire when empty, so existing guests and hosts are unaffected. Without the search domains, short Service names (`my-svc`) would not resolve.

`/etc/hosts` and `/etc/hostname` are kiln-init's own (kiln §9.6), from the spec's `hostname` (the pod name).

---

## 7. Processes, stdio, logs and exit status

### 7.1 Spec translation (pure, unit-tested)

| OCI spec | kiln-proto `Config` |
|---|---|
| `process.args` | `process.entrypoint = args`, `cmd = []`. CRI has already merged the image's entrypoint and cmd with the pod's `command` and `args`. Empty args are refused. |
| `process.env` | `process.env`, unchanged |
| `process.cwd` | `process.working_dir` |
| `process.user.uid`, `gid` | `process.user = "uid:gid"` (`additionalGids` are not carried in v1) |
| `process.terminal` | `tty = Some(24×80)` until the first `ResizePty` |
| a `stdin` FIFO path in the request | `interactive = true` |
| `hostname` | `hostname`, which must pass kiln-proto's hostname rule, else `kiln-<task id prefix>` |
| `linux.resources.cpu.quota / period` | vCPUs = ⌈quota ÷ period⌉, at least 1, at most the config's `max_cpus`. Without a quota, `default_cpus`. |
| `linux.resources.memory.limit` | guest MiB = ⌈limit ÷ 1 MiB⌉, at least 64. Without a limit, `default_memory_mib` (D3: 1024). |
| stop signal | `SIGTERM`. containerd sends the stop signal itself with `Kill`, so `Config.stop_signal` only matters for kiln's own `Shutdown`. |

Capabilities, seccomp, AppArmor, rlimits, `readonlyRootfs` and masked paths have no meaning inside the VM. The VM is the boundary, and the guest runs its process as kiln-init does. They are ignored.

### 7.2 Mounts in the spec

Each entry in `mounts` is classified (pure, unit-tested):

| Class | Examples | v1 behaviour |
|---|---|---|
| Provided by kiln-init | `/proc`, `/dev`, `/dev/pts`, `/dev/shm`, `/dev/mqueue`, `/sys`, `/sys/fs/cgroup` | ignored |
| Translated | `/etc/resolv.conf` | §6.5 |
| Written by kiln-init | `/etc/hosts`, `/etc/hostname` | ignored |
| Not delivered yet | sources under `/var/lib/kubelet/pods/<uid>/volumes/kubernetes.io~{projected,secret,configmap,downward-api,empty-dir}/`, `/dev/termination-log` | ignored. One warning per task goes to the shim log *and* to the container's stderr, so it shows in `kubectl logs`. Delivery of these files is spec §4.5. |
| Anything else | hostPath, CSI (`kubernetes.io~csi`), NFS | refused at `Create`, naming the mount's destination |

The service-account token is a projected volume. It is not inside the guest in v1, so in-cluster API clients fail until §4.5 lands. §12 says so.

### 7.3 stdio and logs

- **Opening the FIFOs.** The shim opens the FIFO paths from `CreateTaskRequest`: `stdout` and `stderr` for writing, and `stdin` for reading when given. It passes them to the worker as its fds 0, 1 and 2, and they become kiln-run's `Streams`.
- **Terminal mode.** With a terminal there is only `stdout`, carrying the terminal stream (kiln port 1028).
- **Logs.** containerd's CRI plugin writes the CRI log file from the FIFOs, as it does for runc. Fluent Bit and `kubectl logs` see the guest's bytes unmodified (kiln T8: app stdio is passed through).
- **Failure messages.** When the guest fails before or around its process, the worker writes kiln's diagnostics to the container's stderr (or stdout with a terminal), prefixed `kiln:`. That covers `InitFailed`, a protocol violation, the VM ending without `Exited`, and the console tail. They are sanitised (T8), so `kubectl logs` explains why.
- **`CloseIO{stdin}`.** The worker stops relaying stdin, and the guest sees EOF.

### 7.4 The worker and its control channel

The worker is the shim binary re-executed as `containerd-shim-kiln-v1 vm` (§8). The shim talks to it over a Unix socketpair whose worker end is inherited at the fd named by `KILN_SHIM_FD`. The protocol is JSON lines, one object per line. The first line is the worker's plan (at most 1 MiB); each message after it is capped at 64 KiB. Children are spawned one at a time, so the inheritable end reaches no other child.

| Direction | Message | Meaning |
|---|---|---|
| shim → worker | `{"start":{}}` | Boot the VM (task `Start`) |
| shim → worker | `{"signal":N}` | Forward signal N (1–31, not 9) to the guest's main process (`Handle::signal`) |
| shim → worker | `{"kill":{}}` | Kill the VM (`Handle::kill`) |
| shim → worker | `{"resize":{"rows":R,"cols":C}}` | `Handle::window_size` |
| shim → worker | `{"closeStdin":{}}` | §7.3 |
| worker → shim | `{"ready":{}}` | Preparation done (task `Create` returns) |
| worker → shim | `{"started":{"vmmPid":P}}` | The VM is booting (task `Start` returns) |
| worker → shim | `{"exited":{"code":C}}` | The session's `Outcome.exit_code` |
| worker → shim | `{"failed":{"message":"…"}}` | Preparation or boot failed. `Create` or `Start` returns the message as an error. |

The worker also exits with the guest's code. If the channel closes without `exited`, for instance because the worker crashed, the task exits with **255**, as spec §5 says for a VMM that dies unexpectedly.

### 7.5 Task API mapping

| Task call | Sandbox task (pause) | Container task (VM) |
|---|---|---|
| `Create` | Validate the spec (refuse host network), spawn `pause` in the pod netns, publish `TaskCreate`. Returns the pause pid. | Validate, translate, classify mounts, parse rootfs, check the device budget and the one-at-a-time rule, attach the pod network (§6.1), spawn the worker and wait for `ready`, publish `TaskCreate`. Returns the worker pid. |
| `Start` | Mark running, publish `TaskStart` | Send `start`, wait for `started`, mark running, publish `TaskStart` |
| `Kill{signal}` | `kill(pause pid, signal)` | `SIGKILL` → `kill`. Any other signal → `signal`. Before `Start`: the worker is killed, and the task exits 128 + signal. `all` is treated like the single process: there is one. |
| `Wait` | Blocks until stopped, then returns `exit_status`, `exited_at` | same |
| `State` | `CREATED` / `RUNNING` / `STOPPED`, pid, stdio paths, exit status | same |
| `Delete` | Refused while running. Afterwards it forgets the task, publishes `TaskDelete`, and returns its exit. | same, and removes the task's directories |
| `Pids` | the pause pid | the worker pid |
| `ResizePty` | — | `resize` |
| `CloseIO` | — | `closeStdin` |
| `Connect` | shim pid, task pid, version | same |
| `Shutdown` | When no task remains, the shim exits | same |
| `Exec` | Error `Unimplemented`: "kiln: exec needs kiln-init exec support (kiln-shim spec §4.1)" | same |
| `Stats`, `Update`, `Pause`, `Resume`, `Checkpoint` | the crate's default `NotFound` ("not supported") | same |

Events are published in containerd's required order: `TaskCreate`, `TaskStart`, `TaskExit`, `TaskDelete`. `TaskExit` is published from the thread that sees the worker's (or pause process's) end, before any `Wait` returns.

---

## 8. Privileges and files

containerd starts shims as root. kiln's T6 says no kiln component runs as root, and vmkit's sandbox helper refuses to run as root. So the roles split:

- **The shim (root)** does only privileged plumbing:
  - the ttrpc service;
  - opening the FIFOs;
  - attaching the pod network (§6.1);
  - creating the per-task directories;
  - spawning `pause` and the workers.
- **The worker** starts as root and does three privileged things, then drops privileges for good:
  1. It `setns`es into the container's network namespace (when it has one).
  2. It joins the container's cgroup:
     - `linux.cgroupsPath` is resolved for cgroup v2, for both the cgroupfs form `/a/b/c` and the systemd form `slice:prefix:name` (→ `…/<slice path>/<prefix>-<name>.scope`).
     - It creates the leaf and writes its pid to `cgroup.procs`.
     - So the VMM is accounted to the pod's cgroup, whose limits include `RuntimeClass.overhead`.
     - A failure is logged and does not stop the task.
  3. It unshares a mount namespace and bind-mounts the layers (§5.5).

  Then it calls `setgroups` (the kiln user's groups, which must include `kvm`), `setgid` and `setuid` to the configured kiln user, and sets `PR_SET_PDEATHSIG(SIGKILL)` again (a credential change clears it). Only after that does it touch kiln-run or vmkit.
- **The VMM** runs in vmkit's unprivileged sandbox, as for `kiln run`, but in the pod netns (§6.3).

| Path | Owner, mode | What |
|---|---|---|
| `/run/kiln-shim/`, `/run/kiln-shim/vm/` | root, 0711 | Traversable, not listable, by the kiln user |
| `/run/kiln-shim/vm/<ns>-<task id>/` | root, then kiln; 0700 | The worker's directory: `layers/<n>.erofs` mount points, `init.erofs`, and `run/` (vmkit's run directory: the VMM's sockets, logs and console FIFO) |
| `<store>/shim-scratch/<ns>-<task id>/scratch.img` | kiln | The VM's scratch disk, on the store's filesystem rather than the `/run` tmpfs |

The worker's directory is created fresh and root-only. The worker makes `layers/` and the bind-mount targets inside it while it is still root. Only then does it hand `layers/` and the directory itself to the kiln user, with a `chown` that does not follow symlinks. So that user never controls a directory in which root still creates files or mounts. A leftover directory (a crashed shim's) is removed first, without following its links.

The worker removes its scratch disk itself, as the kiln user, before it exits: root deletes nothing inside that user's store. The shim removes the worker's directory under `/run/kiln-shim/vm/` once the worker has exited; the worker's layer mounts went with its mount namespace.

The kiln user gets no systemd user session (no lingering). `vmkit::cgroups_available()` is false, so the VMM stays in the pod's cgroup where the worker put it.

containerd's Rust shim crate reaps every child on `SIGCHLD` unless told not to. The shim runs it with `no_reaper` and `no_sub_reaper`, and waits for its pause and worker processes itself.

---

## 9. Lifecycle of a pod, end to end

1. **RunPodSandbox.** containerd starts the shim (`start` action). `start_shim` reads the bundle's `config.json`, groups by `sandbox-id`, and calls `spawn`. The sandbox task's `Create` spawns `pause` in the CNI netns; its `Start` marks it running.
2. **CreateContainer** (init container or app). Translate the spec, attach the pod network on first use, then spawn the worker. As root, the worker joins the pod netns and cgroup and bind-mounts the layers, then drops to kiln. It writes the init layer and the scratch disk, discovers the VMM, builds the `Config`, and sends `ready`.
3. **StartContainer.** On `start`, the worker:
   - creates the VM through vmkit (`VmSpec.tap` = `kiln0` with the pod's MAC);
   - starts the session with the FIFOs as its streams;
   - sends `started`.

   The guest boots, mounts its layers and runs the process. Its output reaches the CRI log.
4. **The process exits.**
   - kiln-init sends `Exited`, and the VM ends.
   - The worker sends `exited`, writes any `kiln:` diagnostics, and exits.
   - The shim marks the task stopped and publishes `TaskExit`.
   - kubelet's restart policy applies.
5. **StopContainer.** containerd sends `Kill(SIGTERM)` (or the image's stop signal). The worker forwards it to the guest. After the grace period, `Kill(SIGKILL)` makes the worker kill the VM (exit 137).
6. **StopPodSandbox / RemovePodSandbox.** Containers are stopped and deleted, then the pause task is killed and deleted. When no tasks remain, `Shutdown` ends the shim. CNI deletes the netns, and the tap and filters go with it.

---

## 10. Configuration

### 10.1 The shim's config file

The file is TOML, root-owned. It is found in this order:
1. containerd's runtime options (`ConfigPath` or the inline body, delivered as `runtimeoptions.v1.Options` in `CreateTaskRequest.options`);
2. `$KILN_SHIM_CONFIG`;
3. `/etc/kiln/shim.toml`.

```toml
# The unprivileged user VMs run as; it must be in the kvm group (kiln T6).
user = "kiln"
# That user's kiln store (KILN_HOME): scratch disks.
store = "/var/lib/kiln"
# Node-pinned boot artifacts (kiln T4): a kernel and a static kiln-init.
kernel = "/var/lib/kiln/boot/vmlinux"
init = "/usr/local/lib/kiln/kiln-init"
vmm = "firecracker"            # or "cloud-hypervisor"
default_cpus = 1
max_cpus = 8
default_memory_mib = 1024      # D3
scratch_disk = "4G"            # each container VM's writable layer
boot_timeout_secs = 30
# Link-local or node addresses the guest may reach anyway, e.g. NodeLocal DNSCache.
egress_allow = ["169.254.20.10/32"]
```

### 10.2 containerd 2.2+ (config version 3)

```toml
version = 3

[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kiln]
  runtime_type = 'io.containerd.kiln.v1'     # containerd-shim-kiln-v1 on containerd's PATH
  snapshotter = 'erofs'
  sandboxer = 'podsandbox'
  [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kiln.options]
    ConfigPath = '/etc/kiln/shim.toml'

# Unpack images for this runtime with the erofs differ at pull time.
[plugins.'io.containerd.cri.v1.images'.runtime_platforms.kiln]
  snapshotter = 'erofs'
```

The node needs `mkfs.erofs` (erofs-utils 1.8 or later) for the erofs differ, plus `ip` and `tc` with `cls_flower` and `act_mirred` (every mainstream kernel ships them). containerd 1.7 has no erofs snapshotter and is not supported.

### 10.3 RuntimeClass

```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: kiln
handler: kiln
overhead:
  podFixed:
    memory: 40Mi
    cpu: 50m
scheduling:
  nodeSelector:
    kiln.dev/kvm: "true"
  tolerations:
    - key: kiln.dev/kvm
      operator: Exists
      effect: NoSchedule
```

---

## 11. Testing

### 11.1 Unit tests (macOS and Linux)

Every pure piece has its own tests:
- spec translation (§7.1);
- mount classification (§7.2);
- rootfs parsing (§5.2), including the containerd 2.1 and 2.2+ shapes and every refusal;
- resources;
- config parsing;
- `resolv.conf` parsing;
- `ip -j` parsing and the tc command list (§6.1);
- the cgroup path resolution (§8);
- the worker protocol (§7.4).

### 11.2 Task service tests (macOS and Linux)

The service runs over a `Backend` trait. A fake backend drives the state machine through the Task calls:
- Create, Start, Wait, Kill, Delete;
- the event order;
- the one-container-at-a-time rule;
- Delete refused while running;
- exit 255 when a worker goes away;
- Exec refused.

The real backend is the Linux worker.

### 11.3 End to end (Linux with KVM)

`scripts/shim-e2e.sh` runs as root on a KVM host (the repo's Lima VM). It uses:
- a private containerd 2.2+ (its own root, state and socket), with CRI, the erofs snapshotter, and the `bridge` and `host-local` CNI plugins;
- crictl;
- a shim config pointing at a kernel and kiln-init.

It runs:
1. `echo hello`, then checks `crictl logs` and exit code 0;
2. `exit 3`, then checks 3;
3. an init container, then the app;
4. busybox `httpd` on port 8080, then `curl` from the node to the pod IP;
5. from inside the guest, connecting to `169.254.169.254:80` and the node's address fails (the guest command reports it in its own exit code);
6. `crictl stop` with a SIGTERM-ignoring process exits 137 after the timeout.

The same steps are written up as a manual recipe in `docs/running.md`.

The vmkit `TapAttach` mode has its own contract test in vmkit. Its guest boots with a tap that the test creates in a netns. The test needs root to create the netns and runs as an ignored-by-default test.

---

## 12. Out of scope for v1, and why

| Item | Why | Where it goes |
|---|---|---|
| `Exec` (`kubectl exec`, exec probes) | kiln-init runs one process and has no exec messages | Spec §4.1 (S1): kiln-proto `ExecStart` / `ExecExited` |
| Projected volumes, Secrets, ConfigMaps, service-account tokens, `emptyDir` | No file delivery into the guest yet | Spec §4.5: files in `Config`, plus `FileUpdate` |
| hostPath, CSI, NFS | No shared filesystems in kiln | Refused (spec §4.5) |
| `Stats`, OOM → `OOMKilled` | Needs guest resource signals | Spec §4.2 |
| In-place resize (`Update`) | Needs hotplug and balloon | Spec §4.3 |
| More than one container VM at once (sidecars) | D2 | Refused when the second container starts |
| containerd sandbox API | Needs the multi-rootfs guest (D2) | Later |
| IPv6 inside the guest | kiln-proto `Network` is IPv4. The tap's ingress drops IPv6. | After kiln's IPv6 work lands |
| Enforcing the node deny list after Service DNAT | tc sees the guest's packets before kube-proxy translates them (§6.1) | Node-side nftables for pod IPs |
| Squashing over the device budget | Not needed for typical images on Cloud Hypervisor or Firecracker aarch64 | Spec §3.1 |
| dm-verity | S4 | Spec §3.1 |
| containerd 2.3+ stdin bootstrap and 2.4 `MountCapabilities` | The Rust crate lacks bootstrap support. The legacy protocol and the RuntimeInfo annotation still work. | When the crate supports it |
| `additionalGids` | kiln-proto's `user` has no supplementary-group list | kiln-proto change |
| Pod network `hostPort`s | CNI `portmap` works on the pod IP unchanged, since the guest owns it | — |

---

## 13. Decisions and deviations from the 2026-10-05 spec

| # | Decision | By |
|---|---|---|
| V1 | Networking is the spec's tap and tc redirect (D4), with vmkit's new attach mode built now (§6.3) | The user, 2026-10-21 |
| V2 | The runtime type is `io.containerd.kiln.v1` (binary `containerd-shim-kiln-v1`), not `.v2`. The suffix versions kiln's shim, not the shim protocol. | The task brief |
| V3 | The `podsandbox` model with a `pause` process; one VM per container task, at most one at a time; init containers in sequence. This gives D2's single-workload pod without the sandbox API. | My call (§4) |
| V4 | The fixed deny list is enforced with tc filters on the tap, not nftables: guest traffic bypasses netfilter. Node addresses block only new connections, so replies to kubelet probes pass. | My call (§6.1) |
| V5 | A root `vm` worker per container drops to a configured kiln user after the netns, cgroup and bind mounts. The shim itself never runs vmkit. | My call (§8) |
| V6 | The kernel and kiln-init are node paths in the shim's config until kiln-init is released and pinned | My call (§5.4) |
| V7 | Unknown mounts (hostPath, CSI) are refused. Projected, Secret, ConfigMap, downward-API and emptyDir volumes are ignored with a visible warning, so ordinary pods run before §4.5 lands. | My call (§7.2) |
| V8 | kiln-proto `Network` gains optional `search` and `options`, and kiln-init routes via an on-link gateway, so cluster DNS and `/32` pod IPs work | My call (§6.5) |
