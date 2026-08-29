#!/usr/bin/env bash
set -euo pipefail

# Verify an operator-owned detached signature over an ASP release checksum.
# The normal release verifier remains authoritative for archive shape and
# digest parsing; this helper adds a no-network GnuPG signature check and an
# optional exact signer-fingerprint allowlist.

usage() {
  cat >&2 <<'USAGE'
usage: verify-release-signature.sh [--fingerprint FINGERPRINT] \
  RELEASE.tar.gz [RELEASE.sha256] [RELEASE.sha256.asc]

The default signature is RELEASE.sha256.asc. GnuPG uses the caller's trusted
keyring; --fingerprint is recommended in promotion automation.
USAGE
  exit 2
}

fingerprint=${ASP_RELEASE_SIGNING_FINGERPRINT:-}
archive=''
checksum=''
signature=''

MAX_ARCHIVE_BYTES=$((512 * 1024 * 1024))
MAX_CHECKSUM_BYTES=16384
MAX_SIGNATURE_BYTES=$((16 * 1024 * 1024))

while (($# > 0)); do
  case "$1" in
    --fingerprint)
      (($# >= 2)) || usage
      fingerprint=$2
      shift 2
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
      elif [[ -z "$signature" ]]; then
        signature=$1
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
  elif [[ -z "$signature" ]]; then
    signature=$1
  else
    echo "too many positional arguments" >&2
    usage
  fi
  shift
fi
(($# == 0)) || usage

[[ -n "$archive" ]] || usage
if [[ -z "$checksum" ]]; then
  checksum="${archive%.tar.gz}.sha256"
fi
if [[ -z "$signature" ]]; then
  signature="${checksum}.asc"
fi

if [[ -n "$fingerprint" && ! "$fingerprint" =~ ^[[:xdigit:]]{40,64}$ ]]; then
  echo "--fingerprint must be a 40- or 64-character hexadecimal fingerprint" >&2
  exit 2
fi
if [[ ! -f "$signature" || -L "$signature" ]]; then
  echo "release signature must be a regular non-symlink file: $signature" >&2
  exit 2
fi

for path in "$archive" "$checksum"; do
  if [[ ! -f "$path" || -L "$path" ]]; then
    echo "release input must be a regular non-symlink file: $path" >&2
    exit 2
  fi
done
archive_dir=$(cd -- "$(dirname -- "$archive")" && pwd)
checksum_dir=$(cd -- "$(dirname -- "$checksum")" && pwd)
if [[ "$archive_dir" != "$checksum_dir" ]]; then
  echo "archive and checksum must be in the same directory" >&2
  exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
command -v gpg >/dev/null 2>&1 || {
  echo "verify-release-signature.sh requires gpg" >&2
  exit 2
}

# Snapshot all signed material before invoking the verifier or GnuPG.  This
# keeps the detached signature bound to the exact checksum file that was
# checked, even when the input directory is shared with a downloader or
# promotion job.
snapshot_dir=$(mktemp -d "${TMPDIR:-/tmp}/asp-release-signature-snapshot.XXXXXX")
chmod 700 "$snapshot_dir"
cleanup_snapshot() {
  rm -rf -- "$snapshot_dir"
}
trap cleanup_snapshot EXIT INT TERM

copy_bounded() {
  local source=$1
  local destination=$2
  local limit=$3
  local temporary
  local size
  temporary=$(mktemp "$snapshot_dir/.copy.XXXXXX")
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

archive_name=$(basename -- "$archive")
checksum_name=$(basename -- "$checksum")
signature_name=$(basename -- "$signature")
stable_archive="$snapshot_dir/$archive_name"
stable_checksum="$snapshot_dir/$checksum_name"
stable_signature="$snapshot_dir/$signature_name"
copy_bounded "$archive" "$stable_archive" "$MAX_ARCHIVE_BYTES"
copy_bounded "$checksum" "$stable_checksum" "$MAX_CHECKSUM_BYTES"
copy_bounded "$signature" "$stable_signature" "$MAX_SIGNATURE_BYTES"

"$script_dir/verify-release.sh" "$stable_archive" "$stable_checksum" >/dev/null

status_file=$(mktemp "${TMPDIR:-/tmp}/asp-release-gpg-status.XXXXXX")
error_file=$(mktemp "${TMPDIR:-/tmp}/asp-release-gpg-error.XXXXXX")
cleanup() {
  rm -f -- "$status_file" "$error_file"
  cleanup_snapshot
}
trap cleanup EXIT INT TERM
if ! gpg --batch --no-auto-key-retrieve --status-fd=1 --verify \
  "$stable_signature" "$stable_checksum" >"$status_file" 2>"$error_file"; then
  sed -n '1,4p' "$error_file" >&2
  echo "release signature verification failed: $signature" >&2
  exit 1
fi
actual_fingerprint=$(awk '$1 == "[GNUPG:]" && $2 == "VALIDSIG" { print toupper($3); exit }' "$status_file")
if [[ -z "$actual_fingerprint" ]]; then
  echo "GnuPG did not report a valid signer fingerprint" >&2
  exit 1
fi
if [[ -n "$fingerprint" && "$actual_fingerprint" != "${fingerprint^^}" ]]; then
  echo "release signature signer fingerprint mismatch" >&2
  echo "expected: ${fingerprint^^}" >&2
  echo "actual:   $actual_fingerprint" >&2
  exit 1
fi
printf 'ASP release signature verified: %s (signer %s)\n' "$signature" "$actual_fingerprint"
