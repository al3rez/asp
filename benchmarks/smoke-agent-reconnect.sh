#!/usr/bin/env bash
set -euo pipefail

# Release smoke for a warm JSONL agent crossing daemon restarts. The agent
# process and its stdin remain alive; only aspd is replaced. Read and
# side-effecting requests must reconnect directly without a redundant journal
# replay, while explicit event consumers retain the separate resume contract.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_AGENT_RECONNECT_PORT:-4547}
health_port=${ASP_AGENT_RECONNECT_HEALTH_PORT:-4647}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-agent-reconnect.XXXXXX")
state_home=$(mktemp -d "${TMPDIR:-/tmp}/asp-agent-reconnect-state.XXXXXX")
input_fifo="$state_home/agent-input"
output="$state_home/agent-output.jsonl"
stderr="$state_home/agent.stderr"
daemon_pid=""
agent_pid=""

cleanup() {
  exec 3>&- 2>/dev/null || true
  if [[ -n "$agent_pid" ]]; then
    kill "$agent_pid" 2>/dev/null || true
    wait "$agent_pid" 2>/dev/null || true
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
    --health-listen "127.0.0.1:$health_port" \
    >"$log_file" 2>&1 &
  daemon_pid=$!

  local ready=0
  for _ in $(seq 1 120); do
    if "$asp_bin" \
        --cert "$workspace/.asp/server-cert.der" \
        --auth-token-file "$workspace/.asp/auth-token" \
        --session-file "$state_home/session.json" \
        doctor "127.0.0.1:$port" >/dev/null 2>&1; then
      ready=1
      break
    fi
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
      break
    fi
    sleep 0.05
  done
  # A stale daemon from a failed replacement can still answer the QUIC doctor
  # probe while the newly started process has already exited on bind/config
  # failure. Require both the probe and the child liveness so the reconnect
  # assertions never accidentally exercise the previous process.
  if [[ "$ready" != 1 ]] || ! kill -0 "$daemon_pid" 2>/dev/null; then
    cat "$log_file" >&2
    echo "ASP agent-reconnect smoke daemon did not become ready" >&2
    exit 1
  fi
}

wait_for_output() {
  local pattern=$1
  for _ in $(seq 1 200); do
    if grep -q "$pattern" "$output" 2>/dev/null; then
      return 0
    fi
    if [[ -n "$agent_pid" ]] && ! kill -0 "$agent_pid" 2>/dev/null; then
      cat "$stderr" >&2 || true
      cat "$output" >&2 || true
      echo "ASP agent exited before output pattern: $pattern" >&2
      return 1
    fi
    sleep 0.05
  done
  cat "$stderr" >&2 || true
  cat "$output" >&2 || true
  echo "ASP agent did not emit output pattern: $pattern" >&2
  return 1
}

start_daemon "$workspace/aspd-0.log"
mkfifo "$input_fifo"
touch "$output" "$stderr"

"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$state_home/session.json" \
  agent "127.0.0.1:$port" \
  <"$input_fifo" >"$output" 2>"$stderr" &
agent_pid=$!

# Opening the writer waits until the agent has opened its input side. The
# ready marker then proves that the initial QUIC/session handshake completed.
exec 3>"$input_fifo"
wait_for_output '"type":"ready"'

printf '%s\n' '{"id":"before","op":"exec_summary","command":"printf agent-before-reconnect"}' >&3
wait_for_output '"id":"before".*"type":"summary"'
grep -q 'YWdlbnQtYmVmb3JlLXJlY29ubmVjdA==' "$output"
before_process_id=$(sed -n 's/.*"id":"before".*"process_id":"\([^"]*\)".*/\1/p' "$output" | head -n 1)
test -n "$before_process_id"

# Replace only the daemon. The agent's process, FIFO, and durable cursor stay
# alive while the transport is unavailable.
kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
start_daemon "$workspace/aspd-1.log"

# The health counter is process-local, so establish the read-retry baseline
# after the replacement daemon starts. The initial agent handshake and EXEC do
# not need RESUME; any future startup path that intentionally refreshes a
# cursor is included in this baseline rather than mistaken for a read retry.
resume_before_read=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
  | awk '$1 == "asp_resume_requests_total" { print $2 }')
test -n "$resume_before_read"

# A point-in-time semantic read is safe to repeat from the durable session ID.
# The optimized retry path should perform HELLO, then this request directly;
# it must not add a RESUME round trip merely to refresh an event cursor that
# the read does not consume.
printf '%s\n' '{"id":"read","op":"inspect","include_tree":false,"include_git_status":false}' >&3
wait_for_output '"id":"read".*"type":"workspace_state"'
resume_after_read=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
  | awk '$1 == "asp_resume_requests_total" { print $2 }')
if [[ "$resume_after_read" != "$resume_before_read" ]]; then
  cat "$output" >&2
  echo "read retry unexpectedly replayed the event journal ($resume_before_read -> $resume_after_read)" >&2
  exit 1
fi

# Repeat the replacement for a durable process-log read. Its byte offset and
# snapshot length make the stream independently retryable, so this path should
# also avoid a journal replay.
kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
start_daemon "$workspace/aspd-2.log"
resume_before_logs=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
  | awk '$1 == "asp_resume_requests_total" { print $2 }')
test -n "$resume_before_logs"
printf '%s\n' "{\"id\":\"logs\",\"op\":\"logs\",\"process_id\":\"$before_process_id\",\"stream\":\"stdout\"}" >&3
wait_for_output '"id":"logs".*"type":"log_end"'
resume_after_logs=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
  | awk '$1 == "asp_resume_requests_total" { print $2 }')
if [[ "$resume_after_logs" != "$resume_before_logs" ]]; then
  cat "$output" >&2
  echo "log read unexpectedly replayed the event journal ($resume_before_logs -> $resume_after_logs)" >&2
  exit 1
fi

# A side-effecting request is safe to retry directly after HELLO because its
# stable request ID is deduplicated by the daemon. Force the ambiguous case:
# wait until the daemon has admitted the process, then kill it while the child
# is still sleeping so the final response is lost with the transport.
printf '%s\n' '{"id":"after","op":"exec_summary","command":"printf agent-after-reconnect; sleep 2"}' >&3
wait_for_output '"id":"after".*"type":"started"'
kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
start_daemon "$workspace/aspd-3.log"
resume_before_after=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
  | awk '$1 == "asp_resume_requests_total" { print $2 }')
test -n "$resume_before_after"
wait_for_output '"id":"after".*"type":"summary"'
grep -q 'YWdlbnQtYWZ0ZXItcmVjb25uZWN0' "$output"
if [[ "$(grep -c '"id":"after".*"type":"started"' "$output")" != 1 ||
      "$(grep -c '"id":"after".*"type":"summary"' "$output")" != 1 ||
      "$(grep -c '"id":"after".*"type":"exit"' "$output")" != 1 ]]; then
  cat "$output" >&2
  echo "side-effect retry did not replay exactly one durable result" >&2
  exit 1
fi
resume_after_after=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
  | awk '$1 == "asp_resume_requests_total" { print $2 }')
if [[ "$resume_after_after" != "$resume_before_after" ]]; then
  cat "$output" >&2
  echo "side-effect retry unexpectedly replayed the event journal ($resume_before_after -> $resume_after_after)" >&2
  exit 1
fi
printf '{"experiment":"agent-reconnect-direct-retry","host":"macos-arm64-loopback","profile":"release","lost_response_after_admission":true,"exactly_one_side_effect_result":true,"read_resume_delta":%s,"log_resume_delta":%s,"side_effect_resume_delta":%s,"status":0}\n' \
  "$((resume_after_read - resume_before_read))" \
  "$((resume_after_logs - resume_before_logs))" \
  "$((resume_after_after - resume_before_after))"

printf '%s\n' '{"id":"close","op":"close"}' >&3
exec 3>&-
for _ in $(seq 1 200); do
  if ! kill -0 "$agent_pid" 2>/dev/null; then
    break
  fi
  sleep 0.05
done
if kill -0 "$agent_pid" 2>/dev/null; then
  cat "$stderr" >&2 || true
  cat "$output" >&2 || true
  echo "ASP agent did not close after reconnect smoke" >&2
  exit 1
fi
wait "$agent_pid" 2>/dev/null || true
agent_pid=""

grep -q '"id":"close".*"type":"closed"' "$output"
if grep -q '"type":"error"' "$output"; then
  cat "$output" >&2
  echo "ASP agent reconnect smoke emitted an error" >&2
  exit 1
fi

printf 'ASP agent reconnect smoke passed\n'
