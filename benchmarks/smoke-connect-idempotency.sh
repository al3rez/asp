#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for the durable-session CLI lifecycle. Repeating the
# normal `connect` command must resume the saved session; creating a second
# session is an explicit `--new` choice so a typo cannot strand processes.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_CONNECT_IDEMPOTENCY_PORT:-4546}
health_port=${ASP_CONNECT_IDEMPOTENCY_HEALTH_PORT:-9466}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-connect-idempotency.XXXXXX")
session_file="$workspace/client-session.json"
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

client=(
  "$asp_bin"
  --cert "$workspace/.asp/server-cert.der"
  --auth-token-file "$workspace/.asp/auth-token"
  --session-file "$session_file"
)
server="127.0.0.1:$port"

ready=0
for _ in $(seq 1 100); do
  if "${client[@]}" doctor "$server" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd.log" >&2
  echo "ASP connect-idempotency smoke daemon did not become ready" >&2
  exit 1
fi

resume_metric() {
  curl -fsS "http://127.0.0.1:$health_port/metrics" \
    | awk '$1 == "asp_resume_requests_total" { print $2; found = 1 } END { if (!found) exit 1 }'
}

resume_before=""
for _ in $(seq 1 100); do
  if resume_before=$(resume_metric 2>/dev/null); then
    break
  fi
  sleep 0.05
done
if [[ -z "$resume_before" ]]; then
  cat "$workspace/aspd.log" >&2
  echo "ASP connect-idempotency smoke health metrics did not become ready" >&2
  exit 1
fi

first=$("${client[@]}" connect "$server")
second=$("${client[@]}" connect "$server")
if [[ -z "$first" || "$first" != "$second" ]]; then
  echo "repeated connect did not reuse the saved session: first=$first second=$second" >&2
  exit 1
fi

replacement=$("${client[@]}" connect "$server" --new)
if [[ -z "$replacement" || "$replacement" == "$first" ]]; then
  echo "connect --new did not create an explicit replacement session" >&2
  exit 1
fi

resumed=$("${client[@]}" connect "$server")
if [[ "$resumed" != "$replacement" ]]; then
  echo "connect after --new did not reuse the replacement session: expected=$replacement actual=$resumed" >&2
  exit 1
fi

# A normal connect must not pay the cost of replaying the session journal just
# to reuse a saved UUID. The explicit `resume` command is the only path that
# should advance this transport counter.
resume_after=$(resume_metric)
if [[ "$resume_after" != "$resume_before" ]]; then
  echo "ordinary connect unexpectedly replayed the session journal: before=$resume_before after=$resume_after" >&2
  exit 1
fi

# Stop the listener before using the local operator listing command. Exactly
# two records prove that the default path did not create a hidden third
# session while the explicit replacement created one intentional additional
# identity.
kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
sessions=$(
  "$aspd_bin" \
    --root "$workspace" \
    --cert "$workspace/.asp/server-cert.der" \
    --key "$workspace/.asp/server-key.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    --list-sessions
)
session_count=$(printf '%s\n' "$sessions" | grep -c '"session_id"' || true)
if [[ "$session_count" != 2 ]]; then
  echo "expected exactly two durable sessions after one explicit replacement, found $session_count" >&2
  printf '%s\n' "$sessions" >&2
  exit 1
fi

printf 'connect idempotency smoke passed (session=%s replacement=%s)\n' "$first" "$replacement"
