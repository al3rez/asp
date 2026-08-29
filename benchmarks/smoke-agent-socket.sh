#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for the supervised local JSONL adapter endpoint. The
# socket is deliberately kept outside the workspace and is required to be
# private; the remote QUIC session remains the same durable ASP session.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_AGENT_SOCKET_SMOKE_PORT:-4565}
health_port=${ASP_AGENT_SOCKET_SMOKE_HEALTH_PORT:-9465}
workspace=$(mktemp -d "/tmp/asp-as.XXXXXX")
state_home=$(mktemp -d "/tmp/asp-st.XXXXXX")
socket="$state_home/agent.sock"
daemon_pid=""
listener_pid=""

cleanup() {
  if [[ -n "$listener_pid" ]]; then
    kill "$listener_pid" 2>/dev/null || true
    wait "$listener_pid" 2>/dev/null || true
  fi
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace" "$state_home"
}
trap cleanup EXIT INT TERM

printf 'socket adapter fixture\n' >"$workspace/fixture.txt"
"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --health-listen "127.0.0.1:$health_port" \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

ready=0
for _ in $(seq 1 100); do
  if "$asp_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      doctor "127.0.0.1:$port" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd.log" >&2
  echo "ASP agent socket smoke daemon did not become ready" >&2
  exit 1
fi

ASP_SERVER="127.0.0.1:$port" \
ASP_CERT="$workspace/.asp/server-cert.der" \
ASP_AUTH_TOKEN_FILE="$workspace/.asp/auth-token" \
ASP_SESSION_FILE="$state_home/session.json" \
"$asp_bin" agent-listen "$socket" \
  >"$state_home/listener.stdout" 2>"$state_home/listener.stderr" &
listener_pid=$!

for _ in $(seq 1 100); do
  if [[ -S "$socket" ]]; then
    break
  fi
  sleep 0.05
done
if [[ ! -S "$socket" ]]; then
  cat "$state_home/listener.stderr" >&2
  echo "ASP agent socket listener did not create its socket" >&2
  exit 1
fi

metric() {
  local name=$1
  curl -fsS "http://127.0.0.1:$health_port/metrics" \
    | awk -v metric_name="$name" '$1 == metric_name { print $2; found = 1 } END { if (!found) exit 1 }'
}

input="$state_home/input.jsonl"
output="$state_home/output.jsonl"
printf '%s\n' \
  '{"id":"ping-1","op":"ping"}' \
  '{"id":"exec-1","op":"exec_summary","command":"printf socket-adapter-ok"}' \
  '{"id":"inspect-1","op":"inspect","read_paths":["fixture.txt"]}' \
  '{"id":"close-1","op":"close"}' >"$input"

"$asp_bin" agent-connect "$socket" <"$input" >"$output"
grep -q '"id":"ping-1".*"type":"pong"' "$output"
grep -q '"id":"exec-1".*"type":"summary"' "$output"
grep -q '"stdout_tail_base64":"c29ja2V0LWFkYXB0ZXItb2s="' "$output"
grep -q '"id":"inspect-1".*"type":"workspace_state"' "$output"
grep -q '"id":"close-1".*"type":"closed"' "$output"

# The first local client returns its still-authenticated QUIC connection to
# the listener's idle pool. A second short-lived client should reuse that
# transport rather than performing another QUIC handshake.
connections_after_first=$(metric asp_quic_connections_total)
active_after_first=$(metric asp_active_connections)
test "$active_after_first" -ge 1
printf '%s\n' \
  '{"id":"ping-2","op":"ping"}' \
  '{"id":"close-2","op":"close"}' >"$state_home/input-second.jsonl"
"$asp_bin" agent-connect "$socket" <"$state_home/input-second.jsonl" >"$state_home/output-second.jsonl"
grep -q '"id":"ping-2".*"type":"pong"' "$state_home/output-second.jsonl"
grep -q '"id":"close-2".*"type":"closed"' "$state_home/output-second.jsonl"
connections_after_second=$(metric asp_quic_connections_total)
test "$connections_after_second" = "$connections_after_first"

# Rotate the bearer file while the pooled transport is idle. The first
# request on that stale connection must reconnect once and use the new token,
# rather than forcing the supervisor or local agent to restart.
printf 'socket-rotated-token-abcdefghijklmnopqrstuvwxyz\n' >"$workspace/.asp/auth-token.new"
chmod 600 "$workspace/.asp/auth-token.new"
mv "$workspace/.asp/auth-token.new" "$workspace/.asp/auth-token"
printf '%s\n' \
  '{"id":"exec-3","op":"exec_summary","command":"printf rotated-ok"}' \
  '{"id":"close-3","op":"close"}' >"$state_home/input-third.jsonl"
"$asp_bin" agent-connect "$socket" <"$state_home/input-third.jsonl" >"$state_home/output-third.jsonl"
grep -q '"id":"exec-3".*"type":"summary"' "$state_home/output-third.jsonl"
grep -q '"stdout_tail_base64":"cm90YXRlZC1vaw=="' "$state_home/output-third.jsonl"
grep -q '"id":"close-3".*"type":"closed"' "$state_home/output-third.jsonl"

# Keep one local adapter active while SIGTERM arrives. The service-manager stop
# path must remove the endpoint immediately, then let the in-flight client
# flush a close response during the bounded drain window.
(
  printf '%s\n' '{"id":"active-ping","op":"ping"}'
  sleep 1
  printf '%s\n' '{"id":"active-close","op":"close"}'
) | "$asp_bin" agent-connect "$socket" >"$state_home/output-active.jsonl" &
active_client_pid=$!
for _ in $(seq 1 100); do
  if grep -q '"id":"active-ping".*"type":"pong"' "$state_home/output-active.jsonl"; then
    break
  fi
  sleep 0.05
done
grep -q '"id":"active-ping".*"type":"pong"' "$state_home/output-active.jsonl"
kill -TERM "$listener_pid"
for _ in $(seq 1 100); do
  if [[ ! -e "$socket" ]]; then
    break
  fi
  sleep 0.05
done
test ! -e "$socket"
if ! wait "$active_client_pid"; then
  cat "$state_home/output-active.jsonl" >&2 || true
  cat "$state_home/listener.stderr" >&2 || true
  exit 1
fi
grep -q '"id":"active-ping".*"type":"pong"' "$state_home/output-active.jsonl"
grep -q '"id":"active-close".*"type":"closed"' "$state_home/output-active.jsonl"
wait "$listener_pid"
listener_pid=""

printf 'ASP agent socket smoke passed\n'
