#!/usr/bin/env bash
# Optional x86_64 Windows/gnu cross-compiler wrapper for macOS developers/CI
# runners that have Zig but no MinGW toolchain.  Cargo/cc-rs may pass Rust's
# full target triple; Zig uses its shorter OS/ABI spelling.
#
# This helper is a compile/link check only.  It does not qualify the Windows
# service manager, PTY backend, filesystem semantics, or network-failure path.
set -euo pipefail

args=()
for arg in "$@"; do
  case "$arg" in
    --target=x86_64-pc-windows-gnu) args+=("--target=x86_64-windows-gnu") ;;
    -target=x86_64-pc-windows-gnu) args+=("-target=x86_64-windows-gnu") ;;
    *) args+=("$arg") ;;
  esac
done

exec zig cc -target x86_64-windows-gnu "${args[@]}"
