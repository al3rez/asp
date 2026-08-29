#!/usr/bin/env bash
set -euo pipefail

# This smoke uses only a loopback socket and a private temporary workspace.
# It intentionally kills/restarts the ASP daemon, never an SSH/Tailscale or
# host networking service.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_SMOKE_PORT:-4543}
disconnect_wait_seconds=${ASP_PERSISTENCE_WAIT_SECONDS:-30}
process_runtime_seconds=$((disconnect_wait_seconds + 20))
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-persistence.XXXXXX")
session_file="$workspace/client-session.json"
recovery_session_file="$workspace/recovery-session.json"
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

ready=0
for _ in $(seq 1 100); do
  if "$asp_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$session_file" \
      doctor "127.0.0.1:$port" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd.log" >&2
  echo "ASP daemon did not become ready" >&2
  exit 1
fi

session_id=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  connect "127.0.0.1:$port")
if [[ ! "$session_id" =~ ^[0-9a-fA-F-]{36}$ ]]; then
  echo "connect did not return a durable session UUID: $session_id" >&2
  exit 1
fi

process_id=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  spawn "127.0.0.1:$port" "sleep $process_runtime_seconds; printf persistence-marker")

# The process is deliberately left running while only aspd is restarted.
kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
sleep 3

"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  >"$workspace/aspd-restarted.log" 2>&1 &
daemon_pid=$!

ready=0
for _ in $(seq 1 100); do
  if "$asp_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$session_file" \
      doctor "127.0.0.1:$port" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd-restarted.log" >&2
  echo "restarted ASP daemon did not become ready" >&2
  exit 1
fi

# A second client host may have no saved cursor at all. The explicit recovery
# form must authorize the durable UUID, replay from the requested cursor, and
# persist the selected session so ordinary point-in-time requests can continue
# without manually reconstructing the JSON cursor file.
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$recovery_session_file" \
  resume "127.0.0.1:$port" \
  --session-id "$session_id" \
  --after-event-id 0 > /dev/null 2>"$workspace/explicit-resume.log"
test -s "$recovery_session_file"

# Prove that a durable process remains alive through a full client/daemon
# outage and the default 30-second disconnect window. Override the wait only
# for a faster local smoke run; production qualification keeps the default.
sleep "$disconnect_wait_seconds"
status=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$recovery_session_file" \
  status "127.0.0.1:$port" "$process_id")
if [[ "$status" != *'"running": true'* ]]; then
  echo "persistent process was not still running after ${disconnect_wait_seconds}s: $status" >&2
  cat "$workspace/aspd-restarted.log" >&2
  exit 1
fi

output=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  logs "127.0.0.1:$port" "$process_id" --offset 0)
if [[ "$output" == *persistence-marker* ]]; then
  echo "persistent process completed before the disconnect window" >&2
  cat "$workspace/aspd-restarted.log" >&2
  exit 1
fi

sleep 21
output=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  logs "127.0.0.1:$port" "$process_id" --offset 0)
if [[ "$output" != *persistence-marker* ]]; then
  echo "persistent process output was not recovered after exit" >&2
  cat "$workspace/aspd-restarted.log" >&2
  exit 1
fi

printf 'ASP persistence smoke passed (process=%s)\n' "$process_id"
