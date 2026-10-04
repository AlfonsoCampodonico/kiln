#!/usr/bin/env bash
# Regenerates crates/kiln-image/assets/ext4-template.img.zst (kiln spec §9.4), which
# kiln-image embeds, byte for byte: the Debian image is pinned by digest, e2fsprogs
# and zstd by version from a fixed snapshot.debian.org date, and the UUID, hash seed
# and timestamps are fixed.
# Needs Docker. Usage: assets/make-ext4-template.sh [out.img.zst]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$root/crates/kiln-image/assets/ext4-template.img.zst}
image=debian@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a # trixie-slim
snapshot=20261001T000000Z
e2fsprogs=1.47.2-3+b12
zstd=1.5.7+dfsg-1
# "kilnscratch" and "kilnhashseed" in hex.
uuid=6b696c6e-7363-7261-7463-680000000001
hash_seed=6b696c6e-6861-7368-7365-656400000001

mkdir -p "$(dirname "$out")"
trap 'rm -f "$out.tmp"' EXIT
docker run --rm -i "$image" sh -eu > "$out.tmp" <<SH
rm -f /etc/apt/sources.list.d/*
echo "deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/$snapshot trixie main" > /etc/apt/sources.list
apt-get update -qq >/dev/null
apt-get install -qq -y --no-install-recommends e2fsprogs=$e2fsprogs zstd=$zstd >/dev/null 2>&1
truncate -s 64M /tmp/scratch.img
# meta_bg without resize_inode lets the guest grow it online far beyond 64 MiB.
# lazy_itable_init=1 keeps the file sparse; the guest mounts with noinit_itable.
E2FSPROGS_FAKE_TIME=1 mke2fs -q -t ext4 -b 4096 -I 256 -i 65536 -O meta_bg,^resize_inode \
  -U $uuid -E hash_seed=$hash_seed,lazy_itable_init=1,lazy_journal_init=0,nodiscard,root_owner=0:0 \
  /tmp/scratch.img
# mke2fs takes the directory hash's signedness from the build host's char type
# (signed on x86_64, unsigned on aarch64); pin it to unsigned (s_flags = 2).
E2FSPROGS_FAKE_TIME=1 debugfs -w -R "ssv flags 2" /tmp/scratch.img >/dev/null 2>&1
e2fsck -fn /tmp/scratch.img >&2
zstd -q -19 -T1 -c /tmp/scratch.img
SH
mv "$out.tmp" "$out"
shasum -a 256 "$out" 2>/dev/null || sha256sum "$out"
