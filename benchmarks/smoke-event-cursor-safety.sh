#!/usr/bin/env bash
set -euo pipefail

# Regression smoke for the durable event-cursor contract. EXEC and SPAWN
# responses expose only a filtered subset of the event journal; consuming those
# responses must not make a later full RESUME skip unrelated events.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_EVENT_CURSOR_SAFETY_PORT:-4559}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-event-cursor.XXXXXX")
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

"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

ready=0
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
  echo "ASP event-cursor smoke daemon did not become ready" >&2
  exit 1
fi

server="127.0.0.1:$port"
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  connect "$server" >/dev/null

# Neither filtered response advances the durable cursor. Leave enough time for
# the detached process to persist its output before the full replay.
spawn_pid=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  spawn "$server" "printf spawn-cursor-marker; sleep 1")
test -n "$spawn_pid"
sleep 0.25

exec_output=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  exec "$server" "printf exec-cursor-marker")
[[ "$exec_output" == *exec-cursor-marker* ]]

# The saved durable cursor is intentionally still at the OPEN_SESSION boundary
# after the filtered SPAWN/EXEC attachments. A full RESUME must therefore
# reconstruct both outputs, including the detached process event sequence.
resume_output=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  resume "$server" 2>&1)
[[ "$resume_output" == *spawn-cursor-marker* ]]
[[ "$resume_output" == *exec-cursor-marker* ]]

printf 'ASP event cursor safety smoke passed (spawn=%s)\n' "$spawn_pid"
