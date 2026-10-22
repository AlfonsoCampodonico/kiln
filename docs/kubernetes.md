# Running pods: the containerd shim

`containerd-shim-kiln-v1` is a containerd shim v2 (runtime type `io.containerd.kiln.v1`). Behind a Kubernetes RuntimeClass `kiln`, it runs a pod's container as a kiln microVM that owns the pod IP. kubelet, the CRI log files, Services, NetworkPolicy and HTTP/TCP probes keep working.

The design is [specs/2026-10-21-kiln-shim-v1-design.md](superpowers/specs/2026-10-21-kiln-shim-v1-design.md). This page is how to run it.

## What works, and what does not yet

**Works:**
- Pods with one workload container at a time: init containers run in sequence, each in its own VM.
- stdout and stderr (with or without a terminal), reaching the CRI log as with runc.
- Exit codes.
- Stop with SIGTERM, then SIGKILL after the grace period.
- `crictl stop`, `rm` and `rmp`.
- The pod IP, inbound and outbound, under the CNI and NetworkPolicy. Cloud metadata (`169.254.0.0/16`) and new connections to the node's own addresses are blocked for the guest.
- Cluster DNS, including search domains.

**Not yet:**
- `kubectl exec` and exec probes (refused with `Unimplemented`).
- Volumes. Projected volumes, Secrets, ConfigMaps, service-account tokens and `emptyDir` are not inside the VM: the pod runs, with a warning in its log. hostPath and CSI are refused.
- `Stats` and `OOMKilled`.
- In-place resize.
- Sidecars and multi-container pods: a second container fails to start while another runs.
- `hostNetwork` pods (refused).
- IPv6 inside the guest.

## Node requirements

- Linux with KVM, and an unprivileged user for the VMs (`kiln` below) in the `kvm` group.
- Firecracker or Cloud Hypervisor, and vmkit's `vmkit-sandbox` helper, set up for that user as vmkit's README describes (including the AppArmor profile on Ubuntu).
- containerd 2.2 or later (the erofs snapshotter and its mount manager), with `mkfs.erofs` from erofs-utils 1.8 or later. The snapshotter loads only where the host kernel has erofs (`modprobe erofs`, and `erofs` in `/etc/modules-load.d/` to keep it), although the shim never mounts the layers on the host.
- `ip` and `tc` (iproute2), and a kernel with `cls_flower`, `cls_matchall` and `act_mirred` (every mainstream distribution kernel has them).
- A kiln kernel (vmkit's `base` profile) and a static `kiln-init` ([testing.md](testing.md#building-kiln-init)).

## Install

1. Put `containerd-shim-kiln-v1` on containerd's `PATH`, e.g. `/usr/local/bin`.
2. Write `/etc/kiln/shim.toml` (root-owned):

   ```toml
   user = "kiln"                                # the VMs' user; in the kvm group
   store = "/var/lib/kiln"                      # that user's directory for scratch disks
   kernel = "/var/lib/kiln/boot/vmlinux"
   init = "/usr/local/lib/kiln/kiln-init"
   vmm = "firecracker"                          # or "cloud-hypervisor"
   default_memory_mib = 1024                    # guest memory without a memory limit
   scratch_disk = "4G"                          # each VM's writable layer
   egress_allow = ["169.254.20.10/32"]          # e.g. NodeLocal DNSCache, despite the link-local deny
   ```

3. Configure containerd (`/etc/containerd/config.toml`, version 3):

   ```toml
   [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kiln]
     runtime_type = 'io.containerd.kiln.v1'
     snapshotter = 'erofs'
     sandboxer = 'podsandbox'
     [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kiln.options]
       ConfigPath = '/etc/kiln/shim.toml'

   [plugins.'io.containerd.cri.v1.images'.runtime_platforms.kiln]
     snapshotter = 'erofs'

   [plugins.'io.containerd.service.v1.diff-service']
     default = ['erofs', 'walking']
   [[plugins.'io.containerd.transfer.v1.local'.unpack_config]]
     platform = 'linux/amd64'                   # or linux/arm64
     snapshotter = 'erofs'
     differ = 'erofs'
   ```

   containerd must also find vmkit's helper and the VMM for the shim. For example, add `Environment=VMKIT_SANDBOX=/usr/local/bin/vmkit-sandbox` to its systemd unit, and put the VMM on its `PATH`.

4. Create the RuntimeClass:

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
   ```

   Then set `runtimeClassName: kiln` on a pod.

## Trying it with crictl

```bash
cat > pod.json <<'EOF'
{ "metadata": { "name": "p1", "namespace": "default", "uid": "p1-uid", "attempt": 1 },
  "hostname": "p1", "log_directory": "/tmp/p1", "linux": {} }
EOF
cat > ctr.json <<'EOF'
{ "metadata": { "name": "hello" }, "image": { "image": "docker.io/library/busybox:1.37" },
  "command": ["sh", "-c", "echo hello from kiln; ip addr show eth0"], "log_path": "hello.log", "linux": {} }
EOF
crictl pull docker.io/library/busybox:1.37
POD=$(crictl runp --runtime kiln pod.json)
CTR=$(crictl create "$POD" ctr.json pod.json)
crictl start "$CTR"
crictl logs "$CTR"                                   # hello from kiln, and the pod IP on eth0
crictl inspect -o go-template --template '{{.status.exitCode}}' "$CTR"
crictl rm "$CTR"; crictl stopp "$POD"; crictl rmp "$POD"
```

`scripts/shim-e2e.sh` runs these checks and more against a private containerd that leaves the system one alone (see its header for its inputs):
- echo;
- an exit code;
- a second container after the first;
- HTTP to the pod IP from the node;
- the metadata and node deny list, with another pod reachable;
- stop with an ignored SIGTERM;
- a concurrent second container refused.

On this repository's Lima VM:

```bash
sudo env KILN_SHIM=$T/debug/containerd-shim-kiln-v1 KILN_KERNEL=... KILN_INIT=... \
  KILN_USER=$USER VMKIT_SANDBOX=... VMM_PATH=$HOME/bin scripts/shim-e2e.sh
```

## When a pod fails

- **The pod's log.** It holds the shim's `kiln:` lines: an init failure, the VM ending without the command's exit, the end of the guest console, and volumes not delivered.
- **containerd's log.** It holds the shim's own errors:
  - a rootfs from another snapshotter (`kiln needs containerd's erofs snapshotter`);
  - a refused mount;
  - a second container;
  - the VM worker's failures, such as the VMM not found, the device budget, or the kiln user's permissions.
- **Leftovers.** A worker's directory is `/run/kiln-shim/vm/<namespace>-<id>`, and its scratch disk is under `<store>/shim-scratch`. Both are removed when the worker ends.
