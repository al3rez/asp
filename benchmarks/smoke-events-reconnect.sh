#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for a durable event subscriber crossing a daemon
# restart. The subscriber stays alive while only aspd is replaced; it must
# reconnect, resubscribe from its saved cursor, and observe events generated
# after the restart without duplicating or losing the pre-restart event.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_EVENTS_RECONNECT_PORT:-4555}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-events-reconnect.XXXXXX")
state_home=$(mktemp -d "${TMPDIR:-/tmp}/asp-events-reconnect-state.XXXXXX")
session_file="$state_home/session.json"
events_output="$state_home/events.jsonl"
events_stderr="$state_home/events.stderr"
daemon_pid=""
events_pid=""

cleanup() {
  if [[ -n "$events_pid" ]]; then
    kill -INT "$events_pid" 2>/dev/null || true
    wait "$events_pid" 2>/dev/null || true
  fi
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace" "$state_home"
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
    echo "ASP events-reconnect smoke daemon did not become ready" >&2
    exit 1
  fi
}

wait_for_event() {
  local marker=$1
  for _ in $(seq 1 200); do
    if grep -q -- "$marker" "$events_output" 2>/dev/null; then
      return 0
    fi
    if [[ -n "$events_pid" ]] && ! kill -0 "$events_pid" 2>/dev/null; then
      cat "$events_stderr" >&2 || true
      cat "$events_output" >&2 || true
      echo "ASP event subscriber exited before event marker: $marker" >&2
      return 1
    fi
    sleep 0.05
  done
  cat "$events_stderr" >&2 || true
  cat "$events_output" >&2 || true
  echo "ASP event subscriber did not observe event marker: $marker" >&2
  return 1
}

start_daemon "$workspace/aspd-0.log"
server="127.0.0.1:$port"
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  connect "$server" >/dev/null

: >"$events_output"
: >"$events_stderr"
"$asp_bin" \
  --consumer-id events-reconnect \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  events "$server" --no-output \
  >"$events_output" 2>"$events_stderr" &
events_pid=$!

before_pid=$("$asp_bin" \
  --consumer-id producer \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  spawn "$server" "printf event-before")
test -n "$before_pid"
wait_for_event event-before

# Replace only the daemon. The subscriber process and durable cursor remain
# alive while the QUIC connection is closed and the reconnect loop backs off.
kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
start_daemon "$workspace/aspd-1.log"

after_pid=$("$asp_bin" \
  --consumer-id producer \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  spawn "$server" "printf event-after")
test -n "$after_pid"
wait_for_event event-after

kill -INT "$events_pid" 2>/dev/null || true
wait "$events_pid" 2>/dev/null || true
events_pid=""

test "$(grep -o event-before "$events_output" | wc -l | tr -d ' ')" = 1
test "$(grep -o event-after "$events_output" | wc -l | tr -d ' ')" = 1

printf 'ASP events reconnect smoke passed (before=%s after=%s)\n' \
  "$before_pid" "$after_pid"
