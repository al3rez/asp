#!/usr/bin/env bash
# Optional x86_64 Linux cross-compiler wrapper for macOS developers/CI runners
# that have Zig but no x86_64-linux-gnu GCC toolchain. Cargo/cc-rs passes
# Rust's full target triple to the compiler; Zig uses its shorter OS/ABI
# spelling. This helper deliberately supports one target only; Linux runtime
# and PTY qualification still belongs on a Linux host.
set -euo pipefail

args=()
for arg in "$@"; do
  case "$arg" in
    --target=x86_64-unknown-linux-gnu) args+=("--target=x86_64-linux-gnu") ;;
    -target=x86_64-unknown-linux-gnu) args+=("-target=x86_64-linux-gnu") ;;
    *) args+=("$arg") ;;
  esac
done

exec zig cc -target x86_64-linux-gnu "${args[@]}"
