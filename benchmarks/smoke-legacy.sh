#!/usr/bin/env bash
set -euo pipefail

# Release-level compatibility smoke. It starts the real daemon and speaks the
# v16 plain length-prefixed framing directly, proving that a tested old peer
# can coexist with the v17 server during a rolling deployment.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
bench_bin=${ASP_BENCH_BIN:-"$repo_root/target/release/asp-bench"}
client_bin=${ASP_CLIENT_BIN:-"$repo_root/target/release/asp"}
port=${ASP_LEGACY_SMOKE_PORT:-4544}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-legacy-smoke.XXXXXX")
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

stop_daemon() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=""
  fi
}

"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

result=""
for _ in $(seq 1 100); do
  if result=$("$bench_bin" legacy-smoke \
      --server "127.0.0.1:$port" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" 2>/dev/null); then
    break
  fi
  sleep 0.05
done
if [[ -z "$result" ]]; then
  cat "$workspace/aspd.log" >&2
  echo "ASP legacy framing smoke did not become ready" >&2
  exit 1
fi

grep -q '"experiment":"legacy-v16-smoke"' <<<"$result"
grep -q '"hello_ok":true' <<<"$result"
grep -q '"health_ok":true' <<<"$result"
printf '%s\n' "$result"

# Exercise the other side of the rolling-upgrade contract as well. The
# current daemon can be pinned to the v16 compatibility ceiling, which gives
# us a deterministic old-peer fixture: a current v17 client must observe the
# failed v17 handshake and retry with plain v16 before issuing HEALTH.
stop_daemon
"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --max-protocol-version 16 \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  >"$workspace/aspd-legacy-only.log" 2>&1 &
daemon_pid=$!

client_result=""
for _ in $(seq 1 100); do
  if client_result=$(
    "$client_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      doctor "127.0.0.1:$port" 2>/dev/null
  ); then
    break
  fi
  sleep 0.05
done
if [[ -z "$client_result" ]]; then
  cat "$workspace/aspd-legacy-only.log" >&2
  echo "ASP current-client legacy fallback did not become ready" >&2
  exit 1
fi

grep -q '"auth_required": true' <<<"$client_result"
printf '%s\n' "$client_result"

# Exercise durable state across the compatibility boundary, not just the
# handshake.  Create the session and process while the daemon is pinned to the
# v16 framing, then restart the same durable workspace with the current v17
# implementation and recover the process status/log through the saved UUID.
session_file="$workspace/rolling-session.json"
"$client_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  connect "127.0.0.1:$port" >/dev/null
process_id=$("$client_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  spawn "127.0.0.1:$port" "sleep 2; printf rolling-upgrade-marker")

stop_daemon
"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  >"$workspace/aspd-v17-after-rollout.log" 2>&1 &
daemon_pid=$!

current_ready=""
for _ in $(seq 1 100); do
  if current_ready=$(
    "$client_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$session_file" \
      doctor "127.0.0.1:$port" 2>/dev/null
  ); then
    break
  fi
  sleep 0.05
done
if [[ -z "$current_ready" ]]; then
  cat "$workspace/aspd-v17-after-rollout.log" >&2
  echo "ASP v17 daemon did not become ready after v16 rollout" >&2
  exit 1
fi

sleep 3
status_after_rollout=$("$client_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  status "127.0.0.1:$port" "$process_id")
grep -q '"running": false' <<<"$status_after_rollout"
rollout_output=$("$client_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  logs "127.0.0.1:$port" "$process_id" --offset 0)
grep -q 'rolling-upgrade-marker' <<<"$rollout_output"

# Exercise the rollback direction as well. The durable workspace was written
# by the v17 daemon above; restart the same binary with its v16 compatibility
# ceiling and require the current client to fall back again while recovering
# the finished process and its log. This is a real framing/state rollback
# drill, although a historical release binary is still required before a
# published compatibility SLO.
stop_daemon
"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --max-protocol-version 16 \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  >"$workspace/aspd-v16-rollback.log" 2>&1 &
daemon_pid=$!

rollback_ready=""
for _ in $(seq 1 100); do
  if rollback_ready=$(
    "$client_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$session_file" \
      doctor "127.0.0.1:$port" 2>/dev/null
  ); then
    break
  fi
  sleep 0.05
done
if [[ -z "$rollback_ready" ]]; then
  cat "$workspace/aspd-v16-rollback.log" >&2
  echo "ASP v16 rollback daemon did not become ready" >&2
  exit 1
fi

rollback_status=$("$client_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  status "127.0.0.1:$port" "$process_id")
grep -q '"running": false' <<<"$rollback_status"
rollback_output=$("$client_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  logs "127.0.0.1:$port" "$process_id" --offset 0)
grep -q 'rolling-upgrade-marker' <<<"$rollback_output"
printf 'ASP legacy framing, rollout, rollback, and current-client fallback smoke passed\n'
