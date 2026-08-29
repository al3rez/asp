#!/usr/bin/env bash
set -euo pipefail

# Install a verified ASP archive without replacing a live binary in place.
# Releases are immutable directories and the supervisor-facing `current`
# pointer is switched atomically. The script does not restart a supervisor.

# Release material is public executable/documentation content. Do not let a
# caller's private umask make the installed binaries unreadable to the service
# account that the supervisor runs under.
umask 022

# Keep release verification and extraction on one private, bounded snapshot.
# Verifying an archive by pathname and extracting that pathname later leaves a
# replacement window (especially when a download directory is shared with an
# updater).  The snapshot is deliberately a little larger than the verifier's
# limits so an over-sized source is copied only up to the limit plus one byte
# and then rejected without an unbounded read.
MAX_ARCHIVE_BYTES=$((512 * 1024 * 1024))
MAX_CHECKSUM_BYTES=16384
MAX_SIGNATURE_BYTES=$((16 * 1024 * 1024))

usage() {
  cat >&2 <<'USAGE'
usage:
  install-release.sh [--prefix PREFIX] [--signature RELEASE.sha256.asc]
    [--fingerprint FINGERPRINT] [--require-signature]
    RELEASE.tar.gz [RELEASE.sha256]
  install-release.sh [--prefix PREFIX] --rollback
  install-release.sh [--prefix PREFIX] --validate-prefix

PREFIX defaults to /usr/local/lib/asp. The archive must be verified by the
bundled deploy/verify-release.sh (or ASP_RELEASE_VERIFIER). Installation is
atomic and does not restart systemd, launchd, or another supervisor. When
--signature or ASP_RELEASE_SIGNING_FINGERPRINT is supplied, the bundled
deploy/verify-release-signature.sh (or ASP_RELEASE_SIGNATURE_VERIFIER) also
authenticates the checksum before extraction; --fingerprint is recommended.
--require-signature makes that check mandatory even when no signature path or
fingerprint environment default is present.
--validate-prefix performs only the non-mutating prefix trust-boundary check;
it is used by the readiness-gated upgrader before acquiring its transaction
lock.
USAGE
  exit 2
}

prefix="${ASP_INSTALL_PREFIX:-/usr/local/lib/asp}"
rollback=0
validate_prefix=0
archive=""
checksum=""
signature="${ASP_RELEASE_SIGNATURE:-}"
fingerprint="${ASP_RELEASE_SIGNING_FINGERPRINT:-}"
signature_flag=0
fingerprint_flag=0
require_signature_flag=0
require_signature="${ASP_REQUIRE_RELEASE_SIGNATURE:-0}"

while (($# > 0)); do
  case "$1" in
    --prefix)
      (($# >= 2)) || usage
      prefix=$2
      shift 2
      ;;
    --rollback)
      rollback=1
      shift
      ;;
    --validate-prefix)
      validate_prefix=1
      shift
      ;;
    --signature)
      (($# >= 2)) || usage
      signature=$2
      signature_flag=1
      shift 2
      ;;
    --fingerprint)
      (($# >= 2)) || usage
      fingerprint=$2
      fingerprint_flag=1
      shift 2
      ;;
    --require-signature)
      require_signature=1
      require_signature_flag=1
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
(( $# == 0 )) || usage

if [[ "$require_signature" != 0 && "$require_signature" != 1 ]]; then
  echo "ASP_REQUIRE_RELEASE_SIGNATURE must be 0 or 1" >&2
  exit 2
fi
if ((rollback == 1)) && ((signature_flag || fingerprint_flag || require_signature_flag)); then
  echo "signature options are only valid when installing an archive" >&2
  exit 2
fi
if ((validate_prefix == 1)); then
  if ((rollback == 1)) || [[ -n "$archive" || -n "$checksum" ]] ||
    ((signature_flag || fingerprint_flag || require_signature_flag)); then
    echo "--validate-prefix cannot be combined with release or signature arguments" >&2
    exit 2
  fi
fi

if [[ "$prefix" != /* || "$prefix" == "/" || -z "$prefix" ]]; then
  echo "--prefix must be an absolute non-root path: $prefix" >&2
  exit 2
fi
if [[ -L "$prefix" || ( -e "$prefix" && ! -d "$prefix" ) ]]; then
  echo "install prefix must be a real directory, not a symlink or file: $prefix" >&2
  exit 1
fi

# The release pointer and immutable binaries are part of the supervisor's
# trust boundary. Do not let `mkdir -p` silently follow an attacker-controlled
# symlink or create a release below a mutable existing directory component
# (for example a writable staging parent). Root-owned compatibility aliases
# such as macOS `/var` and Linux `/bin` are safe when their containing directory
# is not group/world writable; links below a mutable parent are rejected.
# Missing leaf components remain valid and are created after this check.
stat_uid() {
  local path=$1
  local value
  # Probe GNU coreutils first: on Linux `stat -f` means filesystem status and
  # `%u` is free-inode count, not the file owner. BSD/macOS uses `-f` for the
  # file format, so it remains the portable fallback.
  if value=$(stat -c '%u' -- "$path" 2>/dev/null) && [[ "$value" =~ ^[0-9]+$ ]]; then
    printf '%s\n' "$value"
    return 0
  fi
  if value=$(stat -f '%u' -- "$path" 2>/dev/null) && [[ "$value" =~ ^[0-9]+$ ]]; then
    printf '%s\n' "$value"
    return 0
  fi
  return 1
}

stat_mode() {
  local path=$1
  local value
  if value=$(stat -c '%a' -- "$path" 2>/dev/null) && [[ "$value" =~ ^[0-7]+$ ]]; then
    printf '%s\n' "$value"
    return 0
  fi
  if value=$(stat -f '%Lp' -- "$path" 2>/dev/null) && [[ "$value" =~ ^[0-7]+$ ]]; then
    printf '%s\n' "$value"
    return 0
  fi
  return 1
}

trusted_prefix_symlink() {
  local path=$1
  local parent uid mode
  [[ -L "$path" ]] || return 1
  parent=$(dirname -- "$path")
  [[ -d "$parent" && ! -L "$parent" ]] || return 1
  uid=$(stat_uid "$path") || return 1
  mode=$(stat_mode "$parent") || return 1
  [[ "$uid" == "0" && "$mode" =~ ^[0-7]+$ ]] || return 1
  # Convert the permission string explicitly as octal; this also handles
  # BSD's `%Lp` output when a special/sticky bit is present.
  while [[ "$mode" == 0* && ${#mode} -gt 1 ]]; do
    mode=${mode#0}
  done
  [[ -n "$mode" ]] || mode=0
  (( (8#$mode & 18) == 0 ))
}

prefix_directory_mode_is_safe() {
  local path=$1
  local final_component=$2
  local mode
  mode=$(stat_mode "$path") || return 1
  [[ "$mode" =~ ^[0-7]+$ ]] || return 1
  while [[ "$mode" == 0* && ${#mode} -gt 1 ]]; do
    mode=${mode#0}
  done
  local writable=$((8#$mode & 18))
  if (( writable != 0 )); then
    # A sticky shared ancestor such as `/tmp` prevents one user from
    # replacing another user's child. The installation target itself and
    # non-sticky ancestors must still reject group/world writes.
    local sticky=$((8#$mode & 512))
    if (( final_component == 0 && sticky != 0 )); then
      return 0
    fi
    return 1
  fi
  return 0
}

reject_untrusted_prefix_symlinks() {
  local current=$prefix
  while [[ "$current" != "/" && -n "$current" ]]; do
    if [[ -L "$current" ]]; then
      if [[ "$current" == "$prefix" ]] || ! trusted_prefix_symlink "$current"; then
        echo "install prefix must not traverse an untrusted symlink: $current" >&2
        exit 1
      fi
    elif [[ -e "$current" ]]; then
      if [[ ! -d "$current" ]]; then
        echo "install prefix component is not a directory: $current" >&2
        exit 1
      fi
      local final_component=0
      [[ "$current" == "$prefix" ]] && final_component=1
      if ! prefix_directory_mode_is_safe "$current" "$final_component"; then
        echo "install prefix component is group/world writable: $current" >&2
        exit 1
      fi
    fi
    current=$(dirname -- "$current")
  done
}

reject_untrusted_prefix_symlinks

# This mode is intentionally non-mutating.  Keep it after the complete
# ancestor walk and before mkdir/lock/release handling so callers can use it as
# a trust-boundary preflight without creating any state below the prefix.
if ((validate_prefix == 1)); then
  exit 0
fi

releases="$prefix/releases"
current="$prefix/current"
previous="$prefix/previous"

ensure_directory() {
  local path=$1
  if [[ -L "$path" || ( -e "$path" && ! -d "$path" ) ]]; then
    echo "release path must be a real directory, not a symlink or file: $path" >&2
    exit 1
  fi
  mkdir -p -- "$path"
}

ensure_directory "$prefix"
ensure_directory "$releases"

# mkdir is an atomic lock and does not follow an attacker-created symlink.
lock="$prefix/.install.lock"
if ! mkdir -- "$lock" 2>/dev/null; then
  echo "another ASP release installation is in progress: $lock" >&2
  exit 1
fi
cleanup_lock() {
  rmdir -- "$lock" 2>/dev/null || true
}
trap cleanup_lock EXIT INT TERM

safe_release_target() {
  local target=$1
  [[ "$target" =~ ^releases/asp-[A-Za-z0-9._-]+$ ]] || return 1
  [[ -d "$prefix/$target" && ! -L "$prefix/$target" ]] || return 1
  # `-x` alone also succeeds for an executable directory. Rollback pointers
  # may refer to an older tree that was not created by this invocation, so
  # require the same regular, non-symlink binary invariant as fresh installs.
  [[ -f "$prefix/$target/bin/asp" && -f "$prefix/$target/bin/aspd" ]] || return 1
  [[ ! -L "$prefix/$target/bin/asp" && ! -L "$prefix/$target/bin/aspd" ]] || return 1
  [[ -f "$prefix/$target/.archive.sha256" && ! -L "$prefix/$target/.archive.sha256" ]] || return 1
  local digest
  digest=$(tr -d '[:space:]' <"$prefix/$target/.archive.sha256")
  [[ "$digest" =~ ^[[:xdigit:]]{64}$ ]] || return 1
}

atomic_link_replace() {
  local link=$1
  local target=$2
  local temporary="${link}.tmp.$$"
  [[ ! -e "$temporary" && ! -L "$temporary" ]] || {
    echo "temporary release pointer already exists: $temporary" >&2
    exit 1
  }
  ln -s -- "$target" "$temporary"
  # GNU mv has -T and BSD mv has -h; both replace a symlink itself. A plain BSD
  # mv treats a symlink-to-directory as a directory and would silently put the
  # temporary link inside the old release. Prefer the native rename(2) path;
  # Python is the next portable option, and the final fallback briefly removes
  # the pointer but never follows it and restores the old link on failure.
  # Capture capability probes instead of piping them through grep: `mv` exits
  # nonzero for a help/usage probe on some BSDs, and grep -q can close a pipe
  # early under pipefail. Neither behavior should select the wrong rename
  # implementation.
  local mv_help mv_usage
  mv_help=$(mv --help 2>&1 || true)
  if [[ "$mv_help" == *" -T"* ]]; then
    mv -fT -- "$temporary" "$link"
  else
    mv_usage=$(mv 2>&1 || true)
    if [[ "$mv_usage" == *"-h"* ]]; then
      mv -fh -- "$temporary" "$link"
    elif command -v python3 >/dev/null 2>&1; then
      python3 - "$temporary" "$link" <<'PY'
import os
import sys
os.replace(sys.argv[1], sys.argv[2])
PY
    else
      local backup="${link}.old.$$"
      [[ ! -e "$backup" && ! -L "$backup" ]] || {
        echo "temporary release backup already exists: $backup" >&2
        exit 1
      }
      if [[ -e "$link" || -L "$link" ]]; then
        mv -- "$link" "$backup"
      fi
      if ! mv -- "$temporary" "$link"; then
        if [[ ! -e "$link" && ! -L "$link" && ( -e "$backup" || -L "$backup" ) ]]; then
          mv -- "$backup" "$link"
        fi
        return 1
      fi
      [[ ! -e "$backup" && ! -L "$backup" ]] || rmdir -- "$backup" 2>/dev/null || true
    fi
  fi
}

if ((rollback)); then
  [[ -z "$archive" && -z "$checksum" ]] || usage
  if [[ -e "$previous" && ! -L "$previous" ]]; then
    echo "previous release pointer is not a symlink: $previous" >&2
    exit 1
  fi
  [[ -L "$previous" ]] || {
    echo "no previous ASP release is available under $prefix" >&2
    exit 1
  }
  old_target=$(readlink -- "$previous")
  safe_release_target "$old_target" || {
    echo "previous pointer is unsafe or missing: $old_target" >&2
    exit 1
  }
  current_target=""
  if [[ -L "$current" ]]; then
    current_target=$(readlink -- "$current")
    safe_release_target "$current_target" || {
      echo "current pointer is unsafe or missing: $current_target" >&2
      exit 1
    }
  elif [[ -e "$current" ]]; then
    echo "current release pointer is not a symlink: $current" >&2
    exit 1
  fi
  atomic_link_replace "$current" "$old_target"
  if [[ -n "$current_target" ]]; then
    atomic_link_replace "$previous" "$current_target"
  else
    rm -f -- "$previous"
  fi
  printf 'ASP release pointer rolled back to %s\n' "$old_target"
  printf 'Restart the supervisor after validating %s/bin/aspd\n' "$prefix/current"
  exit 0
fi

[[ -n "$archive" ]] || usage
if [[ ! -f "$archive" || -L "$archive" ]]; then
  echo "release archive must be a regular non-symlink file: $archive" >&2
  exit 2
fi

archive_base=$(basename -- "$archive")
case "$archive_base" in
  asp-*.tar.gz) ;;
  *)
    echo "release archive name must match asp-*.tar.gz: $archive_base" >&2
    exit 2
    ;;
esac
release_name=${archive_base%.tar.gz}
if [[ ! "$release_name" =~ ^asp-[A-Za-z0-9._-]+$ ]]; then
  echo "release archive name contains unsafe characters: $release_name" >&2
  exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

# Resolve the conventional sidecar before taking the private snapshot.  The
# verifier enforces the basename/directory contract; these checks provide a
# clear installer error before we allocate staging space.
if [[ -z "$checksum" ]]; then
  checksum="${archive%.tar.gz}.sha256"
fi
if [[ ! -f "$checksum" || -L "$checksum" ]]; then
  echo "release checksum must be a regular non-symlink file: $checksum" >&2
  exit 2
fi
archive_dir=$(cd -- "$(dirname -- "$archive")" && pwd)
checksum_dir=$(cd -- "$(dirname -- "$checksum")" && pwd)
if [[ "$archive_dir" != "$checksum_dir" ]]; then
  echo "archive and checksum must be in the same directory" >&2
  exit 2
fi

if [[ -n "$signature" || -n "$fingerprint" || "$require_signature" == 1 ]]; then
  signature_enforced=1
  signature_verifier="${ASP_RELEASE_SIGNATURE_VERIFIER:-$script_dir/verify-release-signature.sh}"
  if [[ ! -x "$signature_verifier" ]]; then
    echo "release signature verifier is missing or not executable: $signature_verifier" >&2
    echo "run the bundled deploy/verify-release-signature.sh or set ASP_RELEASE_SIGNATURE_VERIFIER" >&2
    exit 2
  fi
  if [[ -z "$signature" ]]; then
    signature="${checksum}.asc"
  fi
  if [[ ! -f "$signature" || -L "$signature" ]]; then
    echo "release signature must be a regular non-symlink file: $signature" >&2
    exit 2
  fi
else
  signature_enforced=0
  signature_verifier=""
fi

verifier="${ASP_RELEASE_VERIFIER:-$script_dir/verify-release.sh}"
if [[ ! -x "$verifier" ]]; then
  echo "release verifier is missing or not executable: $verifier" >&2
  echo "run the bundled deploy/verify-release.sh or set ASP_RELEASE_VERIFIER" >&2
  exit 2
fi
signature_args=()

# Take a bounded snapshot before verification.  All subsequent verification,
# hashing, and extraction uses these private paths, so replacing the original
# download (or its sidecars) after this point cannot alter the installed tree.
verify_dir="$releases/.verify-$release_name-$$"
[[ ! -e "$verify_dir" && ! -L "$verify_dir" ]] || {
  echo "verification staging path already exists: $verify_dir" >&2
  exit 1
}
mkdir -- "$verify_dir"
chmod 700 "$verify_dir"
cleanup_verify_dir() {
  rm -rf -- "$verify_dir"
}
trap 'cleanup_verify_dir; cleanup_lock' EXIT INT TERM

copy_bounded() {
  local source=$1
  local destination=$2
  local limit=$3
  local temporary
  local size
  temporary=$(mktemp "$verify_dir/.copy.XXXXXX")
  # Read at most limit+1 bytes.  The extra byte lets us reject a source that
  # grows while it is being copied instead of allowing an unbounded read.
  if ! head -c "$((limit + 1))" -- "$source" >"$temporary"; then
    rm -f -- "$temporary"
    echo "could not snapshot release input: $source" >&2
    exit 1
  fi
  size=$(wc -c <"$temporary" | tr -d '[:space:]')
  if [[ ! "$size" =~ ^[0-9]+$ ]] || ((size > limit)); then
    rm -f -- "$temporary"
    echo "release input exceeds its bounded snapshot limit: $source" >&2
    exit 1
  fi
  chmod 0444 "$temporary"
  mv -- "$temporary" "$destination"
}

checksum_base=$(basename -- "$checksum")
stable_archive="$verify_dir/$archive_base"
stable_checksum="$verify_dir/$checksum_base"
copy_bounded "$archive" "$stable_archive" "$MAX_ARCHIVE_BYTES"
copy_bounded "$checksum" "$stable_checksum" "$MAX_CHECKSUM_BYTES"

if [[ "$checksum_base" != "${archive_base%.tar.gz}.sha256" ]]; then
  echo "release checksum name must match ${archive_base%.tar.gz}.sha256: $checksum_base" >&2
  exit 2
fi

"$verifier" "$stable_archive" "$stable_checksum"

if ((signature_enforced == 1)); then
  signature_base=$(basename -- "$signature")
  stable_signature="$verify_dir/$signature_base"
  copy_bounded "$signature" "$stable_signature" "$MAX_SIGNATURE_BYTES"
  if [[ -n "$fingerprint" ]]; then
    signature_args=(--fingerprint "$fingerprint" "$stable_archive" "$stable_checksum" "$stable_signature")
  else
    signature_args=("$stable_archive" "$stable_checksum" "$stable_signature")
  fi
  "$signature_verifier" "${signature_args[@]}"
fi

if command -v shasum >/dev/null 2>&1; then
  archive_digest=$(shasum -a 256 -- "$stable_archive" | awk '{print $1}')
elif command -v sha256sum >/dev/null 2>&1; then
  archive_digest=$(sha256sum -- "$stable_archive" | awk '{print $1}')
else
  echo "neither shasum nor sha256sum is available" >&2
  exit 2
fi

release_dir="$releases/$release_name"
if [[ -e "$release_dir" || -L "$release_dir" ]]; then
  [[ -d "$release_dir" && ! -L "$release_dir" ]] || {
    echo "existing release path is not a real directory: $release_dir" >&2
    exit 1
  }
  [[ -f "$release_dir/.archive.sha256" && ! -L "$release_dir/.archive.sha256" ]] || {
    echo "existing release has no archive identity; refusing to overwrite: $release_dir" >&2
    exit 1
  }
  installed_digest=$(tr -d '[:space:]' <"$release_dir/.archive.sha256")
  [[ "$installed_digest" == "$archive_digest" ]] || {
    echo "release name already exists with a different archive: $release_name" >&2
    exit 1
  }
else
  stage="$releases/.staging-$release_name-$$"
  [[ ! -e "$stage" && ! -L "$stage" ]] || {
    echo "staging path already exists: $stage" >&2
    exit 1
  }
  mkdir -- "$stage"
  cleanup_stage() {
    rm -rf -- "$stage"
  }
  trap 'cleanup_stage; cleanup_verify_dir; cleanup_lock' EXIT INT TERM
  tar -xzf "$stable_archive" -C "$stage"
  [[ -x "$stage/bin/asp" && -x "$stage/bin/aspd" ]] || {
    echo "release archive did not extract executable asp/aspd binaries" >&2
    exit 1
  }
  [[ ! -L "$stage/bin/asp" && ! -L "$stage/bin/aspd" ]] || {
    echo "release binaries must not be symlinks" >&2
    exit 1
  }
  printf '%s\n' "$archive_digest" >"$stage/.archive.sha256"
  chmod 0644 "$stage/.archive.sha256"
  mv -- "$stage" "$release_dir"
  unset -f cleanup_stage
  trap cleanup_lock EXIT INT TERM
fi

safe_release_target "releases/$release_name" || {
  echo "installed release failed structural validation: $release_dir" >&2
  exit 1
}

old_target=""
if [[ -L "$current" ]]; then
  old_target=$(readlink -- "$current")
  safe_release_target "$old_target" || {
    echo "current pointer is unsafe or missing: $old_target" >&2
    exit 1
  }
elif [[ -e "$current" ]]; then
  echo "current release pointer is not a symlink: $current" >&2
  exit 1
fi
if [[ -e "$previous" && ! -L "$previous" ]]; then
  echo "previous release pointer is not a symlink: $previous" >&2
  exit 1
fi
if [[ -n "$old_target" && "$old_target" != "releases/$release_name" ]]; then
  atomic_link_replace "$previous" "$old_target"
fi
atomic_link_replace "$current" "releases/$release_name"

printf 'ASP release installed at %s\n' "$release_dir"
printf 'Current pointer: %s\n' "$current"
if [[ -n "$old_target" && "$old_target" != "releases/$release_name" ]]; then
  printf 'Rollback pointer: %s -> %s\n' "$previous" "$old_target"
fi
printf 'Restart the supervisor only after validating %s/bin/aspd\n' "$current"
