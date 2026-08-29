#!/usr/bin/env bash
set -euo pipefail

# Compare two separately captured ASP agent-workload contracts.  Exact-output
# and EXEC_SUMMARY captures must not be merged: they deliberately move
# different application payloads.  This helper pairs the ASP rows by
# scenario/trial and reports the semantic savings, including interface bytes,
# without treating a single unpaired capture as production evidence.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: $0 EXACT.jsonl SUMMARY.jsonl [MIN_TRIALS]" >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "compare-agent-contracts.sh requires jq" >&2
  exit 2
fi

exact=$1
summary=$2
minimum=${3:-${ASP_BENCH_MIN_TRIALS:-30}}
for path in "$exact" "$summary"; do
  if [[ ! -f "$path" || -L "$path" ]]; then
    echo "benchmark result file must be a regular non-symlink file: $path" >&2
    exit 2
  fi
done
if ! [[ "$minimum" =~ ^[1-9][0-9]*$ ]]; then
  echo "MIN_TRIALS must be a positive integer" >&2
  exit 2
fi

# Reuse the strict row/resource/trial validation before doing arithmetic.  A
# pair is only meaningful when each input is a complete, independently
# qualified matrix capture.  The caller can pass 1 for a local smoke, but the
# default remains the 30-trial production gate.
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
bash "$script_dir/qualify-results.sh" "$exact" "$minimum" agent-workload >/dev/null
bash "$script_dir/qualify-results.sh" "$summary" "$minimum" agent-workload >/dev/null

if ! jq -e -n --slurpfile exact_document "$exact" --slurpfile summary_document "$summary" '
  def asp_rows:
    map(select(
      (.experiment == "agent-workload")
      and (.system == "asp")
      and (.status == 0)
    ));
  ($exact_document | asp_rows) as $exact
  | ($summary_document | asp_rows) as $summary
  | if ($exact | length) == 0 or ($summary | length) == 0 then
      error("both documents must contain successful ASP agent-workload rows")
    elif any($exact[]; .summary_output != false) then
      error("the exact document contains an ASP row with summary_output != false")
    elif any($summary[]; .summary_output != true) then
      error("the summary document contains an ASP row with summary_output != true")
    elif (($exact | map(.log_mode // "compressible") | unique)
      != ($summary | map(.log_mode // "compressible") | unique)) then
      error("exact and summary captures must use the same log_mode")
    elif any($exact[]; (.application_payload_bytes | type) != "number")
      or any($summary[]; (.application_payload_bytes | type) != "number") then
      error("both documents must include numeric application_payload_bytes on every ASP row")
    elif (($exact | map(.scenario) | unique) != ($summary | map(.scenario) | unique)) then
      error("exact and summary captures must contain identical scenarios")
    elif (($exact | map(.network_event_kind // "none") | unique)
      != ($summary | map(.network_event_kind // "none") | unique)) then
      error("exact and summary captures must use the same network-event kind")
    elif any(($exact + $summary)[];
      ((.network_event_kind // "none") != "none"
       and ((.network_event_completed // false) != true))) then
      error("network-event captures must report successful completion on every ASP row")
    elif (
      ($exact | sort_by([.scenario, .trial]) | group_by(.scenario)
        | map({scenario: .[0].scenario, trials: map(.trial)}))
      !=
      ($summary | sort_by([.scenario, .trial]) | group_by(.scenario)
        | map({scenario: .[0].scenario, trials: map(.trial)}))
    ) then
      error("exact and summary captures must contain identical ASP trial IDs")
    else
      true
    end
' >/dev/null; then
  echo "agent contract comparison failed: inputs are not paired exact/summary captures" >&2
  exit 1
fi

# The output is intentionally JSON so it can be archived beside raw JSONL and
# consumed by dashboards without scraping human text.  Percentiles use the
# same lower-sample estimator as summarize-results.sh.
jq -c -n --slurpfile exact_document "$exact" --slurpfile summary_document "$summary" '
  def asp_rows:
    map(select(
      (.experiment == "agent-workload")
      and (.system == "asp")
      and (.status == 0)
    ));
  def stats:
    sort as $values
    | if ($values | length) == 0 then
        {samples: 0, p50: null, p90: null, p99: null, min: null, max: null}
      else
        {
          samples: ($values | length),
          p50: $values[((($values | length) - 1) * 0.50) | floor],
          p90: $values[((($values | length) - 1) * 0.90) | floor],
          p99: $values[((($values | length) - 1) * 0.99) | floor],
          min: $values[0],
          max: $values[-1]
        }
      end;
  def field_stats($rows; $field):
    [$rows[] | .[$field] | select(type == "number" and isfinite)] | stats;
  def interface_bytes($row):
    (($row.interface_rx_bytes // 0) + ($row.interface_tx_bytes // 0));
  def reduction($before; $after):
    if $before == 0 then null else (($before - $after) / $before) end;
  def paired_reduction_stats($exact_rows; $summary_rows; $field):
    [range(0; ($exact_rows | length)) as $i
      | ($exact_rows[$i] | .[$field] // 0) as $before
      | ($summary_rows[$i] | .[$field] // 0) as $after
      | reduction($before; $after)
    ] | stats;
  def paired_delta_stats($exact_rows; $summary_rows; $field):
    [range(0; ($exact_rows | length)) as $i
      | ($exact_rows[$i] | .[$field] // 0) as $before
      | ($summary_rows[$i] | .[$field] // 0) as $after
      | ($before - $after)
    ] | stats;
  ($exact_document | asp_rows) as $exact_all
  | ($summary_document | asp_rows) as $summary_all
  | [($exact_all + $summary_all)[] | .scenario] | unique
  | map(. as $scenario
      | ($exact_all | map(select(.scenario == $scenario)) | sort_by(.trial)) as $exact
      | ($summary_all | map(select(.scenario == $scenario)) | sort_by(.trial)) as $summary
      | {
          experiment: "agent-contract",
          system: "asp",
          scenario: $scenario,
          log_mode: (($exact[0].log_mode // "compressible")),
          trials: ($exact | length),
          exact_summary_output: false,
          summary_summary_output: true,
          exact_application_payload_bytes: field_stats($exact; "application_payload_bytes"),
          summary_application_payload_bytes: field_stats($summary; "application_payload_bytes"),
          application_payload_bytes_delta: paired_delta_stats($exact; $summary; "application_payload_bytes"),
          application_payload_reduction: paired_reduction_stats($exact; $summary; "application_payload_bytes"),
          exact_interface_bytes: ([$exact[] | interface_bytes(.)] | stats),
          summary_interface_bytes: ([$summary[] | interface_bytes(.)] | stats),
          interface_bytes_delta: ([range(0; ($exact | length)) as $i
            | (interface_bytes($exact[$i]) - interface_bytes($summary[$i]))] | stats),
          interface_bytes_reduction: ([range(0; ($exact | length)) as $i
            | reduction(interface_bytes($exact[$i]); interface_bytes($summary[$i]))] | map(select(type == "number")) | stats),
          exact_network_blocked_ms: field_stats($exact; "network_blocked_ms"),
          summary_network_blocked_ms: field_stats($summary; "network_blocked_ms"),
          exact_wall_ms: field_stats($exact; "wall_ms"),
          summary_wall_ms: field_stats($summary; "wall_ms"),
          exact_recovery_ms: field_stats($exact; "recovery_ms"),
          summary_recovery_ms: field_stats($summary; "recovery_ms")
        }
    )
' 
