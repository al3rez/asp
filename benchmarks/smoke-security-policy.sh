#!/usr/bin/env bash
set -euo pipefail

# Configuration-level safety smoke. It runs before TLS/state initialization
# can create anything, so an unsafe deployment fails closed without starting a
# listener or generating credentials.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-security-policy.XXXXXX")
trap 'rm -rf -- "$workspace"' EXIT INT TERM

assert_rejected() {
  local expected=$1
  shift
  local output rc
  set +e
  output=$("$aspd_bin" "$@" 2>&1)
  rc=$?
  set -e
  if [[ "$rc" -eq 0 ]]; then
    echo "unsafe configuration was accepted: $*" >&2
    exit 1
  fi
  if [[ "$output" != *"$expected"* ]]; then
    printf 'unexpected rejection for %s:\n%s\n' "$*" "$output" >&2
    exit 1
  fi
}

assert_rejected \
  "unauthenticated non-loopback" \
  --root "$workspace" \
  --listen 192.0.2.10:4433 \
  --allow-non-loopback \
  --insecure-no-auth

assert_rejected \
  "health endpoint must bind to a loopback" \
  --root "$workspace" \
  --listen 127.0.0.1:4433 \
  --insecure-no-auth \
  --health-listen 192.0.2.10:9443

assert_rejected \
  "v0 port forwarding is restricted to loopback" \
  --root "$workspace" \
  --port-target 192.0.2.10:3000

assert_rejected \
  "PORT_OPEN target must be HOST:PORT" \
  --root "$workspace" \
  --port-target not-a-target

printf 'ASP security-policy smoke passed\n'
