#!/usr/bin/env bash
set -euo pipefail

# Create an operator-owned detached signature for a verified ASP release. The
# signature covers the SHA-256 sidecar rather than the archive directly, so a
# verifier can authenticate the exact archive digest and keep the normal
# archive-shape checks in one place. GnuPG is an established signing tool;
# ASP does not implement cryptography here.

usage() {
  cat >&2 <<'USAGE'
usage: sign-release.sh [--key-id KEY] [--output SIGNATURE.asc] \
  RELEASE.tar.gz [RELEASE.sha256]

The default key comes from ASP_RELEASE_SIGNING_KEY and the default signature
path is RELEASE.sha256.asc. The archive and checksum are verified before the
signature is created. The signing key and its trust distribution are owned by
the release operator.
USAGE
  exit 2
}

key_id=${ASP_RELEASE_SIGNING_KEY:-}
signature=''
archive=''
checksum=''

while (($# > 0)); do
  case "$1" in
    --key-id)
      (($# >= 2)) || usage
      key_id=$2
      shift 2
      ;;
    --output)
      (($# >= 2)) || usage
      signature=$2
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

[[ -n "$archive" ]] || usage
if [[ -z "$checksum" ]]; then
  checksum="${archive%.tar.gz}.sha256"
fi
if [[ -z "$signature" ]]; then
  signature="${checksum}.asc"
fi
[[ -n "$key_id" ]] || {
  echo "a signing key is required (--key-id or ASP_RELEASE_SIGNING_KEY)" >&2
  exit 2
}
if [[ "$key_id" == -* || "$key_id" == *$'\n'* || "$key_id" == *$'\r'* ]]; then
  echo "signing key identifier contains unsafe characters" >&2
  exit 2
fi

for path in "$archive" "$checksum"; do
  if [[ ! -f "$path" || -L "$path" ]]; then
    echo "release input must be a regular non-symlink file: $path" >&2
    exit 2
  fi
done
if [[ -e "$signature" || -L "$signature" ]]; then
  echo "signature output already exists; refusing to overwrite: $signature" >&2
  exit 1
fi

archive_dir=$(cd -- "$(dirname -- "$archive")" && pwd)
checksum_dir=$(cd -- "$(dirname -- "$checksum")" && pwd)
signature_parent=$(cd -- "$(dirname -- "$signature")" 2>/dev/null && pwd) || {
  echo "signature output parent does not exist: $(dirname -- "$signature")" >&2
  exit 2
}
[[ "$archive_dir" == "$checksum_dir" ]] || {
  echo "archive and checksum must be in the same directory" >&2
  exit 2
}

signature_path="$signature"
if [[ "$signature_path" != /* ]]; then
  signature_path="$signature_parent/$(basename -- "$signature_path")"
fi
if [[ -e "$signature_path" || -L "$signature_path" ]]; then
  echo "signature output already exists; refusing to overwrite: $signature_path" >&2
  exit 1
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
"$script_dir/verify-release.sh" "$archive" "$checksum" >/dev/null
command -v gpg >/dev/null 2>&1 || {
  echo "sign-release.sh requires gpg" >&2
  exit 2
}

temporary=$(mktemp "$signature_parent/.asp-release-signature.XXXXXX")
cleanup() {
  rm -f -- "$temporary"
}
trap cleanup EXIT INT TERM
chmod 0600 "$temporary"
gpg --batch --yes --armor --detach-sign --local-user "$key_id" \
  --output "$temporary" "$checksum"
chmod 0644 "$temporary"
# rename(2)-style replacement is atomic when the destination is on the same
# filesystem. The output is public release metadata rather than state.
mv -- "$temporary" "$signature_path"
trap - EXIT INT TERM

"$script_dir/verify-release-signature.sh" "$archive" "$checksum" "$signature_path" >/dev/null
printf 'ASP release signature written to %s\n' "$signature_path"
