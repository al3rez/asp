#!/usr/bin/env bash
set -euo pipefail

# Validate a captured benchmark JSONL file before using it as production
# evidence. This intentionally does not compute percentiles; keep that work
# in summarize-results.sh and retain the raw rows for a different estimator or
# confidence interval calculation.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

if [[ $# -lt 1 || $# -gt 3 ]]; then
  echo "usage: $0 RESULTS.jsonl [MIN_TRIALS] [MATRIX_PROFILE]" >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "qualify-results.sh requires jq" >&2
  exit 2
fi

results=$1
min_trials=${2:-${ASP_BENCH_MIN_TRIALS:-30}}
matrix_profile=${3:-${ASP_BENCH_MATRIX_PROFILE:-}}
if [[ ! -f "$results" || -L "$results" ]]; then
  echo "benchmark result file must be a regular non-symlink file: $results" >&2
  exit 2
fi
if ! [[ "$min_trials" =~ ^[1-9][0-9]*$ ]]; then
  echo "MIN_TRIALS must be a positive integer" >&2
  exit 2
fi
case "$matrix_profile" in
  ""|agent-workload|command-latency) ;;
  *)
    echo "MATRIX_PROFILE must be empty, agent-workload, or command-latency" >&2
    exit 2
    ;;
esac

# The ordinary gate validates every cell that is present.  A release comparison
# also needs a coverage contract: otherwise a runner can omit a difficult
# impairment scenario and still produce a green summary.  Keep these names in
# lockstep with docker-benchmark.sh, which emits one-way netem descriptors.
command_latency_scenarios=$(cat <<'EOF'
rtt-0:delay=0ms,jitter=0ms,loss=0%,rate=100mbit
rtt-20:delay=10ms,jitter=0ms,loss=0%,rate=100mbit
rtt-100:delay=50ms,jitter=0ms,loss=0%,rate=100mbit
rtt-200:delay=100ms,jitter=0ms,loss=0%,rate=100mbit
rtt-300:delay=150ms,jitter=0ms,loss=0%,rate=100mbit
loss-1:delay=50ms,jitter=0ms,loss=1%,rate=100mbit
loss-5:delay=50ms,jitter=0ms,loss=5%,rate=100mbit
loss-10:delay=50ms,jitter=0ms,loss=10%,rate=100mbit
jitter-20:delay=50ms,jitter=20ms,loss=0%,rate=100mbit
jitter-100:delay=50ms,jitter=100ms,loss=0%,rate=100mbit
bandwidth-1:delay=50ms,jitter=0ms,loss=0%,rate=1mbit
bandwidth-10:delay=50ms,jitter=0ms,loss=0%,rate=10mbit
corner:delay=150ms,jitter=100ms,loss=10%,rate=1mbit
EOF
)
command_latency_systems='asp,ssh,mosh'
agent_workload_systems='asp,ssh-controlmaster'

# Read the complete JSONL once. The benchmark matrix is intentionally bounded
# (13 cells x 30 trials for the default command sweep), while jq's strict
# parser also gives a useful line number for malformed captures. Required
# Resource and workload fields are evidence, not decoration: missing,
# non-finite, negative, or otherwise non-numeric values must fail before a
# summary can turn them into a plausible-looking percentile. Byte/RSS/count
# counters are integers; elapsed/CPU timings may retain fractional
# milliseconds. The stricter timing/payload contract is enabled for the
# production agent-workload profile; the unprofiled mode remains compatible
# with older hand-authored smoke fixtures.
if ! jq -e -s \
  --argjson minimum "$min_trials" \
  --arg profile "$matrix_profile" \
  --arg required_command_scenarios "$command_latency_scenarios" \
  --arg required_command_systems "$command_latency_systems" \
  --arg required_agent_systems "$agent_workload_systems" '
  def nonnegative_number($name):
    (.[$name] | type == "number" and isfinite and . >= 0);
  def nonnegative_integer($name):
    (.[$name] | type == "number" and isfinite and . >= 0 and floor == .);

  if length == 0 then
    error("benchmark result file is empty")
  elif any(.[];
      type != "object"
      or (.experiment | type) != "string" or (.experiment | length) == 0
      or (.system | type) != "string" or (.system | length) == 0
      or (.scenario | type) != "string" or (.scenario | length) == 0
      or (.trial | type) != "number" or (.trial < 1) or ((.trial % 1) != 0)
      or (.status | type) != "number" or ((.status % 1) != 0)
      or (.experiment == "agent-workload" and (
          (nonnegative_number("client_user_cpu_ms") | not)
          or (nonnegative_number("client_system_cpu_ms") | not)
          or (nonnegative_integer("client_max_rss_kb") | not)
          or (nonnegative_integer("interface_rx_bytes") | not)
          or (nonnegative_integer("interface_tx_bytes") | not)
          or (.system == "asp" and (
              (nonnegative_number("aspd_user_cpu_ms") | not)
              or (nonnegative_number("aspd_system_cpu_ms") | not)
              or (nonnegative_integer("aspd_rss_kb") | not)
            ))
          or ($profile == "agent-workload" and (
              (nonnegative_integer("application_round_trips") | not)
              or (nonnegative_integer("transport_connections") | not)
              or (nonnegative_integer("application_payload_bytes") | not)
              or (nonnegative_number("wall_ms") | not)
              or (nonnegative_number("network_blocked_ms") | not)
              or (nonnegative_number("recovery_ms") | not)
              or (nonnegative_integer("disconnect_seconds") | not)
              or (.persistent_process_observed != true)
              or (.system == "asp" and (
              (nonnegative_integer("quic_tx_datagrams") | not)
              or (nonnegative_integer("quic_tx_bytes") | not)
              or (nonnegative_integer("quic_rx_datagrams") | not)
              or (nonnegative_integer("quic_rx_bytes") | not)
              or (nonnegative_integer("quic_lost_packets") | not)
              or (nonnegative_integer("quic_congestion_events") | not)
              or (nonnegative_integer("quic_last_path_rtt_us") | not)
              or ((.summary_output | type) != "boolean")
              or (nonnegative_integer("summary_tail_bytes") | not)
              or (nonnegative_integer("resumed_events") | not)
                ))
            ))
        ))
    ) then
    error("every row must contain valid identity/status fields; production agent-workload rows must also contain finite non-negative timing/payload/count, CPU/RSS, and transport metrics")
  elif any(.[]; (.status != 0)) then
    error("one or more benchmark trials failed (status must be zero)")
  else
    .
  end
  # A comparison is only meaningful when every system in one experiment and
  # scenario was measured on the same trial IDs. The matrix runner already
  # emits paired IDs; enforce that contract here as well so a hand-edited or
  # partially merged capture cannot look complete.
  | sort_by([.experiment, .scenario, .system, .trial])
  | group_by([.experiment, .scenario])
  | map(
      . as $cell
      | (group_by(.system) | map({
          system: .[0].system,
          trials: (map(.trial) | sort | unique)
        })) as $systems
      | if ($systems | length) > 1
          and any($systems[]; .trials != $systems[0].trials) then
          error("systems in an experiment/scenario cell must share identical trial IDs")
        else
          $cell
        end
    )
  | add
  | sort_by([.experiment, .system, .scenario, .trial])
  | group_by([.experiment, .system, .scenario])
  | map({
      experiment: .[0].experiment,
      system: .[0].system,
      scenario: .[0].scenario,
      trials: length,
      unique_trials: (map(.trial) | unique | length),
      first_trial: (map(.trial) | min),
      last_trial: (map(.trial) | max),
      min_required: $minimum
    })
  | if any(.[]; .trials < .min_required) then
      error("at least MIN_TRIALS rows are required in every experiment/system/scenario cell")
    elif any(.[]; .unique_trials != .trials) then
      error("duplicate trial numbers found in an experiment/system/scenario cell")
    elif any(.[]; .first_trial != 1 or .last_trial != .trials) then
      error("trial IDs in every cell must be contiguous and start at 1")
    elif $profile == "command-latency" and (
      ([.[] | select(.experiment == "command-latency") | .scenario] | unique)
        != ($required_command_scenarios | split("\n") | map(select(length > 0)) | sort)
      or
      ([.[] | select(.experiment == "command-latency") | .system] | unique)
        != ($required_command_systems | split(",") | sort)
    ) then
      error("command-latency profile requires all 13 impairment scenarios and asp/ssh/mosh systems")
    elif $profile == "agent-workload" and (
      ([.[] | select(.experiment == "agent-workload") | .system] | unique)
        != ($required_agent_systems | split(",") | sort)
    ) then
      error("agent-workload profile requires asp and ssh-controlmaster systems")
    else
      .
    end
' "$results" >/dev/null; then
  echo "benchmark qualification failed: $results" >&2
  exit 1
fi

jq -c -s --argjson minimum "$min_trials" '
  sort_by([.experiment, .system, .scenario, .trial])
  | group_by([.experiment, .system, .scenario])
  | .[]
  | {
      experiment: .[0].experiment,
      system: .[0].system,
      scenario: .[0].scenario,
      trials: length,
      min_required: $minimum,
      first_trial: (map(.trial) | min),
      last_trial: (map(.trial) | max)
    }
' "$results"
echo "benchmark qualification passed: $results (minimum trials per cell: $min_trials)"
