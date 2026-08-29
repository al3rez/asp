#!/usr/bin/env bash
# Companion archive wrapper for tools/zig-windows-cc.sh.
set -euo pipefail
exec zig ar "$@"
