#!/usr/bin/env bash
set -euo pipefail

# Execute the exact binaries from a verified release archive rather than the
# source tree.  This is intentionally small and deterministic: it proves the
# fail-closed production profile can start, the packaged client can establish a
# session and transfer a file, and a daemon restart does not lose a detached
# process or its durable log.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
archive=${1:-${ASP_RELEASE_ARCHIVE:-}}
checksum=${2:-${ASP_RELEASE_CHECKSUM:-}}
if [[ -z "$archive" ]]; then
  cat >&2 <<'USAGE'
usage: smoke-packaged-runtime.sh RELEASE.tar.gz [RELEASE.sha256]

The archive must be a verified ASP release.  Set ASP_RELEASE_ARCHIVE and
ASP_RELEASE_CHECKSUM instead of passing positional arguments when desired.
USAGE
  exit 2
fi

if [[ ! -f "$archive" || -L "$archive" ]]; then
  echo "release archive must be a regular non-symlink file: $archive" >&2
  exit 2
fi
verifier=${ASP_RELEASE_VERIFIER:-"$repo_root/deploy/verify-release.sh"}
if [[ ! -x "$verifier" ]]; then
  echo "release verifier is missing or not executable: $verifier" >&2
  exit 2
fi
if [[ -n "$checksum" ]]; then
  "$verifier" "$archive" "$checksum" >/dev/null
else
  "$verifier" "$archive" >/dev/null
fi

extract_dir=$(mktemp -d "${TMPDIR:-/tmp}/asp-packaged-runtime.XXXXXX")
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-packaged-workspace.XXXXXX")
daemon_pid=""
process_id=""
port=${ASP_PACKAGED_RUNTIME_PORT:-}
health_port=${ASP_PACKAGED_RUNTIME_HEALTH_PORT:-}

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill -TERM "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=""
  fi
  # The fixture is finite, but give a detached child a brief opportunity to
  # reap before its workspace is removed if the smoke exits on an assertion.
  sleep 0.2
  rm -rf -- "$extract_dir" "$workspace"
}
trap cleanup EXIT INT TERM

tar -xzf "$archive" -C "$extract_dir"
asp_bin="$extract_dir/bin/asp"
aspd_bin="$extract_dir/bin/aspd"
launcher="$extract_dir/deploy/container/asp-worker-wrapper"
for executable in "$asp_bin" "$aspd_bin" "$launcher"; do
  if [[ ! -f "$executable" || -L "$executable" || ! -x "$executable" ]]; then
    echo "packaged runtime executable is missing or unsafe: $executable" >&2
    exit 1
  fi
done

pick_port() {
  # Ask Python for an ephemeral local port when the caller did not pin one.
  # The daemon binds immediately after this probe; callers running several
  # smokes concurrently should provide explicit, distinct ports instead.
  python3 - <<'PY'
import socket
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}

if [[ -z "$port" ]]; then
  port=$(pick_port)
fi
if [[ -z "$health_port" ]]; then
  health_port=$(pick_port)
fi
if ! [[ "$port" =~ ^[1-9][0-9]*$ ]] || ((port > 65535)); then
  echo "ASP_PACKAGED_RUNTIME_PORT must be an integer from 1 to 65535" >&2
  exit 2
fi
if ! [[ "$health_port" =~ ^[1-9][0-9]*$ ]] || ((health_port > 65535)); then
  echo "ASP_PACKAGED_RUNTIME_HEALTH_PORT must be an integer from 1 to 65535" >&2
  exit 2
fi
if [[ "$port" == "$health_port" ]]; then
  echo "packaged runtime data and health ports must differ" >&2
  exit 2
fi

cert="$workspace/.asp/server-cert.der"
key="$workspace/.asp/server-key.der"
token="$workspace/.asp/auth-token"
session_file="$workspace/client-session.json"
recovery_session_file="$workspace/recovery-session.json"
daemon_log="$workspace/aspd.log"

start_daemon() {
  "$aspd_bin" \
    --production \
    --listen "127.0.0.1:$port" \
    --root "$workspace" \
    --cert "$cert" \
    --key "$key" \
    --auth-token-file "$token" \
    --process-launcher "$launcher" \
    --require-process-launcher \
    --process-cpu-seconds 3600 \
    --exec-timeout-seconds 60 \
    --min-free-bytes 1 \
    --disable-port-forwarding \
    --health-listen "127.0.0.1:$health_port" \
    >"$daemon_log" 2>&1 &
  daemon_pid=$!

  local ready=0
  for _ in $(seq 1 200); do
    if curl -fsS "http://127.0.0.1:$health_port/ready" >/dev/null 2>&1; then
      ready=1
      break
    fi
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
      break
    fi
    sleep 0.05
  done
  if [[ "$ready" != 1 ]]; then
    cat "$daemon_log" >&2
    echo "packaged aspd did not become ready" >&2
    exit 1
  fi
}

start_daemon

"$asp_bin" \
  --cert "$cert" \
  --auth-token-file "$token" \
  --session-file "$session_file" \
  doctor --strict "127.0.0.1:$port" \
  --ready-url "http://127.0.0.1:$health_port/ready" >/dev/null

# Verify the daily shell-profile form as well: ASP_SERVER supplies the
# endpoint while --command keeps the command text from colliding with the
# legacy positional `SERVER COMMAND...` grammar.
env_exec_summary="$workspace/env-exec-summary.log"
env_exec_output=$(ASP_SERVER="127.0.0.1:$port" \
  ASP_CERT="$cert" \
  ASP_AUTH_TOKEN_FILE="$token" \
  ASP_SESSION_FILE="$session_file" \
  "$asp_bin" exec --summary --command 'printf env-server-ok' 2>"$env_exec_summary")
# The human CLI keeps the machine-readable summary on stderr and emits the
# bounded stdout tail on stdout.  Check both channels so this smoke does not
# accidentally assert an adapter/JSONL representation that the CLI never
# promised.
grep -q '^env-server-ok$' <<<"$env_exec_output"
grep -q '^ASP summary: stdout_bytes=13 stderr_bytes=0 stdout_truncated=false stderr_truncated=false$' "$env_exec_summary"

session_id=$("$asp_bin" \
  --cert "$cert" \
  --auth-token-file "$token" \
  --session-file "$session_file" \
  connect "127.0.0.1:$port")
if [[ ! "$session_id" =~ ^[0-9a-fA-F-]{36}$ ]]; then
  echo "packaged connect did not return a durable session UUID: $session_id" >&2
  exit 1
fi

# Exercise the packaged client's explicit concurrent summary path as well as
# its ordinary connection/session setup.  This mode is intentionally limited
# to independent no-output checks, so a release cannot accidentally ship a
# fast path that drops a nonzero result or reorders the markers consumed by an
# automation caller.
parallel_output=$("$asp_bin" \
  --cert "$cert" \
  --auth-token-file "$token" \
  --session-file "$session_file" \
  batch "127.0.0.1:$port" \
  --summary --tail-bytes 0 --parallel 2 \
  --command true --command true 2>&1)
grep -q '^ASP_BATCH_RESULT 0 0$' <<<"$parallel_output"
grep -q '^ASP_BATCH_RESULT 1 0$' <<<"$parallel_output"

set +e
parallel_failure_output=$("$asp_bin" \
  --cert "$cert" \
  --auth-token-file "$token" \
  --session-file "$session_file" \
  batch "127.0.0.1:$port" \
  --summary --tail-bytes 0 --parallel 2 \
  --command true --command false 2>&1)
parallel_failure_status=$?
set -e
if [[ "$parallel_failure_status" != 1 ]] || \
  ! grep -q '^ASP_BATCH_RESULT 0 0$' <<<"$parallel_failure_output" || \
  ! grep -q '^ASP_BATCH_RESULT 1 1$' <<<"$parallel_failure_output"; then
  printf 'packaged parallel batch did not preserve failure semantics (rc=%s):\n%s\n' \
    "$parallel_failure_status" "$parallel_failure_output" >&2
  exit 1
fi

local_file="$workspace/local.txt"
downloaded_file="$workspace/downloaded.txt"
printf 'packaged-runtime-file-%s\n' "$(basename "$archive")" >"$local_file"
ASP_SERVER="127.0.0.1:$port" \
  ASP_CERT="$cert" \
  ASP_AUTH_TOKEN_FILE="$token" \
  ASP_SESSION_FILE="$session_file" \
  "$asp_bin" put "$local_file" packaged-runtime.txt >/dev/null
ASP_SERVER="127.0.0.1:$port" \
  ASP_CERT="$cert" \
  ASP_AUTH_TOKEN_FILE="$token" \
  ASP_SESSION_FILE="$session_file" \
  "$asp_bin" get packaged-runtime.txt "$downloaded_file" >/dev/null
cmp "$local_file" "$downloaded_file"

process_id=$(
  ASP_SERVER="127.0.0.1:$port" \
    ASP_CERT="$cert" \
    ASP_AUTH_TOKEN_FILE="$token" \
    ASP_SESSION_FILE="$session_file" \
    "$asp_bin" spawn --command \
    'i=1; while [ "$i" -le 12 ]; do printf "packaged-runtime-%02d\\n" "$i"; sleep 0.15; i=$((i+1)); done'
)
if [[ -z "$process_id" ]]; then
  echo "packaged spawn did not return a process ID" >&2
  exit 1
fi

# An abrupt daemon loss is the strongest local package-level check: the child
# must outlive aspd and the replacement must reconstruct its state from the
# durable intent/WAL and process log.
kill -KILL "$daemon_pid" 2>/dev/null || true
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
start_daemon

"$asp_bin" \
  --cert "$cert" \
  --auth-token-file "$token" \
  --session-file "$session_file" \
  doctor --strict "127.0.0.1:$port" \
  --ready-url "http://127.0.0.1:$health_port/ready" >/dev/null

# Verify that the packaged client can recover the durable session identity from
# a second cursor location after the daemon restart. This models a new client
# host or a lost local cursor file, not merely a reconnect with the original
# adapter state.
"$asp_bin" \
  --cert "$cert" \
  --auth-token-file "$token" \
  --session-file "$recovery_session_file" \
  resume "127.0.0.1:$port" \
  --session-id "$session_id" \
  --after-event-id 0 > /dev/null 2>"$workspace/explicit-resume.log"
test -s "$recovery_session_file"

for _ in $(seq 1 80); do
  state=$(ASP_SERVER="127.0.0.1:$port" \
    ASP_CERT="$cert" \
    ASP_AUTH_TOKEN_FILE="$token" \
    ASP_SESSION_FILE="$recovery_session_file" \
    "$asp_bin" status "$process_id")
  if jq -e '.running == false' <<<"$state" >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
state=$(ASP_CERT="$cert" \
  ASP_AUTH_TOKEN_FILE="$token" \
  ASP_SESSION_FILE="$recovery_session_file" \
  "$asp_bin" status --server "127.0.0.1:$port" "$process_id")
jq -e '.running == false and .stdout_bytes > 0' <<<"$state" >/dev/null
logs=$(ASP_SERVER="127.0.0.1:$port" \
  ASP_CERT="$cert" \
  ASP_AUTH_TOKEN_FILE="$token" \
  ASP_SESSION_FILE="$recovery_session_file" \
  "$asp_bin" logs "$process_id" --offset 0)
grep -q 'packaged-runtime-12' <<<"$logs"

printf '{"experiment":"packaged-runtime","status":"pass","archive":"%s","process_id":"%s"}\n' \
  "$(basename "$archive")" "$process_id"
