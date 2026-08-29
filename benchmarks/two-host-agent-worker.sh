#!/usr/bin/env bash
set -euo pipefail

# Run one paired agent-workload trial from a Linux client host. The daemon,
# workspaces, and metrics endpoint live on a separate server host. The matrix
# driver sends this file over SSH; stdout contains exactly two JSONL rows and
# diagnostics/progress go to stderr.
#
# The worker deliberately does not provision aspd or copy credentials. The
# operator installs the release, starts a production-shaped daemon, provisions
# the pinned certificate/token on the client, and arranges SSH access to the
# server. This keeps benchmark credentials out of result files and prevents a
# benchmark script from silently changing a production supervisor.

export LANG=C.UTF-8
export LC_ALL=C.UTF-8

if [[ $# -ne 22 && $# -ne 25 ]]; then
  echo "usage: $0 SERVER_SSH SERVER_ROOT ENDPOINT CLIENT_ASP CLIENT_CERT CLIENT_TOKEN SSH_KEY CLIENT_INTERFACE SERVER_INTERFACE SERVER_TC_SUDO SERVER_METRICS_PORT TRIAL RUN_ID DELAY_MS JITTER_MS LOSS_PERCENT RATE_MBIT DISCONNECT_SECONDS LOG_MODE SUMMARY_OUTPUT SUMMARY_TAIL_BYTES PCAP_PATH [NETWORK_EVENT_HOOK NETWORK_EVENT_KIND NETWORK_EVENT_DELAY_SECONDS]" >&2
  exit 2
fi

server_ssh=$1
server_root=$2
endpoint=$3
client_asp=$4
client_cert=$5
client_token=$6
ssh_key=$7
client_interface=$8
server_interface=$9
server_tc_sudo=${10}
server_metrics_port=${11}
trial=${12}
run_id=${13}
delay_ms=${14}
jitter_ms=${15}
loss_percent=${16}
rate_mbit=${17}
disconnect_seconds=${18}
log_mode=${19}
summary_output=${20}
summary_tail_bytes=${21}
pcap_path=${22}
network_event_hook=${23-}
network_event_kind=${24-none}
network_event_delay_seconds=${25-0}

for value_name in server_ssh server_root endpoint client_asp client_cert client_token ssh_key client_interface trial run_id delay_ms jitter_ms loss_percent rate_mbit disconnect_seconds server_metrics_port summary_output summary_tail_bytes network_event_hook network_event_kind network_event_delay_seconds; do
  value=${!value_name}
  if [[ "$value" == *$'\n'* || "$value" == *$'\r'* || "$value" == *';'* || "$value" == *'|'* || "$value" == *'&'* || "$value" == *'`'* || "$value" == *'$('* || "$value" == *'>'* || "$value" == *'<'* ]]; then
    echo "$value_name contains an unsafe shell character" >&2
    exit 2
  fi
done
if [[ ! "$server_root" =~ ^/[A-Za-z0-9._/@+-]+$ ]]; then
  echo "SERVER_ROOT must be an absolute path containing only safe path characters" >&2
  exit 2
fi
if [[ "$server_root" == "/" ]]; then
  echo "SERVER_ROOT must name the daemon workspace, not the filesystem root" >&2
  exit 2
fi
if [[ "$server_ssh" == -* || ! "$server_ssh" =~ ^[A-Za-z0-9._@:-]+$ ]]; then
  echo "SERVER_SSH must be a host/user target without SSH option syntax" >&2
  exit 2
fi
for path_name in client_asp client_cert client_token ssh_key; do
  path_value=${!path_name}
  if [[ ! "$path_value" =~ ^[A-Za-z0-9._/@+-]+$ ]]; then
    echo "$path_name must be a shell-safe client path" >&2
    exit 2
  fi
done
if [[ -n "$network_event_hook" && ! "$network_event_hook" =~ ^/[A-Za-z0-9._/@+-]+$ ]]; then
  echo "NETWORK_EVENT_HOOK must be an absolute shell-safe path" >&2
  exit 2
fi
if [[ ! "$endpoint" =~ ^[A-Za-z0-9._:-]+:[0-9]{1,5}$ ]]; then
  echo "ENDPOINT must be HOST:PORT (IPv6 literals should use a resolved host name)" >&2
  exit 2
fi
endpoint_port=${endpoint##*:}
if ((endpoint_port > 65535)); then
  echo "ENDPOINT port must be <= 65535" >&2
  exit 2
fi
if [[ ! "$client_interface" =~ ^[A-Za-z0-9_.:-]+$ ]]; then
  echo "CLIENT_INTERFACE contains unsafe characters" >&2
  exit 2
fi
if [[ -n "$server_interface" && ! "$server_interface" =~ ^[A-Za-z0-9_.:-]+$ ]]; then
  echo "SERVER_INTERFACE contains unsafe characters" >&2
  exit 2
fi
if [[ "$server_tc_sudo" != 0 && "$server_tc_sudo" != 1 ]]; then
  echo "SERVER_TC_SUDO must be 0 or 1" >&2
  exit 2
fi
for value_name in trial delay_ms jitter_ms server_metrics_port summary_tail_bytes; do
  if ! [[ "${!value_name}" =~ ^[0-9]+$ ]]; then
    echo "$value_name must be a non-negative integer" >&2
    exit 2
  fi
done
if ((trial < 1)); then
  echo "trial must be positive" >&2
  exit 2
fi
if ! [[ "$run_id" =~ ^[A-Za-z0-9._-]+$ ]]; then
  echo "RUN_ID contains unsafe characters" >&2
  exit 2
fi
if ! [[ "$loss_percent" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
  echo "LOSS_PERCENT must be a non-negative percentage" >&2
  exit 2
fi
if ! awk -v loss="$loss_percent" 'BEGIN { exit !(loss >= 0 && loss <= 100) }'; then
  echo "LOSS_PERCENT must be between 0 and 100" >&2
  exit 2
fi
if ! [[ "$rate_mbit" =~ ^[0-9]+$ ]]; then
  echo "RATE_MBIT must be a non-negative integer" >&2
  exit 2
fi
if ! [[ "$disconnect_seconds" =~ ^[0-9]+$ ]]; then
  echo "DISCONNECT_SECONDS must be a non-negative integer" >&2
  exit 2
fi
if ! [[ "$network_event_delay_seconds" =~ ^[0-9]+$ ]]; then
  echo "NETWORK_EVENT_DELAY_SECONDS must be a non-negative integer" >&2
  exit 2
fi
if ((network_event_delay_seconds > 600)); then
  echo "NETWORK_EVENT_DELAY_SECONDS must be at most 600 seconds" >&2
  exit 2
fi
case "$network_event_kind" in
  none)
    [[ -z "$network_event_hook" ]] || {
      echo "NETWORK_EVENT_HOOK requires NETWORK_EVENT_KIND migration, sleep-wake, or custom" >&2
      exit 2
    }
    ;;
  migration|sleep-wake|custom)
    [[ -n "$network_event_hook" ]] || {
      echo "NETWORK_EVENT_KIND $network_event_kind requires NETWORK_EVENT_HOOK" >&2
      exit 2
    }
    ;;
  *)
    echo "NETWORK_EVENT_KIND must be none, migration, sleep-wake, or custom" >&2
    exit 2
    ;;
esac
case "$log_mode" in
  compressible) log_command='head -c 10485760 /dev/zero' ;;
  incompressible) log_command='head -c 10485760 /dev/urandom' ;;
  mixed) log_command='head -c 5242880 /dev/zero; head -c 5242880 /dev/urandom' ;;
  *)
    echo "LOG_MODE must be compressible, incompressible, or mixed" >&2
    exit 2
    ;;
esac
if [[ "$summary_output" != 0 && "$summary_output" != 1 ]]; then
  echo "SUMMARY_OUTPUT must be 0 or 1" >&2
  exit 2
fi
if ! [[ "$summary_tail_bytes" =~ ^[1-9][0-9]*$ ]] || ((summary_tail_bytes > 1048576)); then
  echo "SUMMARY_TAIL_BYTES must be an integer from 1 to 1048576" >&2
  exit 2
fi
if [[ -n "$pcap_path" && ! "$pcap_path" =~ ^/tmp/asp-two-host-[A-Za-z0-9._-]+\.pcap$ ]]; then
  echo "PCAP_PATH must be an absolute /tmp/asp-two-host-*.pcap path" >&2
  exit 2
fi

command -v ssh >/dev/null 2>&1 || { echo 'two-host worker requires ssh' >&2; exit 2; }
command -v jq >/dev/null 2>&1 || { echo 'two-host worker requires jq' >&2; exit 2; }
command -v tc >/dev/null 2>&1 || { echo 'two-host worker requires Linux tc' >&2; exit 2; }
command -v ip >/dev/null 2>&1 || { echo 'two-host worker requires iproute2' >&2; exit 2; }
command -v timeout >/dev/null 2>&1 || { echo 'two-host worker requires timeout' >&2; exit 2; }
[[ -x "$client_asp" ]] || { echo "ASP client is not executable: $client_asp" >&2; exit 2; }
[[ -f "$client_cert" ]] || { echo "ASP certificate is missing: $client_cert" >&2; exit 2; }
[[ -f "$client_token" ]] || { echo "ASP token file is missing: $client_token" >&2; exit 2; }
[[ -f "$ssh_key" ]] || { echo "SSH key is missing: $ssh_key" >&2; exit 2; }
command -v /usr/bin/time >/dev/null 2>&1 || { echo 'two-host worker requires /usr/bin/time' >&2; exit 2; }
if [[ -n "$network_event_hook" ]]; then
  [[ -f "$network_event_hook" && ! -L "$network_event_hook" && -x "$network_event_hook" ]] || {
    echo "network event hook must be a regular executable (not a symlink): $network_event_hook" >&2
    exit 2
  }
  hook_mode=$(stat -c '%a' "$network_event_hook" 2>/dev/null || true)
  [[ "$hook_mode" =~ ^[0-7]+$ ]] || {
    echo "cannot inspect network event hook permissions: $network_event_hook" >&2
    exit 2
  }
  if (( (8#$hook_mode & 18) != 0 )); then
    echo "network event hook must not be group/world writable: $network_event_hook" >&2
    exit 2
  fi
fi
[[ -r "/sys/class/net/$client_interface/statistics/rx_bytes" ]] || {
  echo "client interface statistics are unavailable: $client_interface" >&2
  exit 2
}

ssh_base=(
  ssh -q -i "$ssh_key"
  -o BatchMode=yes
  -o ConnectTimeout=15
  -o ServerAliveInterval=5
  -o ServerAliveCountMax=3
  "$server_ssh"
)
server_exec() {
  "${ssh_base[@]}" "$@"
}

remote_fixture() {
  # Pass only generated/sanitized path arguments. The fixture is isolated
  # below the daemon root and is removed again after the trial.
  server_exec bash -s -- "$server_root" "$asp_workspace" "$ssh_workspace" <<'REMOTE_FIXTURE'
set -euo pipefail
root=$1
asp_workspace=$2
ssh_workspace=$3
case "$root/$asp_workspace/$ssh_workspace" in
  *[!A-Za-z0-9._/@+-]*) echo 'fixture path contains unsafe characters' >&2; exit 2 ;;
esac
for workspace in "$asp_workspace" "$ssh_workspace"; do
  rm -rf -- "$root/$workspace"
  mkdir -p -- "$root/$workspace/src"
  printf 'alpha function\nTODO: improve alpha\n' >"$root/$workspace/src/alpha.txt"
  printf 'beta function calls alpha\n' >"$root/$workspace/src/beta.txt"
  printf 'gamma function\nalpha integration\n' >"$root/$workspace/src/gamma.txt"
  printf '#!/bin/sh\nset -eu\ngrep -q alpha src/alpha.txt\ngrep -q beta src/beta.txt\ngrep -q gamma src/gamma.txt\n' >"$root/$workspace/test.sh"
  chmod 0755 "$root/$workspace/test.sh"
  git -C "$root/$workspace" init -q
  git -C "$root/$workspace" config user.email benchmark@example.invalid
  git -C "$root/$workspace" config user.name 'ASP benchmark'
  git -C "$root/$workspace" add .
  git -C "$root/$workspace" commit -qm fixture
done
REMOTE_FIXTURE
}

server_metrics() {
  server_exec curl -fsS --max-time 5 "http://127.0.0.1:$server_metrics_port/metrics"
}

require_metric() {
  local name=$1
  local file=$2
  local value
  value=$(awk -v metric="$name" '$1 == metric { print $2; found=1; exit } END { if (!found) exit 1 }' "$file") || {
    echo "server metrics are missing $name" >&2
    exit 1
  }
  if ! [[ "$value" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    echo "server metric $name is not a non-negative number" >&2
    exit 1
  fi
  printf '%s\n' "$value"
}

counter_delta() {
  local before=$1 after=$2
  awk -v before="$before" -v after="$after" 'BEGIN { delta=after-before; if (delta < 0) delta=0; printf "%.6f", delta }'
}

interface_counter() {
  local direction=$1
  cat "/sys/class/net/$client_interface/statistics/$direction"
}

apply_tc() {
  local interface=$1
  local sudo_prefix=$2
  local -a command=(tc qdisc replace dev "$interface" root netem delay "${delay_ms}ms" "${jitter_ms}ms" loss "${loss_percent}%")
  if ((rate_mbit > 0)); then
    command+=(rate "${rate_mbit}mbit")
  fi
  if [[ "$sudo_prefix" == 1 ]]; then
    sudo -n "${command[@]}"
  else
    "${command[@]}"
  fi
}

delete_tc() {
  local interface=$1
  local sudo_prefix=$2
  if [[ "$sudo_prefix" == 1 ]]; then
    sudo -n tc qdisc del dev "$interface" root 2>/dev/null || true
  else
    tc qdisc del dev "$interface" root 2>/dev/null || true
  fi
}

asp_workspace="agent-fixture-asp-$run_id-$trial"
ssh_workspace="agent-fixture-ssh-$run_id-$trial"
session_file="/tmp/asp-two-host-session-$run_id-$trial.json"
control_path="/tmp/asp-two-host-control-$run_id-$trial"
asp_result=/tmp/asp-two-host-asp-$run_id-$trial.json
asp_time=/tmp/asp-two-host-asp-time-$run_id-$trial
ssh_time=/tmp/asp-two-host-ssh-time-$run_id-$trial
ssh_output=/tmp/asp-two-host-ssh-output-$run_id-$trial
server_before=/tmp/asp-two-host-server-before-$run_id-$trial.metrics
server_after=/tmp/asp-two-host-server-after-$run_id-$trial.metrics
network_event_pid=''
network_event_status_file=''
network_event_log_file=''
network_event_system=''
network_event_completed_json=null
network_event_duration_json=null
pcap_pid=''
summary_args=()
if [[ "$summary_output" == 1 ]]; then
  summary_args=(--summary-output --tail-bytes "$summary_tail_bytes")
fi

cleanup() {
  set +e
  if [[ -n "$network_event_pid" ]]; then
    kill "$network_event_pid" 2>/dev/null || true
    wait "$network_event_pid" 2>/dev/null || true
  fi
  if [[ -n "$pcap_pid" ]]; then
    kill "$pcap_pid" 2>/dev/null || true
    wait "$pcap_pid" 2>/dev/null || true
  fi
  delete_tc "$client_interface" 0
  if [[ -n "$server_interface" ]]; then
    if [[ "$server_tc_sudo" == 1 ]]; then
      server_exec sudo -n tc qdisc del dev "$server_interface" root 2>/dev/null || true
    else
      server_exec tc qdisc del dev "$server_interface" root 2>/dev/null || true
    fi
  fi
  ssh -q -i "$ssh_key" -o BatchMode=yes -o ConnectTimeout=10 -S "$control_path" -O exit "$server_ssh" 2>/dev/null || true
  rm -f -- "$session_file" "$asp_result" "$asp_time" "$ssh_time" "$ssh_output" "$server_before" "$server_after"
  if [[ -n "$network_event_status_file" || -n "$network_event_log_file" ]]; then
    rm -f -- "$network_event_status_file" "$network_event_log_file"
  fi
  server_exec bash -s -- "$server_root" "$asp_workspace" "$ssh_workspace" <<'REMOTE_CLEANUP' >/dev/null 2>&1 || true
set -euo pipefail
root=$1
for workspace in "$2" "$3"; do
  case "$root/$workspace" in
    *[!A-Za-z0-9._/@+-]*) exit 2 ;;
  esac
  rm -rf -- "$root/$workspace"
done
REMOTE_CLEANUP
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

start_network_event() {
  local system=$1
  if [[ -z "$network_event_hook" ]]; then
    network_event_pid=''
    network_event_status_file=''
    network_event_log_file=''
    network_event_system=''
    network_event_completed_json=null
    network_event_duration_json=null
    return 0
  fi

  network_event_system=$system
  network_event_status_file="/tmp/asp-two-host-network-event-$run_id-$trial-$system.status"
  network_event_log_file="/tmp/asp-two-host-network-event-$run_id-$trial-$system.log"
  rm -f -- "$network_event_status_file" "$network_event_log_file"
  # The hook is intentionally an executable path, not a shell string. It can
  # switch routes, rebind an interface, or request an operator-controlled
  # sleep/wake event. It must restore the path and exit only after the event is
  # complete so the result row has a verifiable boundary.
  (
    sleep "$network_event_delay_seconds"
    event_started=$(date +%s%N)
    set +e
    ASP_NETWORK_EVENT_KIND="$network_event_kind" \
    ASP_NETWORK_EVENT_SYSTEM="$system" \
    ASP_NETWORK_EVENT_TRIAL="$trial" \
    ASP_NETWORK_EVENT_RUN_ID="$run_id" \
    ASP_NETWORK_EVENT_ENDPOINT="$endpoint" \
    ASP_NETWORK_EVENT_INTERFACE="$client_interface" \
    ASP_NETWORK_EVENT_SERVER_INTERFACE="$server_interface" \
      timeout --kill-after=10s 600s "$network_event_hook" >"$network_event_log_file" 2>&1
    event_status=$?
    event_finished=$(date +%s%N)
    printf '%s %s %s\n' "$event_status" "$event_started" "$event_finished" >"$network_event_status_file"
    exit "$event_status"
  ) &
  network_event_pid=$!
  network_event_completed_json=false
  network_event_duration_json=null
}

wait_network_event() {
  [[ -n "$network_event_hook" ]] || return 0
  local wait_status=0 event_status event_started event_finished
  wait "$network_event_pid" || wait_status=$?
  network_event_pid=''
  if [[ ! -s "$network_event_status_file" ]]; then
    echo "network event hook produced no status (system=$network_event_system, wait_status=$wait_status)" >&2
    sed -n '1,120p' "$network_event_log_file" 2>/dev/null || true
    return 1
  fi
  read -r event_status event_started event_finished <"$network_event_status_file" || {
    echo "network event hook status is malformed: $network_event_status_file" >&2
    return 1
  }
  if ! [[ "$event_status" =~ ^[0-9]+$ && "$event_started" =~ ^[0-9]+$ && "$event_finished" =~ ^[0-9]+$ ]]; then
    echo "network event hook status contains invalid timing fields: $network_event_status_file" >&2
    return 1
  fi
  if ((event_finished < event_started)); then
    echo "network event hook timing moved backwards: $network_event_status_file" >&2
    return 1
  fi
  network_event_duration_json=$(awk -v started="$event_started" -v finished="$event_finished" 'BEGIN { printf "%.6f", (finished - started) / 1000000 }')
  if ((event_status != 0 || wait_status != 0)); then
    network_event_completed_json=false
    echo "network event hook failed (system=$network_event_system, status=$event_status, wait_status=$wait_status)" >&2
    sed -n '1,120p' "$network_event_log_file" 2>/dev/null || true
    return 1
  fi
  network_event_completed_json=true
}

remote_fixture
server_metrics >"$server_before"

apply_tc "$client_interface" 0
if [[ -n "$server_interface" ]]; then
  server_tc=(tc qdisc replace dev "$server_interface" root netem delay "${delay_ms}ms" "${jitter_ms}ms" loss "${loss_percent}%")
  if ((rate_mbit > 0)); then
    server_tc+=(rate "${rate_mbit}mbit")
  fi
  if [[ "$server_tc_sudo" == 1 ]]; then
    server_exec sudo -n "${server_tc[@]}"
  else
    server_exec "${server_tc[@]}"
  fi
fi

if [[ -n "$pcap_path" ]]; then
  command -v tcpdump >/dev/null 2>&1 || {
    echo 'pcap requested but tcpdump is missing' >&2
    exit 2
  }
  endpoint_port=${endpoint##*:}
  tcpdump -i "$client_interface" -w "$pcap_path" "udp port $endpoint_port" >/dev/null 2>&1 &
  pcap_pid=$!
  sleep 0.2
  if ! kill -0 "$pcap_pid" 2>/dev/null; then
    echo 'tcpdump exited before the trial started' >&2
    exit 1
  fi
fi

rx_before=$(interface_counter rx_bytes)
tx_before=$(interface_counter tx_bytes)
server_cpu_before=$(require_metric asp_process_cpu_time_us_total "$server_before")
require_metric asp_process_max_rss_bytes "$server_before" >/dev/null
start_network_event asp
set +e
/usr/bin/time -f '%U %S %M' -o "$asp_time" --
  timeout --kill-after=5s 900s "$client_asp" \
    --cert "$client_cert" \
    --auth-token-file "$client_token" \
    --session-file "$session_file" \
    agent-workload "$endpoint" \
    --workspace "$asp_workspace" \
    --disconnect-seconds "$disconnect_seconds" \
    --log-mode "$log_mode" \
    "${summary_args[@]}" >"$asp_result"
asp_status=$?
set -e
if ! wait_network_event; then
  echo "ASP agent workload network event failed" >&2
  exit 1
fi
rx_after=$(interface_counter rx_bytes)
tx_after=$(interface_counter tx_bytes)
# Take the post-run metrics snapshot only after the interface counters are
# sampled. The SSH control request used to read loopback /metrics would
# otherwise contaminate the ASP transport-byte measurement.
server_metrics >"$server_after"
server_cpu_after=$(require_metric asp_process_cpu_time_us_total "$server_after")
server_rss_after=$(require_metric asp_process_max_rss_bytes "$server_after")
if [[ "$asp_status" -ne 0 || ! -s "$asp_result" ]]; then
  echo "ASP agent workload failed (status=$asp_status)" >&2
  tail -n 40 "$asp_result" 2>/dev/null || true
  exit 1
fi
read -r asp_client_user asp_client_system asp_client_rss <"$asp_time"
aspd_cpu_ms=$(awk -v before="$server_cpu_before" -v after="$server_cpu_after" 'BEGIN { delta=after-before; if (delta < 0) delta=0; printf "%.6f", delta / 1000 }')
aspd_rss_kb=$(awk -v bytes="$server_rss_after" 'BEGIN { if (bytes < 0) bytes=0; printf "%d", bytes / 1024 }')
asp_row=$(jq -c \
  --arg scenario "delay=${delay_ms}ms,jitter=${jitter_ms}ms,loss=${loss_percent}%,rate=${rate_mbit}mbit" \
  --arg log_mode "$log_mode" \
  --argjson trial "$trial" \
  --argjson rx_bytes "$(counter_delta "$rx_before" "$rx_after")" \
  --argjson tx_bytes "$(counter_delta "$tx_before" "$tx_after")" \
  --argjson aspd_user_cpu_ms "$aspd_cpu_ms" \
  --argjson aspd_system_cpu_ms 0 \
  --argjson aspd_rss_kb "$aspd_rss_kb" \
  --arg client_user_cpu_ms "$asp_client_user" \
  --arg client_system_cpu_ms "$asp_client_system" \
  --argjson client_max_rss_kb "$asp_client_rss" \
  --arg network_event_kind "$network_event_kind" \
  --argjson network_event_completed "$network_event_completed_json" \
  --argjson network_event_duration_ms "$network_event_duration_json" \
  --arg pcap_path "$pcap_path" \
  '. + {scenario:$scenario,log_mode:$log_mode,trial:$trial,interface_rx_bytes:($rx_bytes|tonumber),interface_tx_bytes:($tx_bytes|tonumber),status:0,aspd_user_cpu_ms:($aspd_user_cpu_ms|tonumber),aspd_system_cpu_ms:($aspd_system_cpu_ms|tonumber),aspd_rss_kb:($aspd_rss_kb|tonumber),client_user_cpu_ms:(($client_user_cpu_ms|tonumber)*1000),client_system_cpu_ms:(($client_system_cpu_ms|tonumber)*1000),client_max_rss_kb:$client_max_rss_kb,network_event_kind:$network_event_kind,network_event_completed:$network_event_completed,network_event_duration_ms:$network_event_duration_ms,pcap_path:(if $pcap_path == "" then null else $pcap_path end)}' \
  "$asp_result")
printf '%s\n' "$asp_row"

ssh_base=(
  ssh -q -i "$ssh_key"
  -o BatchMode=yes
  -o ConnectTimeout=15
  -o ServerAliveInterval=5
  -o ServerAliveCountMax=3
  -o ControlPath="$control_path"
  "$server_ssh"
)
ssh_started=$(date +%s%N)
rx_before=$(interface_counter rx_bytes)
tx_before=$(interface_counter tx_bytes)
ssh_master_start=$(date +%s%N)
"${ssh_base[@]}" -MNf
ssh_master_end=$(date +%s%N)
ssh_blocked_ns=$((ssh_master_end - ssh_master_start))
ssh_round_trips=0
ssh_payload_bytes=0
ssh_client_user_cpu_ms=0
ssh_client_system_cpu_ms=0
ssh_client_max_rss_kb=0
ssh_timed() {
  local started finished size user_cpu system_cpu max_rss
  started=$(date +%s%N)
  /usr/bin/time -f '%U %S %M' -o "$ssh_time" --
    "${ssh_base[@]}" "$@" >"$ssh_output"
  finished=$(date +%s%N)
  ssh_blocked_ns=$((ssh_blocked_ns + finished - started))
  ssh_round_trips=$((ssh_round_trips + 1))
  size=$(wc -c <"$ssh_output")
  ssh_payload_bytes=$((ssh_payload_bytes + size))
  read -r user_cpu system_cpu max_rss <"$ssh_time"
  ssh_client_user_cpu_ms=$(awk -v total="$ssh_client_user_cpu_ms" -v value="$user_cpu" 'BEGIN { printf "%.6f", total + value * 1000 }')
  ssh_client_system_cpu_ms=$(awk -v total="$ssh_client_system_cpu_ms" -v value="$system_cpu" 'BEGIN { printf "%.6f", total + value * 1000 }')
  if ((max_rss > ssh_client_max_rss_kb)); then
    ssh_client_max_rss_kb=$max_rss
  fi
}

ssh_workspace_path="$server_root/$ssh_workspace"
start_network_event ssh-controlmaster
ssh_timed "find $ssh_workspace_path -maxdepth 2 -type f -print | sort"
ssh_timed "git -C $ssh_workspace_path status --short"
ssh_timed "rg -n TODO $ssh_workspace_path"
ssh_timed "rg -n alpha $ssh_workspace_path"
ssh_timed "rg -n function $ssh_workspace_path"
for path in alpha beta gamma; do
  ssh_timed "cat $ssh_workspace_path/src/$path.txt"
  ssh_timed "printf '\\nagent edit src/$path.txt\\n' >>$ssh_workspace_path/src/$path.txt"
done
ssh_timed "cd $ssh_workspace_path && ./test.sh"
ssh_timed "$log_command"
ssh_timed "git -C $ssh_workspace_path diff --stat && git -C $ssh_workspace_path diff"
ssh_timed "wc -l $ssh_workspace_path/src/*.txt"
ssh_timed "nohup sh -c 'sleep $disconnect_seconds; printf persistent-agent-work-complete >$ssh_workspace_path/.complete' >/dev/null 2>&1 &"

"${ssh_base[@]}" -O exit >/dev/null 2>&1 || true
sleep "$((disconnect_seconds + 1))"
recovery_started=$(date +%s%N)
ssh_master_start=$(date +%s%N)
"${ssh_base[@]}" -MNf
ssh_master_end=$(date +%s%N)
ssh_blocked_ns=$((ssh_blocked_ns + ssh_master_end - ssh_master_start))
ssh_timed "test \"\$(cat $ssh_workspace_path/.complete)\" = persistent-agent-work-complete"
recovery_finished=$(date +%s%N)
ssh_timed "git -C $ssh_workspace_path status --short"
ssh_finished=$(date +%s%N)
if ! wait_network_event; then
  echo "SSH agent workload network event failed" >&2
  exit 1
fi
rx_after=$(interface_counter rx_bytes)
tx_after=$(interface_counter tx_bytes)
jq -cn \
  --arg experiment agent-workload \
  --arg system ssh-controlmaster \
  --arg scenario "delay=${delay_ms}ms,jitter=${jitter_ms}ms,loss=${loss_percent}%,rate=${rate_mbit}mbit" \
  --arg log_mode "$log_mode" \
  --argjson trial "$trial" \
  --argjson application_round_trips "$ssh_round_trips" \
  --argjson transport_connections 2 \
  --argjson application_payload_bytes "$ssh_payload_bytes" \
  --argjson wall_ms "$((ssh_finished - ssh_started))e-6" \
  --argjson network_blocked_ms "$((ssh_blocked_ns))e-6" \
  --argjson recovery_ms "$((recovery_finished - recovery_started))e-6" \
  --argjson disconnect_seconds "$disconnect_seconds" \
  --argjson interface_rx_bytes "$(counter_delta "$rx_before" "$rx_after")" \
  --argjson interface_tx_bytes "$(counter_delta "$tx_before" "$tx_after")" \
  --arg client_user_cpu_ms "$ssh_client_user_cpu_ms" \
  --arg client_system_cpu_ms "$ssh_client_system_cpu_ms" \
  --argjson client_max_rss_kb "$ssh_client_max_rss_kb" \
  --arg network_event_kind "$network_event_kind" \
  --argjson network_event_completed "$network_event_completed_json" \
  --argjson network_event_duration_ms "$network_event_duration_json" \
  --arg pcap_path "$pcap_path" \
  '{experiment:$experiment,system:$system,scenario:$scenario,log_mode:$log_mode,trial:$trial,application_round_trips:$application_round_trips,transport_connections:$transport_connections,application_payload_bytes:$application_payload_bytes,wall_ms:$wall_ms,network_blocked_ms:$network_blocked_ms,recovery_ms:$recovery_ms,disconnect_seconds:$disconnect_seconds,interface_rx_bytes:$interface_rx_bytes,interface_tx_bytes:$interface_tx_bytes,persistent_process_observed:true,status:0,client_user_cpu_ms:($client_user_cpu_ms|tonumber),client_system_cpu_ms:($client_system_cpu_ms|tonumber),client_max_rss_kb:$client_max_rss_kb,network_event_kind:$network_event_kind,network_event_completed:$network_event_completed,network_event_duration_ms:$network_event_duration_ms,pcap_path:(if $pcap_path == "" then null else $pcap_path end)}'
