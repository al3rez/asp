#!/usr/bin/env bash
set -euo pipefail

# Short abrupt-restart drill. It deliberately SIGKILLs aspd while a detached
# process is producing output, restarts the daemon several times, and checks
# that the durable process/log state contains each marker exactly once. This
# exercises the failure path that a graceful persistence smoke cannot cover.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_RECONNECT_CHAOS_PORT:-4563}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-reconnect-chaos.XXXXXX")
session_file="$workspace/client-session.json"
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

start_daemon() {
  local log_file=$1
  "$aspd_bin" \
    --listen "127.0.0.1:$port" \
    --root "$workspace" \
    --cert "$workspace/.asp/server-cert.der" \
    --key "$workspace/.asp/server-key.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    >"$log_file" 2>&1 &
  daemon_pid=$!

  local ready=0
  for _ in $(seq 1 120); do
    if "$asp_bin" \
        --cert "$workspace/.asp/server-cert.der" \
        --auth-token-file "$workspace/.asp/auth-token" \
        --session-file "$session_file" \
        doctor "127.0.0.1:$port" >/dev/null 2>&1; then
      ready=1
      break
    fi
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
      break
    fi
    sleep 0.05
  done
  if [[ "$ready" != 1 ]]; then
    cat "$log_file" >&2
    echo "ASP reconnect-chaos daemon did not become ready" >&2
    exit 1
  fi
}

start_daemon "$workspace/aspd-0.log"

process_id=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  spawn "127.0.0.1:$port" \
  'i=1; while [ "$i" -le 40 ]; do printf "chaos-%02d\n" "$i"; sleep 0.2; i=$((i+1)); done')
test -n "$process_id"

for restart in $(seq 1 3); do
  sleep 0.65
  kill -KILL "$daemon_pid" 2>/dev/null || true
  wait "$daemon_pid" 2>/dev/null || true
  daemon_pid=""
  start_daemon "$workspace/aspd-$restart.log"
done

status=""
for _ in $(seq 1 120); do
  status=$("$asp_bin" \
    --cert "$workspace/.asp/server-cert.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    --session-file "$session_file" \
    status "127.0.0.1:$port" "$process_id")
  if [[ "$status" == *'"running": false'* ]]; then
    break
  fi
  sleep 0.1
done
if [[ "$status" != *'"running": false'* ]]; then
  echo "chaos process did not finish after repeated daemon kills: $status" >&2
  exit 1
fi

output=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  logs "127.0.0.1:$port" "$process_id" --offset 0)
for marker in $(seq -w 1 40); do
  grep -q "chaos-$marker" <<<"$output"
done
marker_count=$(grep -o 'chaos-' <<<"$output" | wc -l | tr -d ' ')
if [[ "$marker_count" != 40 ]]; then
  echo "expected 40 durable output markers, got $marker_count" >&2
  exit 1
fi

printf 'ASP reconnect-chaos smoke passed (process=%s, restarts=3)\n' "$process_id"
