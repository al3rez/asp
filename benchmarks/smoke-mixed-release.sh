#!/usr/bin/env bash
set -euo pipefail

# Exercise an independently built historical release against the current
# release in both directions. The transfer harness owns the daemon lifecycle:
# it pauses an in-flight FILE_PUT and artifact upload, replaces the daemon
# binary, and verifies byte-for-byte continuation. This wrapper adds archive
# verification/extraction and repeats the drill as old->new and new->old.
#
# A historical archive may predate ASP's current deployment helper, so the old
# input gets a strict checksum/path/link/binary validation fallback when the
# current verifier quite correctly rejects its older manifest. The new archive
# must pass the current verifier.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
old_archive=${1:-${ASP_OLD_RELEASE_ARCHIVE:-}}
new_archive=${2:-${ASP_NEW_RELEASE_ARCHIVE:-}}
old_checksum=${3:-${ASP_OLD_RELEASE_CHECKSUM:-}}
new_checksum=${4:-${ASP_NEW_RELEASE_CHECKSUM:-}}
if [[ -z "$old_archive" || -z "$new_archive" ]]; then
  cat >&2 <<'USAGE'
usage: smoke-mixed-release.sh OLD_RELEASE.tar.gz NEW_RELEASE.tar.gz \
  OLD_RELEASE.sha256 NEW_RELEASE.sha256

The new archive must pass the current release verifier. A historical archive
may use the bounded legacy validation path when it predates current packaging
helpers, but both archives must have an explicit SHA-256 sidecar. Set
ASP_OLD_RELEASE_* and ASP_NEW_RELEASE_* instead of arguments when desired.
USAGE
  exit 2
fi

# A mixed-release result is only useful when the two binaries are identified
# immutably.  The current verifier can infer a sidecar for a new archive, but
# the legacy fallback cannot safely do that for an old package whose naming
# convention is unknown.  Require both checksum paths explicitly so a caller
# cannot accidentally publish an unverified historical-compatibility result.
if [[ -z "$old_checksum" || -z "$new_checksum" ]]; then
  echo "both old and new release checksum files are required" >&2
  exit 2
fi

for archive in "$old_archive" "$new_archive"; do
  if [[ ! -f "$archive" || -L "$archive" ]]; then
    echo "release archive must be a regular non-symlink file: $archive" >&2
    exit 2
  fi
done
if [[ -n "$old_checksum" && ( ! -f "$old_checksum" || -L "$old_checksum" ) ]]; then
  echo "old release checksum must be a regular non-symlink file: $old_checksum" >&2
  exit 2
fi
if [[ -n "$new_checksum" && ( ! -f "$new_checksum" || -L "$new_checksum" ) ]]; then
  echo "new release checksum must be a regular non-symlink file: $new_checksum" >&2
  exit 2
fi

source_verifier=${ASP_RELEASE_VERIFIER:-"$repo_root/deploy/verify-release.sh"}
if [[ ! -x "$source_verifier" ]]; then
  echo "release verifier is missing or not executable: $source_verifier" >&2
  exit 2
fi

verify_checksum() {
  local archive=$1
  local checksum=$2
  local expected actual
  expected=$(awk 'NF {print $1; exit}' "$checksum")
  if [[ ! "$expected" =~ ^[[:xdigit:]]{64}$ ]]; then
    echo "release checksum does not contain a SHA-256 digest: $checksum" >&2
    return 1
  fi
  if command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 -- "$archive" | awk '{print $1}')
  else
    actual=$(sha256sum -- "$archive" | awk '{print $1}')
  fi
  [[ "$actual" == "$expected" ]] || {
    echo "release checksum mismatch: $archive" >&2
    return 1
  }
}

legacy_verify() {
  local archive=$1
  local entries
  entries=$(tar -tzf "$archive")
  [[ -n "$entries" ]] || {
    echo "historical release archive is empty: $archive" >&2
    return 1
  }
  # Reject absolute/parent paths, links, and private state before extraction.
  if printf '%s\n' "$entries" | grep -Eq '(^/|(^|/)\.\.(/|$)|(^|/)\.asp(/|$)|(^|/)(auth-token|server-key)(\.|$)|(^|/).*(\.(key|pem))$)'; then
    echo "historical release archive contains an unsafe/private path: $archive" >&2
    return 1
  fi
  if tar -tvzf "$archive" | awk 'substr($0, 1, 1) != "d" && substr($0, 1, 1) != "-" { found = 1 } END { exit(found ? 0 : 1) }'; then
    echo "historical release archive contains a special file or link: $archive" >&2
    return 1
  fi
  printf '%s\n' "$entries" | grep -Fqx './bin/asp'
  printf '%s\n' "$entries" | grep -Fqx './bin/aspd'
}

verify_checksum "$old_archive" "$old_checksum"
verify_checksum "$new_archive" "$new_checksum"
"$source_verifier" "$new_archive" "$new_checksum" >/dev/null

archive_digest() {
  local archive=$1
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -- "$archive" | awk '{print $1}'
  else
    sha256sum -- "$archive" | awk '{print $1}'
  fi
}

old_archive_digest=$(archive_digest "$old_archive")
new_archive_digest=$(archive_digest "$new_archive")
same_archive=false
if [[ "$old_archive_digest" == "$new_archive_digest" ]]; then
  same_archive=true
  if [[ "${ASP_MIXED_RELEASE_ALLOW_SAME_ARCHIVE:-0}" != 1 ]]; then
    echo "old and new archives have the same SHA-256; set ASP_MIXED_RELEASE_ALLOW_SAME_ARCHIVE=1 for mechanics-only testing" >&2
    exit 2
  fi
fi

# Prefer the strict verifier for a historical archive when it is compatible
# with the current manifest. If it predates current helper files, retain the
# bounded legacy checks above instead of pretending it is a current package.
set +e
"$source_verifier" "$old_archive" "$old_checksum" >/dev/null 2>&1
old_verifier_rc=$?
set -e
if [[ "$old_verifier_rc" != 0 ]]; then
  legacy_verify "$old_archive"
  echo "historical archive uses legacy manifest validation: $(basename "$old_archive")" >&2
fi

extract_dir=$(mktemp -d "${TMPDIR:-/tmp}/asp-mixed-release.XXXXXX")
cleanup() {
  rm -rf -- "$extract_dir"
}
trap cleanup EXIT INT TERM
old_root="$extract_dir/old"
new_root="$extract_dir/new"
mkdir -p -- "$old_root" "$new_root"
tar -xzf "$old_archive" -C "$old_root"
tar -xzf "$new_archive" -C "$new_root"

for pair in \
  "$old_root/bin/asp" "$old_root/bin/aspd" \
  "$new_root/bin/asp" "$new_root/bin/aspd"; do
  if [[ ! -f "$pair" || -L "$pair" || ! -x "$pair" ]]; then
    echo "mixed-release binary is missing or unsafe: $pair" >&2
    exit 1
  fi
done

pick_port() {
  python3 - <<'PY'
import socket
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}

old_max_protocol=${ASP_MIXED_RELEASE_OLD_MAX_PROTOCOL_VERSION:-}
new_max_protocol=${ASP_MIXED_RELEASE_NEW_MAX_PROTOCOL_VERSION:-}
for protocol_version in "$old_max_protocol" "$new_max_protocol"; do
  if [[ -n "$protocol_version" && "$protocol_version" != 16 && "$protocol_version" != 17 ]]; then
    echo "ASP_MIXED_RELEASE_*_MAX_PROTOCOL_VERSION must be 16, 17, or empty" >&2
    exit 2
  fi
done

run_direction() {
  local name=$1
  local initial_daemon=$2
  local restarted_daemon=$3
  local client=$4
  local port=$5
  local initial_max=$6
  local restarted_max=$7
  echo "running mixed-release direction: $name" >&2
  ASPD_INITIAL_BIN="$initial_daemon" \
  ASPD_RESTARTED_BIN="$restarted_daemon" \
  ASP_BIN="$client" \
  ASP_TRANSFER_RESTART_PORT="$port" \
  ASP_TRANSFER_RESTART_SIZE_MB="${ASP_MIXED_RELEASE_SIZE_MB:-8}" \
  ASP_TRANSFER_RESTART_INITIAL_MAX_PROTOCOL_VERSION="$initial_max" \
  ASP_TRANSFER_RESTART_RESTARTED_MAX_PROTOCOL_VERSION="$restarted_max" \
    bash "$repo_root/benchmarks/smoke-transfer-restart.sh"
}

run_direction old-to-new \
  "$old_root/bin/aspd" "$new_root/bin/aspd" "$new_root/bin/asp" \
  "${ASP_MIXED_RELEASE_FORWARD_PORT:-$(pick_port)}" \
  "$old_max_protocol" "$new_max_protocol"
run_direction new-to-old \
  "$new_root/bin/aspd" "$old_root/bin/aspd" "$old_root/bin/asp" \
  "${ASP_MIXED_RELEASE_REVERSE_PORT:-$(pick_port)}" \
  "$new_max_protocol" "$old_max_protocol"

run_exec_direction() {
  local name=$1
  local initial_daemon=$2
  local restarted_daemon=$3
  local client=$4
  local port=$5
  local health_port=$6
  local initial_max=$7
  local restarted_max=$8
  echo "running mixed-release EXEC direction: $name" >&2
  ASPD_INITIAL_BIN="$initial_daemon" \
  ASPD_RESTARTED_BIN="$restarted_daemon" \
  ASP_BIN="$client" \
  ASP_TIMEOUT_RESTART_SMOKE_PORT="$port" \
  ASP_TIMEOUT_RESTART_SMOKE_HEALTH_PORT="$health_port" \
  ASP_TIMEOUT_RESTART_INITIAL_MAX_PROTOCOL_VERSION="$initial_max" \
  ASP_TIMEOUT_RESTART_RESTARTED_MAX_PROTOCOL_VERSION="$restarted_max" \
    bash "$repo_root/benchmarks/smoke-exec-timeout-restart.sh"
}

run_exec_direction old-to-new \
  "$old_root/bin/aspd" "$new_root/bin/aspd" "$new_root/bin/asp" \
  "${ASP_MIXED_RELEASE_FORWARD_EXEC_PORT:-$(pick_port)}" \
  "${ASP_MIXED_RELEASE_FORWARD_EXEC_HEALTH_PORT:-$(pick_port)}" \
  "$old_max_protocol" "$new_max_protocol"
run_exec_direction new-to-old \
  "$new_root/bin/aspd" "$old_root/bin/aspd" "$old_root/bin/asp" \
  "${ASP_MIXED_RELEASE_REVERSE_EXEC_PORT:-$(pick_port)}" \
  "${ASP_MIXED_RELEASE_REVERSE_EXEC_HEALTH_PORT:-$(pick_port)}" \
  "$new_max_protocol" "$old_max_protocol"

printf '{"experiment":"mixed-release","status":"pass","same_archive":%s,"old_archive":"%s","new_archive":"%s","directions":["old-to-new","new-to-old"]}\n' \
  "$same_archive" "$(basename "$old_archive")" "$(basename "$new_archive")"
