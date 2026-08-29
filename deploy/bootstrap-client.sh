#!/usr/bin/env bash
set -euo pipefail

# Bootstrap the trusted single-workspace bearer-token client over an existing
# SSH trust relationship.  This helper is deliberately not a PKI replacement:
# shared or Internet-facing deployments should provision mTLS identities and
# secrets through their operator-owned system instead of copying a token.

usage() {
  cat >&2 <<'USAGE'
usage: bootstrap-client.sh --output-dir DIR SSH_TARGET REMOTE_ROOT

Fetch REMOTE_ROOT/.asp/server-cert.der and REMOTE_ROOT/.asp/auth-token over
the existing OpenSSH trust relationship and atomically install them in the
dedicated local DIR.  SSH host-key checking and authentication are delegated
to the user's normal ssh/scp configuration; this script never disables either.

DIR must be an absolute, dedicated credential directory.  SSH_TARGET accepts
an ordinary user@host or SSH config alias, and REMOTE_ROOT must be a bounded
absolute path such as /srv/asp/workspace.
USAGE
  exit 2
}

output_dir=""
ssh_target=""
remote_root=""

while (($# > 0)); do
  case "$1" in
    --output-dir)
      (($# >= 2)) || usage
      output_dir=$2
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
      if [[ -z "$ssh_target" ]]; then
        ssh_target=$1
      elif [[ -z "$remote_root" ]]; then
        remote_root=$1
      else
        echo "too many positional arguments" >&2
        usage
      fi
      shift
      ;;
  esac
done

if (($# > 0)); then
  if [[ -z "$ssh_target" ]]; then
    ssh_target=$1
  elif [[ -z "$remote_root" ]]; then
    remote_root=$1
  else
    echo "too many positional arguments" >&2
    usage
  fi
  shift
fi
(( $# == 0 )) || usage

[[ -n "$output_dir" && -n "$ssh_target" && -n "$remote_root" ]] || usage
output_dir=${output_dir%/}
remote_root=${remote_root%/}
if [[ "$output_dir" != /* || "$output_dir" == "/" ]]; then
  echo "--output-dir must be an absolute non-root path: $output_dir" >&2
  exit 2
fi
if [[ ! "$ssh_target" =~ ^([A-Za-z0-9._-]+@)?[A-Za-z0-9._-]+$ ]]; then
  echo "SSH_TARGET must be an ordinary user@host or SSH config alias" >&2
  exit 2
fi
if [[ "$remote_root" != /* || "$remote_root" == "/" || "$remote_root" == "/." ||
  "$remote_root" == *"/../"* || "$remote_root" == *"/./"* ||
  "$remote_root" == */.. || "$remote_root" == */. ||
  "$remote_root" == ../* ||
  ! "$remote_root" =~ ^/[A-Za-z0-9._/-]+$ ]]; then
  echo "REMOTE_ROOT must be a bounded absolute path without traversal: $remote_root" >&2
  exit 2
fi

command -v scp >/dev/null 2>&1 || {
  echo "bootstrap-client.sh requires scp" >&2
  exit 2
}

output_parent=$(dirname -- "$output_dir")
output_name=$(basename -- "$output_dir")
if [[ -L "$output_parent" || ( -e "$output_parent" && ! -d "$output_parent" ) ]]; then
  echo "credential parent must be a real directory: $output_parent" >&2
  exit 1
fi
mkdir -p -- "$output_parent"
if [[ -L "$output_dir" || ( -e "$output_dir" && ! -d "$output_dir" ) ]]; then
  echo "credential output must be a real directory, not a symlink: $output_dir" >&2
  exit 1
fi

# A dedicated directory must not contain a session cursor or unrelated files;
# the atomic directory replacement below would otherwise remove user state.
if [[ -d "$output_dir" ]]; then
  shopt -s nullglob dotglob
  for entry in "$output_dir"/*; do
    [[ -e "$entry" || -L "$entry" ]] || continue
    case "$(basename -- "$entry")" in
      server-cert.der|auth-token) ;;
      *)
        echo "credential output contains an unexpected entry: $entry" >&2
        exit 1
        ;;
    esac
  done
  shopt -u nullglob dotglob
fi

stage=$(mktemp -d "$output_parent/.asp-bootstrap-${output_name}.XXXXXX")
backup=""
restore_backup=0
cleanup() {
  if [[ -n "$backup" && "$restore_backup" == 1 &&
    ! -e "$output_dir" && ! -L "$output_dir" && -d "$backup" ]]; then
    mv -- "$backup" "$output_dir" || true
  fi
  if [[ -d "$stage" ]]; then
    rm -rf -- "$stage"
  fi
  if [[ -n "$backup" && "$restore_backup" == 0 && -d "$backup" ]]; then
    rm -rf -- "$backup"
  fi
}
trap cleanup EXIT INT TERM

remote_prefix="$ssh_target:$remote_root/.asp"
# Keep bootstrap failure bounded when a host is asleep or a route disappears,
# while leaving authentication/prompt policy to the operator's normal scp
# configuration. The server-alive probes also prevent a half-open transfer
# from holding the staging directory indefinitely.
scp_options=(
  -q
  -o ConnectTimeout=20
  -o ServerAliveInterval=5
  -o ServerAliveCountMax=3
)
scp "${scp_options[@]}" "$remote_prefix/server-cert.der" "$stage/server-cert.der"
scp "${scp_options[@]}" "$remote_prefix/auth-token" "$stage/auth-token"

for credential in "$stage/server-cert.der" "$stage/auth-token"; do
  if [[ ! -f "$credential" || -L "$credential" || ! -s "$credential" ]]; then
    echo "SSH bootstrap did not produce a non-empty regular credential file" >&2
    exit 1
  fi
  chmod 0600 "$credential"
done

if [[ -e "$output_dir" || -L "$output_dir" ]]; then
  backup="$output_parent/.${output_name}.bootstrap-old.$$"
  [[ ! -e "$backup" && ! -L "$backup" ]] || {
    echo "bootstrap backup path already exists: $backup" >&2
    exit 1
  }
  mv -- "$output_dir" "$backup"
  restore_backup=1
fi
if ! mv -- "$stage" "$output_dir"; then
  echo "could not publish bootstrapped credentials" >&2
  exit 1
fi
restore_backup=0
stage=""
if [[ -n "$backup" ]]; then
  rm -rf -- "$backup"
  backup=""
fi
chmod 0700 "$output_dir"

printf 'ASP client credentials installed in %s\n' "$output_dir"
printf 'Use --cert %s/server-cert.der --auth-token-file %s/auth-token\n' \
  "$output_dir" "$output_dir"
