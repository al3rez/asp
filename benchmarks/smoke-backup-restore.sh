#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for the checksummed backup/verify/restore lifecycle. It
# runs only against a private loopback daemon and a temporary workspace.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-backup-smoke.XXXXXX")
backup_parent=$(mktemp -d "${TMPDIR:-/tmp}/asp-backup-destination.XXXXXX")
backup="$backup_parent/state"
port=${ASP_BACKUP_SMOKE_PORT:-4547}
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace" "$backup_parent"
}
trap cleanup EXIT INT TERM

mkdir -p "$workspace/src"
printf 'backup smoke fixture\n' >"$workspace/src/fixture.txt"

"$aspd_bin" --listen "127.0.0.1:$port" --root "$workspace" --cert "$workspace/.asp/server-cert.der" --key "$workspace/.asp/server-key.der" --auth-token-file "$workspace/.asp/auth-token" >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

for _ in $(seq 1 100); do
  if "$asp_bin" --cert "$workspace/.asp/server-cert.der" --auth-token-file "$workspace/.asp/auth-token" --session-file "$workspace/session.json" doctor "127.0.0.1:$port" >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
"$asp_bin" --cert "$workspace/.asp/server-cert.der" --auth-token-file "$workspace/.asp/auth-token" --session-file "$workspace/session.json" doctor "127.0.0.1:$port" >/dev/null

# The backup/restore maintenance commands require the daemon lock, so stop
# the live process before invoking them.
kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""

"$aspd_bin" --root "$workspace" --backup-state "$backup" >/dev/null
"$aspd_bin" --root "$workspace" --verify-state "$backup" >/dev/null

# A payload not listed in the manifest must fail verification; the original
# backup remains intact and is verified again below.
printf 'tamper\n' >"$backup/state/tampered"
if "$aspd_bin" --root "$workspace" --verify-state "$backup" >/dev/null 2>&1; then
  echo "tampered backup unexpectedly verified" >&2
  exit 1
fi
rm "$backup/state/tampered"
"$aspd_bin" --root "$workspace" --verify-state "$backup" >/dev/null

"$aspd_bin" --root "$workspace" --restore-state "$backup" --force-restore >/dev/null
"$aspd_bin" --root "$workspace" --verify-state "$backup" >/dev/null
test -f "$workspace/.asp/server-cert.der"
test -f "$workspace/.asp/server-key.der"
test -f "$workspace/src/fixture.txt"

printf 'ASP backup/restore smoke passed\n'
