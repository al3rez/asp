#!/usr/bin/env bash
set -euo pipefail

# Verify that a short-lived client which exits after connecting still drains
# its QUIC endpoint.  A lost CONNECTION_CLOSE used to leave the server's
# per-principal connection lease occupied until the idle timeout, so a burst
# of ordinary command errors could look like a capacity incident.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_CLIENT_FAILURE_CLEANUP_PORT:-4566}
health_port=${ASP_CLIENT_FAILURE_CLEANUP_HEALTH_PORT:-9466}

workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-client-failure-cleanup.XXXXXX")
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
  --health-listen "127.0.0.1:$health_port" \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

ready=0
for _ in $(seq 1 120); do
  if "$asp_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$workspace/client-session.json" \
      doctor "127.0.0.1:$port" >/dev/null 2>"$workspace/doctor.err"; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd.log" >&2 || true
  cat "$workspace/doctor.err" >&2 || true
  echo 'ASP client failure-cleanup smoke daemon did not become ready' >&2
  exit 1
fi

# The readiness probe is itself short-lived. Wait for its lease to disappear
# before measuring the intentionally failing invocation.
for _ in $(seq 1 100); do
  active=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
    | awk '$1 == "asp_active_connections" { print $2; found = 1 } END { if (!found) exit 1 }')
  if [[ "$active" == 0 ]]; then
    break
  fi
  sleep 0.05
done
test "${active:-1}" = 0

set +e
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$workspace/client-session.json" \
  logs "127.0.0.1:$port" 00000000-0000-0000-0000-000000000000 \
  --stream not-a-stream >"$workspace/failure.out" 2>"$workspace/failure.err"
status=$?
set -e
if [[ "$status" == 0 ]]; then
  echo 'invalid logs stream unexpectedly succeeded' >&2
  exit 1
fi

# Cleanup should be observable promptly; allowing the full 15-second QUIC
# idle timeout would hide the regression this smoke is intended to catch.
released=0
for _ in $(seq 1 100); do
  active=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
    | awk '$1 == "asp_active_connections" { print $2; found = 1 } END { if (!found) exit 1 }')
  if [[ "$active" == 0 ]]; then
    released=1
    break
  fi
  sleep 0.05
done
if [[ "$released" != 1 ]]; then
  cat "$workspace/failure.err" >&2 || true
  cat "$workspace/aspd.log" >&2 || true
  echo "client failure left an active server connection (active=${active:-unknown})" >&2
  exit 1
fi

printf 'ASP client failure-cleanup smoke passed\n'
