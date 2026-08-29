#!/usr/bin/env bash
set -euo pipefail

# Install and activate one verified ASP release with an explicit readiness
# gate. The supervisor command is deliberately supplied by the operator so
# this helper works with systemd, launchd, or another service manager without
# embedding a second process-control implementation. If the new daemon does
# not become ready, the previous immutable release pointer is restored and the
# same restart command is run again before the helper returns failure.

umask 022

usage() {
  cat >&2 <<'USAGE'
usage:
  upgrade-release.sh --ready-url LOOPBACK_READY_URL \
    --restart-command 'SUPERVISOR_RESTART_COMMAND' \
    [--prefix PREFIX] [--ready-timeout-seconds N] \
    [--signature RELEASE.sha256.asc] [--fingerprint FINGERPRINT] \
    [--require-signature] \
    RELEASE.tar.gz [RELEASE.sha256]

The current release must already be installed and ready. The archive is
verified by deploy/install-release.sh, switched atomically, and restarted only
through the supplied command. If readiness fails after the restart, the
previous release is restored and restarted automatically.
When --signature or ASP_RELEASE_SIGNING_FINGERPRINT is supplied, the bundled
deploy/verify-release-signature.sh (or ASP_RELEASE_SIGNATURE_VERIFIER) also
authenticates the checksum before installation; --fingerprint is recommended.
--require-signature makes that check mandatory even when no signature path or
fingerprint environment default is present.

The readiness URL must be the daemon's loopback HTTP /ready endpoint. The
restart command is an operator-controlled shell command, for example:
  systemctl restart aspd-production.service
  launchctl kickstart -k gui/$(id -u)/com.asp.aspd.production

Use --skip-current-ready only for a deliberate recovery when the current
daemon is already down; a failed upgrade still attempts rollback readiness.
USAGE
  exit 2
}

prefix="${ASP_INSTALL_PREFIX:-/usr/local/lib/asp}"
archive=""
checksum=""
ready_url="${ASP_READY_URL:-}"
restart_command="${ASP_RESTART_COMMAND:-}"
ready_timeout_seconds="${ASP_READY_TIMEOUT_SECONDS:-120}"
skip_current_ready=0
signature="${ASP_RELEASE_SIGNATURE:-}"
fingerprint="${ASP_RELEASE_SIGNING_FINGERPRINT:-}"
require_signature="${ASP_REQUIRE_RELEASE_SIGNATURE:-0}"

while (($# > 0)); do
  case "$1" in
    --prefix)
      (($# >= 2)) || usage
      prefix=$2
      shift 2
      ;;
    --ready-url)
      (($# >= 2)) || usage
      ready_url=$2
      shift 2
      ;;
    --restart-command)
      (($# >= 2)) || usage
      restart_command=$2
      shift 2
      ;;
    --ready-timeout-seconds)
      (($# >= 2)) || usage
      ready_timeout_seconds=$2
      shift 2
      ;;
    --skip-current-ready)
      skip_current_ready=1
      shift
      ;;
    --signature)
      (($# >= 2)) || usage
      signature=$2
      shift 2
      ;;
    --fingerprint)
      (($# >= 2)) || usage
      fingerprint=$2
      shift 2
      ;;
    --require-signature)
      require_signature=1
      shift
      ;;
    --help|-h)
      usage
      ;;
    --)
      shift
      break
      ;;
    -* )
      echo "unknown option: $1" >&2
      usage
      ;;
    *)
      if [[ -z "$archive" ]]; then
        archive=$1
      elif [[ -z "$checksum" ]]; then
        checksum=$1
      else
        echo "too many positional arguments" >&2
        usage
      fi
      shift
      ;;
  esac
done

if (($# > 0)); then
  if [[ -z "$archive" ]]; then
    archive=$1
  elif [[ -z "$checksum" ]]; then
    checksum=$1
  else
    echo "too many positional arguments" >&2
    usage
  fi
  shift
fi
(($# == 0)) || usage

if [[ "$prefix" != /* || "$prefix" == "/" || -z "$prefix" ]]; then
  echo "--prefix must be an absolute non-root path: $prefix" >&2
  exit 2
fi
if [[ -L "$prefix" || ( -e "$prefix" && ! -d "$prefix" ) ]]; then
  echo "install prefix must be a real directory, not a symlink or file: $prefix" >&2
  exit 1
fi
if [[ -z "$archive" ]]; then
  usage
fi
if [[ -z "$ready_url" ]]; then
  echo "--ready-url is required; use the daemon's loopback /ready endpoint" >&2
  exit 2
fi
if [[ -z "$restart_command" ]]; then
  echo "--restart-command is required; no supervisor restart was inferred" >&2
  exit 2
fi
if [[ ! "$ready_timeout_seconds" =~ ^[0-9]+$ ]] || ((ready_timeout_seconds < 1 || ready_timeout_seconds > 3600)); then
  echo "--ready-timeout-seconds must be an integer from 1 to 3600" >&2
  exit 2
fi

# The health endpoint is intentionally loopback-only in ASP. Keeping this
# helper strict prevents a typo or copied URL from turning a deployment tool
# into a general-purpose HTTP probe or SSRF primitive. Require a numeric,
# in-range port when one is supplied; shell globs such as `[0-9]*` would also
# accept values like `1oops`.
if [[ "$ready_url" == 'http://127.0.0.1/ready' || "$ready_url" == 'http://[::1]/ready' ]]; then
  :
elif [[ "$ready_url" =~ ^http://127\.0\.0\.1:([0-9]+)/ready$ ]]; then
  ready_port=${BASH_REMATCH[1]}
  if ((10#$ready_port < 1 || 10#$ready_port > 65535)); then
    echo "--ready-url port must be from 1 to 65535: $ready_url" >&2
    exit 2
  fi
elif [[ "$ready_url" =~ ^http://\[::1\]:([0-9]+)/ready$ ]]; then
  ready_port=${BASH_REMATCH[1]}
  if ((10#$ready_port < 1 || 10#$ready_port > 65535)); then
    echo "--ready-url port must be from 1 to 65535: $ready_url" >&2
    exit 2
  fi
else
  echo "--ready-url must be a loopback http://.../ready URL: $ready_url" >&2
  exit 2
fi
if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required for the readiness gate" >&2
  exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
installer="${ASP_RELEASE_INSTALLER:-$script_dir/install-release.sh}"
if [[ ! -x "$installer" ]]; then
  echo "release installer is missing or not executable: $installer" >&2
  exit 2
fi

# Validate every existing prefix component before creating the upgrade lock.
# The installer repeats this check immediately before its own mutation, while
# this early probe prevents the upgrader from placing its lock below a
# symlinked or group/world-writable ancestor first.  An older/custom installer
# that lacks the explicit preflight is rejected rather than silently weakening
# the deployment boundary.
if ! "$installer" --prefix "$prefix" --validate-prefix; then
  echo "release prefix failed trust preflight: $prefix" >&2
  exit 1
fi

if [[ "$require_signature" != 0 && "$require_signature" != 1 ]]; then
  echo "ASP_REQUIRE_RELEASE_SIGNATURE must be 0 or 1" >&2
  exit 2
fi
if [[ -n "$signature" || -n "$fingerprint" || "$require_signature" == 1 ]]; then
  signature_verifier="${ASP_RELEASE_SIGNATURE_VERIFIER:-$script_dir/verify-release-signature.sh}"
  if [[ ! -x "$signature_verifier" ]]; then
    echo "release signature verifier is missing or not executable: $signature_verifier" >&2
    echo "run the bundled deploy/verify-release-signature.sh or set ASP_RELEASE_SIGNATURE_VERIFIER" >&2
    exit 2
  fi
  if [[ -z "$checksum" ]]; then
    checksum="${archive%.tar.gz}.sha256"
  fi
  if [[ -z "$signature" ]]; then
    signature="${checksum}.asc"
  fi
  if [[ -n "$fingerprint" ]]; then
    signature_args=(--fingerprint "$fingerprint" "$archive" "$checksum" "$signature")
  else
    signature_args=("$archive" "$checksum" "$signature")
  fi
  "$signature_verifier" "${signature_args[@]}"
fi

current="$prefix/current"
if [[ ! -L "$current" ]]; then
  echo "no current release pointer exists under $prefix: $current" >&2
  echo "use install-release.sh for the initial installation" >&2
  exit 1
fi

# Serialize the full readiness/install/restart/rollback transaction.  The
# installer has its own short lock, but that lock alone would still allow two
# upgraders to interleave supervisor restarts and restore the wrong pointer.
# `mkdir` is atomic and does not follow an attacker-created symlink; a stale
# lock is deliberately fail-closed and must be removed by an operator only
# after verifying that no upgrade process remains.  Acquire it only after the
# current-pointer check so an uninstalled prefix reports the useful initial
# installation error above rather than a misleading lock error.
upgrade_lock="$prefix/.upgrade.lock"
if [[ -L "$upgrade_lock" ]]; then
  echo "upgrade lock is an unsafe symlink: $upgrade_lock" >&2
  exit 1
fi
if [[ -e "$upgrade_lock" ]]; then
  echo "another ASP release upgrade is in progress: $upgrade_lock" >&2
  exit 1
fi
if ! mkdir -- "$upgrade_lock" 2>/dev/null; then
  echo "another ASP release upgrade is in progress: $upgrade_lock" >&2
  exit 1
fi
cleanup_upgrade_lock() {
  rmdir -- "$upgrade_lock" 2>/dev/null || true
}
trap cleanup_upgrade_lock EXIT INT TERM

# Re-check after taking the lock as well.  This narrows the window between the
# preflight and the first pointer/release mutation and makes any unexpected
# prefix replacement fail closed before the transaction proceeds.
if ! "$installer" --prefix "$prefix" --validate-prefix; then
  echo "release prefix changed during trust preflight: $prefix" >&2
  exit 1
fi

old_target=$(readlink -- "$current")
if [[ ! "$old_target" =~ ^releases/asp-[A-Za-z0-9._-]+$ ]]; then
  echo "current release pointer is unsafe: $old_target" >&2
  exit 1
fi
old_release_dir="$prefix/$old_target"
if [[ ! -d "$old_release_dir" || -L "$old_release_dir" ||
  ! -f "$old_release_dir/bin/asp" || -L "$old_release_dir/bin/asp" ||
  ! -f "$old_release_dir/bin/aspd" || -L "$old_release_dir/bin/aspd" ||
  ! -f "$old_release_dir/.archive.sha256" || -L "$old_release_dir/.archive.sha256" ]]; then
  echo "current release target is not a complete immutable release: $old_target" >&2
  exit 1
fi
old_digest=$(tr -d '[:space:]' <"$old_release_dir/.archive.sha256")
if [[ ! "$old_digest" =~ ^[[:xdigit:]]{64}$ ]]; then
  echo "current release archive identity is invalid: $old_target" >&2
  exit 1
fi

ready() {
  local status
  # Retry probes are expected to see connection-refused/503 while a supervisor
  # is rotating binaries; keep those transient diagnostics out of service logs
  # and report only the final readiness result below.
  status=$(curl --silent --noproxy '*' \
    --connect-timeout 2 --max-time 5 --max-redirs 0 --proto '=http' \
    --output /dev/null --write-out '%{http_code}' "$ready_url") || return 1
  [[ "$status" =~ ^2[0-9]{2}$ ]]
}

wait_ready() {
  local deadline=$((SECONDS + ready_timeout_seconds))
  while ((SECONDS <= deadline)); do
    if ready; then
      return 0
    fi
    sleep 1
  done
  return 1
}

restart_supervisor() {
  # The command is explicit operator configuration, not data received from a
  # remote peer. `--` keeps a leading dash in a command from becoming a bash
  # option, while preserving multi-step supervisor commands when required.
  bash -c -- "$restart_command"
}

rollback_and_verify() {
  echo "new release failed readiness; restoring $old_target" >&2
  if ! "$installer" --prefix "$prefix" --rollback >&2; then
    echo "CRITICAL: automatic ASP release rollback failed" >&2
    return 1
  fi
  if ! restart_supervisor; then
    echo "CRITICAL: supervisor restart failed while restoring ASP rollback" >&2
    return 1
  fi
  if ! wait_ready; then
    echo "CRITICAL: rolled-back ASP release did not become ready" >&2
    return 1
  fi
  return 0
}

if ((skip_current_ready == 0)); then
  if ! wait_ready; then
    echo "current ASP release is not ready; refusing to mutate $prefix" >&2
    exit 1
  fi
fi

installer_args=(--prefix "$prefix")
if [[ -n "$signature" ]]; then
  installer_args+=(--signature "$signature")
fi
if [[ -n "$fingerprint" ]]; then
  installer_args+=(--fingerprint "$fingerprint")
fi
installer_args+=("$archive")
if [[ -n "$checksum" ]]; then
  installer_args+=("$checksum")
fi
# The installer takes a private bounded snapshot and verifies that snapshot
# immediately before extraction. The earlier verification happens before
# readiness waits, so passing the options through here keeps signature policy
# enforced without extracting a mutable download pathname.
"$installer" "${installer_args[@]}"

if ! restart_supervisor || ! wait_ready; then
  if rollback_and_verify; then
    echo "ASP release upgrade failed; previous release restored and ready" >&2
    exit 1
  fi
  echo "ASP release upgrade failed and rollback readiness could not be verified" >&2
  exit 3
fi

new_target=$(readlink -- "$current")
printf 'ASP release upgrade succeeded: %s -> %s\n' "$old_target" "$new_target"
