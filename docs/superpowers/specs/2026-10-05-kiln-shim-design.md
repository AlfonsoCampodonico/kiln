# kiln-shim (codename slip) — Design Spec

**Date:** 2026-10-05
**Status:** Draft rev 1. Decisions D1–D4 were confirmed by the user on 2026-10-05 (§9).
**Home:** this spec belongs in the **kiln** repo. It's drafted on the potter branch because another session is working in kiln (`m3a-guest`). Move it to `kiln/docs/superpowers/specs/` once that work lands.
**Related:** the kiln spec (rev 2.7), the potter spec (rev 1.3), and `docs/superpowers/ROADMAP.md`.

---

## 1. Goals

`kiln-shim` is a containerd shim v2 (`io.containerd.kiln.v2`) behind a Kubernetes RuntimeClass `kiln`. It runs each pod in a microVM, using `vmkit` and `kiln-init`.

It is a deploy path for a managed Kubernetes platform. A tenant pod opts in with `runtimeClassName: kiln`, and everything else keeps working unchanged:
- the platform's operators, its per-environment namespaces and Services/Ingress;
- kubelet probes;
- a node-level Fluent Bit log collector;
- `kubectl logs` and `kubectl exec`.

### Success criteria

1. A PHP web app built by potter runs in a tenant namespace with `runtimeClassName: kiln`. It serves traffic through the existing Ingress, passes HTTP and exec probes, and its logs arrive in the log pipeline with no Fluent Bit changes.
2. `kubectl exec -it` (TTY) and non-TTY exec work. That covers one-off commands run through exec.
3. When the guest runs out of memory, the container reports `OOMKilled` with exit code 137, as a runc container does.
4. The same pod spec runs with runc or with `kiln` by changing only `runtimeClassName`. Ordinary OCI images also run, not only potter-built ones.
5. Pod start is within **1.5 s** of runc on a warm node (image present). VMM memory overhead is ≤ **40 MiB** per pod and is declared in `RuntimeClass.overhead`.
6. No guest-asserted value (stats, OOM reports, exit codes) drives a security decision.

### Non-goals (v1)

- Pods with more than one workload container (D2). Init containers and ephemeral debug containers are covered in §4.4.
- Live migration.
- GPU or other device passthrough.
- Windows nodes.
- CSI block volumes. `emptyDir` and projected volumes (Secrets, ConfigMaps, service-account tokens) are supported; see §4.5.
- Replacing Kata Containers in general. This is purpose-built for kiln's guest and threat model.

---

## 2. Infrastructure prerequisite: KVM on the nodes (D1)

Every kiln node, and every potter build node using `VmExecutor`, needs `/dev/kvm`.

| Option | Instances | Notes |
|---|---|---|
| **A. Nested virtualization** (recommended to evaluate first) | Virtual C8i/M8i/R8i since February 2026, plus C7i/M7i/R7i, the C8id/M8id/R8id and flex variants, I7i and X8i since June 2026. No extra cost | Fits existing Kubernetes node groups: a new node group with nested virtualization enabled, plus a node label. **Intel only.** The overhead of nested Firecracker and Cloud Hypervisor must be benchmarked (boot time, vsock throughput, EPT miss costs) before committing. |
| **B. Bare metal** | `*.metal` (Intel, AMD, Graviton) | Best performance and the only aarch64 option. A large blast radius per node and coarse capacity steps. |
| **C. Mixed** | Nested for x86_64, metal Graviton for aarch64 | Matches kiln's two-arch commitment. |

**Recommendation:** start with A on C8i in a low-traffic canary region, and run kiln's bench and E2E matrix there. Move to C only if aarch64 demand justifies it.

**Scheduling:**
- Kiln nodes carry the label `kiln.dev/kvm=true` and the taint `kiln.dev/kvm:NoSchedule`.
- `RuntimeClass.scheduling` adds the matching nodeSelector and toleration, so pods that request `kiln` only land on KVM nodes.

---

## 3. Architecture

```
kubelet ─CRI─► containerd 2.3+ ─shim v2 ttrpc─► kiln-shim (one per pod)
                   │ erofs snapshotter (native EROFS layers, dm-verity)
                   ▼
            snapshot mounts ──► kiln-shim ──vmkit──► Firecracker / Cloud Hypervisor (sandboxed, unprivileged)
                                   │  kiln-proto over vsock
                                   ▼
                               kiln-init (PID 1 in guest)
```

- **One VM per pod** (D2). This uses containerd's sandbox API. The pod sandbox *is* the VM, and the workload container is its main process.
- **The shim process:**
  - containerd starts the shim as root, as it does for every shim.
  - The shim does the privileged setup only: opening the tap in the pod netns, attaching block devices from snapshot mounts, and the cgroup placement kubelet expects.
  - It then launches the VMM through `vmkit::sandbox`, unprivileged, as kiln's T6 requires.
  - The shim's own attack surface is the containerd ttrpc API and kiln-proto. It parses kiln-proto with kiln's bounded, schema-validated decoder.
- **Kernel and init:** the pinned kernel and kiln-init come from the node's kiln install, never from the image (kiln T4). Images can't change the kernel cmdline.

### 3.1 Images

- **The containerd 2.3 erofs snapshotter and differ** are configured for the `kiln` runtime.
  - Images whose layers are already EROFS-native (potter and kiln output) are pulled as is.
  - Plain OCI tar images are converted to erofs on the node by containerd's erofs differ.
  - Either way, every image runs.
- **Required change in kiln:** kiln's layer media type and erofs profile must be accepted by containerd's native EROFS layer support.
  - Align the media type with containerd's, or register kiln's as an alias, and verify that kiln's erofs profile (block size, no compression in v1, xattrs) mounts through the snapshotter.
  - This is a kiln spec change and a test, tracked as a prerequisite.
- **dm-verity:** when the snapshotter provides verity metadata, the shim passes the root hash to the guest, and kiln-init sets up `dm-verity` over each layer device. This strengthens kiln's T1 at run time.
- **Attachment:**
  - The shim receives the snapshot mounts (erofs blobs plus the overlay description).
  - It attaches each erofs blob as a read-only virtio-blk disk, adds the pinned init layer and a scratch disk for the upper, and passes the layer order to kiln-init. This is exactly kiln's run model.
  - Over the device budget, it squashes from the bottom with kiln's squash, cached on the node.

### 3.2 Networking

- The CNI creates the pod netns and its veth as usual. The shim creates `tap0` inside the pod netns and links veth and tap with **tc mirred redirect** in both directions (Kata's `tcfilter` model).
- The guest gets the pod's IP, routes and DNS through kiln-proto `Config.network`. To Kubernetes the VM *is* the pod IP, so Services, Ingress, NetworkPolicy and probes work unchanged.
- **Egress policy under the shim:** kiln's standalone default-deny (T5) does not apply, because apps must reach managed databases and caches on private addresses.
  - Routing policy belongs to the CNI and NetworkPolicy.
  - The shim **always** enforces a fixed deny list, in nftables inside the pod netns, applied before the VMM starts and unreachable from the guest: link-local (including instance metadata `169.254.169.254` and `fd00:ec2::254`) and the node's own addresses.
- `vmkit` inbound port publishing is not used under the shim. It remains for standalone `kiln run`.

### 3.3 Logs and stdio

- containerd hands the shim the container's stdio FIFOs, and the CRI log file is written by containerd from those FIFOs, as for runc.
- The shim relays the stdout/stderr messages from kiln-proto into them unmodified.
- A node-level Fluent Bit log collector tails the CRI log path as today. No pipeline change.

---

## 4. Required kiln-proto and kiln-init extensions

These are kiln PRs, each with its own plan. The protocol stays a single protocol (ROADMAP rule).

### 4.1 Exec

- **Host → guest:** `ExecStart{exec_id, argv, env, cwd, user, tty, window}`.
- **Guest → host:** `ExecStarted{exec_id, pid}`, then `ExecExited{exec_id, code|signal}`.
- **Stdio multiplexing:**
  - The stdio messages gain a `proc` field: `main | exec:<id>`. The main process stays `main`, so existing messages are unchanged on the wire. Old guests reject the new message types, and the host checks the guest version in `Hello`.
  - `Signal` and `WindowSize` gain `proc` too.
- **Limits:** at most 32 concurrent execs per VM (configurable). Each exec's stdio buffering is bounded like the main process's. Exec ids are host-chosen and validated.
- **Semantics:**
  - The process runs in the guest's root, with the image's env merged with `env`.
  - There are no container namespaces inside the guest. The VM is the isolation boundary.
  - `user` follows kiln-init's existing id rules.
  - Exec processes are killed when the main process exits.
- **Mapping to the shim:** the task `Exec`, `ResizePty`, `CloseIO` and `Kill(exec_id)` APIs map directly. That gives `kubectl exec` and kubelet exec probes.

### 4.2 Guest resource signals

- **Guest → host, every 10 s by default and on request:**
  - `Stats{mem_total, mem_available, mem_rss_main, cpu_usage_ns, pressure{cpu,mem,io}}`, read by kiln-init from `/proc/meminfo`, `/proc/stat` and PSI.
  - `OomKill{pid, proc, comm}`, sent when the guest kernel OOM-kills a process. kiln-init watches `/proc/vmstat` `oom_kill` and the kernel log for the victim.
- **Shim behaviour:**
  - Task `Stats` returns the guest stats alongside the VMM cgroup's own usage, so kubelet, cAdvisor and the metrics pipeline see both.
  - If the main process dies after an `OomKill` that named it, the shim reports exit 137 with reason `OOMKilled`.
- **Trust:** stats and OOM reports are guest-asserted. They are used for reporting only, never for quota or security. The host-side memory limit is the VM's configured memory; the guest cannot exceed it.

### 4.3 Resources and resize

- **CPU:** vCPUs = `ceil(limits.cpu)`, or `ceil(requests.cpu)` when there's no limit, with a minimum of 1. The VMM process's cgroup gets the CFS quota the pod's cgroup already has, so CPU fairness is enforced by the host.
- **Memory:**
  - Guest memory = `limits.memory`. Without a limit, the shim uses the RuntimeClass default (D3: 1 GiB).
  - `RuntimeClass.overhead.podFixed` declares the VMM overhead (proposed 40 MiB, 50m CPU), so the scheduler accounts for it.
- **In-place pod resize:**
  - CPU uses vCPU hotplug where the VMM supports it.
  - Memory growth uses virtio-mem on Cloud Hypervisor, capability-gated (`Capabilities.supports_memory_hotplug`).
  - Memory shrink uses the balloon (`supports_balloon`).
  - When the VMM can't do the resize, the shim reports it as infeasible and kubelet restarts the pod. Firecracker cannot hot-add memory.
- **Builder VMM:** Cloud Hypervisor is the recommended VMM for the shim too (31 devices, memory hotplug).

### 4.4 Containers in the pod

- **Init containers:** run sequentially in the same VM before the workload. Each is booted from its own image's layers, so the init container and the app see different root filesystems.
  - The VM is rebooted between images (D2).
  - The alternative is guest-side multi-rootfs support (mount several layer stacks and pivot per process), which is deferred.
- **Ephemeral debug containers** (`kubectl debug`): refused in v1, with a clear error. `kubectl exec` covers the use case.
- **Sidecars** (native sidecar init containers): not supported in v1. Platforms that need a sidecar run it as kiln-init `services` inside the same image (the potter spec, §6.1.1).

### 4.5 Volumes

- **Projected volumes and `emptyDir`:**
  - Secrets, ConfigMaps, service-account tokens and the downward API are delivered as files through kiln-proto `Config` at start.
  - Updates arrive as `FileUpdate{path, content, mode}` messages, so kubelet's atomic symlink-swap semantics are reproduced by kiln-init.
  - The size is bounded (projected volumes are small by design).
- **`emptyDir`:** a directory on the VM's scratch disk. `medium: Memory` becomes a guest tmpfs, sized by `sizeLimit`.
- **Anything else** (hostPath, CSI, NFS): refused at create with a clear error in v1.

---

## 5. Lifecycle

| containerd call | Shim action |
|---|---|
| `CreateSandbox` (RunPodSandbox) | Prepare the tap and tc redirect in the pod netns, apply the fixed deny list, create the sandbox dirs. No VM yet. |
| `Create` (workload container) | Resolve the snapshot mounts into the disk list and allocate the scratch disk. Boot the VM through vmkit, paused before kiln-init runs the workload. |
| `Start` | Send `Config` (process, env, network, files, services). kiln-init starts the workload, and `Running` maps to the started task. |
| `Exec` / `Kill` / `ResizePty` / `CloseIO` | kiln-proto messages (§4.1). |
| `Wait` / `Exited` | From `Exited` (main) or `ExecExited`. OOM is mapped per §4.2. |
| `Stats` | §4.2. |
| `Delete` / `StopSandbox` | `Shutdown` with the pod's termination grace period, then kill the VMM, release the disks, and remove the tc and nft rules. |

If the VMM process dies unexpectedly, containerd sees the task exit with code 255 and reason `VMMExited`. kubelet's restart policy applies.

---

## 6. Threat model additions (relative to kiln §3)

| # | Property | Enforced by |
|---|---|---|
| S1 | A pod's guest cannot reach instance metadata or the node's addresses, whatever NetworkPolicy says | A fixed nft deny list in the pod netns, applied before VMM start (§3.2) |
| S2 | A guest cannot affect another pod's VM, netns, disks or logs | One VM and netns per pod; sandboxed unprivileged VMM; per-pod disk paths |
| S3 | The guest cannot pick the kernel, init or cmdline | Node-pinned artifacts (kiln T4) |
| S4 | Guest-reported stats, OOM reports and exit codes are never used for security or quota | §4.2 |
| S5 | Exec cannot be initiated by the guest, and exec ids can't be forged to hijack another process's stdio | Host-chosen ids, validated by the shim. The guest can only report on ids the host created |
| S6 | Layer tampering on the node is detected at read time when verity metadata exists | dm-verity in the guest (§3.1) |

---

## 7. Testing

- **The containerd shim conformance suite** (`critest` and the containerd integration tests) runs against `kiln` on KVM CI. Known unsupported features are skipped explicitly: multi-container pods, debug containers, hostPath.
- **E2E on kind with KVM nodes:**
  - deploy the potter-built PHP fixture behind an Ingress and check HTTP probes;
  - `kubectl exec` with and without a TTY;
  - a memory hog gets `OOMKilled`;
  - Secret and ConfigMap updates propagate;
  - CPU and memory resize on Cloud Hypervisor.
- **Log pipeline:** a Fluent Bit tail of the CRI log path receives the app's lines byte-for-byte.
- **Security:**
  - from inside the guest: metadata IP, node IP, a neighbour pod's VM;
  - a forged exec id from the guest;
  - oversized `Stats`/`OomKill`;
  - a tampered layer with verity metadata.
- **Benchmarks** (pod start vs runc, density, overhead): run on nested C8i and on metal, to feed decision D1.

---

## 8. Milestones

| # | Milestone | Depends on |
|---|---|---|
| S0 | KVM node group spike: nested C8i vs metal, kiln bench and E2E there | kiln M3b (`kiln run`) |
| S1 | kiln-proto exec + resource signals, kiln-init `FileUpdate` (kiln PRs) | kiln M3a |
| S2 | Media type alignment with containerd's erofs snapshotter (kiln PR + test) | kiln M1 |
| S3 | `kiln-shim`: lifecycle, networking, logs, exec, stats, projected volumes | S1, S2, kiln M3b |
| S4 | Resize, verity, init containers | S3 |
| S5 | Platform canary: one canary region, opt-in RuntimeClass on selected tenant namespaces | S0, S3 |

---

## 9. Decisions (confirmed 2026-10-05)

| # | Decision | Confirmed |
|---|---|---|
| D1 | KVM capacity | Nested virtualization on C8i first; metal Graviton only for aarch64 demand (§2) |
| D2 | Pod shape | One VM per pod, a single workload container in v1, init containers by reboot; multi-rootfs deferred |
| D3 | Default guest memory without a limit | 1 GiB |
| D4 | Egress under the shim | CNI and NetworkPolicy, plus a fixed metadata and node deny list. kiln's standalone default-deny does not apply (§3.2) |
