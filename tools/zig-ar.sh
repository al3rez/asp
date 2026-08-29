#!/usr/bin/env bash
# Companion archive wrapper for tools/zig-cc.sh.
set -euo pipefail
exec zig ar "$@"
