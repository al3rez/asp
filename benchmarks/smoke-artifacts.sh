#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for immutable, content-addressed artifact transfer.
# It exercises multi-frame upload, restart-safe retrieval, bounded ranges, and
# the JSONL coding-agent adapter on a private loopback daemon.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_ARTIFACT_SMOKE_PORT:-4551}
health_port=${ASP_ARTIFACT_SMOKE_HEALTH_PORT:-9451}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-artifacts.XXXXXX")
session_file="$workspace/client-session.json"
cross_session_file="$workspace/client-session-cross.json"
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

# Two full QUIC payload frames make it possible to catch offset/chunking bugs.
source_file="$workspace/source.bin"
dd if=/dev/zero of="$source_file" bs=65536 count=2 status=none
printf 'artifact smoke trailer\n' >>"$source_file"

start_daemon() {
  "$aspd_bin" \
    --listen "127.0.0.1:$port" \
    --root "$workspace" \
    --cert "$workspace/.asp/server-cert.der" \
    --key "$workspace/.asp/server-key.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    --health-listen "127.0.0.1:$health_port" \
    >"$workspace/aspd.log" 2>&1 &
  daemon_pid=$!
  local ready=0
  for _ in $(seq 1 120); do
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
    cat "$workspace/aspd.log" >&2
    echo "ASP artifact smoke daemon did not become ready" >&2
    exit 1
  fi
}

start_daemon

put_output=$(ASP_SERVER="127.0.0.1:$port" \
  ASP_CERT="$workspace/.asp/server-cert.der" \
  ASP_AUTH_TOKEN_FILE="$workspace/.asp/auth-token" \
  ASP_SESSION_FILE="$session_file" \
  "$asp_bin" artifact-put "$source_file" --name smoke-output)
artifact_id=$(sed -E 's/.*sha256 ([0-9a-f]+),.*/\1/' <<<"$put_output")
if [[ ! "$artifact_id" =~ ^[0-9a-f]{64}$ ]]; then
  echo "artifact-put did not return a SHA-256 id: $put_output" >&2
  exit 1
fi

# A second upload of the same digest is acknowledged before body transfer;
# this is the bandwidth-saving path for repeated test/build outputs.
duplicate_output=$($asp_bin \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  artifact-put "127.0.0.1:$port" "$source_file" --name duplicate-name)
duplicate_id=$(sed -E 's/.*sha256 ([0-9a-f]+),.*/\1/' <<<"$duplicate_output")
test "$duplicate_id" = "$artifact_id"
duplicate_event=$(sed -E 's/.*event_id=([0-9]+).*/\1/' <<<"$duplicate_output")
first_event=$(sed -E 's/.*event_id=([0-9]+).*/\1/' <<<"$put_output")
test "$duplicate_event" = "$first_event"

# The request-byte counter includes the begin/control frames but not a body
# when deduplication wins. A half-object threshold catches an accidental full
# retransmit while remaining tolerant of handshake/session metadata overhead.
request_bytes_before=$(curl -fsS "http://127.0.0.1:$health_port/metrics" | awk '$1 == "asp_request_bytes_total" { print $2 }')
# A fresh session for the same authenticated principal should link the
# already-verified content-addressed object before any body chunks arrive.
# This catches the cross-session bandwidth/disk fast path rather than only the
# same-session idempotent replay above.
cross_output=$($asp_bin \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$cross_session_file" \
  artifact-put "127.0.0.1:$port" "$source_file" --name cross-session)
cross_id=$(sed -E 's/.*sha256 ([0-9a-f]+),.*/\1/' <<<"$cross_output")
test "$cross_id" = "$artifact_id"
request_bytes_after=$(curl -fsS "http://127.0.0.1:$health_port/metrics" | awk '$1 == "asp_request_bytes_total" { print $2 }')
source_bytes=$(wc -c <"$source_file")
request_delta=$((request_bytes_after - request_bytes_before))
if (( request_delta >= source_bytes / 2 )); then
  echo "cross-session artifact upload retransmitted too many bytes: delta=$request_delta source=$source_bytes" >&2
  exit 1
fi
dedup_hits=$(curl -fsS "http://127.0.0.1:$health_port/metrics" | awk '$1 == "asp_artifact_dedup_hits_total" { print $2 }')
dedup_bytes=$(curl -fsS "http://127.0.0.1:$health_port/metrics" | awk '$1 == "asp_artifact_dedup_bytes_total" { print $2 }')
test "$dedup_hits" -ge 1
test "$dedup_bytes" -ge "$source_bytes"
cross_session_id=$(jq -r --arg server "127.0.0.1:$port" '.servers[$server].session_id' "$cross_session_file")
cross_object="$workspace/.asp/sessions/$cross_session_id/artifacts/$artifact_id"
test -f "$cross_object"
if [[ "$(uname -s)" == Darwin ]]; then
  cross_links=$(stat -f '%l' "$cross_object")
else
  cross_links=$(stat -c '%h' "$cross_object")
fi
test "$cross_links" -ge 2

cross_copy="$workspace/cross-copy.bin"
$asp_bin \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$cross_session_file" \
  artifact-get --server "127.0.0.1:$port" "$artifact_id" "$cross_copy" >/dev/null
cmp "$source_file" "$cross_copy"

full_copy="$workspace/full-copy.bin"
$asp_bin \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  artifact-get "127.0.0.1:$port" "$artifact_id" "$full_copy" >/dev/null
cmp "$source_file" "$full_copy"

expected_range="$workspace/expected-range.bin"
actual_range="$workspace/actual-range.bin"
dd if="$source_file" of="$expected_range" bs=1 skip=65536 count=4096 status=none
$asp_bin \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  artifact-get "127.0.0.1:$port" "$artifact_id" "$actual_range" \
  --offset 65536 --length 4096 >/dev/null
cmp "$expected_range" "$actual_range"

# The immutable object and its journal record must survive a daemon restart.
kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
start_daemon
restart_copy="$workspace/restart-copy.bin"
$asp_bin \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  artifact-get "127.0.0.1:$port" "$artifact_id" "$restart_copy" >/dev/null
cmp "$source_file" "$restart_copy"
cross_restart_copy="$workspace/cross-restart-copy.bin"
$asp_bin \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$cross_session_file" \
  artifact-get "127.0.0.1:$port" "$artifact_id" "$cross_restart_copy" >/dev/null
cmp "$source_file" "$cross_restart_copy"

# Exercise the bounded JSONL adapter path as an agent would use it.
agent_put="$workspace/agent-put.jsonl"
agent_put_out="$workspace/agent-put.out"
printf '%s\n' \
  '{"id":"artifact-put-1","op":"artifact_put","name":"agent-output","data_base64":"YWdlbnQtYXJ0aWZhY3QK"}' \
  '{"id":"close-1","op":"close"}' >"$agent_put"
$asp_bin \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  agent "127.0.0.1:$port" <"$agent_put" >"$agent_put_out"
agent_artifact_id=$(jq -r 'select(.type == "artifact_stored" and .id == "artifact-put-1") | .artifact_id' "$agent_put_out")
test -n "$agent_artifact_id" && test "$agent_artifact_id" != null

agent_get="$workspace/agent-get.jsonl"
agent_get_out="$workspace/agent-get.out"
printf '%s\n' \
  "{\"id\":\"artifact-get-1\",\"op\":\"artifact_get\",\"artifact_id\":\"$agent_artifact_id\"}" \
  '{"id":"close-2","op":"close"}' >"$agent_get"
$asp_bin \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  agent "127.0.0.1:$port" <"$agent_get" >"$agent_get_out"
jq -e 'select(.type == "artifact_data" and .id == "artifact-get-1" and .data_base64 == "YWdlbnQtYXJ0aWZhY3QK")' "$agent_get_out" >/dev/null || {
  cat "$agent_get_out" >&2
  exit 1
}

printf 'ASP artifact smoke passed (artifact=%s)\n' "$artifact_id"
