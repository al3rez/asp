#!/usr/bin/env bash
set -euo pipefail

# Local operator lifecycle smoke. It inventories durable sessions, refuses to
# delete a session while a child is running, then deletes the quiescent session
# only after a daemon restart has reconstructed its terminal state.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_SESSION_ADMIN_PORT:-4564}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-session-admin.XXXXXX")
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
  for _ in $(seq 1 120); do
    if "$asp_bin" \
        --cert "$workspace/.asp/server-cert.der" \
        --auth-token-file "$workspace/.asp/auth-token" \
        --session-file "$session_file" \
        doctor "127.0.0.1:$port" >/dev/null 2>&1; then
      return
    fi
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
      break
    fi
    sleep 0.05
  done
  cat "$log_file" >&2
  echo "ASP session-admin daemon did not become ready" >&2
  exit 1
}

assert_rejected() {
  local expected=$1
  shift
  local output rc
  set +e
  output=$("$aspd_bin" "$@" 2>&1)
  rc=$?
  set -e
  if [[ "$rc" -eq 0 || "$output" != *"$expected"* ]]; then
    printf 'unexpected session-admin result (rc=%s):\n%s\n' "$rc" "$output" >&2
    exit 1
  fi
}

start_daemon "$workspace/aspd-0.log"
session_id=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  connect "127.0.0.1:$port")
process_id=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  spawn "127.0.0.1:$port" "sleep 3; printf session-admin-done")
test -n "$session_id" && test -n "$process_id"

# The daemon lock makes local inventory/delete unavailable while the service
# is running, preventing a second process from racing the active session map.
assert_rejected "another aspd instance owns" \
  --root "$workspace" --list-sessions

kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""

inventory=$("$aspd_bin" --root "$workspace" --list-sessions)
printf '%s\n' "$inventory" | jq -e --arg id "$session_id" \
  '.[] | select(.session_id == $id and .running_processes == 1)' >/dev/null
assert_rejected "running processes remain" \
  --root "$workspace" --delete-session "$session_id"

start_daemon "$workspace/aspd-1.log"
status=""
for _ in $(seq 1 100); do
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
  echo "session-admin process did not finish: $status" >&2
  exit 1
fi
kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""

"$aspd_bin" --root "$workspace" --delete-session "$session_id" >/dev/null
inventory=$("$aspd_bin" --root "$workspace" --list-sessions)
test "$(printf '%s' "$inventory" | jq 'length')" = 0

printf 'ASP session-admin smoke passed (session=%s)\n' "$session_id"
