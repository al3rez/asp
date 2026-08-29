#!/usr/bin/env bash
set -euo pipefail

# Build and exercise the production-shaped container without publishing its
# UDP port. The client runs inside the same isolated container, so this smoke
# checks image contents, read-only root/tmpfs behavior, credential generation,
# authentication, EXEC, SPAWN, and durable process-log retrieval.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
docker_bin=${DOCKER_BIN:-docker}
tag="asp-container-smoke:${PPID}-${RANDOM}"
name="asp-container-smoke-${PPID}-${RANDOM}"
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-container-smoke.XXXXXX")

cleanup() {
  "$docker_bin" rm -f "$name" >/dev/null 2>&1 || true
  "$docker_bin" image rm "$tag" >/dev/null 2>&1 || true
  rm -rf -- "$workspace"
}
trap cleanup EXIT INT TERM

command -v "$docker_bin" >/dev/null 2>&1 || {
  echo "docker is required for the container smoke" >&2
  exit 1
}

"$docker_bin" build --file "$repo_root/deploy/container/Dockerfile" --tag "$tag" "$repo_root"
"$docker_bin" run --detach \
  --name "$name" \
  --read-only \
  --tmpfs /tmp:mode=1777 \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  --pids-limit=512 \
  --memory=2g \
  --cpus=2 \
  --volume "$workspace:/workspace" \
  "$tag" >/dev/null

ready=0
for _ in $(seq 1 120); do
  if "$docker_bin" exec "$name" /usr/local/bin/asp \
      --cert /workspace/.asp/server-cert.der \
      --auth-token-file /workspace/.asp/auth-token \
      --session-file /workspace/.asp/client-session.json \
      doctor --strict 127.0.0.1:4433 \
      --ready-url http://127.0.0.1:9443/ready >/dev/null 2>"$workspace/doctor.err"; then
    ready=1
    break
  fi
  sleep 0.25
done
if [[ "$ready" != 1 ]]; then
  "$docker_bin" logs "$name" >&2 || true
  cat "$workspace/doctor.err" >&2 || true
  echo "containerized ASP daemon did not become ready" >&2
  exit 1
fi
test "$($docker_bin exec "$name" /usr/bin/id -u)" = 10001
"$docker_bin" exec "$name" /usr/bin/test -w /workspace
"$docker_bin" exec "$name" /usr/bin/test -x /usr/local/libexec/asp-worker-wrapper

output=$("$docker_bin" exec "$name" /usr/local/bin/asp \
  --cert /workspace/.asp/server-cert.der \
  --auth-token-file /workspace/.asp/auth-token \
  --session-file /workspace/.asp/client-session.json \
  exec 127.0.0.1:4433 "printf container-exec-ok")
if [[ "$output" != *container-exec-ok* ]]; then
  echo "container EXEC did not return its marker: $output" >&2
  exit 1
fi

process_id=$("$docker_bin" exec "$name" /usr/local/bin/asp \
  --cert /workspace/.asp/server-cert.der \
  --auth-token-file /workspace/.asp/auth-token \
  --session-file /workspace/.asp/client-session.json \
  spawn 127.0.0.1:4433 "sleep 1; printf container-log-ok")
sleep 2
logs=$("$docker_bin" exec "$name" /usr/local/bin/asp \
  --cert /workspace/.asp/server-cert.der \
  --auth-token-file /workspace/.asp/auth-token \
  --session-file /workspace/.asp/client-session.json \
  logs 127.0.0.1:4433 "$process_id" --offset 0)
if [[ "$logs" != *container-log-ok* ]]; then
  echo "container durable log retrieval did not return its marker: $logs" >&2
  exit 1
fi

printf 'ASP container smoke passed (process=%s)\n' "$process_id"
