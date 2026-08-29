#!/usr/bin/env bash
set -euo pipefail

# Verify that two local consumers can follow one durable session without
# sharing a replay cursor. It also exercises the optional wire-level durable
# consumer ACK and verifies that the server persists its lease sidecar.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_CONSUMER_SMOKE_PORT:-4546}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-consumer-smoke.XXXXXX")
state_home=$(mktemp -d "${TMPDIR:-/tmp}/asp-consumer-state.XXXXXX")
session_file="$state_home/sessions.json"
daemon_pid=""
filtered_pid=""

cleanup() {
  if [[ -n "$filtered_pid" ]]; then
    kill -INT "$filtered_pid" 2>/dev/null || true
    wait "$filtered_pid" 2>/dev/null || true
  fi
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace" "$state_home"
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
  if XDG_STATE_HOME="$state_home" "$asp_bin" \
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
  echo "ASP consumer cursor smoke daemon did not become ready" >&2
  exit 1
fi

server="127.0.0.1:$port"
XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  connect "$server" >/dev/null

# Consumer A creates and resumes a process, advancing only its own cursor.
process_a=$(XDG_STATE_HOME="$state_home" "$asp_bin" \
  --consumer-id agent-a \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  spawn "$server" "printf consumer-a")
sleep 0.2
XDG_STATE_HOME="$state_home" "$asp_bin" \
  --consumer-id agent-a \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  resume "$server" >"$state_home/a-resume.out" 2>"$state_home/a-resume.err"

# A later process is attached through A, but A deliberately does not resume
# after its exit event. B bootstraps from the legacy cursor and must still see
# the complete retained history.
process_b=$(XDG_STATE_HOME="$state_home" "$asp_bin" \
  --consumer-id agent-a \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  spawn "$server" "printf consumer-b")
sleep 0.2
XDG_STATE_HOME="$state_home" "$asp_bin" \
  --consumer-id agent-b \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  resume "$server" >"$state_home/b-resume.out" 2>"$state_home/b-resume.err"
b_cursor=$(jq -r --arg server "$server" '.consumers[$server]["agent-b"].last_event_id' "$session_file")

# A process-filtered subscriber receives only A's lifecycle events, while the
# captured boundary also contains B's events. The optional catch-up marker
# must let it advance across those hidden IDs instead of holding compaction
# behind a lease that can never reach the journal head.
XDG_STATE_HOME="$state_home" "$asp_bin" \
  --consumer-id filtered \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  events "$server" --process-id "$process_a" --no-output \
  >"$state_home/filtered-events.out" 2>"$state_home/filtered-events.err" &
filtered_pid=$!
filtered_cursor=""
for _ in $(seq 1 100); do
  filtered_cursor=$(jq -r --arg server "$server" \
    '.consumers[$server]["filtered"].last_event_id // empty' \
    "$session_file" 2>/dev/null || true)
  if [[ "$filtered_cursor" =~ ^[0-9]+$ ]] && ((filtered_cursor >= b_cursor)); then
    break
  fi
  if ! kill -0 "$filtered_pid" 2>/dev/null; then
    cat "$state_home/filtered-events.err" >&2 || true
    echo "filtered event consumer exited before the backlog boundary" >&2
    exit 1
  fi
  sleep 0.05
done
if ! [[ "$filtered_cursor" =~ ^[0-9]+$ ]] || ((filtered_cursor < b_cursor)); then
  cat "$state_home/filtered-events.err" >&2 || true
  echo "filtered event consumer did not ACK the captured boundary" >&2
  exit 1
fi
kill -INT "$filtered_pid" 2>/dev/null || true
wait "$filtered_pid"
filtered_pid=""

test -n "$process_a"
test -n "$process_b"
grep -q consumer-a "$state_home/a-resume.out"
grep -q consumer-a "$state_home/b-resume.out"
grep -q consumer-b "$state_home/b-resume.out"

test -f "$session_file"
base_cursor=$(jq -r --arg server "$server" '.servers[$server].last_event_id' "$session_file")
a_cursor=$(jq -r --arg server "$server" '.consumers[$server]["agent-a"].last_event_id' "$session_file")
b_cursor=$(jq -r --arg server "$server" '.consumers[$server]["agent-b"].last_event_id' "$session_file")
filtered_cursor=$(jq -r --arg server "$server" '.consumers[$server]["filtered"].last_event_id' "$session_file")
test "$base_cursor" -ge 1
test "$a_cursor" -ge "$base_cursor"
test "$b_cursor" -gt "$base_cursor"
test "$b_cursor" -gt "$a_cursor"
test "$filtered_cursor" -ge "$b_cursor"

session_id=$(jq -r --arg server "$server" '.servers[$server].session_id' "$session_file")
test -f "$workspace/.asp/sessions/$session_id/event-consumers.bin"

printf 'ASP consumer cursor smoke passed (base=%s agent-a=%s agent-b=%s)\n' \
  "$base_cursor" "$a_cursor" "$b_cursor"
