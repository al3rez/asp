#!/usr/bin/env bash
set -euo pipefail

# Release-level guardrail smoke: an attached EXEC must not pin a daemon
# forever, while the process group and durable exit result remain observable.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_TIMEOUT_SMOKE_PORT:-4548}
health_port=${ASP_TIMEOUT_SMOKE_HEALTH_PORT:-9448}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-exec-timeout.XXXXXX")
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --exec-timeout-seconds 1 \
  --health-listen "127.0.0.1:$health_port" \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

ready=0
for _ in $(seq 1 100); do
  if "$asp_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$workspace/client-session.json" \
      doctor "127.0.0.1:$port" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd.log" >&2
  echo "ASP timeout smoke daemon did not become ready" >&2
  exit 1
fi

set +e
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$workspace/client-session.json" \
  exec "127.0.0.1:$port" "sleep 30" >"$workspace/exec.out" 2>"$workspace/exec.err"
status=$?
set -e
if [[ "$status" -ne 124 ]]; then
  echo "expected timed-out EXEC to return 124, got $status" >&2
  cat "$workspace/exec.err" >&2 || true
  exit 1
fi

metrics=$(curl -fsS "http://127.0.0.1:$health_port/metrics")
printf '%s\n' "$metrics" | awk '$1 == "asp_process_timeouts_total" && $2 >= 1 { found = 1 } END { exit(found ? 0 : 1) }'

printf 'ASP EXEC timeout smoke passed (exit=%s)\n' "$status"
