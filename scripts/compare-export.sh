#!/usr/bin/env bash
# Converts a local Docker image with kiln and checks that the overlay of its erofs
# layers, mounted by the Linux kernel, matches `docker export` of the same image.
# Needs Docker (Docker Desktop works) and a built `target/release/kiln`.
# Usage: scripts/compare-export.sh IMAGE [PLATFORM] [MAX_LAYERS]
set -euo pipefail
image="$1"
platform="${2:-linux/arm64}"
max_layers="${3:-10}"
root=$(cd "$(dirname "$0")/.." && pwd)
kiln="$root/target/release/kiln"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

docker save --platform "$platform" "$image" -o "$work/image.tar"
"$kiln" --store "$work/store" convert --platform "$platform" --max-layers "$max_layers" --tag cmp:1 "$work/image.tar"
mkdir "$work/layers"
"$kiln" --store "$work/store" inspect --json cmp:1 \
  | python3 -c 'import json,sys; [print(l["digest"].split(":")[1]) for l in json.load(sys.stdin)["images"][0]["manifest"]["layers"]]' \
  | nl -v0 -nrz -w3 | while read -r i hex; do cp "$work/store/blobs/sha256/$hex" "$work/layers/$i.erofs"; done
cid=$(docker create --platform "$platform" "$image")
docker export "$cid" -o "$work/ref.tar"
docker rm "$cid" >/dev/null
cp "$root/scripts/compare_tree.py" "$work/"

docker run --rm --privileged --platform "$platform" -v "$work":/w debian:trixie-slim bash -euc '
  apt-get -qq update >/dev/null
  DEBIAN_FRONTEND=noninteractive apt-get -qq install -y python3 >/dev/null 2>&1
  for i in $(seq 0 63); do [ -e /dev/loop$i ] || mknod /dev/loop$i b 7 $i; done
  mkdir -p /ref /merged
  tar --numeric-owner --xattrs --xattrs-include="*" -xpf /w/ref.tar -C /ref
  lower=""
  for f in $(ls /w/layers | sort); do
    mkdir -p /l/$f && mount -t erofs -o ro,loop /w/layers/$f /l/$f
    lower="/l/$f${lower:+:$lower}"
  done
  # The fsconfig API rejects long lowerdir strings (many layers); use mount(2).
  LIBMOUNT_FORCE_MOUNT2=always mount -t overlay overlay -o "lowerdir=$lower,xino=on,redirect_dir=off,index=off,metacopy=off" /merged
  python3 /w/compare_tree.py /ref /merged
'
