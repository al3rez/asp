#!/usr/bin/env bash
set -euo pipefail

# Release-level admission smoke. Hold one authenticated JSONL adapter per
# connection until the per-principal ceiling is full, require the next
# connection to fail closed, then release every holder and verify that leases
# return to zero. This is an admission-boundary check, not a capacity SLO or a
# substitute for a longer soak on independent hosts.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_CAPACITY_REJECTION_PORT:-4598}
health_port=${ASP_CAPACITY_REJECTION_HEALTH_PORT:-9498}
hold_seconds=${ASP_CAPACITY_REJECTION_HOLD_SECONDS:-30}

if ! [[ "$hold_seconds" =~ ^[1-9][0-9]*$ ]] || ((hold_seconds > 300)); then
  echo 'ASP_CAPACITY_REJECTION_HOLD_SECONDS must be an integer from 1 to 300' >&2
  exit 2
fi

workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-capacity-rejection.XXXXXX")
state=$(mktemp -d "${TMPDIR:-/tmp}/asp-capacity-rejection-state.XXXXXX")
daemon_pid=""
client_pids=()
feeder_pids=()

cleanup() {
  for pid in "${feeder_pids[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in "${client_pids[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in "${feeder_pids[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  for pid in "${client_pids[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -r -- "$workspace" "$state"
}
trap cleanup EXIT INT TERM

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
      --session-file "$state/bootstrap-session.json" \
      doctor "127.0.0.1:$port" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd.log" >&2
  echo 'ASP capacity-rejection smoke daemon did not become ready' >&2
  exit 1
fi

wait_for_zero_connections() {
  local active=1
  for _ in $(seq 1 400); do
    active=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
      | awk '$1 == "asp_active_connections" { print $2; found = 1 } END { if (!found) exit 1 }')
    if [[ "$active" == 0 ]]; then
      break
    fi
    sleep 0.05
  done
  test "$active" = 0
}
wait_for_zero_connections

principal_limit=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
  | awk '$1 == "asp_principal_active_connections_limit" { print $2; found = 1 } END { if (!found) exit 1 }')
if ! [[ "$principal_limit" =~ ^[1-9][0-9]*$ ]]; then
  echo 'ASP daemon reported an invalid per-principal connection limit' >&2
  exit 1
fi

# Create one durable session before copying its identity to each holder. A
# distinct session file per client avoids concurrent cursor writes while all
# holders still authenticate to the same durable session/principal.
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$state/base-session.json" \
  connect "127.0.0.1:$port" >/dev/null
wait_for_zero_connections

for i in $(seq 1 "$principal_limit"); do
  holder="$state/holder-$i"
  mkdir -p "$holder"
  cp "$state/base-session.json" "$holder/session.json"
  mkfifo "$holder/input"
  # Keep the FIFO writer open after the ping so the adapter's QUIC connection
  # remains attached until the overflow attempt has completed.
  {
    printf '{"id":"hold-%s","op":"ping"}\n' "$i"
    sleep "$hold_seconds" &
    hold_pid=$!
    # Terminating the feeder must also terminate its child sleep; otherwise an
    # orphaned writer keeps the FIFO open and makes the release smoke wait for
    # the full hold interval during normal teardown.
    trap 'kill "$hold_pid" 2>/dev/null || true; wait "$hold_pid" 2>/dev/null || true; exit 0' TERM INT
    wait "$hold_pid"
  } >"$holder/input" &
  feeder_pids+=("$!")
  "$asp_bin" \
    --cert "$workspace/.asp/server-cert.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    --session-file "$holder/session.json" \
    agent "127.0.0.1:$port" \
    <"$holder/input" \
    >"$holder/output.jsonl" 2>"$holder/stderr" &
  client_pids+=("$!")
done

active=0
for _ in $(seq 1 240); do
  active=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
    | awk '$1 == "asp_active_connections" { print $2; found = 1 } END { if (!found) exit 1 }')
  if [[ "$active" == "$principal_limit" ]]; then
    break
  fi
  sleep 0.05
done
if [[ "$active" != "$principal_limit" ]]; then
  cat "$workspace/aspd.log" >&2
  echo "ASP capacity smoke admitted $active of $principal_limit holder connections" >&2
  exit 1
fi

for i in $(seq 1 "$principal_limit"); do
  grep -q '"type":"pong"' "$state/holder-$i/output.jsonl"
done

# The overflow client must fail at HELLO with a stable admission error. The
# exact CLI exit code is not a protocol contract; the server rejection counter
# and nonzero client result are the assertions.
overflow_output="$state/overflow.out"
overflow_error="$state/overflow.err"
set +e
printf '{"id":"overflow","op":"ping"}\n' | \
  "$asp_bin" \
    --cert "$workspace/.asp/server-cert.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    --session-file "$state/base-session.json" \
    agent "127.0.0.1:$port" \
    >"$overflow_output" 2>"$overflow_error"
overflow_rc=$?
set -e
if ((overflow_rc == 0)); then
  echo 'ASP capacity overflow connection unexpectedly succeeded' >&2
  cat "$overflow_output" >&2 || true
  exit 1
fi

rejections=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
  | awk '$1 == "asp_principal_connection_rejections" { print $2; found = 1 } END { if (!found) exit 1 }')
if ! [[ "$rejections" =~ ^[1-9][0-9]*$ ]]; then
  cat "$overflow_error" >&2 || true
  echo 'ASP capacity overflow did not increment the connection rejection counter' >&2
  exit 1
fi

# Close all FIFO writers first. EOF lets each adapter perform its normal
# connection drain; only the EXIT/INT/TERM cleanup path force-kills clients.
for pid in "${feeder_pids[@]}"; do
  kill "$pid" 2>/dev/null || true
done
for pid in "${feeder_pids[@]}"; do
  wait "$pid" 2>/dev/null || true
done
for pid in "${client_pids[@]}"; do
  wait "$pid" 2>/dev/null || true
done
wait_for_zero_connections

printf 'ASP capacity-rejection smoke passed (limit=%s rejections=%s)\n' \
  "$principal_limit" "$rejections"
