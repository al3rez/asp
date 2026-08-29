#!/usr/bin/env bash
set -euo pipefail

# Contract-only regression test for the two-host qualification runner. It does
# not contact a host or claim a performance result; it catches argument,
# shaping, and output-manifest regressions in CI where a second Linux host is
# intentionally unavailable.

export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
runner="$repo_root/benchmarks/two-host-agent-matrix.sh"
grid="$repo_root/benchmarks/two-host-agent-grid.sh"
worker="$repo_root/benchmarks/two-host-agent-worker.sh"
compare="$repo_root/benchmarks/compare-agent-contracts.sh"
command -v jq >/dev/null 2>&1 || { echo 'jq is required' >&2; exit 2; }
[[ -x "$runner" && -x "$grid" && -x "$worker" && -x "$compare" ]] || { echo 'two-host runner files must be executable' >&2; exit 2; }

manifest=$(
  "$runner" --dry-run \
    --output /tmp/asp-two-host-contract.jsonl \
    --client bench-client \
    --server bench-server \
    --endpoint 100.64.0.2:4433 \
    --server-root /srv/asp/workspace \
    --client-asp /usr/local/bin/asp \
    --client-cert /home/bench/server-cert.der \
    --client-auth-token /home/bench/auth-token \
    --client-ssh-key /home/bench/id_ed25519 \
    --client-interface eth0 \
    --server-interface eth0 \
    --server-tc-sudo \
    --trials 30 \
    --delay-ms 50 \
    --jitter-ms 20 \
    --loss-percent 5 \
    --rate-mbit 10 \
    --disconnect-seconds 30 \
    --log-mode incompressible \
    --summary \
    --summary-tail-bytes 4096 \
    --network-event-hook /usr/local/bin/network-event-hook \
    --network-event-kind migration \
    --network-event-delay 7 \
    --run-id contract-smoke
)
jq -e '
  .experiment == "agent-workload"
  and .profile == "two-host"
  and .trials == 30
  and .summary_output == true
  and .server_tc_sudo == true
  and .server_metrics_port == 9443
  and .pcap_dir == null
  and .network_event_hook == "/usr/local/bin/network-event-hook"
  and .network_event_kind == "migration"
  and .network_event_delay_seconds == 7
  and .summary_tail_bytes == 4096
  and .scenario == "delay=50ms,jitter=20ms,loss=5%,rate=10mbit"
  and (.requires | index("independent client and server hosts"))
' <<<"$manifest" >/dev/null

set +e
"$runner" --dry-run \
  --output /tmp/asp-two-host-contract.jsonl \
  --client 'client host' \
  --server bench-server \
  --endpoint 100.64.0.2:4433 \
  --server-root /srv/asp/workspace \
  --client-asp /usr/local/bin/asp \
  --client-cert /home/bench/server-cert.der \
  --client-auth-token /home/bench/auth-token \
  --client-ssh-key /home/bench/id_ed25519 \
  --client-interface eth0 >/dev/null 2>&1
rc=$?
set -e
if ((rc == 0)); then
  echo 'two-host runner accepted an unsafe client target' >&2
  exit 1
fi

set +e
"$runner" --dry-run \
  --output /tmp/asp-two-host-contract.jsonl \
  --client bench-client \
  --server bench-server \
  --endpoint 100.64.0.2:4433 \
  --server-root /srv/asp/workspace \
  --client-asp /usr/local/bin/asp \
  --client-cert /home/bench/server-cert.der \
  --client-auth-token /home/bench/auth-token \
  --client-ssh-key /home/bench/id_ed25519 \
  --client-interface eth0 \
  --network-event-kind migration >/dev/null 2>&1
rc=$?
set -e
if ((rc == 0)); then
  echo 'two-host runner accepted a migration kind without an event hook' >&2
  exit 1
fi

set +e
"$worker" 1 2 3 4 5 6 7 8 9 0 9443 1 smoke 50 0 0 100 0 compressible 0 8192 '' >/dev/null 2>&1
rc=$?
set -e
if ((rc == 0)); then
  echo 'two-host worker accepted an invalid server root' >&2
  exit 1
fi

grid_manifest=$(
  "$grid" --dry-run \
    --output /tmp/asp-two-host-grid-contract.jsonl \
    --client bench-client \
    --server bench-server \
    --endpoint 100.64.0.2:4433 \
    --server-root /srv/asp/workspace \
    --client-asp /usr/local/bin/asp \
    --client-cert /home/bench/server-cert.der \
    --client-auth-token /home/bench/auth-token \
    --client-ssh-key /home/bench/id_ed25519 \
    --client-interface eth0 \
    --rtt-ms 0,20 \
    --loss-percent 0,5 \
    --jitter-ms 0 \
    --rate-mbit 1 \
    --trials 2 \
    --checkpoint-dir /tmp/asp-two-host-grid-checkpoint \
    --network-event-hook /usr/local/bin/network-event-hook \
    --network-event-kind sleep-wake \
    --network-event-delay 11 \
    --resume \
    --run-id grid-contract
)
jq -e '
  .experiment == "agent-workload"
  and .profile == "two-host-grid"
  and .cells == 4
  and .trials == 2
  and .rtt_ms == [0,20]
  and .loss_percent == [0,5]
  and .jitter_ms == [0]
  and .rate_mbit == [1]
  and .checkpoint_dir == "/tmp/asp-two-host-grid-checkpoint"
  and .network_event_hook == "/usr/local/bin/network-event-hook"
  and .network_event_kind == "sleep-wake"
  and .network_event_delay_seconds == 11
  and .resume == true
  and (.requires | index("one qualified result per cell"))
' <<<"$grid_manifest" >/dev/null

set +e
"$grid" --dry-run \
  --output /tmp/asp-two-host-grid-contract.jsonl \
  --client bench-client \
  --server bench-server \
  --endpoint 100.64.0.2:4433 \
  --server-root /srv/asp/workspace \
  --client-asp /usr/local/bin/asp \
  --client-cert /home/bench/server-cert.der \
  --client-auth-token /home/bench/auth-token \
  --client-ssh-key /home/bench/id_ed25519 \
  --client-interface eth0 \
  --rtt-ms '0,,20' >/dev/null 2>&1
rc=$?
set -e
if ((rc == 0)); then
  echo 'two-host grid accepted an empty list element' >&2
  exit 1
fi

set +e
"$grid" --dry-run \
  --output /tmp/asp-two-host-grid-contract.jsonl \
  --client bench-client \
  --server bench-server \
  --endpoint 100.64.0.2:4433 \
  --server-root /srv/asp/workspace \
  --client-asp /usr/local/bin/asp \
  --client-cert /home/bench/server-cert.der \
  --client-auth-token /home/bench/auth-token \
  --client-ssh-key /home/bench/id_ed25519 \
  --client-interface eth0 \
  --rtt-ms 20,20 >/dev/null 2>&1
rc=$?
set -e
if ((rc == 0)); then
  echo 'two-host grid accepted duplicate cell values' >&2
  exit 1
fi

set +e
"$grid" --dry-run \
  --output /tmp/asp-two-host-grid-contract.jsonl \
  --client bench-client \
  --server bench-server \
  --endpoint 100.64.0.2:4433 \
  --server-root /srv/asp/workspace \
  --client-asp /usr/local/bin/asp \
  --client-cert /home/bench/server-cert.der \
  --client-auth-token /home/bench/auth-token \
  --client-ssh-key /home/bench/id_ed25519 \
  --client-interface eth0 \
  --resume >/dev/null 2>&1
rc=$?
set -e
if ((rc == 0)); then
  echo 'two-host grid accepted --resume without a checkpoint directory' >&2
  exit 1
fi

# Keep the paired exact-vs-summary comparison honest when a physical event is
# requested. This synthetic one-trial fixture avoids contacting a host while
# proving that a migration row cannot be compared with a no-event row.
contract_tmp=$(mktemp -d "${TMPDIR:-/tmp}/asp-two-host-contract.XXXXXX")
cleanup_contract_tmp() {
  rm -rf -- "$contract_tmp"
}
trap cleanup_contract_tmp EXIT
jq -cn \
  '{experiment:"agent-workload",system:"asp",status:0,trial:1,scenario:"delay=0ms,jitter=0ms,loss=0%,rate=1mbit",summary_output:false,summary_tail_bytes:8192,log_mode:"compressible",application_round_trips:12,transport_connections:2,application_payload_bytes:10,wall_ms:20,network_blocked_ms:10,recovery_ms:3,disconnect_seconds:0,resumed_events:1,persistent_process_observed:true,quic_tx_datagrams:1,quic_tx_bytes:1,quic_rx_datagrams:1,quic_rx_bytes:1,quic_lost_packets:0,quic_congestion_events:0,quic_last_path_rtt_us:1,interface_rx_bytes:10,interface_tx_bytes:10,client_user_cpu_ms:0,client_system_cpu_ms:0,client_max_rss_kb:1,aspd_user_cpu_ms:0,aspd_system_cpu_ms:0,aspd_rss_kb:1}' \
  >"$contract_tmp/exact.jsonl"
jq -cn \
  '{experiment:"agent-workload",system:"ssh-controlmaster",status:0,trial:1,scenario:"delay=0ms,jitter=0ms,loss=0%,rate=1mbit",application_round_trips:18,transport_connections:2,application_payload_bytes:10,wall_ms:25,network_blocked_ms:11,recovery_ms:4,disconnect_seconds:0,persistent_process_observed:true,interface_rx_bytes:10,interface_tx_bytes:10,client_user_cpu_ms:0,client_system_cpu_ms:0,client_max_rss_kb:1}' \
  >>"$contract_tmp/exact.jsonl"
jq -cn \
  '{experiment:"agent-workload",system:"asp",status:0,trial:1,scenario:"delay=0ms,jitter=0ms,loss=0%,rate=1mbit",summary_output:true,summary_tail_bytes:8192,log_mode:"compressible",application_round_trips:12,transport_connections:2,application_payload_bytes:1,wall_ms:19,network_blocked_ms:9,recovery_ms:2,disconnect_seconds:0,resumed_events:1,persistent_process_observed:true,quic_tx_datagrams:1,quic_tx_bytes:1,quic_rx_datagrams:1,quic_rx_bytes:1,quic_lost_packets:0,quic_congestion_events:0,quic_last_path_rtt_us:1,interface_rx_bytes:1,interface_tx_bytes:1,client_user_cpu_ms:0,client_system_cpu_ms:0,client_max_rss_kb:1,aspd_user_cpu_ms:0,aspd_system_cpu_ms:0,aspd_rss_kb:1}' \
  >"$contract_tmp/summary.jsonl"
jq -cn \
  '{experiment:"agent-workload",system:"ssh-controlmaster",status:0,trial:1,scenario:"delay=0ms,jitter=0ms,loss=0%,rate=1mbit",application_round_trips:18,transport_connections:2,application_payload_bytes:10,wall_ms:25,network_blocked_ms:11,recovery_ms:4,disconnect_seconds:0,persistent_process_observed:true,interface_rx_bytes:10,interface_tx_bytes:10,client_user_cpu_ms:0,client_system_cpu_ms:0,client_max_rss_kb:1}' \
  >>"$contract_tmp/summary.jsonl"
bash "$compare" "$contract_tmp/exact.jsonl" "$contract_tmp/summary.jsonl" 1 >/dev/null
jq -c 'if .system == "asp" then .network_event_kind = "migration" | .network_event_completed = true else . end' \
  "$contract_tmp/exact.jsonl" >"$contract_tmp/mismatch.jsonl"
set +e
bash "$compare" "$contract_tmp/mismatch.jsonl" "$contract_tmp/summary.jsonl" 1 >/dev/null 2>&1
rc=$?
set -e
if ((rc == 0)); then
  echo 'agent contract comparison accepted mismatched network-event metadata' >&2
  exit 1
fi

# Resource counters are part of the evidence contract. A negative interface
# byte count must not survive qualification merely because it is JSON numeric.
jq -cn \
  '{experiment:"agent-workload",system:"asp",status:0,trial:1,scenario:"delay=0ms,jitter=0ms,loss=0%,rate=1mbit",application_payload_bytes:10,interface_rx_bytes:-1,interface_tx_bytes:10,client_user_cpu_ms:0,client_system_cpu_ms:0,client_max_rss_kb:1,aspd_user_cpu_ms:0,aspd_system_cpu_ms:0,aspd_rss_kb:1}' \
  >"$contract_tmp/negative-resource.jsonl"
set +e
bash "$repo_root/benchmarks/qualify-results.sh" "$contract_tmp/negative-resource.jsonl" 1 >/dev/null 2>&1
rc=$?
set -e
if ((rc == 0)); then
  echo 'benchmark qualifier accepted a negative interface-byte metric' >&2
  exit 1
fi
jq -cn \
  '{experiment:"agent-workload",system:"asp",status:0,trial:1,scenario:"delay=0ms,jitter=0ms,loss=0%,rate=1mbit",application_payload_bytes:10,interface_rx_bytes:10.5,interface_tx_bytes:10,client_user_cpu_ms:0,client_system_cpu_ms:0,client_max_rss_kb:1,aspd_user_cpu_ms:0,aspd_system_cpu_ms:0,aspd_rss_kb:1}' \
  >"$contract_tmp/fractional-resource.jsonl"
set +e
bash "$repo_root/benchmarks/qualify-results.sh" "$contract_tmp/fractional-resource.jsonl" 1 >/dev/null 2>&1
rc=$?
set -e
if ((rc == 0)); then
  echo 'benchmark qualifier accepted a fractional interface-byte counter' >&2
  exit 1
fi
jq -c 'if .system == "asp" then .wall_ms = -1 else . end' \
  "$contract_tmp/exact.jsonl" >"$contract_tmp/negative-timing.jsonl"
set +e
bash "$repo_root/benchmarks/qualify-results.sh" "$contract_tmp/negative-timing.jsonl" 1 agent-workload >/dev/null 2>&1
rc=$?
set -e
if ((rc == 0)); then
  echo 'strict agent qualifier accepted a negative wall-time metric' >&2
  exit 1
fi

echo 'two-host qualification contract smoke passed'
