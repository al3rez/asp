#!/usr/bin/env bash
set -euo pipefail

# Summarize a raw JSONL benchmark without hiding failed trials. The input is
# intentionally kept as JSONL so the original rows remain suitable for later
# statistical analysis and packet-capture correlation.
if [[ $# -ne 1 ]]; then
  echo "usage: $0 RESULTS.jsonl" >&2
  exit 2
fi

input=$1
if [[ ! -f "$input" ]]; then
  echo "benchmark result file does not exist: $input" >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required to summarize benchmark JSONL" >&2
  exit 2
fi

# Percentiles use the nearest lower sample (floor((n-1)*p)) after sorting.
# This is deterministic for small trial sets and makes the reported rule
# explicit; retain the raw rows when a different estimator is needed.
jq -s '
  def percentile($p):
    if length == 0 then null else .[((length - 1) * $p) | floor] end;
  def numeric_stats($field):
    [ .[] | .[$field] | select(type == "number" and isfinite) ]
    | sort
    | {
        samples: length,
        p50: percentile(0.50),
        p90: percentile(0.90),
        p99: percentile(0.99),
        min: (if length == 0 then null else .[0] end),
        max: (if length == 0 then null else .[-1] end)
      };
  map(select(type == "object"))
  | group_by([(.experiment // "unknown"), (.system // "unknown"), (.scenario // "unknown")])
  | map(
      . as $rows
      | {
          experiment: ($rows[0].experiment // "unknown"),
          system: ($rows[0].system // "unknown"),
          scenario: ($rows[0].scenario // "unknown"),
          trials: ($rows | length),
          failures: ([ $rows[] | select((.status // 0) != 0) ] | length),
          wall_ns: ($rows | numeric_stats("wall_ns")),
          # The agent workload records fractional milliseconds so a JSONL
          # adapter can include process startup and reconnect timing without
          # rounding to nanoseconds. Keep both fields: command-latency rows
          # use wall_ns while agent rows use wall_ms.
          wall_ms: ($rows | numeric_stats("wall_ms")),
          rx_bytes: ($rows | numeric_stats("rx_bytes")),
          tx_bytes: ($rows | numeric_stats("tx_bytes")),
          application_round_trips: ($rows | numeric_stats("application_round_trips")),
          network_blocked_ms: ($rows | numeric_stats("network_blocked_ms")),
          recovery_ms: ($rows | numeric_stats("recovery_ms")),
          client_user_cpu_ms: ($rows | numeric_stats("client_user_cpu_ms")),
          client_system_cpu_ms: ($rows | numeric_stats("client_system_cpu_ms")),
          client_max_rss_kb: ($rows | numeric_stats("client_max_rss_kb")),
          aspd_user_cpu_ms: ($rows | numeric_stats("aspd_user_cpu_ms")),
          aspd_system_cpu_ms: ($rows | numeric_stats("aspd_system_cpu_ms")),
          aspd_rss_kb: ($rows | numeric_stats("aspd_rss_kb")),
          application_payload_bytes: ($rows | numeric_stats("application_payload_bytes")),
          interface_rx_bytes: ($rows | numeric_stats("interface_rx_bytes")),
          interface_tx_bytes: ($rows | numeric_stats("interface_tx_bytes")),
          quic_tx_bytes: ($rows | numeric_stats("quic_tx_bytes")),
          quic_rx_bytes: ($rows | numeric_stats("quic_rx_bytes"))
        }
    )
' "$input"
