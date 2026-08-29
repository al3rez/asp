#!/usr/bin/env bash
set -euo pipefail

# End-to-end PORT_OPEN policy smoke. It proves that an explicitly allowed
# loopback service can be reached, while an unlisted service is rejected before
# ASP opens a TCP connection to it.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_PORT_POLICY_SMOKE_PORT:-4562}
health_port=${ASP_PORT_POLICY_SMOKE_HEALTH_PORT:-9462}
forward_port=${ASP_PORT_POLICY_SMOKE_FORWARD_PORT:-18082}
denied_forward_port=${ASP_PORT_POLICY_SMOKE_DENIED_FORWARD_PORT:-18083}
allowed_target_port=${ASP_PORT_POLICY_SMOKE_ALLOWED_TARGET_PORT:-18081}
denied_target_port=${ASP_PORT_POLICY_SMOKE_DENIED_TARGET_PORT:-18084}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-port-policy.XXXXXX")
session_file="$workspace/client-session.json"
daemon_pid=""
forward_pid=""
denied_forward_pid=""
allowed_server_pid=""
denied_server_pid=""

cleanup() {
  for pid in "$forward_pid" "$denied_forward_pid" "$daemon_pid" "$allowed_server_pid" "$denied_server_pid"; do
    if [[ -n "$pid" ]]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

start_echo_server() {
  local target_port=$1
  local ready_file=$2
  local stop_file=$3
  python3 - "$target_port" "$ready_file" "$stop_file" <<'PY' &
import pathlib
import socket
import sys

port = int(sys.argv[1])
ready = pathlib.Path(sys.argv[2])
stop = pathlib.Path(sys.argv[3])
server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", port))
server.listen(8)
ready.write_text("ready")
try:
    while not stop.exists():
        server.settimeout(0.2)
        try:
            client, _ = server.accept()
        except socket.timeout:
            continue
        with client:
            client.settimeout(2)
            try:
                data = client.recv(65536)
            except socket.timeout:
                data = b""
            if data == b"__asp_stop__":
                stop.write_text("stopped")
                break
            if data:
                client.sendall(data)
finally:
    server.close()
PY
}

start_daemon() {
  "$aspd_bin" \
    --listen "127.0.0.1:$port" \
    --root "$workspace" \
    --cert "$workspace/.asp/server-cert.der" \
    --key "$workspace/.asp/server-key.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    --health-listen "127.0.0.1:$health_port" \
    --port-target "127.0.0.1:$allowed_target_port" \
    >>"$workspace/aspd.log" 2>&1 &
  daemon_pid=$!
}

wait_for_daemon() {
  local ready=0
  for _ in $(seq 1 120); do
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
      break
    fi
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
    echo "ASP PORT_OPEN policy smoke daemon did not become ready" >&2
    exit 1
  fi
}

allowed_ready="$workspace/allowed.ready"
allowed_stop="$workspace/allowed.stop"
start_echo_server "$allowed_target_port" "$allowed_ready" "$allowed_stop"
allowed_server_pid=$!
denied_ready="$workspace/denied.ready"
denied_stop="$workspace/denied.stop"
start_echo_server "$denied_target_port" "$denied_ready" "$denied_stop"
denied_server_pid=$!
for _ in $(seq 1 100); do
  [[ -f "$allowed_ready" && -f "$denied_ready" ]] && break
  sleep 0.01
done
test -f "$allowed_ready"
test -f "$denied_ready"

start_daemon
wait_for_daemon

"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  connect "127.0.0.1:$port" >/dev/null

"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  forward "127.0.0.1:$port" \
  --listen "127.0.0.1:$forward_port" \
  --target "127.0.0.1:$allowed_target_port" \
  >"$workspace/forward.log" 2>&1 &
forward_pid=$!

for _ in $(seq 1 100); do
  if python3 - "$forward_port" <<'PY'
import socket
import sys

client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
client.settimeout(0.2)
try:
    client.connect(("127.0.0.1", int(sys.argv[1])))
except OSError:
    sys.exit(1)
finally:
    client.close()
PY
  then
    break
  fi
  sleep 0.05
done

python3 - "$forward_port" <<'PY'
import socket
import sys

payload = b"asp-port-policy-ok"
client = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=2)
with client:
    client.sendall(payload)
    received = client.recv(len(payload))
if received != payload:
    raise SystemExit(f"allowed PORT_OPEN payload mismatch: {received!r}")
PY

# Replacing only the daemon must leave the local forwarding listener alive. The
# existing TCP flow is intentionally stream-scoped and may fail, but a new
# local flow must use the forwarding client's resumed QUIC connection without
# requiring the user or supervisor to restart `asp forward`.
resumes_before=$(grep -c 'ASP forward transport resumed' "$workspace/forward.log" || true)
old_daemon_pid=$daemon_pid
kill "$old_daemon_pid"
wait "$old_daemon_pid" 2>/dev/null || true
daemon_pid=""
start_daemon
wait_for_daemon

resumed=0
for _ in $(seq 1 160); do
  resumes_after=$(grep -c 'ASP forward transport resumed' "$workspace/forward.log" || true)
  if (( resumes_after > resumes_before )); then
    resumed=1
    break
  fi
  sleep 0.05
done
if [[ "$resumed" != 1 ]]; then
  cat "$workspace/forward.log" >&2
  echo "ASP forwarding listener did not reconnect after daemon replacement" >&2
  exit 1
fi

python3 - "$forward_port" <<'PY'
import socket
import sys

payload = b"asp-port-policy-reconnected"
client = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=2)
with client:
    client.sendall(payload)
    received = client.recv(len(payload))
if received != payload:
    raise SystemExit(f"reconnected PORT_OPEN payload mismatch: {received!r}")
PY

kill "$forward_pid" 2>/dev/null || true
wait "$forward_pid" 2>/dev/null || true
forward_pid=""

"$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$session_file" \
  forward "127.0.0.1:$port" \
  --listen "127.0.0.1:$denied_forward_port" \
  --target "127.0.0.1:$denied_target_port" \
  >"$workspace/denied-forward.log" 2>&1 &
denied_forward_pid=$!

for _ in $(seq 1 100); do
  if python3 - "$denied_forward_port" <<'PY'
import socket
import sys

client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
client.settimeout(0.2)
try:
    client.connect(("127.0.0.1", int(sys.argv[1])))
except OSError:
    sys.exit(1)
finally:
    client.close()
PY
  then
    break
  fi
  sleep 0.05
done

python3 - "$denied_forward_port" <<'PY'
import socket
import sys

client = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=2)
with client:
    client.sendall(b"must-be-denied")
    client.settimeout(2)
    try:
        received = client.recv(1)
    except (ConnectionResetError, ConnectionAbortedError, BrokenPipeError, socket.timeout):
        received = b""
if received:
    raise SystemExit(f"unlisted PORT_OPEN target unexpectedly returned data: {received!r}")
PY

if [[ -f "$denied_stop" ]]; then
  echo "unlisted PORT_OPEN target was dialed despite the allowlist" >&2
  exit 1
fi

policy_entries=$(curl -fsS "http://127.0.0.1:$health_port/metrics" | awk '$1 == "asp_port_target_policy_entries" { print $2 }')
policy_rejections=$(curl -fsS "http://127.0.0.1:$health_port/metrics" | awk '$1 == "asp_port_target_rejections_total" { print $2 }')
test "$policy_entries" -eq 1
test "$policy_rejections" -ge 1

printf 'ASP PORT_OPEN policy smoke passed (entries=%s rejections=%s)\n' \
  "$policy_entries" "$policy_rejections"
