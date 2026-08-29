#!/usr/bin/env bash
set -euo pipefail

# Run the structured coding-agent fixture in fresh containers and atomically
# collect trial-numbered JSONL rows. Each container gets a clean workspace,
# daemon, SSH server, and network namespace, so one trial cannot warm or mutate
# the next. This is intentionally separate from docker-benchmark.sh: the
# fixture includes a deliberate disconnect/reconnect and compares semantic
# operations with a warm SSH ControlMaster.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 RESULTS.jsonl [TRIALS]" >&2
  exit 2
fi
if ! command -v docker >/dev/null 2>&1; then
  echo "docker-agent-matrix.sh requires docker" >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "docker-agent-matrix.sh requires jq" >&2
  exit 2
fi

destination=$1
trials=${2:-${ASP_AGENT_REPEATS:-30}}
image=${ASP_BENCH_IMAGE:-asp-bench}
delay_ms=${ASP_AGENT_DELAY_MS:-50}
jitter_ms=${ASP_AGENT_JITTER_MS:-0}
loss_percent=${ASP_AGENT_LOSS_PERCENT:-0}
rate_mbit=${ASP_AGENT_RATE_MBIT:-100}
disconnect_seconds=${ASP_AGENT_DISCONNECT_SECONDS:-30}
summary=${ASP_AGENT_SUMMARY:-0}
summary_tail_bytes=${ASP_AGENT_SUMMARY_TAIL_BYTES:-8192}
log_mode=${ASP_AGENT_LOG_MODE:-compressible}

if ! [[ "$trials" =~ ^[1-9][0-9]*$ ]] || ((trials > 1000)); then
  echo "TRIALS must be an integer from 1 to 1000" >&2
  exit 2
fi
if ! [[ "$delay_ms" =~ ^[0-9]+$ ]]; then
  echo "ASP_AGENT_DELAY_MS must be a non-negative integer" >&2
  exit 2
fi
if ! [[ "$jitter_ms" =~ ^[0-9]+$ ]]; then
  echo "ASP_AGENT_JITTER_MS must be a non-negative integer" >&2
  exit 2
fi
if ! [[ "$loss_percent" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
  echo "ASP_AGENT_LOSS_PERCENT must be a non-negative percentage" >&2
  exit 2
fi
if ! [[ "$rate_mbit" =~ ^[1-9][0-9]*$ ]]; then
  echo "ASP_AGENT_RATE_MBIT must be a positive integer" >&2
  exit 2
fi
if ! [[ "$disconnect_seconds" =~ ^[0-9]+$ ]]; then
  echo "ASP_AGENT_DISCONNECT_SECONDS must be a non-negative integer" >&2
  exit 2
fi
if [[ "$summary" != 0 && "$summary" != 1 ]]; then
  echo "ASP_AGENT_SUMMARY must be 0 or 1" >&2
  exit 2
fi
if ! [[ "$summary_tail_bytes" =~ ^[1-9][0-9]*$ ]] || ((summary_tail_bytes > 1048576)); then
  echo "ASP_AGENT_SUMMARY_TAIL_BYTES must be an integer from 1 to 1048576" >&2
  exit 2
fi
case "$log_mode" in
  compressible|incompressible|mixed) ;;
  *)
    echo "ASP_AGENT_LOG_MODE must be compressible, incompressible, or mixed" >&2
    exit 2
    ;;
esac

destination_parent=$(dirname -- "$destination")
mkdir -p -- "$destination_parent"
destination_parent=$(cd -- "$destination_parent" && pwd)
destination_name=$(basename -- "$destination")
destination_path="$destination_parent/$destination_name"
# Keep the temporary volume beside the requested output. Docker Desktop often
# shares the repository but not an arbitrary system TMPDIR with its VM; using
# the destination's parent makes the mount work on both macOS and Linux.
workdir=$(mktemp -d "$destination_parent/.asp-agent-matrix.XXXXXX")
staging="$workdir/results.jsonl"
temporary=""
: >"$staging"
cleanup() {
  if [[ -n "$temporary" ]]; then
    rm -f -- "$temporary"
  fi
  rm -rf -- "$workdir"
}
trap cleanup EXIT INT TERM

for trial in $(seq 1 "$trials"); do
  trial_file="$workdir/trial-$trial.jsonl"
  docker run --rm \
    --cap-add NET_ADMIN \
    --entrypoint /usr/local/bin/asp-agent-workload \
    -e "ASP_AGENT_TRIAL=$trial" \
    -e "ASP_AGENT_DELAY_MS=$delay_ms" \
    -e "ASP_AGENT_JITTER_MS=$jitter_ms" \
    -e "ASP_AGENT_LOSS_PERCENT=$loss_percent" \
    -e "ASP_AGENT_RATE_MBIT=$rate_mbit" \
    -e "ASP_AGENT_DISCONNECT_SECONDS=$disconnect_seconds" \
    -e "ASP_AGENT_SUMMARY=$summary" \
    -e "ASP_AGENT_SUMMARY_TAIL_BYTES=$summary_tail_bytes" \
    -e "ASP_AGENT_LOG_MODE=$log_mode" \
    -v "$workdir:/results" \
    "$image" \
    "/results/trial-$trial.jsonl"

  if ! jq -e -s --argjson trial "$trial" '
      length == 2
      and all(.[]; type == "object" and (.trial == $trial) and (.status == 0))
      and ([.[] | .system] | sort) == ["asp", "ssh-controlmaster"]
    ' "$trial_file" >/dev/null; then
    echo "agent workload trial $trial did not produce exactly one successful ASP and SSH row" >&2
    exit 1
  fi
  cat "$trial_file" >>"$staging"
  echo "completed agent workload trial $trial/$trials" >&2
done

# A rename makes an interrupted matrix leave the caller's previous capture
# untouched. The qualification script can then reject incomplete cells before
# anyone publishes a summary.
temporary=$(mktemp "$destination_parent/.asp-agent-matrix-$destination_name.XXXXXX")
cp "$staging" "$temporary"
# Re-run the same strict row/resource/pair validation on the final atomic file.
# This keeps a future change to the per-trial check from accidentally allowing
# an incomplete publication, while the profile deliberately leaves scenario
# coverage to the caller (exact and summary contracts are separate captures).
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
bash "$script_dir/qualify-results.sh" "$temporary" "$trials" agent-workload >/dev/null
mv -f -- "$temporary" "$destination_path"
temporary=""
printf 'ASP agent matrix written to %s (%s trials per system)\n' "$destination_path" "$trials"
