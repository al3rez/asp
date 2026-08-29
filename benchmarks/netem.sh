#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "netem.sh requires Linux" >&2
  exit 2
fi

if [[ $# -lt 5 ]]; then
  echo "usage: sudo $0 DEVICE DELAY_MS JITTER_MS LOSS_PERCENT RATE_MBIT -- COMMAND..." >&2
  exit 2
fi

device=$1
delay_ms=$2
jitter_ms=$3
loss_percent=$4
rate_mbit=$5
shift 5

if [[ "${1:-}" != "--" ]]; then
  echo "missing -- before command" >&2
  exit 2
fi
shift

cleanup() {
  tc qdisc del dev "$device" root 2>/dev/null || true
}
trap cleanup EXIT INT TERM

tc qdisc replace dev "$device" root netem \
  delay "${delay_ms}ms" "${jitter_ms}ms" \
  loss "${loss_percent}%" \
  rate "${rate_mbit}mbit"

"$@"

