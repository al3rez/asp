#!/usr/bin/env bash
set -euo pipefail

# Exercise deploy/bootstrap-client.sh without depending on an SSH daemon. The
# fake scp preserves the helper's argument shape while the real helper still
# performs all validation, staging, permission, and atomic-publication work.

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
helper="$repo_root/deploy/bootstrap-client.sh"
fixture=$(mktemp -d "${TMPDIR:-/tmp}/asp-bootstrap-smoke.XXXXXX")
trap 'rm -rf -- "$fixture"' EXIT INT TERM

remote_root="$fixture/remote"
source_dir="$remote_root/.asp"
fake_bin="$fixture/bin"
output_dir="$fixture/client-credentials"
mkdir -m 700 "$remote_root" "$fake_bin"
mkdir -m 700 "$source_dir"
printf '%s\n' 'CERTIFICATE-ONE' >"$source_dir/server-cert.der"
printf '%s\n' 'TOKEN-ONE-012345678901234567890123' >"$source_dir/auth-token"

cat >"$fake_bin/scp" <<'FAKE_SCP'
#!/usr/bin/env bash
set -euo pipefail
[[ "${1:-}" == "-q" ]] || exit 2
shift
for expected in ConnectTimeout=20 ServerAliveInterval=5 ServerAliveCountMax=3; do
  [[ "${1:-}" == "-o" && "${2:-}" == "$expected" ]] || exit 2
  shift 2
done
[[ $# == 2 ]] || exit 2
source=$1
destination=$2
case "$source" in
  */server-cert.der) cp -- "$ASP_BOOTSTRAP_CERT_SOURCE" "$destination" ;;
  */auth-token) cp -- "$ASP_BOOTSTRAP_TOKEN_SOURCE" "$destination" ;;
  *) exit 3 ;;
esac
FAKE_SCP
chmod 0755 "$fake_bin/scp"

file_mode() {
  if stat -c '%a' "$1" >/dev/null 2>&1; then
    stat -c '%a' "$1"
  else
    stat -f '%Lp' "$1"
  fi
}

export ASP_BOOTSTRAP_CERT_SOURCE="$source_dir/server-cert.der"
export ASP_BOOTSTRAP_TOKEN_SOURCE="$source_dir/auth-token"
PATH="$fake_bin:$PATH" "$helper" \
  --output-dir "$output_dir" bootstrap-host "$remote_root" >"$fixture/first.out"

test "$(sed -n '1p' "$output_dir/server-cert.der")" = 'CERTIFICATE-ONE'
test "$(sed -n '1p' "$output_dir/auth-token")" = 'TOKEN-ONE-012345678901234567890123'
test "$(file_mode "$output_dir")" = 700
test "$(file_mode "$output_dir/server-cert.der")" = 600
test "$(file_mode "$output_dir/auth-token")" = 600
! grep -Fq 'TOKEN-ONE' "$fixture/first.out"

# A second bootstrap replaces the complete pair, never exposing a half-updated
# directory to the client.
printf '%s\n' 'CERTIFICATE-TWO' >"$source_dir/server-cert.der"
printf '%s\n' 'TOKEN-TWO-012345678901234567890123' >"$source_dir/auth-token"
PATH="$fake_bin:$PATH" "$helper" \
  --output-dir "$output_dir" bootstrap-host "$remote_root" >/dev/null
test "$(sed -n '1p' "$output_dir/server-cert.der")" = 'CERTIFICATE-TWO'
test "$(sed -n '1p' "$output_dir/auth-token")" = 'TOKEN-TWO-012345678901234567890123'

# Never replace a dedicated credential directory that has accumulated unrelated
# state (for example a session cursor).
printf '%s\n' 'must-survive' >"$output_dir/unexpected-state"
set +e
PATH="$fake_bin:$PATH" "$helper" \
  --output-dir "$output_dir" bootstrap-host "$remote_root" >"$fixture/rejected.out" 2>&1
rejected_rc=$?
set -e
test "$rejected_rc" -ne 0
grep -Fq 'unexpected entry' "$fixture/rejected.out"
test "$(sed -n '1p' "$output_dir/server-cert.der")" = 'CERTIFICATE-TWO'
test "$(sed -n '1p' "$output_dir/auth-token")" = 'TOKEN-TWO-012345678901234567890123'

printf 'ASP client bootstrap smoke passed\n'
