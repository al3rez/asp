#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for an upload interrupted while bytes are in flight.
# The client is paused after the server has persisted a nonzero prefix, the
# daemon is killed and restarted, and the original process must resume the
# same request from the durable staging offset.  This exercises both streamed
# FILE_PUT and content-addressed artifact uploads.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
initial_aspd_bin=${ASPD_INITIAL_BIN:-"$aspd_bin"}
restarted_aspd_bin=${ASPD_RESTARTED_BIN:-"$aspd_bin"}
port=${ASP_TRANSFER_RESTART_PORT:-4597}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-transfer-restart.XXXXXX")
daemon_pid=""
transfer_pid=""

cleanup() {
  if [[ -n "$transfer_pid" ]]; then
    kill -CONT "$transfer_pid" 2>/dev/null || true
    kill "$transfer_pid" 2>/dev/null || true
    wait "$transfer_pid" 2>/dev/null || true
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

size_mb=${ASP_TRANSFER_RESTART_SIZE_MB:-64}
if [[ ! "$size_mb" =~ ^[0-9]+$ ]] || (( size_mb < 8 || size_mb > 512 )); then
  echo "ASP_TRANSFER_RESTART_SIZE_MB must be between 8 and 512" >&2
  exit 1
fi
initial_max_protocol=${ASP_TRANSFER_RESTART_INITIAL_MAX_PROTOCOL_VERSION:-}
restarted_max_protocol=${ASP_TRANSFER_RESTART_RESTARTED_MAX_PROTOCOL_VERSION:-}
for protocol_version in "$initial_max_protocol" "$restarted_max_protocol"; do
  if [[ -n "$protocol_version" && "$protocol_version" != 16 && "$protocol_version" != 17 ]]; then
    echo "ASP_TRANSFER_RESTART_*_MAX_PROTOCOL_VERSION must be 16, 17, or empty" >&2
    exit 1
  fi
done
total_bytes=$((size_mb * 1024 * 1024))

source="$workspace/source.bin"
# Random bytes keep this test from passing only because compression hides an
# accidentally retransmitted body.  The size is bounded above and can be
# lowered for constrained CI runners.
dd if=/dev/urandom of="$source" bs=1048576 count="$size_mb" status=none

start_daemon() {
  local log_path=$1
  local max_protocol_version=${2:-}
  local daemon_binary=${3:-$aspd_bin}
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
    >"$log_path" 2>&1 &
  daemon_pid=$!
}

wait_ready() {
  local ready=0
  for _ in $(seq 1 160); do
    if "$asp_bin" \
        --cert "$workspace/.asp/server-cert.der" \
        --auth-token-file "$workspace/.asp/auth-token" \
        --session-file "$workspace/session.json" \
        doctor "127.0.0.1:$port" >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 0.05
  done
  if [[ "$ready" != 1 ]]; then
    echo "ASP transfer-restart daemon did not become ready" >&2
    return 1
  fi
}

stop_daemon() {
  if [[ -n "$daemon_pid" ]]; then
    kill -KILL "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=""
  fi
}

start_daemon "$workspace/aspd-initial.log" "$initial_max_protocol" "$initial_aspd_bin"
wait_ready

"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$workspace/session.json" \
  connect "127.0.0.1:$port" >/dev/null
session_id=$(jq -r --arg server "127.0.0.1:$port" '.servers[$server].session_id' "$workspace/session.json")
if [[ ! "$session_id" =~ ^[0-9a-fA-F-]{36}$ ]]; then
  echo "could not resolve durable session id" >&2
  exit 1
fi

run_interrupted_transfer() {
  local kind=$1
  local source_path=$2
  local checkpoint="${source_path%.*}.asp-upload"
  local stage_root="$workspace/.asp/sessions/$session_id/uploads"
  local transfer_log="$workspace/$kind-transfer"
  local stage=""
  local request_id=""
  local stage_bytes=0

  if [[ "$kind" == artifact ]]; then
    checkpoint="${source_path%.*}.asp-artifact-upload"
    stage_root="$workspace/.asp/sessions/$session_id/artifacts/uploads"
  fi

  if [[ "$kind" == file ]]; then
    "$asp_bin" \
      --reconnect-timeout-ms 120000 \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$workspace/session.json" \
      put "127.0.0.1:$port" "$source_path" remote.bin --force \
      >"$transfer_log.out" 2>"$transfer_log.err" &
  else
    "$asp_bin" \
      --reconnect-timeout-ms 120000 \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$workspace/session.json" \
      artifact-put "127.0.0.1:$port" "$source_path" --name restart-smoke \
      >"$transfer_log.out" 2>"$transfer_log.err" &
  fi
  transfer_pid=$!

  # Stop early enough that the durable staging prefix cannot already equal the
  # complete source.  The stopped client may have a small amount queued in the
  # kernel, but killing the daemon immediately below bounds the persisted
  # prefix and makes the interruption deterministic.
  for _ in $(seq 1 4000); do
    if [[ -s "$checkpoint" ]]; then
      request_id=$(jq -r '.request_id' "$checkpoint" 2>/dev/null || true)
    fi
    if [[ -n "$request_id" && "$request_id" != null \
        && -f "$stage_root/$request_id/payload.part" ]]; then
      stage="$stage_root/$request_id/payload.part"
      stage_bytes=$(wc -c <"$stage")
      if (( stage_bytes > 0 && stage_bytes < total_bytes / 2 )); then
        break
      fi
    fi
    sleep 0.005
  done
  if [[ -z "$stage" || ! -f "$stage" || "$stage_bytes" -le 0 \
      || "$stage_bytes" -ge $((total_bytes / 2)) ]]; then
    echo "$kind transfer never exposed a bounded durable prefix" >&2
    cat "$transfer_log.err" >&2 || true
    exit 1
  fi

  kill -STOP "$transfer_pid"
  sleep 0.05
  stop_daemon

  start_daemon "$workspace/aspd-$kind-restarted.log" "$restarted_max_protocol" "$restarted_aspd_bin"
  wait_ready
  kill -CONT "$transfer_pid"

  set +e
  wait "$transfer_pid"
  local transfer_rc=$?
  set -e
  transfer_pid=""
  if [[ "$transfer_rc" != 0 ]]; then
    echo "$kind transfer did not resume after daemon restart (rc=$transfer_rc)" >&2
    cat "$transfer_log.out" >&2 || true
    cat "$transfer_log.err" >&2 || true
    cat "$workspace/aspd-$kind-restarted.log" >&2 || true
    exit 1
  fi
  if [[ -e "$checkpoint" ]]; then
    echo "$kind transfer left its client checkpoint behind" >&2
    exit 1
  fi

  if [[ "$kind" == file ]]; then
    "$asp_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$workspace/session.json" \
      get "127.0.0.1:$port" remote.bin "$workspace/file-result.bin" >/dev/null
    cmp "$source" "$workspace/file-result.bin"
  else
    local artifact_id
    artifact_id=$(sed -nE 's/.*sha256 ([0-9a-f]+),.*/\1/p' "$transfer_log.out")
    if [[ ! "$artifact_id" =~ ^[0-9a-f]{64}$ ]]; then
      echo "artifact transfer did not return a SHA-256 id" >&2
      cat "$transfer_log.out" >&2
      exit 1
    fi
    "$asp_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$workspace/session.json" \
      artifact-get "127.0.0.1:$port" "$artifact_id" \
      "$workspace/artifact-result.bin" >/dev/null
    cmp "$source" "$workspace/artifact-result.bin"
  fi
  printf '%s transfer resumed from %s bytes\n' "$kind" "$stage_bytes"
}

run_interrupted_transfer file "$source"
run_interrupted_transfer artifact "$source"

printf 'ASP transfer-restart smoke passed (size=%s MiB)\n' "$size_mb"
