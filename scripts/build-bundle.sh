#!/usr/bin/env bash
# Separate feature sets matter: the maintenance/root/stdio programs must load
# even when the host has no GUI libraries. Cargo features unify within one build.
set -euo pipefail
cargo build --locked --release --bin trustd-user
case "${RUNNER_OS:-$(uname -s)}" in
  Windows|MINGW*|MSYS*|CYGWIN*) privileged=trustd-system ;;
  *) privileged=trustd-root ;;
esac
cargo build --locked --release --no-default-features \
  --bin trustd --bin trustd-user-headless --bin trustd-stdio --bin "$privileged"
