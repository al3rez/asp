#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for the operator-supplied EXEC/SPAWN/PTY process boundary.
# The fixture wrapper intentionally does no isolation; it only proves ASP
# passes the final shell command through an absolute executable and preserves
# normal output/process identity semantics. Real deployments should substitute
# a reviewed bwrap/supervisor wrapper and enable --require-process-launcher.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_PROCESS_LAUNCHER_SMOKE_PORT:-4550}
health_port=${ASP_PROCESS_LAUNCHER_SMOKE_HEALTH_PORT:-9450}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-process-launcher.XXXXXX")
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

launcher="$workspace/launcher.sh"
launcher_log="$workspace/launcher.log"
export ASP_PROCESS_LAUNCHER_LOG="$launcher_log"
printf '%s\n' \
  '#!/bin/sh' \
  'set -eu' \
  'if [ -n "${ASP_PROCESS_LAUNCHER_LOG:-}" ]; then printf "%s\\n" "$1" >>"$ASP_PROCESS_LAUNCHER_LOG"; fi' \
  'exec "$@"' >"$launcher"
chmod 700 "$launcher"

"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --health-listen "127.0.0.1:$health_port" \
  --process-launcher "$launcher" \
  --require-process-launcher \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

ready=0
for _ in $(seq 1 100); do
  if "$asp_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$workspace/client-session.json" \
      doctor "127.0.0.1:$port" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd.log" >&2
  echo "ASP process-launcher smoke daemon did not become ready" >&2
  exit 1
fi

exec_output=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$workspace/client-session.json" \
  exec "127.0.0.1:$port" "printf launcher-exec-ok")
grep -q 'launcher-exec-ok' <<<"$exec_output"
initial_metrics=$(curl -fsS "http://127.0.0.1:$health_port/metrics")
launch_count=$(printf '%s\n' "$initial_metrics" | awk '$1 == "asp_process_launch_duration_us_count" { print $2 }')
launch_failures=$(printf '%s\n' "$initial_metrics" | awk '$1 == "asp_process_launch_failures_total" { print $2 }')
test "${launch_count:-0}" -ge 1
test "${launch_failures:-1}" -eq 0

# PTY creation must use the same reviewed boundary as EXEC/SPAWN. Feed the
# shell input in two writes so the client receives the detach byte separately
# from the command; this works without requiring the smoke itself to own a
# terminal. The PTY integration test is skipped only when tmux is unavailable.
tmux_available=0
for candidate in /usr/bin/tmux /bin/tmux /usr/local/bin/tmux /opt/homebrew/bin/tmux; do
  if [[ -x "$candidate" ]]; then
    tmux_available=1
    break
  fi
done
if [[ "${ASP_TMUX_PATH:-}" = /* && -x "${ASP_TMUX_PATH}" ]]; then
  tmux_available=1
fi
if [[ "$tmux_available" == 1 ]]; then
  run_shell_with_deadline() {
    if command -v timeout >/dev/null 2>&1; then
      timeout 10 "$@"
    else
      "$@"
    fi
  }
  set +e
  {
    # Build the marker from two fragments so a terminal input echo cannot
    # satisfy the output assertion below.
    printf "printf 'launcher-'\$(printf 'pty-ok')\\n"
    # tmux may need a short scheduling window to start the shell and execute
    # the first command on a busy CI host.  Keep this bounded; the outer
    # timeout (when available) still caps the complete PTY smoke.
    sleep 1
    printf '\035'
  } | run_shell_with_deadline "$asp_bin" \
    --cert "$workspace/.asp/server-cert.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    --session-file "$workspace/client-session.json" \
    shell "127.0.0.1:$port" \
    >"$workspace/pty-shell.out" 2>"$workspace/pty-shell.err"
  pty_status=$?
  set -e
  if [[ "$pty_status" -ne 0 ]]; then
    cat "$workspace/pty-shell.err" >&2
    echo "ASP process-launcher PTY smoke failed with status $pty_status" >&2
    exit 1
  fi
  pty_output_ready=0
  for _ in $(seq 1 40); do
    if grep -q 'launcher-pty-ok' "$workspace/pty-shell.out"; then
      pty_output_ready=1
      break
    fi
    sleep 0.05
  done
  if [[ "$pty_output_ready" != 1 ]]; then
    printf 'PTY output did not contain the launcher marker:\n' >&2
    sed -n '1,120p' "$workspace/pty-shell.out" >&2 || true
    sed -n '1,120p' "$workspace/pty-shell.err" >&2 || true
    exit 1
  fi
  tmux_path=""
  for _ in $(seq 1 40); do
    tmux_path=$(grep -E '/tmux$' "$launcher_log" | tail -1 || true)
    if [[ -n "$tmux_path" ]]; then
      break
    fi
    sleep 0.05
  done
  test -n "$tmux_path"
  if command -v jq >/dev/null 2>&1; then
    pty_session_id=$(jq -r --arg server "127.0.0.1:$port" '.servers[$server].session_id // empty' "$workspace/client-session.json")
    if [[ -n "$pty_session_id" ]]; then
      "$tmux_path" kill-session -t "asp-$pty_session_id" 2>/dev/null || true
    fi
  fi
fi

process_id=$("$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$workspace/client-session.json" \
  spawn "127.0.0.1:$port" "sleep 8; printf launcher-spawn-ok")
test -n "$process_id"

# Restart the daemon while the launcher-owned process is still alive. This
# exercises the same persisted PID/wrapper identity check used by a real
# deployment, not only the initial spawn path.
kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""

"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --health-listen "127.0.0.1:$health_port" \
  --process-launcher "$launcher" \
  --require-process-launcher \
  >"$workspace/aspd-restarted.log" 2>&1 &
daemon_pid=$!

ready=0
for _ in $(seq 1 100); do
  if "$asp_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$workspace/client-session.json" \
      doctor "127.0.0.1:$port" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd-restarted.log" >&2
  echo "ASP process-launcher restart smoke daemon did not become ready" >&2
  exit 1
fi

sleep 0.2
logs_output=""
for _ in $(seq 1 120); do
  logs_output=$("$asp_bin" \
    --cert "$workspace/.asp/server-cert.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    --session-file "$workspace/client-session.json" \
    logs "127.0.0.1:$port" "$process_id" --stream stdout) || true
  if grep -q 'launcher-spawn-ok' <<<"$logs_output"; then
    break
  fi
  sleep 0.1
done
grep -q 'launcher-spawn-ok' <<<"$logs_output"
metrics=$(curl -fsS "http://127.0.0.1:$health_port/metrics")
printf '%s\n' "$metrics" | grep -Eq '^asp_process_launcher_configured 1$'
printf '%s\n' "$metrics" | grep -Eq '^asp_process_launcher_required 1$'
printf '%s\n' "$metrics" | grep -Eq '^asp_process_launcher_healthy 1$'

# Readiness must also surface an operator replacing the reviewed executable;
# this is what lets a supervisor stop routing new work before a process
# request discovers the drift itself.
drifted_launcher="$workspace/launcher.drifted"
printf '%s\n' '#!/bin/sh' 'exit 99' >"$drifted_launcher"
chmod 700 "$drifted_launcher"
mv "$drifted_launcher" "$launcher"
ready_status=$(curl -sS -o "$workspace/ready-drift.json" -w '%{http_code}' \
  "http://127.0.0.1:$health_port/ready")
test "$ready_status" = 503
grep -q '"ready":false' "$workspace/ready-drift.json"
grep -q '"process_launcher_healthy":false' "$workspace/ready-drift.json"

printf 'ASP process-launcher smoke passed\n'
