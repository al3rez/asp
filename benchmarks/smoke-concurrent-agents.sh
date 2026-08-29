#!/usr/bin/env bash
set -euo pipefail

# Release-level bounded concurrency smoke for independent coding-agent
# adapters. Each worker has its own local cursor/session file but shares one
# server workspace, exercising QUIC admission, semantic reads, process
# summaries, and the serialized file-commit boundary at the same time.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
workers=${ASP_CONCURRENT_AGENTS:-12}
port=${ASP_CONCURRENT_AGENTS_PORT:-4597}
health_port=${ASP_CONCURRENT_AGENTS_HEALTH_PORT:-9497}

if ! [[ "$workers" =~ ^[1-9][0-9]*$ ]]; then
  echo "ASP_CONCURRENT_AGENTS must be a positive integer" >&2
  exit 2
fi

workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-concurrent-agents.XXXXXX")
state=$(mktemp -d "${TMPDIR:-/tmp}/asp-concurrent-agents-state.XXXXXX")
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -r -- "$workspace" "$state"
}
trap cleanup EXIT INT TERM

printf 'shared fixture\n' >"$workspace/fixture.txt"
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
for _ in $(seq 1 120); do
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
  echo 'ASP concurrent-agent smoke daemon did not become ready' >&2
  exit 1
fi

# The readiness probe itself uses a short-lived QUIC connection. Wait until
# the server has released that connection lease before exercising the
# configured principal ceiling; otherwise a boundary run can fail merely
# because the probe's close is still being observed by the daemon.
for _ in $(seq 1 100); do
  active=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
    | awk '$1 == "asp_active_connections" { print $2; found = 1 } END { if (!found) exit 1 }')
  if [[ "$active" == 0 ]]; then
    break
  fi
  sleep 0.05
done
test "${active:-1}" = 0

# This is an all-success smoke, so refuse an input that exceeds the daemon's
# advertised per-principal connection ceiling.  Capacity/rejection testing
# belongs in a separate harness; allowing it here would turn an intentional
# admission response into dozens of misleading worker failures.
principal_limit=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
  | awk '$1 == "asp_principal_active_connections_limit" { print $2; found = 1 } END { if (!found) exit 1 }')
if ! [[ "$principal_limit" =~ ^[0-9]+$ ]]; then
  echo 'ASP daemon reported an invalid per-principal connection limit' >&2
  exit 1
fi
if (( workers > principal_limit )); then
  echo "ASP_CONCURRENT_AGENTS=$workers exceeds the daemon's all-success principal connection limit ($principal_limit); use a capacity/rejection harness" >&2
  exit 2
fi

pids=()
for i in $(seq 1 "$workers"); do
  worker_state="$state/worker-$i"
  mkdir -p "$worker_state"
  input="$worker_state/input.jsonl"
  output="$worker_state/output.jsonl"
  encoded=$(printf 'worker-%s' "$i" | base64 | tr -d '\n')
  printf '%s\n' \
    "{\"id\":\"ping-$i\",\"op\":\"ping\"}" \
    "{\"id\":\"exec-$i\",\"op\":\"exec_summary\",\"command\":\"printf worker-$i\"}" \
    "{\"id\":\"inspect-$i\",\"op\":\"inspect\",\"read_paths\":[\"fixture.txt\"]}" \
    "{\"id\":\"put-$i\",\"op\":\"file_put\",\"path\":\"worker-$i.txt\",\"data_base64\":\"$encoded\"}" \
    "{\"id\":\"close-$i\",\"op\":\"close\"}" >"$input"
  XDG_STATE_HOME="$worker_state" "$asp_bin" \
    --cert "$workspace/.asp/server-cert.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    agent "127.0.0.1:$port" \
    <"$input" >"$output" 2>"$worker_state/stderr" &
  pids+=("$!")
done

failed=0
for index in "${!pids[@]}"; do
  if ! wait "${pids[$index]}"; then
    echo "concurrent agent $((index + 1)) failed" >&2
    cat "$state/worker-$((index + 1))/stderr" >&2 || true
    cat "$state/worker-$((index + 1))/output.jsonl" >&2 || true
    failed=1
  fi
done
test "$failed" = 0

for i in $(seq 1 "$workers"); do
  output="$state/worker-$i/output.jsonl"
  grep -q '"id":"ping-'"$i"'.*"type":"pong"' "$output"
  grep -q '"id":"exec-'"$i"'.*"type":"summary"' "$output"
  grep -q '"id":"inspect-'"$i"'.*"type":"workspace_state"' "$output"
  grep -q '"id":"put-'"$i"'.*"type":"file_stored"' "$output"
  grep -q '"id":"close-'"$i"'.*"type":"closed"' "$output"
  test "$(<"$workspace/worker-$i.txt")" = "worker-$i"
done

metrics=$(curl -fsS "http://127.0.0.1:$health_port/metrics")
printf '%s\n' "$metrics" | grep -Eq '^asp_request_failures 0$'
printf '%s\n' "$metrics" | grep -Eq '^asp_frame_memory_bytes 0$'
printf '%s\n' "$metrics" | grep -Eq '^asp_response_memory_bytes 0$'

printf 'ASP concurrent-agent smoke passed (workers=%s)\n' "$workers"
