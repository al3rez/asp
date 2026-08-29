#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for live TLS reload. It uses a private loopback daemon
# and verifies that SIGHUP accepts a complete pair but never generates a new
# identity when a replacement certificate is temporarily absent.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-tls-reload.XXXXXX")
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

RUST_LOG=info "$aspd_bin" \
  --listen 127.0.0.1:0 \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

for _ in $(seq 1 100); do
  grep -q "ASP server ready" "$workspace/aspd.log" && break
  sleep 0.05
done
grep -q "ASP server ready" "$workspace/aspd.log"

mkdir "$workspace/pins"
cp "$workspace/.asp/server-cert.der" "$workspace/pins/old.der"
"$asp_bin" \
  --cert "$workspace/pins" \
  --auth-token-file "$workspace/.asp/auth-token" \
  doctor "127.0.0.1:$(sed -n 's/.*listening on 127.0.0.1:\([0-9][0-9]*\).*/\1/p' "$workspace/aspd.log" | head -n 1)" \
  >/dev/null

kill -HUP "$daemon_pid"
for _ in $(seq 1 100); do
  grep -q "reloaded TLS configuration" "$workspace/aspd.log" && break
  sleep 0.05
done
grep -q "reloaded TLS configuration" "$workspace/aspd.log"

mv "$workspace/.asp/server-cert.der" "$workspace/.asp/server-cert.der.missing"
kill -HUP "$daemon_pid"
for _ in $(seq 1 100); do
  grep -q "TLS reload rejected" "$workspace/aspd.log" && break
  sleep 0.05
done
grep -q "TLS reload rejected" "$workspace/aspd.log"
test ! -e "$workspace/.asp/server-cert.der"
mv "$workspace/.asp/server-cert.der.missing" "$workspace/.asp/server-cert.der"

kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
printf 'ASP TLS reload smoke passed\n'
