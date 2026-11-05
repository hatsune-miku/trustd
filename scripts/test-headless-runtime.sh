#!/usr/bin/env bash
# Run the actual bundle in a minimal runtime, not the GUI-equipped build runner.
set -euo pipefail
bundle=$(realpath "${1:?bundle directory required}")
smoke=$(realpath "$(dirname "$0")/headless_smoke.py")
docker run --rm --init -i \
  -v "$bundle:/bundle:ro" -v "$smoke:/smoke.py:ro" \
  ubuntu:24.04 sh -ec '
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends python3 libssl3t64 ca-certificates procps >/dev/null
    python3 /smoke.py /bundle
  '
