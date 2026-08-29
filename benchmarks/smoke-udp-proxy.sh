#!/usr/bin/env bash
set -euo pipefail

# End-to-end userspace shaping smoke. It starts a private loopback aspd,
# routes a real ASP doctor request through asp-bench's UDP proxy, and keeps a
# JSONL agent alive across a proxy outage/restart. This is deliberately not a
# production relay.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
bench_bin=${ASP_BENCH_BIN:-"$repo_root/target/release/asp-bench"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
target_port=${ASP_PROXY_TARGET_PORT:-4567}
proxy_port=${ASP_PROXY_LISTEN_PORT:-4568}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-udp-proxy-smoke.XXXXXX")
state_home=$(mktemp -d "${TMPDIR:-/tmp}/asp-udp-proxy-agent.XXXXXX")
input_fifo="$state_home/agent-input"
output="$state_home/agent-output.jsonl"
stderr="$state_home/agent.stderr"
daemon_pid=""
proxy_pid=""
agent_pid=""

cleanup() {
  exec 3>&- 2>/dev/null || true
  if [[ -n "$agent_pid" ]]; then
    kill "$agent_pid" 2>/dev/null || true
    wait "$agent_pid" 2>/dev/null || true
  fi
  if [[ -n "$proxy_pid" ]]; then
    kill -TERM "$proxy_pid" 2>/dev/null || true
    wait "$proxy_pid" 2>/dev/null || true
  fi
  if [[ -n "$daemon_pid" ]]; then
    kill -TERM "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace" "$state_home"
}
trap cleanup EXIT INT TERM

start_proxy() {
  local log_file=$1
  "$bench_bin" udp-proxy \
    --listen "127.0.0.1:$proxy_port" \
    --target "127.0.0.1:$target_port" \
    --delay-ms 1 \
    --jitter-ms 1 \
    --loss-percent 0 \
    --rate-mbit 100 \
    >"$log_file" 2>&1 &
  proxy_pid=$!

  local proxy_ready=0
  for _ in $(seq 1 100); do
    if grep -Eq 'status.*listening' "$log_file"; then
      proxy_ready=1
      break
    fi
    if ! kill -0 "$proxy_pid" 2>/dev/null; then
      break
    fi
    sleep 0.02
  done
  if [[ "$proxy_ready" != 1 ]]; then
    cat "$log_file" >&2
    echo "ASP UDP proxy did not become ready" >&2
    exit 1
  fi
}

wait_for_agent_output() {
  local pattern=$1
  for _ in $(seq 1 800); do
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

"$aspd_bin" \
  --listen "127.0.0.1:$target_port" \
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
      doctor "127.0.0.1:$target_port" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.02
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd.log" >&2
  echo "ASP daemon did not become ready for UDP proxy smoke" >&2
  exit 1
fi
start_proxy "$workspace/proxy-0.log"

result=$(
  "$asp_bin" \
    --cert "$workspace/.asp/server-cert.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    doctor "127.0.0.1:$proxy_port"
)
printf '%s\n' "$result"
grep -q '"auth_required": true' <<<"$result"

# Keep one authenticated JSONL adapter alive across a real path outage. The
# QUIC idle bound is 15 seconds; waiting past it ensures the next request must
# exercise the adapter's reconnect/resume path rather than merely riding a
# briefly delayed packet. Restarting the proxy on the same address models a
# recovered route while preserving the durable session on the daemon.
mkfifo "$input_fifo"
touch "$output" "$stderr"
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$state_home/session.json" \
  --reconnect-timeout-ms 30000 \
  agent "127.0.0.1:$proxy_port" \
  <"$input_fifo" >"$output" 2>"$stderr" &
agent_pid=$!
exec 3>"$input_fifo"
wait_for_agent_output '"type":"ready"'

printf '%s\n' '{"id":"before-path","op":"exec_summary","command":"printf agent-before-path"}' >&3
wait_for_agent_output '"id":"before-path".*"type":"summary"'

# A 17-second outage is intentionally longer than the configured QUIC idle
# timeout. Send the next request while the path is still absent, then bring it
# back so the client has to redial and resume the same session.
kill -INT "$proxy_pid"
wait "$proxy_pid"
proxy_pid=""
grep -Eq 'status.*stopped' "$workspace/proxy-0.log"
sleep 17
printf '%s\n' '{"id":"after-path","op":"exec_summary","command":"printf agent-after-path"}' >&3
start_proxy "$workspace/proxy-1.log"
wait_for_agent_output '"id":"after-path".*"type":"summary"'

if grep -q '"type":"error"' "$output"; then
  cat "$stderr" >&2 || true
  cat "$output" >&2 || true
  echo "ASP agent path-reconnect smoke emitted an error" >&2
  exit 1
fi

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
  echo "ASP agent did not close after path-reconnect smoke" >&2
  exit 1
fi
wait "$agent_pid" 2>/dev/null || true
agent_pid=""
grep -q '"id":"close".*"type":"closed"' "$output"

printf 'ASP QUIC/TLS userspace UDP proxy and path-reconnect smoke passed\n'
