#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for filesystem changes made outside ASP. The daemon's
# native watcher should invalidate semantic caches and append one durable
# FILE_CHANGED event per observed state, even when the editor-style write
# causes multiple backend callbacks.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_FILE_EVENTS_PORT:-4556}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-file-events.XXXXXX")
state_home=$(mktemp -d "${TMPDIR:-/tmp}/asp-file-events-state.XXXXXX")
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
  "$aspd_bin" \
    --listen "127.0.0.1:$port" \
    --root "$workspace" \
    --cert "$workspace/.asp/server-cert.der" \
    --key "$workspace/.asp/server-key.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    >"$workspace/aspd.log" 2>&1 &
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
    cat "$workspace/aspd.log" >&2
    echo "ASP file-events smoke daemon did not become ready" >&2
    exit 1
  fi
}

wait_for_event() {
  local path=$1
  for _ in $(seq 1 200); do
    if grep -q 'file_changed' "$events_output" 2>/dev/null \
        && grep -q "$path" "$events_output" 2>/dev/null; then
      return 0
    fi
    if [[ -n "$events_pid" ]] && ! kill -0 "$events_pid" 2>/dev/null; then
      cat "$events_stderr" >&2 || true
      cat "$events_output" >&2 || true
      echo "ASP file event subscriber exited before event marker: $path" >&2
      return 1
    fi
    sleep 0.05
  done
  cat "$events_stderr" >&2 || true
  cat "$events_output" >&2 || true
  echo "ASP file event subscriber did not observe event marker: $path" >&2
  return 1
}

mkdir -p "$workspace/src"
printf 'initial\n' >"$workspace/src/external.txt"
start_daemon
server="127.0.0.1:$port"
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  connect "$server" >/dev/null

: >"$events_output"
: >"$events_stderr"
"$asp_bin" \
  --consumer-id file-events \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  events "$server" --no-output \
  >"$events_output" 2>"$events_stderr" &
events_pid=$!

# An editor-style atomic replacement often emits create/modify/rename bursts.
# One direct write is enough to exercise that backend behavior; the worker's
# digest/metadata observation must coalesce callbacks into one event.
sleep 0.20
printf 'changed once\n' >"$workspace/src/external.txt"
wait_for_event 'src/external.txt'
sleep 0.50
changed_count=$(grep -c '"file_changed"' "$events_output" || true)
if [[ "$changed_count" != 1 ]]; then
  cat "$events_output" >&2
  echo "expected one FILE_CHANGED event after one external write, got $changed_count" >&2
  exit 1
fi

rm "$workspace/src/external.txt"
for _ in $(seq 1 200); do
  if [[ "$(grep -c '"file_changed"' "$events_output" 2>/dev/null || true)" -ge 2 ]]; then
    break
  fi
  sleep 0.05
done
deleted_count=$(grep -c '"file_changed"' "$events_output" || true)
if [[ "$deleted_count" != 2 ]]; then
  cat "$events_output" >&2
  echo "expected one FILE_CHANGED event for deletion, got $deleted_count total" >&2
  exit 1
fi

# An ASP-owned atomic upload has its durable FILE_MUTATION event already. The
# following watcher callbacks must be suppressed by the observation seeded at
# the commit gate, so subscribers do not see a second FILE_CHANGED event.
printf 'owned by asp\n' >"$state_home/owned.txt"
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  put "$server" "$state_home/owned.txt" src/owned.txt >/dev/null
sleep 0.50
owned_count=$(grep -c '"file_changed"' "$events_output" || true)
if [[ "$owned_count" != 2 ]]; then
  cat "$events_output" >&2
  echo "ASP-owned upload generated an unexpected FILE_CHANGED duplicate (count=$owned_count)" >&2
  exit 1
fi

printf 'ASP external file-events smoke passed (events=%s)\n' "$owned_count"
