#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for a real interactive PTY attachment crossing a daemon
# restart.  The client is fed through a FIFO so the shell remains attached
# while only aspd is stopped; tmux owns the remote shell and must survive.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_PTY_RECONNECT_SMOKE_PORT:-4554}
health_port=${ASP_PTY_RECONNECT_HEALTH_PORT:-9454}
restart_signal=${ASP_PTY_RECONNECT_DAEMON_SIGNAL:-TERM}
case "$restart_signal" in
  TERM|KILL|INT) ;;
  *)
    printf 'unsupported ASP_PTY_RECONNECT_DAEMON_SIGNAL: %s\n' "$restart_signal" >&2
    exit 2
    ;;
esac
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-pty-reconnect.XXXXXX")
input_fifo="$workspace/input"
daemon_pid=""
shell_pid=""
tmux_session=""

cleanup() {
  exec 3>&- 2>/dev/null || true
  if [[ -n "$shell_pid" ]]; then
    kill "$shell_pid" 2>/dev/null || true
    wait "$shell_pid" 2>/dev/null || true
  fi
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  if [[ -z "$tmux_session" && -f "$workspace/client-session.json" ]]; then
    tmux_session=$(awk -F'"' '/"session_id"[[:space:]]*:/ { print $4; exit }' "$workspace/client-session.json")
  fi
  if [[ "$tmux_session" =~ ^[0-9a-fA-F-]{36}$ ]] && command -v tmux >/dev/null 2>&1; then
    tmux kill-session -t "asp-$tmux_session" 2>/dev/null || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

tmux_available=0
for candidate in /usr/bin/tmux /bin/tmux /usr/local/bin/tmux /opt/homebrew/bin/tmux; do
  if [[ -x "$candidate" ]]; then
    tmux_available=1
    break
  fi
done
if [[ "${ASP_TMUX_PATH:-}" = /* && -x "${ASP_TMUX_PATH}" ]]; then
  tmux_available=1
fi
if [[ "$tmux_available" != 1 ]]; then
  printf 'ASP PTY reconnect smoke skipped (tmux unavailable)\n'
  exit 0
fi

mkfifo "$input_fifo"
start_daemon() {
  "$aspd_bin" \
    --listen "127.0.0.1:$port" \
    --root "$workspace" \
    --cert "$workspace/.asp/server-cert.der" \
    --key "$workspace/.asp/server-key.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    --health-listen "127.0.0.1:$health_port" \
    >"$workspace/aspd-$1.log" 2>&1 &
  daemon_pid=$!
}

wait_ready() {
  local ready=0
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
    cat "$workspace/aspd-$1.log" >&2
    echo "ASP PTY reconnect smoke daemon did not become ready ($1)" >&2
    exit 1
  fi
}

start_daemon initial
wait_ready initial

# The client has no controlling terminal in this smoke, so TerminalGuard is a
# no-op and the FIFO bytes are still delivered as ordinary PTY input.
set +e
"$asp_bin" \
  --prefer-pty-delta \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --session-file "$workspace/client-session.json" \
  shell "127.0.0.1:$port" \
  <"$input_fifo" >"$workspace/shell.out" 2>"$workspace/shell.err" &
shell_pid=$!
set -e

# Opening the writer waits until the client has entered its read loop.
exec 3>"$input_fifo"
# Build the marker from fragments so terminal input echo cannot satisfy the
# output assertion.
printf "printf 'pty-before-'\$(printf 'ok')\\n\n" >&3
before_seen=0
for _ in $(seq 1 100); do
  if grep -q 'pty-before-ok' "$workspace/shell.out" 2>/dev/null; then
    before_seen=1
    break
  fi
  sleep 0.05
done
if [[ "$before_seen" != 1 ]]; then
  cat "$workspace/shell.err" >&2
  cat "$workspace/shell.out" >&2
  echo 'ASP PTY reconnect smoke did not observe pre-restart output' >&2
  exit 1
fi

# Push a distinct marker out of the visible 24-row screen before the daemon
# restart. Truncating the local capture after the marker and filler arrive
# means a later match proves the negotiated bounded scrollback page was
# rendered during reattach, rather than replaying bytes captured before the
# disconnect.
printf "%s\n" "i=0; printf '%s\\n' pty-history-\$(printf marker); while [ \$i -lt 40 ]; do printf 'pty-filler-%s\\n' \"\$i\"; i=\$((i+1)); done" >&3
history_ready=0
for _ in $(seq 1 100); do
  if grep -q 'pty-filler-39' "$workspace/shell.out" 2>/dev/null; then
    history_ready=1
    break
  fi
  sleep 0.05
done
if [[ "$history_ready" != 1 ]]; then
  cat "$workspace/shell.err" >&2
  cat "$workspace/shell.out" >&2
  echo 'ASP PTY reconnect smoke did not prepare scrollback history' >&2
  exit 1
fi
: >"$workspace/shell.out"
if [[ "${ASP_PTY_RECONNECT_DEBUG_PAUSE:-0}" == 1 ]]; then
  sleep 15
fi

# This smoke opts into the plain row-delta capability. Require at least one
# actual delta before restarting so the reconnect assertion exercises the
# negotiated replaceable-state path rather than only the reliable PTY bytes.
delta_seen=0
for _ in $(seq 1 100); do
  if curl -fsS "http://127.0.0.1:$health_port/metrics" 2>/dev/null \
      | awk '$1 == "asp_pty_state_delta_datagrams_sent_total" && $2 > 0 { found = 1 } END { exit(found ? 0 : 1) }'; then
    delta_seen=1
    break
  fi
  sleep 0.05
done
if [[ "$delta_seen" != 1 ]]; then
  cat "$workspace/aspd-initial.log" >&2
  echo 'ASP PTY reconnect smoke did not observe a negotiated PTY row delta' >&2
  exit 1
fi

# Stop only aspd.  The tmux-owned shell must remain alive while the client
# enters its unbounded reconnect loop.
kill "-$restart_signal" "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
sleep 0.3
start_daemon restarted
wait_ready restarted
if [[ "${ASP_PTY_RECONNECT_DEBUG_PAUSE:-0}" == 1 ]]; then
  sleep 15
fi
# Give the client a conservative opportunity to finish RESUME/PTY_OPEN before
# sending input; bytes typed while disconnected are intentionally discarded.
# The reconnect path uses bounded 10-second handshakes, so this delay avoids a
# false negative when the old QUIC close notification and the new daemon's
# readiness probe are observed in different scheduler turns.
sleep 3
# A hard-killed daemon cannot send CONNECTION_CLOSE.  Quinn detects that loss
# through the negotiated keepalive/idle policy, so allow the full bounded
# fifteen-second transport window plus scheduler margin before declaring the
# reconnect broken.  Graceful TERM/INT restarts still complete immediately.
history_seen=0
for _ in $(seq 1 600); do
  if grep -q 'pty-history-marker' "$workspace/shell.out" 2>/dev/null; then
    history_seen=1
    break
  fi
  sleep 0.05
done
if [[ "$history_seen" != 1 ]]; then
  cat "$workspace/shell.err" >&2
  cat "$workspace/shell.out" >&2
  cat "$workspace/aspd-restarted.log" >&2 || true
  echo 'ASP PTY reconnect smoke did not render bounded scrollback' >&2
  exit 1
fi
printf "printf 'pty-after-'\$(printf 'ok')\\n\n" >&3

after_seen=0
for _ in $(seq 1 120); do
  if grep -q 'pty-after-ok' "$workspace/shell.out" 2>/dev/null; then
    after_seen=1
    break
  fi
  sleep 0.05
done
if [[ "$after_seen" != 1 ]]; then
  cat "$workspace/shell.err" >&2
  cat "$workspace/shell.out" >&2
  echo 'ASP PTY reconnect smoke did not observe post-restart output' >&2
  exit 1
fi

# Detach cleanly using ASP's documented escape byte, then ensure the client
# does not remain stuck in its reconnect loop.
printf '\035' >&3
exec 3>&-
for _ in $(seq 1 100); do
  if ! kill -0 "$shell_pid" 2>/dev/null; then
    break
  fi
  sleep 0.05
done
if kill -0 "$shell_pid" 2>/dev/null; then
  cat "$workspace/shell.err" >&2
  echo 'ASP PTY reconnect shell did not detach' >&2
  exit 1
fi
wait "$shell_pid" 2>/dev/null || true
shell_pid=""

printf 'ASP PTY reconnect smoke passed\n'
