#!/usr/bin/env bash
set -euo pipefail

# Verify an ASP binary archive before it is installed.  This is deliberately
# independent of Rust and can be run by a deployment host with only a POSIX
# shell, tar, and shasum/sha256sum.  It checks both the digest and the archive
# shape; a checksum alone cannot detect an operator selecting the wrong
# artifact or an archive containing an unsafe path.

# Keep an untrusted download from consuming unbounded verifier/extraction
# resources. The current release archives are only a few megabytes; these
# limits leave ample room for a future larger bundle while bounding the
# compressed input and number of filesystem entries before extraction.
MAX_ARCHIVE_BYTES=$((512 * 1024 * 1024))
MAX_ARCHIVE_ENTRIES=4096
MAX_CHECKSUM_BYTES=16384

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 RELEASE.tar.gz [RELEASE.sha256]" >&2
  exit 2
fi

archive=$1
reported_archive=$archive
if [[ $# == 2 ]]; then
  checksum=$2
else
  checksum="${archive%.tar.gz}.sha256"
fi

for path in "$archive" "$checksum"; do
  if [[ ! -f "$path" || -L "$path" ]]; then
    echo "release input must be a regular non-symlink file: $path" >&2
    exit 2
  fi
done

archive_dir=$(cd -- "$(dirname -- "$archive")" && pwd)
archive_name=$(basename -- "$archive")
checksum_dir=$(cd -- "$(dirname -- "$checksum")" && pwd)
checksum_name=$(basename -- "$checksum")

# Keep the verifier's input name bounded before passing it to tar.  GNU tar
# and BSD tar differ in how they handle an archive argument beginning with a
# dash; accepting such a name would let an untrusted path be interpreted as
# an option (and would make the standalone verifier weaker than the installer,
# which already enforces this release naming contract).
if [[ ! "$archive_name" =~ ^asp-[A-Za-z0-9._-]+\.tar\.gz$ ]]; then
  echo "release archive name must match asp-*.tar.gz: $archive_name" >&2
  exit 2
fi

expected_checksum_name="${archive_name%.tar.gz}.sha256"
if [[ "$checksum_name" != "$expected_checksum_name" ]]; then
  echo "release checksum name must match $expected_checksum_name: $checksum_name" >&2
  exit 2
fi

if [[ "$archive_dir" != "$checksum_dir" ]]; then
  echo "archive and checksum must be in the same directory" >&2
  exit 2
fi

# Take a bounded private snapshot before any tar listing or extraction.  The
# standalone verifier is often run directly on a downloaded pathname; without
# this copy, replacing that pathname between `tar -tzf` and `tar -xzf` could
# make the verifier validate one archive and extract another.  The installer
# already supplies a snapshot, but keeping this boundary here protects direct
# verifier invocations as well.
snapshot_dir=$(mktemp -d "${TMPDIR:-/tmp}/asp-release-snapshot.XXXXXX")
chmod 700 "$snapshot_dir"
extract_dir=''
cleanup() {
  if [[ -n "${extract_dir:-}" ]]; then
    rm -rf -- "$extract_dir"
  fi
  rm -rf -- "$snapshot_dir"
}
trap cleanup EXIT INT TERM

copy_bounded() {
  local source=$1
  local destination=$2
  local limit=$3
  local temporary
  local size
  temporary=$(mktemp "$snapshot_dir/.copy.XXXXXX")
  # Read at most limit+1 bytes.  The extra byte lets us reject a source that
  # grows while it is being copied instead of allowing an unbounded read.
  if ! head -c "$((limit + 1))" -- "$source" >"$temporary"; then
    rm -f -- "$temporary"
    echo "could not snapshot release input: $source" >&2
    exit 1
  fi
  size=$(wc -c <"$temporary" | tr -d '[:space:]')
  if [[ ! "$size" =~ ^[0-9]+$ ]] || ((size > limit)); then
    rm -f -- "$temporary"
    echo "release input exceeds its bounded snapshot limit: $source" >&2
    exit 1
  fi
  chmod 0444 "$temporary"
  mv -- "$temporary" "$destination"
}

stable_archive="$snapshot_dir/$archive_name"
stable_checksum="$snapshot_dir/$checksum_name"
copy_bounded "$archive" "$stable_archive" "$MAX_ARCHIVE_BYTES"
copy_bounded "$checksum" "$stable_checksum" "$MAX_CHECKSUM_BYTES"
archive="$stable_archive"
checksum="$stable_checksum"
archive_dir="$snapshot_dir"
checksum_dir="$snapshot_dir"

archive_bytes=$(wc -c <"$archive" | tr -d '[:space:]')
if [[ ! "$archive_bytes" =~ ^[0-9]+$ ]]; then
  echo "could not determine release archive size: $archive" >&2
  exit 1
fi
if ((archive_bytes > MAX_ARCHIVE_BYTES)); then
  echo "release archive exceeds ${MAX_ARCHIVE_BYTES}-byte compressed limit: $archive" >&2
  exit 1
fi
checksum_bytes=$(wc -c <"$checksum" | tr -d '[:space:]')
if [[ ! "$checksum_bytes" =~ ^[0-9]+$ ]] || ((checksum_bytes > MAX_CHECKSUM_BYTES)); then
  echo "release checksum exceeds ${MAX_CHECKSUM_BYTES}-byte limit: $checksum" >&2
  exit 1
fi

# Do not hand an untrusted sidecar to `*sum -c`: its records can name files
# other than the archive and would turn verification into an arbitrary local
# file probe. Accept exactly one conventional checksum record, then hash the
# already-validated archive directly.
expected_digest=$(awk -v expected="$archive_name" '
  BEGIN { records = 0; valid = 1 }
  /^[[:space:]]*$/ { valid = 0; next }
  {
    records++
    if (NF != 2) {
      valid = 0
      next
    }
    digest = $1
    filename = $2
    if (substr(filename, 1, 1) == "*") {
      filename = substr(filename, 2)
    }
    if (length(digest) != 64 || digest !~ /^[[:xdigit:]]+$/ || filename != expected) {
      valid = 0
    } else {
      print tolower(digest)
    }
  }
  END {
    if (records != 1 || !valid) {
      exit 1
    }
  }
' "$checksum") || {
  echo "release checksum must contain exactly one SHA-256 record for $archive_name" >&2
  exit 1
}

if command -v shasum >/dev/null 2>&1; then
  actual_digest=$(cd -- "$archive_dir" && shasum -a 256 -- "$archive_name" | awk '{ print tolower($1) }')
elif command -v sha256sum >/dev/null 2>&1; then
  actual_digest=$(cd -- "$archive_dir" && sha256sum -- "$archive_name" | awk '{ print tolower($1) }')
else
  echo "neither shasum nor sha256sum is available" >&2
  exit 2
fi
if [[ "$actual_digest" != "$expected_digest" ]]; then
  echo "release checksum mismatch: $archive_name" >&2
  exit 1
fi

entries=$(tar -tzf "$archive")
if [[ -z "$entries" ]]; then
  echo "release archive is empty: $archive" >&2
  exit 1
fi
entry_count=$(printf '%s\n' "$entries" | wc -l | tr -d '[:space:]')
if [[ ! "$entry_count" =~ ^[0-9]+$ ]] || ((entry_count > MAX_ARCHIVE_ENTRIES)); then
  echo "release archive exceeds ${MAX_ARCHIVE_ENTRIES}-entry limit: $archive" >&2
  exit 1
fi
duplicates=$(printf '%s\n' "$entries" | LC_ALL=C sort | uniq -d)
if [[ -n "$duplicates" ]]; then
  echo "release archive contains duplicate entries:" >&2
  printf '%s\n' "$duplicates" >&2
  exit 1
fi

# Reject absolute names, parent traversal, and non-regular tar entries before
# extraction.  The generated archive contains only regular files/directories;
# rejecting links, FIFOs, devices, and sockets keeps this verifier safe for an
# untrusted downloaded file.
if printf '%s\n' "$entries" | grep -Eq '(^|/)\.\.(/|$)|^/'; then
  echo "release archive contains an unsafe path" >&2
  exit 1
fi
if tar -tvzf "$archive" | awk 'substr($0, 1, 1) != "d" && substr($0, 1, 1) != "-" { found = 1 } END { exit(found ? 0 : 1) }'; then
  echo "release archive must contain only regular files and directories" >&2
  exit 1
fi

require_entry() {
  local entry=$1
  # Do not use grep -q here: with `set -o pipefail`, grep's early exit can
  # make the printf producer report SIGPIPE and turn a present entry into a
  # false missing-entry failure.  Consume the complete list instead.
  if ! printf '%s\n' "$entries" | grep -Fx -- "$entry" >/dev/null; then
    echo "release archive is missing $entry" >&2
    exit 1
  fi
}

require_entry './bin/asp'
require_entry './bin/aspd'
require_entry './README.md'
require_entry './LICENSE-MIT'
require_entry './LICENSE-APACHE'
require_entry './Cargo.lock'
require_entry './SBOM.spdx.json'
require_entry './docs/schema.json'
require_entry './docs/ARCHITECTURE.md'
require_entry './docs/PROTOCOL.md'
require_entry './docs/PRODUCTION_READINESS.md'
require_entry './docs/PROBLEM.md'
require_entry './docs/EVENT_MODEL.md'
require_entry './docs/SCHEMA.md'
require_entry './docs/BENCHMARKS.md'
require_entry './deploy/systemd/aspd.service'
require_entry './deploy/systemd/aspd-production.service'
require_entry './deploy/systemd/README.md'
require_entry './deploy/launchd/com.asp.aspd.plist'
require_entry './deploy/launchd/com.asp.aspd-production.plist'
require_entry './deploy/launchd/README.md'
require_entry './deploy/container/Dockerfile'
require_entry './deploy/container/README.md'
require_entry './deploy/container/asp-worker-wrapper'
require_entry './deploy/verify-release.sh'
require_entry './deploy/install-release.sh'
require_entry './deploy/upgrade-release.sh'
require_entry './deploy/bootstrap-client.sh'
require_entry './deploy/generate-sbom.sh'
require_entry './deploy/sign-release.sh'
require_entry './deploy/verify-release-signature.sh'

if printf '%s\n' "$entries" | grep -Eq '(^|/)\.asp(/|$)|(^|/)(auth-token|server-key)(\.|$)|(^|/).*\.(key|pem)$'; then
  echo "release archive unexpectedly contains credentials or durable state" >&2
  exit 1
fi

extract_dir=$(mktemp -d "${TMPDIR:-/tmp}/asp-release-verify.XXXXXX")
chmod 700 "$extract_dir"
tar -xzf "$archive" -C "$extract_dir"

for binary in asp aspd; do
  path="$extract_dir/bin/$binary"
  if [[ ! -f "$path" || -L "$path" || ! -x "$path" ]]; then
    echo "release binary is not a regular executable: $binary" >&2
    exit 1
  fi
done

for helper in verify-release install-release upgrade-release bootstrap-client generate-sbom sign-release verify-release-signature; do
  path="$extract_dir/deploy/$helper.sh"
  if [[ ! -f "$path" || -L "$path" || ! -x "$path" ]]; then
    echo "release deployment helper is not a regular executable: $helper.sh" >&2
    exit 1
  fi
done

worker_wrapper="$extract_dir/deploy/container/asp-worker-wrapper"
if [[ ! -f "$worker_wrapper" || -L "$worker_wrapper" || ! -x "$worker_wrapper" ]]; then
  echo "container worker wrapper is not a regular executable" >&2
  exit 1
fi

if command -v jq >/dev/null 2>&1; then
  jq empty "$extract_dir/docs/schema.json" >/dev/null
  jq -e '.spdxVersion == "SPDX-2.3" and (.packages | type == "array" and length > 0) and (.relationships | type == "array")' "$extract_dir/SBOM.spdx.json" >/dev/null
fi

printf 'ASP release verified: %s\n' "$reported_archive"
