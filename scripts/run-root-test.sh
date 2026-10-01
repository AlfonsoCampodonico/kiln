#!/usr/bin/env bash
# Builds one kiln-erofs integration test as the current user, then runs it as root.
# Usage: scripts/run-root-test.sh <test-name>
set -euo pipefail
test_name="$1"
bin=$(cargo test -p kiln-erofs --test "$test_name" --no-run --message-format=json \
  | jq -r --arg t "$test_name" 'select(.reason == "compiler-artifact" and .target.name == $t and .executable != null) | .executable' \
  | tail -1)
sudo --preserve-env=KILN_KERNEL_TESTS,KILN_ORACLE "$bin" --test-threads=1
