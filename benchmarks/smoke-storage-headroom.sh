#!/usr/bin/env bash
set -euo pipefail

# End-to-end storage circuit-breaker smoke. It starts a private daemon with a
# threshold one byte above the filesystem's current free space, proving that
# read-only health/inspection remains available while durable mutations fail
# closed before a session or process is created.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_STORAGE_HEADROOM_SMOKE_PORT:-4563}
health_port=${ASP_STORAGE_HEADROOM_SMOKE_HEALTH_PORT:-9463}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-storage-headroom.XXXXXX")
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

# Use the daemon's bounded maximum (1 PiB). Normal CI/desktop volumes are
# many orders of magnitude below this, and a fixed value keeps the smoke
# independent of platform-specific `df`/filesystem accounting.
min_free_bytes=$((1 << 50))

"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --health-listen "127.0.0.1:$health_port" \
  --min-free-bytes "$min_free_bytes" \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

live=0
for _ in $(seq 1 120); do
  if curl -fsS "http://127.0.0.1:$health_port/live" >/dev/null 2>&1; then
    live=1
    break
  fi
  if ! kill -0 "$daemon_pid" 2>/dev/null; then
    break
  fi
  sleep 0.05
done
if [[ "$live" != 1 ]]; then
  cat "$workspace/aspd.log" >&2
  echo "ASP storage-headroom smoke daemon did not become live" >&2
  exit 1
fi

# The supervisor preflight must catch the same condition before a restart,
# once the daemon has provisioned the credentials needed by --validate-config.
set +e
preflight_output=$("$aspd_bin" \
  --validate-config \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --min-free-bytes "$min_free_bytes" 2>&1)
preflight_status=$?
set -e
if [[ "$preflight_status" -eq 0 ]] || [[ "$preflight_output" != *"configured storage headroom is unavailable"* ]]; then
  printf 'storage preflight unexpectedly passed (status=%s):\n%s\n' \
    "$preflight_status" "$preflight_output" >&2
  exit 1
fi

# `/ready` is intentionally unavailable, but it must report the reason rather
# than taking the process down. Keep curl's non-zero 503 result out of `set -e`.
ready_status=$(curl -sS -o "$workspace/ready.json" -w '%{http_code}' \
  "http://127.0.0.1:$health_port/ready")
test "$ready_status" = 503
grep -q '"ready":false' "$workspace/ready.json"
grep -q '"storage_headroom_ok":false' "$workspace/ready.json"

metrics=$(curl -fsS "http://127.0.0.1:$health_port/metrics")
printf '%s\n' "$metrics" | grep -Eq '^asp_storage_free_bytes [0-9]+$'
printf '%s\n' "$metrics" | grep -Eq "^asp_storage_free_bytes_limit $min_free_bytes$"
printf '%s\n' "$metrics" | grep -Eq '^asp_storage_headroom_ok 0$'
printf '%s\n' "$metrics" | grep -Eq '^asp_storage_headroom_rejections_total 0$'

# Health is a read-only authenticated protocol request and remains usable even
# while durable mutations are blocked. OPEN_SESSION is the first mutation and
# must fail with the stable machine-readable storage_headroom code.
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  doctor "127.0.0.1:$port" >"$workspace/doctor.json"
grep -Eq '"protocol_version"[[:space:]]*:' "$workspace/doctor.json"

set +e
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$workspace/client-session.json" \
  connect "127.0.0.1:$port" >"$workspace/connect.out" 2>"$workspace/connect.err"
connect_status=$?
set -e
if [[ "$connect_status" -eq 0 ]]; then
  echo "OPEN_SESSION unexpectedly succeeded below storage headroom" >&2
  exit 1
fi
grep -q 'storage_headroom' "$workspace/connect.err"

metrics=$(curl -fsS "http://127.0.0.1:$health_port/metrics")
printf '%s\n' "$metrics" | grep -Eq '^asp_storage_headroom_rejections_total [1-9][0-9]*$'
test ! -e "$workspace/client-session.json"

printf 'ASP storage-headroom smoke passed (threshold=%s status=%s)\n' \
  "$min_free_bytes" "$connect_status"
