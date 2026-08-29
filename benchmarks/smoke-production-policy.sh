#!/usr/bin/env bash
set -euo pipefail

# Configuration-level acceptance test for the explicit production profile.
# The profile deliberately refuses to start before it can create state when
# authentication, health/metrics, an external process boundary, or command
# limits are missing. A separate positive launch proves that a complete
# profile reaches the normal daemon initialization path.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_PRODUCTION_POLICY_PORT:-4595}
health_port=${ASP_PRODUCTION_POLICY_HEALTH_PORT:-9455}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-production-policy.XXXXXX")
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

if ! [[ "$port" =~ ^[1-9][0-9]*$ ]] || ((port > 65535)); then
  echo "ASP_PRODUCTION_POLICY_PORT must be an integer from 1 to 65535" >&2
  exit 2
fi
if ! [[ "$health_port" =~ ^[1-9][0-9]*$ ]] || ((health_port > 65535)); then
  echo "ASP_PRODUCTION_POLICY_HEALTH_PORT must be an integer from 1 to 65535" >&2
  exit 2
fi
if [[ "$port" == "$health_port" ]]; then
  echo "ASP_PRODUCTION_POLICY_PORT and ASP_PRODUCTION_POLICY_HEALTH_PORT must differ" >&2
  exit 2
fi

assert_rejected() {
  local expected=$1
  shift
  local output rc
  set +e
  output=$("$aspd_bin" "$@" 2>&1)
  rc=$?
  set -e
  if [[ "$rc" -eq 0 ]]; then
    echo "production policy unexpectedly accepted: $*" >&2
    exit 1
  fi
  if [[ "$output" != *"$expected"* ]]; then
    printf 'unexpected production-policy rejection for %s:\n%s\n' "$*" "$output" >&2
    exit 1
  fi
}

base=(--production --root "$workspace" --listen "127.0.0.1:$port")
assert_rejected "requires client authentication" "${base[@]}" --insecure-no-auth
assert_rejected "requires --health-listen" "${base[@]}"
assert_rejected "requires --process-launcher" "${base[@]}" \
  --health-listen 127.0.0.1:0
assert_rejected "requires --process-cpu-seconds" "${base[@]}" \
  --health-listen 127.0.0.1:0 \
  --process-launcher /bin/sh
assert_rejected "requires --exec-timeout-seconds" "${base[@]}" \
  --health-listen 127.0.0.1:0 \
  --process-launcher /bin/sh \
  --process-cpu-seconds 3600
assert_rejected "forbids --allow-process-privilege-gain" "${base[@]}" \
  --health-listen 127.0.0.1:0 \
  --process-launcher /bin/sh \
  --process-cpu-seconds 3600 \
  --exec-timeout-seconds 60 \
  --allow-process-privilege-gain
assert_rejected "requires --min-free-bytes" "${base[@]}" \
  --health-listen 127.0.0.1:0 \
  --process-launcher /bin/sh \
  --process-cpu-seconds 3600 \
  --exec-timeout-seconds 60 \
  --disable-port-forwarding
assert_rejected "requires --port-target entries or --disable-port-forwarding" "${base[@]}" \
  --health-listen 127.0.0.1:0 \
  --process-launcher /bin/sh \
  --process-cpu-seconds 3600 \
  --exec-timeout-seconds 60 \
  --min-free-bytes 1

chmod 0777 "$workspace"
assert_rejected "group/world writes" "${base[@]}" \
  --health-listen 127.0.0.1:0 \
  --process-launcher /bin/sh \
  --process-cpu-seconds 3600 \
  --exec-timeout-seconds 60 \
  --min-free-bytes 1 \
  --disable-port-forwarding
chmod 0700 "$workspace"

# A complete policy must still fail before opening a listener when the durable
# PTY supervisor is missing.  Keep this override scoped to the one invocation;
# the positive launch below uses the host's normal tmux installation.
set +e
tmux_output=$(ASP_TMUX_PATH="$workspace/missing-tmux" "$aspd_bin" \
  "${base[@]}" \
  --health-listen 127.0.0.1:0 \
  --process-launcher /bin/sh \
  --process-cpu-seconds 3600 \
  --exec-timeout-seconds 60 \
  --min-free-bytes 1 \
  --disable-port-forwarding 2>&1)
tmux_rc=$?
set -e
if [[ "$tmux_rc" -eq 0 || "$tmux_output" != *"requires an executable tmux"* ]]; then
  printf 'missing tmux was not rejected before startup (rc=%s):\n%s\n' \
    "$tmux_rc" "$tmux_output" >&2
  exit 1
fi

launcher="$workspace/launcher.sh"
printf '%s\n' '#!/bin/sh' 'exec "$@"' >"$launcher"
chmod 700 "$launcher"

"$aspd_bin" \
  "${base[@]}" \
  --health-listen "127.0.0.1:$health_port" \
  --process-launcher "$launcher" \
  --process-cpu-seconds 3600 \
  --exec-timeout-seconds 60 \
  --min-free-bytes 1 \
  --disable-port-forwarding \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

ready=0
for _ in $(seq 1 100); do
  if curl -fsS "http://127.0.0.1:$health_port/live" >/dev/null 2>&1; then
    ready=1
    break
  fi
  if ! kill -0 "$daemon_pid" 2>/dev/null; then
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd.log" >&2
  echo "complete production profile did not reach daemon readiness" >&2
  exit 1
fi

# The authenticated client-side gate must agree with the production profile
# before an operator routes work to it.  Keep the health JSON on stdout out of
# the smoke log, but require the strict checks (protocol, authentication, and
# durable tmux) to pass against the same endpoint.
"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$workspace/client-session.json" \
  doctor --strict "127.0.0.1:$port" \
  --ready-url "http://127.0.0.1:$health_port/ready" >/dev/null

# Exercise the machine-readable readiness contract itself, including the
# live-auth drift gate.  A supervisor needs an actionable reason rather than
# only an HTTP status, and restoring the same file proves the daemon recovers
# without a restart after a valid atomic credential replacement.
healthy_body=$(curl -fsS "http://127.0.0.1:$health_port/ready")
jq -e '.ready == true and (.ready_reasons | length) == 0 and .quic_stateless_retry_enabled == true' <<<"$healthy_body" >/dev/null
# The production profile enables Quinn's native stateless-retry path.  A
# loopback client may already be address-validated and therefore legitimately
# leave the counter at zero, but the metric names must be present so an
# operator can observe unvalidated remote handshakes on the real interface.
metrics_body=$(curl -fsS "http://127.0.0.1:$health_port/metrics")
grep -Eq '^asp_quic_stateless_retry_enabled 1$' <<<"$metrics_body"
grep -Eq '^asp_quic_stateless_retries_total [0-9]+$' <<<"$metrics_body"
grep -Eq '^asp_quic_stateless_retry_failures_total [0-9]+$' <<<"$metrics_body"
grep -Eq '^asp_resume_replay_limited_total 0$' <<<"$metrics_body"
auth_backup="$workspace/auth-token.backup"
mv "$workspace/.asp/auth-token" "$auth_backup"
unhealthy_status=$(curl -sS -o "$workspace/unhealthy-ready.json" \
  -w '%{http_code}' "http://127.0.0.1:$health_port/ready")
if [[ "$unhealthy_status" != 503 ]]; then
  echo "readiness did not fail closed after auth source removal (status=$unhealthy_status)" >&2
  exit 1
fi
jq -e '.ready == false and (.ready_reasons | index("auth_config_unhealthy")) != null' \
  "$workspace/unhealthy-ready.json" >/dev/null
mv "$auth_backup" "$workspace/.asp/auth-token"
healthy_body=$(curl -fsS "http://127.0.0.1:$health_port/ready")
jq -e '.ready == true and (.ready_reasons | length) == 0 and .quic_stateless_retry_enabled == true' <<<"$healthy_body" >/dev/null

printf 'ASP production-policy smoke passed\n'
