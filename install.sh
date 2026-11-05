#!/bin/sh
# Install or update through the local maintenance wizard.
set -eu

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) trustd_platform=linux-x86_64 ;;
  Darwin/arm64|Darwin/aarch64) trustd_platform=macos-aarch64 ;;
  *) printf '%s\n' 'No trustd release package is available for this operating system / architecture.' >&2; exit 1 ;;
esac
if [ "$(id -u)" = 0 ]; then
  printf '%s\n' 'Run this command as the intended login user. The wizard requests administrator access when needed.' >&2
  exit 1
fi
command -v curl >/dev/null 2>&1 || { printf '%s\n' 'curl is required.' >&2; exit 1; }
trustd_tmp=$(mktemp -d "${TMPDIR:-/tmp}/trustd-bootstrap.XXXXXXXX")
trap 'rm -rf -- "$trustd_tmp"' 0
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

trustd_fetch() {
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 --retry 2 "$1" -o "$2"
}
trustd_fetch 'https://api.github.com/repos/hatsune-miku/trustd/releases/latest' "$trustd_tmp/release.json"
trustd_tag=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$trustd_tmp/release.json")
case "$trustd_tag" in
  v[0-9]*) ;;
  *) printf '%s\n' 'The latest release has an invalid version tag.' >&2; exit 1 ;;
esac
case "$trustd_tag" in *[!A-Za-z0-9._-]*) printf '%s\n' 'Invalid release tag.' >&2; exit 1 ;; esac
trustd_url="https://github.com/hatsune-miku/trustd/releases/download/$trustd_tag"
trustd_archive="trustd-$trustd_platform.tar.gz"
printf 'Downloading trustd %s...\n' "$trustd_tag"
trustd_fetch "$trustd_url/$trustd_archive" "$trustd_tmp/$trustd_archive"
trustd_fetch "$trustd_url/SHA256SUMS" "$trustd_tmp/SHA256SUMS"
trustd_expected=$(awk -v asset="$trustd_archive" '$2 == asset {print $1}' "$trustd_tmp/SHA256SUMS")
case "$trustd_expected" in ''|*[!0-9a-fA-F]*) printf '%s\n' 'The release checksum is missing or ambiguous.' >&2; exit 1 ;; esac
[ "${#trustd_expected}" -eq 64 ] || { printf '%s\n' 'Invalid release checksum.' >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
  trustd_actual=$(sha256sum "$trustd_tmp/$trustd_archive" | awk '{print $1}')
else
  trustd_actual=$(shasum -a 256 "$trustd_tmp/$trustd_archive" | awk '{print $1}')
fi
[ "$trustd_actual" = "$trustd_expected" ] || { printf '%s\n' 'Release checksum verification failed.' >&2; exit 1; }
tar -xzf "$trustd_tmp/$trustd_archive" -C "$trustd_tmp"
trustd_bundle="$trustd_tmp/trustd-$trustd_platform"
for trustd_role in trustd trustd-root trustd-user trustd-stdio; do
  [ -f "$trustd_bundle/$trustd_role" ] || { printf '%s\n' 'The release package is incomplete.' >&2; exit 1; }
  chmod +x "$trustd_bundle/$trustd_role"
done
printf '%s\n' 'Starting the maintenance wizard. Temporary downloads are removed when it exits.'
"$trustd_bundle/trustd" maintenance "$@"
