#!/usr/bin/env bash
set -euo pipefail

# Exercise the exact signing helpers shipped in a release archive. This uses
# an ephemeral test key only to prove the promotion boundary and fingerprint
# check; production keys/trust distribution remain operator-owned.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
archive=${1:-${ASP_RELEASE_ARCHIVE:-}}
checksum=${2:-${ASP_RELEASE_CHECKSUM:-}}
if [[ -z "$archive" ]]; then
  cat >&2 <<'USAGE'
usage: smoke-release-signature.sh RELEASE.tar.gz [RELEASE.sha256]

The archive must contain deploy/sign-release.sh and
deploy/verify-release-signature.sh. Set ASP_RELEASE_ARCHIVE and
ASP_RELEASE_CHECKSUM instead of passing positional arguments when desired.
USAGE
  exit 2
fi

if [[ ! -f "$archive" || -L "$archive" ]]; then
  echo "release archive must be a regular non-symlink file: $archive" >&2
  exit 2
fi
verifier=${ASP_RELEASE_VERIFIER:-"$repo_root/deploy/verify-release.sh"}
if [[ ! -x "$verifier" ]]; then
  echo "release verifier is missing or not executable: $verifier" >&2
  exit 2
fi
if [[ -n "$checksum" ]]; then
  "$verifier" "$archive" "$checksum" >/dev/null
else
  checksum="${archive%.tar.gz}.sha256"
  "$verifier" "$archive" "$checksum" >/dev/null
fi
command -v gpg >/dev/null 2>&1 || {
  echo "gpg is required for the release-signature smoke" >&2
  exit 2
}

extract_dir=$(mktemp -d "${TMPDIR:-/tmp}/asp-release-signature-extract.XXXXXX")
gnupg_home=$(mktemp -d "${TMPDIR:-/tmp}/asp-release-signature-gpg.XXXXXX")
chmod 700 "$gnupg_home"
cleanup() {
  rm -rf -- "$extract_dir" "$gnupg_home"
}
trap cleanup EXIT INT TERM

tar -xzf "$archive" -C "$extract_dir"
signer="$extract_dir/deploy/sign-release.sh"
signature_verifier="$extract_dir/deploy/verify-release-signature.sh"
for helper in "$signer" "$signature_verifier"; do
  if [[ ! -f "$helper" || -L "$helper" || ! -x "$helper" ]]; then
    echo "packaged release-signature helper is missing or unsafe: $helper" >&2
    exit 1
  fi
done

export GNUPGHOME="$gnupg_home"
gpg --batch --pinentry-mode loopback --passphrase '' \
  --quick-gen-key 'ASP Release Smoke <asp-release-smoke@example.invalid>' \
  ed25519 sign 1d >/dev/null 2>&1
fingerprint=$(gpg --with-colons --list-secret-keys |
  awk -F: '$1 == "fpr" { print $10; exit }')
[[ "$fingerprint" =~ ^[[:xdigit:]]{40,64}$ ]] || {
  echo "ephemeral GPG key did not produce a valid fingerprint" >&2
  exit 1
}

signature="$gnupg_home/release.sha256.asc"
"$signer" --key-id "$fingerprint" --output "$signature" \
  "$archive" "$checksum" >/dev/null
"$signature_verifier" --fingerprint "$fingerprint" \
  "$archive" "$checksum" "$signature" >/dev/null

missing_signature_prefix="$gnupg_home/missing-signature-prefix"
set +e
missing_signature_output=$("$extract_dir/deploy/install-release.sh" \
  --prefix "$missing_signature_prefix" \
  --require-signature \
  "$archive" "$checksum" 2>&1)
missing_signature_rc=$?
set -e
if [[ "$missing_signature_rc" == 0 || "$missing_signature_output" != *"release signature"* ]]; then
  printf 'installer did not require a missing signature (rc=%s):\n%s\n' \
    "$missing_signature_rc" "$missing_signature_output" >&2
  exit 1
fi

# The deployment helper must be able to enforce the same signature before it
# mutates a release prefix, not merely expose a standalone verification tool.
install_prefix="$gnupg_home/install-prefix"
"$extract_dir/deploy/install-release.sh" \
  --prefix "$install_prefix" \
  --signature "$signature" \
  --fingerprint "$fingerprint" \
  --require-signature \
  "$archive" "$checksum" >/dev/null
[[ -L "$install_prefix/current" ]]
[[ -x "$install_prefix/current/bin/asp" ]]

# Regression for archive-path replacement between verification and extraction.
# The injected verifier replaces the caller's download with an invalid archive
# after it is invoked.  The packaged installer must still install the bounded
# snapshot it took before verification; an implementation that extracts the
# original pathname would fail here (or install the replacement).
race_archive="$gnupg_home/asp-release-race.tar.gz"
cp -- "$archive" "$race_archive"
race_checksum="${race_archive%.tar.gz}.sha256"
if command -v shasum >/dev/null 2>&1; then
  (cd "$(dirname "$race_archive")" && shasum -a 256 "$(basename "$race_archive")" >"$(basename "$race_checksum")")
else
  (cd "$(dirname "$race_archive")" && sha256sum "$(basename "$race_archive")" >"$(basename "$race_checksum")")
fi
if command -v shasum >/dev/null 2>&1; then
  race_digest=$(shasum -a 256 -- "$race_archive" | awk '{print $1}')
else
  race_digest=$(sha256sum -- "$race_archive" | awk '{print $1}')
fi
race_verifier="$gnupg_home/race-verifier.sh"
printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' \
  'printf "replacement" >"$ASP_RACE_ARCHIVE"' >"$race_verifier"
chmod 700 "$race_verifier"
race_install_prefix="$gnupg_home/race-install-prefix"
ASP_RELEASE_VERIFIER="$race_verifier" ASP_RACE_ARCHIVE="$race_archive" \
  "$extract_dir/deploy/install-release.sh" \
  --prefix "$race_install_prefix" "$race_archive" "$race_checksum" >/dev/null
if [[ "$(tr -d '[:space:]' <"$race_install_prefix/current/.archive.sha256")" != "$race_digest" ]]; then
  echo "installer extracted a replaced archive instead of its verified snapshot" >&2
  exit 1
fi

# The standalone verifier must keep the same invariant.  A tiny tar shim
# replaces the caller's archive immediately before the extraction invocation;
# the verifier should still complete from its private snapshot.  The shim is
# only used for this regression and delegates every tar operation to the host
# implementation.
verify_race_archive="$gnupg_home/asp-verify-race.tar.gz"
cp -- "$archive" "$verify_race_archive"
verify_race_checksum="${verify_race_archive%.tar.gz}.sha256"
if command -v shasum >/dev/null 2>&1; then
  (cd "$(dirname "$verify_race_archive")" && shasum -a 256 "$(basename "$verify_race_archive")" >"$(basename "$verify_race_checksum")")
else
  (cd "$(dirname "$verify_race_archive")" && sha256sum "$(basename "$verify_race_archive")" >"$(basename "$verify_race_checksum")")
fi
tar_shim_dir="$gnupg_home/tar-shim"
mkdir -- "$tar_shim_dir"
real_tar=$(command -v tar)
tar_shim="$tar_shim_dir/tar"
printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' \
  'for arg in "$@"; do' \
  '  if [[ "$arg" == "-xzf" ]]; then printf replacement >"$ASP_VERIFY_RACE_ARCHIVE"; break; fi' \
  'done' \
  'exec "$ASP_REAL_TAR" "$@"' >"$tar_shim"
chmod 700 "$tar_shim"
PATH="$tar_shim_dir:$PATH" ASP_REAL_TAR="$real_tar" \
  ASP_VERIFY_RACE_ARCHIVE="$verify_race_archive" \
  "$extract_dir/deploy/verify-release.sh" \
  "$verify_race_archive" "$verify_race_checksum" >/dev/null

# Snapshotting must not weaken the original sidecar contract.  Keep a matching
# checksum in a different directory and require every packaged verifier path to
# reject it before creating or switching any release pointer.
split_checksum_dir="$gnupg_home/split-checksum"
mkdir -- "$split_checksum_dir"
split_checksum="$split_checksum_dir/$(basename "$checksum")"
cp -- "$checksum" "$split_checksum"
set +e
split_verify_output=$(
  "$extract_dir/deploy/verify-release.sh" "$archive" "$split_checksum" 2>&1
)
split_verify_rc=$?
split_signature_output=$(
  "$extract_dir/deploy/verify-release-signature.sh" \
    "$archive" "$split_checksum" "$signature" 2>&1
)
split_signature_rc=$?
split_install_output=$(
  "$extract_dir/deploy/install-release.sh" \
    --prefix "$gnupg_home/split-install-prefix" "$archive" "$split_checksum" 2>&1
)
split_install_rc=$?
set -e
for split_rc in "$split_verify_rc" "$split_signature_rc" "$split_install_rc"; do
  if [[ "$split_rc" != 2 ]]; then
    printf 'release helper accepted archive/checksum from different directories (rc=%s)\n' \
      "$split_rc" >&2
    printf '%s\n%s\n%s\n' "$split_verify_output" "$split_signature_output" \
      "$split_install_output" >&2
    exit 1
  fi
done

# A signed upgrade rechecks the signature in the installer immediately before
# extraction. Use an intentionally unavailable readiness endpoint so the test
# never starts a daemon; reaching rollback failure proves the signed install
# path ran without accepting an unsigned archive.
signed_upgrade_prefix="$gnupg_home/signed-upgrade-prefix"
"$extract_dir/deploy/install-release.sh" \
  --prefix "$signed_upgrade_prefix" \
  "$archive" "$checksum" >/dev/null
set +e
signed_upgrade_output=$("$extract_dir/deploy/upgrade-release.sh" \
  --prefix "$signed_upgrade_prefix" \
  --ready-url http://127.0.0.1:1/ready \
  --restart-command true \
  --ready-timeout-seconds 1 \
  --skip-current-ready \
  --signature "$signature" \
  --fingerprint "$fingerprint" \
  --require-signature \
  "$archive" "$checksum" 2>&1)
signed_upgrade_rc=$?
set -e
if [[ "$signed_upgrade_rc" == 0 || "$signed_upgrade_output" == *"signature verification failed"* ||
  "$signed_upgrade_output" == *"fingerprint mismatch"* ]]; then
  printf 'signed upgrade did not reach its expected readiness/rollback boundary (rc=%s):\n%s\n' \
    "$signed_upgrade_rc" "$signed_upgrade_output" >&2
  exit 1
fi

wrong_install_prefix="$gnupg_home/wrong-install-prefix"

wrong_fingerprint=$(printf '0%.0s' $(seq 1 "${#fingerprint}"))
if "$signature_verifier" --fingerprint "$wrong_fingerprint" \
  "$archive" "$checksum" "$signature" >/dev/null 2>&1; then
  echo "release signature verifier accepted an unexpected fingerprint" >&2
  exit 1
fi
if "$extract_dir/deploy/install-release.sh" \
  --prefix "$wrong_install_prefix" \
  --signature "$signature" \
  --fingerprint "$wrong_fingerprint" \
  --require-signature \
  "$archive" "$checksum" >/dev/null 2>&1; then
  echo "release installer accepted an unexpected fingerprint" >&2
  exit 1
fi

# Upgrade must authenticate before looking up or mutating the current release
# pointer. A valid signature reaches the expected no-current error; a wrong
# fingerprint is rejected first.
upgrade_prefix="$gnupg_home/upgrade-prefix"
set +e
valid_upgrade_output=$("$extract_dir/deploy/upgrade-release.sh" \
  --prefix "$upgrade_prefix" \
  --ready-url http://127.0.0.1:1/ready \
  --restart-command true \
  --signature "$signature" \
  --fingerprint "$fingerprint" \
  --require-signature \
  "$archive" "$checksum" 2>&1)
valid_upgrade_rc=$?
invalid_upgrade_output=$("$extract_dir/deploy/upgrade-release.sh" \
  --prefix "$upgrade_prefix-invalid" \
  --ready-url http://127.0.0.1:1/ready \
  --restart-command true \
  --signature "$signature" \
  --fingerprint "$wrong_fingerprint" \
  --require-signature \
  "$archive" "$checksum" 2>&1)
invalid_upgrade_rc=$?
set -e
if [[ "$valid_upgrade_rc" == 0 || "$valid_upgrade_output" != *"no current release pointer"* ]]; then
  printf 'upgrade did not reach the expected signed no-current boundary (rc=%s):\n%s\n' \
    "$valid_upgrade_rc" "$valid_upgrade_output" >&2
  exit 1
fi
if [[ "$invalid_upgrade_rc" == 0 || "$invalid_upgrade_output" != *"fingerprint mismatch"* ]]; then
  printf 'upgrade accepted or misreported an unexpected signer (rc=%s):\n%s\n' \
    "$invalid_upgrade_rc" "$invalid_upgrade_output" >&2
  exit 1
fi

printf '{"experiment":"release-signature","status":"pass","fingerprint":"%s"}\n' \
  "$fingerprint"
