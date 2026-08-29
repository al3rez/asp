#!/usr/bin/env bash
set -euo pipefail

# Exercise deploy/upgrade-release.sh against real packaged ASP binaries. The
# first rollout deliberately leaves the daemon down after switching pointers,
# so the helper must restore the previous release and restart it. The second
# rollout starts the new binary normally and must complete. This is an
# operator-workflow smoke, not a substitute for a historical-binary or
# multi-host upgrade qualification.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
archive=${1:-${ASP_RELEASE_ARCHIVE:-}}
checksum=${2:-${ASP_RELEASE_CHECKSUM:-}}
if [[ -z "$archive" ]]; then
  cat >&2 <<'USAGE'
usage: smoke-upgrade-release.sh RELEASE.tar.gz [RELEASE.sha256]

The archive must be a verified ASP release. Set ASP_RELEASE_ARCHIVE and
ASP_RELEASE_CHECKSUM instead of passing positional arguments when desired.
USAGE
  exit 2
fi

source_verifier=${ASP_RELEASE_VERIFIER:-"$repo_root/deploy/verify-release.sh"}
if [[ ! -x "$source_verifier" ]]; then
  echo "release verifier is missing or not executable: $source_verifier" >&2
  exit 2
fi
if [[ -n "$checksum" ]]; then
  "$source_verifier" "$archive" "$checksum" >/dev/null
else
  "$source_verifier" "$archive" >/dev/null
fi

extract_dir=$(mktemp -d "${TMPDIR:-/tmp}/asp-upgrade-release.XXXXXX")
verifier=${ASP_RELEASE_PACKAGED_VERIFIER:-"$extract_dir/deploy/verify-release.sh"}
installer=${ASP_RELEASE_PACKAGED_INSTALLER:-"$extract_dir/deploy/install-release.sh"}
upgrader=${ASP_RELEASE_PACKAGED_UPGRADER:-"$extract_dir/deploy/upgrade-release.sh"}
tar -xzf "$archive" -C "$extract_dir"
for executable in "$verifier" "$installer" "$upgrader"; do
  if [[ ! -x "$executable" ]]; then
    echo "release deployment helper is missing or not executable: $executable" >&2
    exit 2
  fi
done
if [[ -n "$checksum" ]]; then
  "$verifier" "$archive" "$checksum" >/dev/null
else
  "$verifier" "$archive" >/dev/null
fi

# URL validation happens before any release/pointer mutation. Exercise that
# boundary from the extracted helper so a future edit cannot accidentally turn
# this deployment tool into a general-purpose or remote HTTP probe.
for invalid_ready_url in \
  'https://127.0.0.1:9443/ready' \
  'http://localhost:9443/ready' \
  'http://127.0.0.2:9443/ready' \
  'http://127.0.0.1:1oops/ready' \
  'http://127.0.0.1:0/ready' \
  'http://127.0.0.1:65536/ready'; do
  set +e
  invalid_output=$(
    "$upgrader" \
      --prefix "/tmp/asp-upgrade-invalid-$$" \
      --ready-url "$invalid_ready_url" \
      --restart-command true \
      --skip-current-ready \
      "$archive" 2>&1
  )
  invalid_rc=$?
  set -e
  if [[ "$invalid_rc" != 2 || "$invalid_output" != *"--ready-url"* ]]; then
    printf 'upgrade helper accepted invalid readiness URL (rc=%s): %s\n%s\n' \
      "$invalid_rc" "$invalid_ready_url" "$invalid_output" >&2
    exit 1
  fi
done

workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-upgrade-smoke.XXXXXX")
prefix="$workspace/prefix"
daemon_workspace="$workspace/daemon-workspace"
pid_file="$workspace/aspd.pid"
mode_file="$workspace/restart-mode"
daemon_log="$workspace/aspd.log"
daemon_pid=""

cleanup() {
  if [[ -f "$pid_file" ]]; then
    daemon_pid=$(cat "$pid_file" 2>/dev/null || true)
    if [[ "$daemon_pid" =~ ^[0-9]+$ ]]; then
      kill -KILL "$daemon_pid" 2>/dev/null || true
      wait "$daemon_pid" 2>/dev/null || true
    fi
  fi
  rm -rf -- "$workspace" "$extract_dir"
}
trap cleanup EXIT INT TERM

# The upgrader must reject an unsafe ancestor before it creates an upgrade
# lock or reaches the current-pointer check.  Exercise both classes covered by
# the installer trust walk against the packaged helper.
prefix_trust_root="$workspace/prefix-trust"
mkdir -p -- "$prefix_trust_root/real" "$prefix_trust_root/writable"
ln -s -- "$prefix_trust_root/real" "$prefix_trust_root/link"
chmod 0777 "$prefix_trust_root/writable"

# A clean preflight must not create the missing leaf, release directories, or
# a lock; it is safe to run from an operator's promotion check before any
# release material is available.
clean_preflight_prefix="$prefix_trust_root/clean/asp"
"$installer" --prefix "$clean_preflight_prefix" --validate-prefix
if [[ -e "$clean_preflight_prefix" || -L "$clean_preflight_prefix" ||
  -e "${clean_preflight_prefix%/asp}/releases" ||
  -e "${clean_preflight_prefix%/asp}/.install.lock" ]]; then
  echo "installer prefix preflight mutated the prefix" >&2
  exit 1
fi

for unsafe_prefix in \
  "$prefix_trust_root/link/asp" \
  "$prefix_trust_root/writable/asp"; do
  set +e
  unsafe_output=$(
    "$upgrader" \
      --prefix "$unsafe_prefix" \
      --ready-url 'http://127.0.0.1:1/ready' \
      --restart-command true \
      --skip-current-ready \
      "$archive" 2>&1
  )
  unsafe_rc=$?
  set -e
  if [[ "$unsafe_rc" == 0 || "$unsafe_output" != *"prefix"* ]]; then
    printf 'upgrade helper accepted unsafe prefix (rc=%s): %s\n%s\n' \
      "$unsafe_rc" "$unsafe_prefix" "$unsafe_output" >&2
    exit 1
  fi
done

pick_port() {
  python3 - <<'PY'
import socket
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}

port=${ASP_UPGRADE_SMOKE_PORT:-$(pick_port)}
health_port=${ASP_UPGRADE_SMOKE_HEALTH_PORT:-$(pick_port)}
if ! [[ "$port" =~ ^[1-9][0-9]*$ ]] || ((port > 65535)); then
  echo "ASP_UPGRADE_SMOKE_PORT must be an integer from 1 to 65535" >&2
  exit 2
fi
if ! [[ "$health_port" =~ ^[1-9][0-9]*$ ]] || ((health_port > 65535)); then
  echo "ASP_UPGRADE_SMOKE_HEALTH_PORT must be an integer from 1 to 65535" >&2
  exit 2
fi
if [[ "$port" == "$health_port" ]]; then
  echo "upgrade smoke data and health ports must differ" >&2
  exit 2
fi

mkdir -p -- "$daemon_workspace"
chmod 700 "$daemon_workspace"
if [[ -n "$checksum" ]]; then
  "$installer" --prefix "$prefix" "$archive" "$checksum" >/dev/null
else
  "$installer" --prefix "$prefix" "$archive" >/dev/null
fi

archive_base=$(basename -- "$archive")
upgrade_archive="$workspace/${archive_base%.tar.gz}-upgrade.tar.gz"
cp -- "$archive" "$upgrade_archive"
upgrade_checksum="${upgrade_archive%.tar.gz}.sha256"
if command -v shasum >/dev/null 2>&1; then
  (cd "$workspace" && shasum -a 256 "$(basename "$upgrade_archive")" >"$(basename "$upgrade_checksum")")
else
  (cd "$workspace" && sha256sum "$(basename "$upgrade_archive")" >"$(basename "$upgrade_checksum")")
fi

initial_target=$(readlink -- "$prefix/current")

# This generated supervisor shim is intentionally tiny: it starts the binary
# selected by the atomic current pointer, and its fail_once mode simulates a
# restart that succeeds at the supervisor layer but never reaches readiness.
# The production helper must then roll back and invoke the shim again.
restart_script="$workspace/restart-supervisor.sh"
printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' \
  "prefix=$(printf '%q' "$prefix")" \
  "workspace=$(printf '%q' "$daemon_workspace")" \
  "pid_file=$(printf '%q' "$pid_file")" \
  "mode_file=$(printf '%q' "$mode_file")" \
  "daemon_log=$(printf '%q' "$daemon_log")" \
  "port=$(printf '%q' "$port")" \
  "health_port=$(printf '%q' "$health_port")" \
  'if [[ "$(cat "$mode_file" 2>/dev/null || true)" == fail_once ]]; then' \
  '    if [[ -f "$pid_file" ]]; then kill -KILL "$(cat "$pid_file")" 2>/dev/null || true; fi' \
  '    rm -f -- "$pid_file"' \
  '    printf "%s\n" normal >"$mode_file"' \
  '    exit 0' \
  'fi' \
  'if [[ -f "$pid_file" ]]; then kill -KILL "$(cat "$pid_file")" 2>/dev/null || true; fi' \
  'rm -f -- "$pid_file"' \
  '"$prefix/current/bin/aspd" --production --listen "127.0.0.1:$port" --root "$workspace" --cert "$workspace/.asp/server-cert.der" --key "$workspace/.asp/server-key.der" --auth-token-file "$workspace/.asp/auth-token" --process-launcher "$prefix/current/deploy/container/asp-worker-wrapper" --require-process-launcher --process-cpu-seconds 3600 --exec-timeout-seconds 60 --min-free-bytes 1 --disable-port-forwarding --health-listen "127.0.0.1:$health_port" >"$daemon_log" 2>&1 &' \
  'printf "%s\n" "$!" >"$pid_file"' >"$restart_script"
chmod 700 "$restart_script"

wait_ready() {
  local ready=0
  for _ in $(seq 1 200); do
    if curl -fsS --noproxy '*' "http://127.0.0.1:$health_port/ready" >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 0.05
  done
  if [[ "$ready" != 1 ]]; then
    cat "$daemon_log" >&2 || true
    return 1
  fi
}

printf '%s\n' normal >"$mode_file"
"$restart_script"
wait_ready

# A second rollout must fail closed instead of interleaving pointer swaps and
# supervisor restarts with the first operator.  The lock is intentionally
# modeled as an empty directory, matching the atomic mkdir contract used by
# the packaged helper.
mkdir -- "$prefix/.upgrade.lock"
set +e
locked_output=$(
  "$upgrader" \
    --prefix "$prefix" \
    --ready-url "http://127.0.0.1:$health_port/ready" \
    --restart-command "$restart_script" \
    --ready-timeout-seconds 1 \
    --skip-current-ready \
    "$upgrade_archive" "$upgrade_checksum" 2>&1
)
locked_rc=$?
set -e
if [[ "$locked_rc" == 0 || "$locked_output" != *"another ASP release upgrade is in progress"* ]]; then
  printf 'upgrade helper did not reject a concurrent rollout (rc=%s):\n%s\n' \
    "$locked_rc" "$locked_output" >&2
  exit 1
fi
rmdir -- "$prefix/.upgrade.lock"

# The current release must be healthy before an upgrade mutates the pointer.
printf '%s\n' fail_once >"$mode_file"
set +e
failure_output=$(
  "$upgrader" \
    --prefix "$prefix" \
    --ready-url "http://127.0.0.1:$health_port/ready" \
    --restart-command "$restart_script" \
    --ready-timeout-seconds 5 \
    "$upgrade_archive" "$upgrade_checksum" 2>&1
)
failure_rc=$?
set -e
if [[ "$failure_rc" == 0 || "$failure_output" != *"previous release restored and ready"* ]]; then
  printf 'upgrade helper did not restore a failed rollout (rc=%s):\n%s\n' \
    "$failure_rc" "$failure_output" >&2
  exit 1
fi
if [[ "$(readlink -- "$prefix/current")" != "$initial_target" ]]; then
  echo "failed rollout did not restore the original current pointer" >&2
  exit 1
fi
wait_ready

# A normal restart path should now activate the same release successfully.
"$upgrader" \
  --prefix "$prefix" \
  --ready-url "http://127.0.0.1:$health_port/ready" \
  --restart-command "$restart_script" \
  --ready-timeout-seconds 10 \
  "$upgrade_archive" "$upgrade_checksum" >/dev/null
if [[ "$(readlink -- "$prefix/current")" != "releases/${archive_base%.tar.gz}-upgrade" ]]; then
  echo "successful rollout did not publish the upgrade release" >&2
  exit 1
fi
wait_ready

printf '{"experiment":"upgrade-release","status":"pass","initial_target":"%s","final_target":"%s"}\n' \
  "$initial_target" "$(readlink -- "$prefix/current")"
