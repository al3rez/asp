#!/usr/bin/env bash
set -euo pipefail

# Build a self-contained ASP binary release without copying workspace state or
# credentials. The archive contains the client/server pair, protocol schema,
# deployment templates, license texts, and the exact lockfile used for the
# build.

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
output_dir="${1:-${repo_root}/dist}"
target="${ASP_TARGET:-}"

if [[ -e "$output_dir" && ! -d "$output_dir" ]]; then
  printf 'release output is not a directory: %s\n' "$output_dir" >&2
  exit 2
fi
mkdir -p "$output_dir"

version="$(awk -F'"' '$1 == "version = " { print $2; exit }' "$repo_root/Cargo.toml")"
if [[ -z "$version" ]]; then
  printf 'could not determine workspace version\n' >&2
  exit 2
fi

build_args=(build --locked --workspace --all-features --release)
binary_dir="$repo_root/target/release"
archive_target="$(rustc -vV | awk '$1 == "host:" { print $2 }')"
if [[ -n "$target" ]]; then
  build_args+=(--target "$target")
  binary_dir="$repo_root/target/$target/release"
  archive_target="$target"
fi

# Make the checked-in Linux cross-build path usable from one command on a
# macOS developer/release host.  `ring` (used by rustls) invokes the C
# toolchain directly, so configuring only Cargo's linker is not enough.  Keep
# every variable overrideable: CI or a release service with a real GCC/clang
# toolchain remains authoritative, while a host with Zig gets a deterministic
# fallback instead of an opaque "x86_64-linux-gnu-gcc not found" failure.
if [[ "$archive_target" == "x86_64-unknown-linux-gnu" ]] && command -v zig >/dev/null 2>&1; then
  if [[ -z "${CC_x86_64_unknown_linux_gnu:-}" ]]; then
    export CC_x86_64_unknown_linux_gnu="$repo_root/tools/zig-cc.sh"
  fi
  if [[ -z "${CXX_x86_64_unknown_linux_gnu:-}" ]]; then
    export CXX_x86_64_unknown_linux_gnu="$repo_root/tools/zig-cc.sh"
  fi
  if [[ -z "${AR_x86_64_unknown_linux_gnu:-}" ]]; then
    export AR_x86_64_unknown_linux_gnu="$repo_root/tools/zig-ar.sh"
  fi
  if [[ -z "${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER:-}" ]]; then
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$repo_root/tools/zig-cc.sh"
  fi
fi

(cd "$repo_root" && cargo "${build_args[@]}")

for binary in asp aspd; do
  if [[ ! -f "$binary_dir/$binary" ]]; then
    printf 'release binary is missing: %s/%s\n' "$binary_dir" "$binary" >&2
    exit 1
  fi
done

archive_name="asp-${version}-${archive_target}"
stage_dir="$(mktemp -d "${TMPDIR:-/tmp}/asp-release.XXXXXX")"
file_list="$(mktemp "${TMPDIR:-/tmp}/asp-release-files.XXXXXX")"
cleanup() {
  rm -rf -- "$stage_dir"
  rm -f -- "$file_list"
}
trap cleanup EXIT

install -d -m 0755 \
  "$stage_dir/bin" \
  "$stage_dir/docs" \
  "$stage_dir/docs/research" \
  "$stage_dir/deploy/systemd" \
  "$stage_dir/deploy/launchd" \
  "$stage_dir/deploy/container"
install -m 0755 "$binary_dir/asp" "$stage_dir/bin/asp"
install -m 0755 "$binary_dir/aspd" "$stage_dir/bin/aspd"
install -m 0644 "$repo_root/Cargo.lock" "$stage_dir/Cargo.lock"
install -m 0644 "$repo_root/README.md" "$stage_dir/README.md"
install -m 0644 "$repo_root/LICENSE-MIT" "$stage_dir/LICENSE-MIT"
install -m 0644 "$repo_root/LICENSE-APACHE" "$stage_dir/LICENSE-APACHE"
install -m 0644 "$repo_root/docs/schema.json" "$stage_dir/docs/schema.json"
for document in "$repo_root"/docs/*.md; do
  install -m 0644 "$document" "$stage_dir/docs/$(basename "$document")"
done
for document in "$repo_root"/docs/research/*.md; do
  install -m 0644 "$document" "$stage_dir/docs/research/$(basename "$document")"
done
install -m 0644 "$repo_root/deploy/systemd/aspd.service" "$stage_dir/deploy/systemd/aspd.service"
install -m 0644 "$repo_root/deploy/systemd/aspd-production.service" "$stage_dir/deploy/systemd/aspd-production.service"
install -m 0644 "$repo_root/deploy/systemd/README.md" "$stage_dir/deploy/systemd/README.md"
install -m 0644 "$repo_root/deploy/launchd/com.asp.aspd.plist" "$stage_dir/deploy/launchd/com.asp.aspd.plist"
install -m 0644 "$repo_root/deploy/launchd/com.asp.aspd-production.plist" "$stage_dir/deploy/launchd/com.asp.aspd-production.plist"
install -m 0644 "$repo_root/deploy/launchd/README.md" "$stage_dir/deploy/launchd/README.md"
install -m 0644 "$repo_root/deploy/container/Dockerfile" "$stage_dir/deploy/container/Dockerfile"
install -m 0644 "$repo_root/deploy/container/README.md" "$stage_dir/deploy/container/README.md"
install -m 0555 "$repo_root/deploy/container/asp-worker-wrapper" "$stage_dir/deploy/container/asp-worker-wrapper"
install -m 0755 "$repo_root/deploy/verify-release.sh" "$stage_dir/deploy/verify-release.sh"
install -m 0755 "$repo_root/deploy/install-release.sh" "$stage_dir/deploy/install-release.sh"
install -m 0755 "$repo_root/deploy/upgrade-release.sh" "$stage_dir/deploy/upgrade-release.sh"
install -m 0755 "$repo_root/deploy/bootstrap-client.sh" "$stage_dir/deploy/bootstrap-client.sh"
install -m 0755 "$repo_root/deploy/generate-sbom.sh" "$stage_dir/deploy/generate-sbom.sh"
install -m 0755 "$repo_root/deploy/sign-release.sh" "$stage_dir/deploy/sign-release.sh"
install -m 0755 "$repo_root/deploy/verify-release-signature.sh" "$stage_dir/deploy/verify-release-signature.sh"
"$repo_root/deploy/generate-sbom.sh" \
  "$stage_dir/SBOM.spdx.json" "$version" "$archive_target"

archive_path="$output_dir/${archive_name}.tar.gz"
# Make the archive reproducible across invocations and build hosts.  BSD tar
# (the default on macOS) does not provide GNU tar's --sort/--mtime switches,
# so normalize the staged tree explicitly and feed a byte-sorted file list.
# The list already contains every directory and file; disable tar's implicit
# directory recursion so entries are emitted exactly once.
# The fixed epoch is deliberate: the archive is identified by its checksum,
# while release provenance belongs in the surrounding signing/promotion
# system.  Numeric root ownership avoids leaking the packager's local UID or
# group into a public artifact.  The final gzip -n also clears the timestamp
# bsdtar would otherwise embed in its gzip header.
TZ=UTC find "$stage_dir" -exec touch -t 197001010000 {} +
(
  cd "$stage_dir"
  LC_ALL=C find . -print | LC_ALL=C sort >"$file_list"
)
tar -C "$stage_dir" --format ustar \
  --no-recursion \
  --uid 0 --gid 0 --uname root --gname root \
  -cf - -T "$file_list" | gzip -n >"$archive_path"

checksum_path="$output_dir/${archive_name}.sha256"
if command -v shasum >/dev/null 2>&1; then
  (cd "$output_dir" && shasum -a 256 "$(basename "$archive_path")" > "$(basename "$checksum_path")")
elif command -v sha256sum >/dev/null 2>&1; then
  (cd "$output_dir" && sha256sum "$(basename "$archive_path")" > "$(basename "$checksum_path")")
else
  printf 'neither shasum nor sha256sum is available\n' >&2
  exit 1
fi
# The archive and checksum contain only public release material.  Set their
# modes explicitly instead of inheriting a caller's restrictive umask (which
# otherwise makes a generated release unreadable to the deployment user).
chmod 0644 "$archive_path" "$checksum_path"

"$repo_root/deploy/verify-release.sh" "$archive_path" "$checksum_path"

printf 'ASP release written to %s\n' "$archive_path"
printf 'SHA-256 written to %s\n' "$checksum_path"
