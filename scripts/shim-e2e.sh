#!/usr/bin/env bash
# End-to-end pods through containerd's CRI with the kiln shim (kiln-shim v1 design §11.3).
#
# Runs as root on a Linux host with KVM. It starts a private containerd (its own root,
# state, sockets and CRI streaming port, so a system containerd is left alone) with the
# erofs snapshotter and differ and a bridge CNI network, then runs pods with crictl and
# checks what they print and how they exit. Prints PASS or FAIL per case; exits 1 if any
# case failed.
#
# Needs: containerd 2.2+ and ctr, crictl, mkfs.erofs (erofs-utils), ip, tc, curl, the CNI
# plugins bridge, host-local and loopback, Firecracker or Cloud Hypervisor, and:
#   KILN_SHIM      the containerd-shim-kiln-v1 binary
#   KILN_KERNEL    a kiln kernel (vmkit's base profile)
#   KILN_INIT      a static kiln-init binary
#   KILN_USER      the unprivileged user VMs run as (in the kvm group; vmkit's sandbox
#                  helper must be allowed user namespaces for it, see vmkit's README)
#   VMKIT_SANDBOX  vmkit's sandbox helper
# Optional: KILN_VMM (firecracker), CNI_BIN (/usr/local/libexec/cni then /opt/cni/bin),
# IMAGE (docker.io/library/busybox:1.37), VMM_PATH (prepended to PATH for the VMMs),
# KEEP=1 to leave the private containerd running afterwards.
set -euo pipefail

: "${KILN_SHIM:?}" "${KILN_KERNEL:?}" "${KILN_INIT:?}" "${KILN_USER:?}" "${VMKIT_SANDBOX:?}"
VMM=${KILN_VMM:-firecracker}
IMAGE=${IMAGE:-docker.io/library/busybox:1.37}
E2E=/var/lib/kiln-shim-e2e
RUN=/run/kiln-shim-e2e
SOCK=$RUN/containerd.sock
CNI_BIN=${CNI_BIN:-$( [ -d /usr/local/libexec/cni ] && echo /usr/local/libexec/cni || echo /opt/cni/bin )}
case $(uname -m) in aarch64) ARCH=arm64 ;; x86_64) ARCH=amd64 ;; *) echo "unsupported arch"; exit 1 ;; esac

[ "$(id -u)" = 0 ] || { echo "run as root"; exit 1; }
# containerd's erofs snapshotter loads only where the host kernel has erofs.
modprobe erofs
mkdir -p "$E2E/bin" "$E2E/cni" "$E2E/store" "$E2E/pods" "$RUN"
chown "$KILN_USER" "$E2E/store"
install -m 0755 "$KILN_SHIM" "$E2E/bin/containerd-shim-kiln-v1"

cat > "$E2E/cni/10-kiln-e2e.conflist" <<'JSON'
{
  "cniVersion": "1.0.0",
  "name": "kiln-e2e",
  "plugins": [
    {
      "type": "bridge", "bridge": "kilne2e0", "isGateway": true, "ipMasq": true,
      "ipam": { "type": "host-local", "ranges": [[{ "subnet": "10.89.0.0/24" }]], "routes": [{ "dst": "0.0.0.0/0" }] }
    }
  ]
}
JSON

cat > "$E2E/shim.toml" <<TOML
user = "$KILN_USER"
store = "$E2E/store"
kernel = "$KILN_KERNEL"
init = "$KILN_INIT"
vmm = "$VMM"
default_memory_mib = 256
scratch_disk = "256M"
TOML

cat > "$E2E/config.toml" <<TOML
version = 3
root = "$E2E/root"
state = "$RUN/state"

[grpc]
  address = "$SOCK"
[ttrpc]
  address = "$SOCK.ttrpc"

[plugins.'io.containerd.cri.v1.images']
  snapshotter = 'erofs'

[plugins.'io.containerd.cri.v1.runtime'.containerd]
  default_runtime_name = 'kiln'
[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kiln]
  runtime_type = 'io.containerd.kiln.v1'
  snapshotter = 'erofs'
  sandboxer = 'podsandbox'
  [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kiln.options]
    ConfigPath = '$E2E/shim.toml'
[plugins.'io.containerd.cri.v1.runtime'.cni]
  bin_dirs = ['$CNI_BIN']
  conf_dir = '$E2E/cni'

[plugins.'io.containerd.grpc.v1.cri']
  stream_server_address = '127.0.0.1'
  stream_server_port = '0'

[plugins.'io.containerd.service.v1.diff-service']
  default = ['erofs', 'walking']
[[plugins.'io.containerd.transfer.v1.local'.unpack_config]]
  platform = 'linux/$ARCH'
  snapshotter = 'erofs'
  differ = 'erofs'
TOML

PATH="$E2E/bin:${VMM_PATH:+$VMM_PATH:}$PATH" VMKIT_SANDBOX="$VMKIT_SANDBOX" \
  containerd --config "$E2E/config.toml" >"$E2E/containerd.log" 2>&1 &
CONTAINERD=$!
stop_all() {
  local rc=$?
  if [ "${KEEP:-0}" != 1 ]; then
    cri rmp -fa >/dev/null 2>&1 || true
    kill "$CONTAINERD" 2>/dev/null || true
    wait "$CONTAINERD" 2>/dev/null || true
  fi
  exit $rc
}
trap stop_all EXIT
cat > "$E2E/crictl.yaml" <<YAML
runtime-endpoint: unix://$SOCK
image-endpoint: unix://$SOCK
timeout: 60
YAML
cri() { crictl --config "$E2E/crictl.yaml" "$@"; }
for _ in $(seq 50); do [ -S "$SOCK" ] && cri version >/dev/null 2>&1 && break; sleep 0.2; done
cri pull "$IMAGE" >/dev/null

FAILED=0
pass() { echo "PASS $1"; }
fail() { echo "FAIL $1: $2"; FAILED=1; }

pod() { # pod <name>: runs a pod sandbox, prints its id
  cat > "$E2E/pods/$1.json" <<JSON
{ "metadata": { "name": "$1", "namespace": "kiln-e2e", "uid": "$1-uid", "attempt": 1 },
  "hostname": "$1", "log_directory": "$E2E/pods/$1-logs", "linux": {} }
JSON
  cri runp "$E2E/pods/$1.json"
}
container() { # container <pod name> <pod id> <name> <json argv>: creates and starts it, prints its id
  cat > "$E2E/pods/$1-$3.json" <<JSON
{ "metadata": { "name": "$3" }, "image": { "image": "$IMAGE" }, "command": $4,
  "log_path": "$3.log", "linux": {} }
JSON
  local id
  id=$(cri create "$2" "$E2E/pods/$1-$3.json" "$E2E/pods/$1.json")
  cri start "$id" >/dev/null
  echo "$id"
}
exited() { # exited <container id> <seconds>: waits for it to exit, prints its exit code
  for _ in $(seq $(($2 * 5))); do
    if [ "$(cri inspect -o go-template --template '{{.status.state}}' "$1")" = CONTAINER_EXITED ]; then
      cri inspect -o go-template --template '{{.status.exitCode}}' "$1"
      return
    fi
    sleep 0.2
  done
  echo timeout
}

# 1. stdout reaches the CRI log, and exit 0 is reported.
P1=$(pod p1)
C=$(container p1 "$P1" echo '["echo", "hello from kiln"]')
code=$(exited "$C" 60)
if [ "$code" = 0 ] && cri logs "$C" 2>&1 | grep -q "hello from kiln"; then pass echo; else
  fail echo "exit $code, logs: $(cri logs "$C" 2>&1 | tail -5)"; fi

# 2. A second container in the same pod after the first exited (init containers), and its exit code.
C=$(container p1 "$P1" three '["sh", "-c", "echo to stderr >&2; exit 3"]')
code=$(exited "$C" 60)
if [ "$code" = 3 ] && cri logs "$C" 2>&1 | grep -q "to stderr"; then pass exit-code; else fail exit-code "exit $code"; fi

# 3. The guest owns the pod IP: the node reaches its HTTP server.
P2=$(pod p2)
container p2 "$P2" web '["httpd", "-f", "-p", "8080", "-h", "/etc"]' >/dev/null
IP=$(cri inspectp -o go-template --template '{{.status.network.ip}}' "$P2")
body=""
for _ in $(seq 100); do body=$(curl -s --max-time 1 "http://$IP:8080/hostname" || true); [ -n "$body" ] && break; sleep 0.3; done
if [ "$body" = p2 ]; then pass pod-ip; else fail pod-ip "GET http://$IP:8080/hostname gave ${body:-nothing}"; fi

# 4. From the guest: cloud metadata and the node are unreachable; another pod is reachable.
NODE=$(ip -4 -j route get 1.1.1.1 | sed -n 's/.*"prefsrc":"\([0-9.]*\)".*/\1/p')
P3=$(pod p3)
C=$(container p3 "$P3" probe "[\"sh\", \"-c\", \"for t in 169.254.169.254:80 $NODE:22 $IP:8080; do if nc -w 2 \${t%:*} \${t#*:} </dev/null; then echo \$t open; else echo \$t closed; fi; done\"]")
exited "$C" 60 >/dev/null
out=$(cri logs "$C" 2>&1)
if echo "$out" | grep -q "169.254.169.254:80 closed" && echo "$out" | grep -q "$NODE:22 closed" \
  && echo "$out" | grep -q "$IP:8080 open"; then pass deny-list; else fail deny-list "$(echo "$out" | tr '\n' ' ')"; fi

# 5. Stop: SIGTERM reaches the guest; a process that ignores it is killed after the timeout.
P4=$(pod p4)
C=$(container p4 "$P4" stubborn '["sh", "-c", "trap \"\" TERM; echo ready; sleep 600"]')
for _ in $(seq 100); do cri logs "$C" 2>&1 | grep -q ready && break; sleep 0.3; done
cri stop -t 3 "$C" >/dev/null
code=$(exited "$C" 30)
if [ "$code" = 137 ]; then pass stop; else fail stop "exit $code"; fi

# 6. A second container running at the same time in one pod is refused. CRI creates the
# shim's task when the container starts, so the refusal comes from `crictl start`.
P5=$(pod p5)
container p5 "$P5" first '["sleep", "600"]' >/dev/null
cat > "$E2E/pods/p5-second.json" <<JSON
{ "metadata": { "name": "second" }, "image": { "image": "$IMAGE" }, "command": ["true"], "log_path": "second.log", "linux": {} }
JSON
C=$(cri create "$P5" "$E2E/pods/p5-second.json" "$E2E/pods/p5.json")
if err=$(cri start "$C" 2>&1); then
  fail one-at-a-time "started: $err"
elif echo "$err" | grep -q "one container per pod at a time"; then pass one-at-a-time; else fail one-at-a-time "$err"; fi

exit $FAILED
