#!/usr/bin/env bash
# Reproducible local Windows/gnu compile check for macOS/Linux hosts with Zig.
# This is intentionally a compile check only; Windows runtime qualification is
# performed on a Windows host in CI and remains separate from this helper.
set -euo pipefail

if ! command -v zig >/dev/null 2>&1; then
  printf '%s\n' 'check-windows-gnu: Zig is required (https://ziglang.org/download/)' >&2
  exit 2
fi

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
export CC_x86_64_pc_windows_gnu="${CC_x86_64_pc_windows_gnu:-$repo_root/tools/zig-windows-cc.sh}"
export CXX_x86_64_pc_windows_gnu="${CXX_x86_64_pc_windows_gnu:-$repo_root/tools/zig-windows-cc.sh}"
export AR_x86_64_pc_windows_gnu="${AR_x86_64_pc_windows_gnu:-$repo_root/tools/zig-windows-ar.sh}"
export RANLIB_x86_64_pc_windows_gnu="${RANLIB_x86_64_pc_windows_gnu:-true}"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="${CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER:-$repo_root/tools/zig-windows-cc.sh}"

cd "$repo_root"
exec cargo check --locked --workspace --all-features --target x86_64-pc-windows-gnu "$@"
