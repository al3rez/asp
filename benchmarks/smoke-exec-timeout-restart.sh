#!/usr/bin/env bash
set -euo pipefail

# Verify that an attached EXEC deadline is durable across an abrupt daemon
# loss. The child must survive the daemon crash, be recovered by the next
# daemon, and still terminate with ASP's timeout result.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
initial_aspd_bin=${ASPD_INITIAL_BIN:-"$aspd_bin"}
restarted_aspd_bin=${ASPD_RESTARTED_BIN:-"$aspd_bin"}
port=${ASP_TIMEOUT_RESTART_SMOKE_PORT:-4549}
health_port=${ASP_TIMEOUT_RESTART_SMOKE_HEALTH_PORT:-9449}
initial_max_protocol=${ASP_TIMEOUT_RESTART_INITIAL_MAX_PROTOCOL_VERSION:-}
restarted_max_protocol=${ASP_TIMEOUT_RESTART_RESTARTED_MAX_PROTOCOL_VERSION:-}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-exec-timeout-restart.XXXXXX")
session_file="$workspace/client-session.json"
daemon_pid=""
exec_pid=""

cleanup() {
  if [[ -n "$exec_pid" ]]; then
    kill "$exec_pid" 2>/dev/null || true
    wait "$exec_pid" 2>/dev/null || true
  fi
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

for executable in "$asp_bin" "$initial_aspd_bin" "$restarted_aspd_bin"; do
  if [[ ! -f "$executable" || -L "$executable" || ! -x "$executable" ]]; then
    echo "release binary is missing or unsafe: $executable" >&2
    exit 1
  fi
done
for protocol_version in "$initial_max_protocol" "$restarted_max_protocol"; do
  if [[ -n "$protocol_version" && "$protocol_version" != 16 && "$protocol_version" != 17 ]]; then
    echo "ASP_TIMEOUT_RESTART_*_MAX_PROTOCOL_VERSION must be 16, 17, or empty" >&2
    exit 1
  fi
done
for endpoint in "$port" "$health_port"; do
  if ! [[ "$endpoint" =~ ^[1-9][0-9]*$ ]] || ((endpoint > 65535)); then
    echo "ASP_TIMEOUT_RESTART_SMOKE_{PORT,HEALTH_PORT} must be integers from 1 to 65535" >&2
    exit 2
  fi
done
if [[ "$port" == "$health_port" ]]; then
  echo "ASP timeout-restart data and health ports must differ" >&2
  exit 2
fi

start_daemon() {
  local daemon_binary=${1:-$aspd_bin}
  local log_path=${2:?daemon log path is required}
  local max_protocol_version=${3:-}
  local protocol_args=()
  if [[ -n "$max_protocol_version" ]]; then
    protocol_args=(--max-protocol-version "$max_protocol_version")
  fi
  "$daemon_binary" \
    "${protocol_args[@]}" \
    --listen "127.0.0.1:$port" \
    --root "$workspace" \
    --cert "$workspace/.asp/server-cert.der" \
    --key "$workspace/.asp/server-key.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    --exec-timeout-seconds 3 \
    --health-listen "127.0.0.1:$health_port" \
    >"$log_path" 2>&1 &
  daemon_pid=$!
}

wait_ready() {
  local log_path=$1
  local ready=0
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
    cat "$log_path" >&2
    echo "ASP timeout-restart smoke daemon did not become ready" >&2
    exit 1
  fi
}

start_daemon "$initial_aspd_bin" "$workspace/aspd.log" "$initial_max_protocol"
wait_ready "$workspace/aspd.log"

"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  connect "127.0.0.1:$port" >/dev/null

# Keep the client attached so it exercises request retry/resume as well as the
# server's recovered-process monitor. Restart immediately after acceptance;
# the three-second deadline leaves enough room for the new daemon to bind.
set +e
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  exec "127.0.0.1:$port" "sleep 30" >"$workspace/exec.out" 2>"$workspace/exec.err" &
exec_pid=$!
sleep 0.35
kill -KILL "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""

start_daemon "$restarted_aspd_bin" "$workspace/aspd-restarted.log" "$restarted_max_protocol"
wait_ready "$workspace/aspd-restarted.log"

wait "$exec_pid"
status=$?
set -e
exec_pid=""
if [[ "$status" -ne 124 ]]; then
  echo "expected recovered timed-out EXEC to return 124, got $status" >&2
  cat "$workspace/exec.err" >&2 || true
  cat "$workspace/aspd-restarted.log" >&2 || true
  exit 1
fi

metrics=$(curl -fsS "http://127.0.0.1:$health_port/metrics")
printf '%s\n' "$metrics" | awk '$1 == "asp_process_timeouts_total" && $2 >= 1 { found = 1 } END { exit(found ? 0 : 1) }'

printf 'ASP EXEC timeout-restart smoke passed (exit=%s)\n' "$status"
