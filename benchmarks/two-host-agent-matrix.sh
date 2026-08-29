#!/usr/bin/env bash
set -euo pipefail

# Run the structured agent workload from an independently managed client host
# against an independently managed ASP server host. The worker is transferred
# over SSH for each trial, so the control machine never needs access to the
# server's workspace or credentials. Results are staged and atomically renamed
# only after paired/resource-aware qualification succeeds.

export LANG=C.UTF-8
export LC_ALL=C.UTF-8

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
worker="$script_dir/two-host-agent-worker.sh"

usage() {
  cat >&2 <<'USAGE'
usage: two-host-agent-matrix.sh --output RESULTS.jsonl --client SSH_TARGET \
  --server SSH_TARGET --endpoint HOST:PORT --server-root ABS_PATH \
  [--server-aspd PATH] \
  --client-asp PATH --client-cert PATH --client-auth-token PATH \
  --client-ssh-key PATH --client-interface IFACE [options]

Required paths are interpreted on the client host. The server must run aspd
with a loopback metrics endpoint (default port 9443). SSH uses normal
host-key verification; the benchmark never disables it.

Options:
  --trials N                 trials per cell (default: 30)
  --server-interface IFACE   also shape the server egress interface
  --server-aspd PATH         aspd binary path on the server (default: /usr/local/bin/aspd)
  --server-tc-sudo           run server tc through sudo -n
  --server-metrics-port N    server loopback /metrics port (default: 9443)
  --delay-ms N               one-way netem delay on each shaped side (default: 50)
  --jitter-ms N              netem jitter (default: 0)
  --loss-percent P           netem loss percentage (default: 0)
  --rate-mbit N              per-direction netem rate, 0 means unlimited (default: 100)
  --disconnect-seconds N     deliberate outage in each workload (default: 30)
  --log-mode MODE            compressible, incompressible, or mixed (default: compressible)
  --summary                  use EXEC_SUMMARY with an 8 KiB tail for ASP
  --summary-tail-bytes N     bounded EXEC_SUMMARY tail (default: 8192)
  --pcap-dir DIR             fetch one client-side UDP pcap per trial
  --network-event-hook PATH  client-host executable for migration/sleep hook
  --network-event-kind KIND  migration, sleep-wake, or custom (default: none)
  --network-event-delay N    seconds before invoking the event hook (default: 5)
  --run-id ID                stable safe run identifier (default: generated)
  --dry-run                  validate arguments and print the planned contract
USAGE
}

die() {
  echo "two-host-agent-matrix.sh: $*" >&2
  exit 2
}

require_value() {
  [[ $# -ge 2 && -n "$2" ]] || die "$1 requires a value"
}

output=''
client_target=''
server_target=''
endpoint=''
server_root=''
server_aspd=${ASP_TWO_HOST_SERVER_ASPD:-/usr/local/bin/aspd}
client_asp=''
client_cert=''
client_token=''
client_ssh_key=''
client_interface=''
server_interface=''
server_tc_sudo=0
server_metrics_port=9443
trials=${ASP_TWO_HOST_TRIALS:-30}
delay_ms=${ASP_TWO_HOST_DELAY_MS:-50}
jitter_ms=${ASP_TWO_HOST_JITTER_MS:-0}
loss_percent=${ASP_TWO_HOST_LOSS_PERCENT:-0}
rate_mbit=${ASP_TWO_HOST_RATE_MBIT:-100}
disconnect_seconds=${ASP_TWO_HOST_DISCONNECT_SECONDS:-30}
log_mode=${ASP_TWO_HOST_LOG_MODE:-compressible}
summary=0
summary_tail_bytes=${ASP_TWO_HOST_SUMMARY_TAIL_BYTES:-8192}
pcap_dir=''
network_event_hook=''
network_event_kind=none
network_event_delay_seconds=5
run_id=${ASP_TWO_HOST_RUN_ID:-}
dry_run=0

while (($# > 0)); do
  case "$1" in
    --output) require_value "$1" "${2-}"; output=$2; shift 2 ;;
    --client) require_value "$1" "${2-}"; client_target=$2; shift 2 ;;
    --server) require_value "$1" "${2-}"; server_target=$2; shift 2 ;;
    --endpoint) require_value "$1" "${2-}"; endpoint=$2; shift 2 ;;
    --server-root) require_value "$1" "${2-}"; server_root=$2; shift 2 ;;
    --server-aspd) require_value "$1" "${2-}"; server_aspd=$2; shift 2 ;;
    --client-asp) require_value "$1" "${2-}"; client_asp=$2; shift 2 ;;
    --client-cert) require_value "$1" "${2-}"; client_cert=$2; shift 2 ;;
    --client-auth-token) require_value "$1" "${2-}"; client_token=$2; shift 2 ;;
    --client-ssh-key) require_value "$1" "${2-}"; client_ssh_key=$2; shift 2 ;;
    --client-interface) require_value "$1" "${2-}"; client_interface=$2; shift 2 ;;
    --server-interface) require_value "$1" "${2-}"; server_interface=$2; shift 2 ;;
    --server-tc-sudo) server_tc_sudo=1; shift ;;
    --server-metrics-port) require_value "$1" "${2-}"; server_metrics_port=$2; shift 2 ;;
    --trials) require_value "$1" "${2-}"; trials=$2; shift 2 ;;
    --delay-ms) require_value "$1" "${2-}"; delay_ms=$2; shift 2 ;;
    --jitter-ms) require_value "$1" "${2-}"; jitter_ms=$2; shift 2 ;;
    --loss-percent) require_value "$1" "${2-}"; loss_percent=$2; shift 2 ;;
    --rate-mbit) require_value "$1" "${2-}"; rate_mbit=$2; shift 2 ;;
    --disconnect-seconds) require_value "$1" "${2-}"; disconnect_seconds=$2; shift 2 ;;
    --log-mode) require_value "$1" "${2-}"; log_mode=$2; shift 2 ;;
    --summary) summary=1; shift ;;
    --summary-tail-bytes) require_value "$1" "${2-}"; summary_tail_bytes=$2; shift 2 ;;
    --pcap-dir) require_value "$1" "${2-}"; pcap_dir=$2; shift 2 ;;
    --network-event-hook) require_value "$1" "${2-}"; network_event_hook=$2; shift 2 ;;
    --network-event-kind) require_value "$1" "${2-}"; network_event_kind=$2; shift 2 ;;
    --network-event-delay) require_value "$1" "${2-}"; network_event_delay_seconds=$2; shift 2 ;;
    --run-id) require_value "$1" "${2-}"; run_id=$2; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

[[ -n "$output" ]] || die "--output is required"
[[ -n "$client_target" ]] || die "--client is required"
[[ -n "$server_target" ]] || die "--server is required"
[[ -n "$endpoint" ]] || die "--endpoint is required"
[[ -n "$server_root" ]] || die "--server-root is required"
[[ -n "$client_asp" ]] || die "--client-asp is required"
[[ -n "$client_cert" ]] || die "--client-cert is required"
[[ -n "$client_token" ]] || die "--client-auth-token is required"
[[ -n "$client_ssh_key" ]] || die "--client-ssh-key is required"
[[ -n "$client_interface" ]] || die "--client-interface is required"

# The worker executes shell snippets for the generated fixture and SSH
# baseline. Reject whitespace/metacharacters before they cross either SSH
# boundary. Operator-controlled paths can still contain '-' and '@', but not
# shell syntax; use a wrapper or a pre-created fixture for more exotic paths.
for name in client_target server_target endpoint server_root server_aspd client_asp client_cert client_token client_ssh_key client_interface server_interface pcap_dir network_event_hook network_event_kind run_id; do
  value=${!name}
  if [[ "$value" == *[[:space:]]* || "$value" == *$'\n'* || "$value" == *$'\r'* || "$value" == *';'* || "$value" == *'|'* || "$value" == *'&'* || "$value" == *'`'* || "$value" == *'$('* || "$value" == *'>'* || "$value" == *'<'* ]]; then
    die "$name contains whitespace or an unsafe shell character"
  fi
done
[[ "$server_root" =~ ^/[A-Za-z0-9._/@+-]+$ ]] || die "--server-root must be an absolute safe path"
[[ "$server_root" != "/" ]] || die "--server-root must name the daemon workspace, not /"
[[ "$client_target" != -* && "$client_target" =~ ^[A-Za-z0-9._@:-]+$ ]] || die "--client must be a host/user target without SSH option syntax"
[[ "$server_target" != -* && "$server_target" =~ ^[A-Za-z0-9._@:-]+$ ]] || die "--server must be a host/user target without SSH option syntax"
[[ "$endpoint" =~ ^[A-Za-z0-9._:-]+:[0-9]{1,5}$ ]] || die "--endpoint must be HOST:PORT"
endpoint_port=${endpoint##*:}
((endpoint_port <= 65535)) || die "--endpoint port must be <= 65535"
[[ "$client_interface" =~ ^[A-Za-z0-9_.:-]+$ ]] || die "--client-interface contains unsafe characters"
[[ -z "$server_interface" || "$server_interface" =~ ^[A-Za-z0-9_.:-]+$ ]] || die "--server-interface contains unsafe characters"
for name in client_asp client_cert client_token client_ssh_key; do
  [[ "${!name}" =~ ^[A-Za-z0-9._/@+-]+$ ]] || die "--${name//_/-} must be a shell-safe client path"
done
[[ -z "$network_event_hook" || "$network_event_hook" =~ ^/[A-Za-z0-9._/@+-]+$ ]] || die "--network-event-hook must be an absolute safe client path"
case "$network_event_kind" in
  none)
    [[ -z "$network_event_hook" ]] || die "--network-event-hook requires --network-event-kind migration, sleep-wake, or custom"
    ;;
  migration|sleep-wake|custom)
    [[ -n "$network_event_hook" ]] || die "--network-event-kind $network_event_kind requires --network-event-hook"
    ;;
  *) die "--network-event-kind must be none, migration, sleep-wake, or custom" ;;
esac
[[ "$server_aspd" =~ ^[A-Za-z0-9._/@+-]+$ ]] || die "--server-aspd must be a shell-safe path"
[[ -z "$pcap_dir" || "$pcap_dir" =~ ^/[A-Za-z0-9._/@+-]+$ ]] || die "--pcap-dir must be an absolute safe directory"
[[ -z "$run_id" || "$run_id" =~ ^[A-Za-z0-9._-]+$ ]] || die "--run-id contains unsafe characters"

for name in trials server_metrics_port delay_ms jitter_ms rate_mbit disconnect_seconds summary_tail_bytes network_event_delay_seconds; do
  value=${!name}
  [[ "$value" =~ ^[0-9]+$ ]] || die "$name must be a non-negative integer"
done
((trials >= 1 && trials <= 1000)) || die "--trials must be between 1 and 1000"
((server_metrics_port <= 65535)) || die "--server-metrics-port must be <= 65535"
((summary_tail_bytes >= 1 && summary_tail_bytes <= 1048576)) || die "--summary-tail-bytes must be 1..1048576"
((network_event_delay_seconds <= 600)) || die "--network-event-delay must be at most 600 seconds"
[[ "$loss_percent" =~ ^[0-9]+([.][0-9]+)?$ ]] || die "--loss-percent must be a non-negative percentage"
awk -v loss="$loss_percent" 'BEGIN { exit !(loss >= 0 && loss <= 100) }' || die "--loss-percent must be between 0 and 100"
case "$log_mode" in
  compressible|incompressible|mixed) ;;
  *) die "--log-mode must be compressible, incompressible, or mixed" ;;
esac
command -v ssh >/dev/null 2>&1 || die "ssh is required on the control host"
command -v jq >/dev/null 2>&1 || die "jq is required on the control host"
[[ -x "$worker" ]] || die "worker is missing or not executable: $worker"

if [[ -z "$run_id" ]]; then
  run_id="$(date -u +%Y%m%dT%H%M%SZ)-$$"
fi
if [[ -n "$pcap_dir" ]]; then
  mkdir -p -- "$pcap_dir"
  pcap_dir=$(cd -- "$pcap_dir" && pwd)
fi

scenario="delay=${delay_ms}ms,jitter=${jitter_ms}ms,loss=${loss_percent}%,rate=${rate_mbit}mbit"
if ((dry_run == 1)); then
  jq -cn \
    --arg output "$output" \
    --arg client "$client_target" \
    --arg server "$server_target" \
    --arg endpoint "$endpoint" \
    --arg server_root "$server_root" \
    --arg server_aspd "$server_aspd" \
    --arg server_interface "$server_interface" \
    --arg pcap_dir "$pcap_dir" \
    --arg network_event_hook "$network_event_hook" \
    --arg network_event_kind "$network_event_kind" \
    --arg scenario "$scenario" \
    --arg log_mode "$log_mode" \
    --arg run_id "$run_id" \
    --argjson trials "$trials" \
    --argjson server_metrics_port "$server_metrics_port" \
    --argjson server_tc_sudo "$server_tc_sudo" \
    --argjson disconnect_seconds "$disconnect_seconds" \
    --argjson summary_tail_bytes "$summary_tail_bytes" \
    --argjson summary "$summary" \
    --argjson network_event_delay_seconds "$network_event_delay_seconds" \
    '{experiment:"agent-workload",profile:"two-host",output:$output,client:$client,server:$server,endpoint:$endpoint,server_root:$server_root,server_aspd:$server_aspd,server_interface:$server_interface,server_tc_sudo:($server_tc_sudo == 1),server_metrics_port:$server_metrics_port,pcap_dir:(if $pcap_dir == "" then null else $pcap_dir end),network_event_hook:(if $network_event_hook == "" then null else $network_event_hook end),network_event_kind:$network_event_kind,network_event_delay_seconds:$network_event_delay_seconds,scenario:$scenario,log_mode:$log_mode,trials:$trials,disconnect_seconds:$disconnect_seconds,summary_output:($summary == 1),summary_tail_bytes:$summary_tail_bytes,run_id:$run_id,requires:["independent client and server hosts","Linux tc/iproute2 on client","aspd loopback /metrics","normal SSH host-key verification"]}'
  exit 0
fi

destination_parent=$(dirname -- "$output")
mkdir -p -- "$destination_parent"
destination_parent=$(cd -- "$destination_parent" && pwd)
destination_name=$(basename -- "$output")
destination_path="$destination_parent/$destination_name"
workdir=$(mktemp -d "$destination_parent/.asp-two-host.XXXXXX")
staging="$workdir/results.jsonl"
temporary=''
metadata_temporary=''
: >"$staging"
cleanup() {
  if [[ -n "$temporary" ]]; then
    rm -f -- "$temporary"
  fi
  if [[ -n "$metadata_temporary" ]]; then
    rm -f -- "$metadata_temporary"
  fi
  rm -rf -- "$workdir"
}
trap cleanup EXIT

cleanup_client_shape() {
  # A SIGKILLed worker cannot run its own trap. Best-effort cleanup from the
  # control host prevents a failed qualification from leaving either host
  # shaped for unrelated traffic. The client-side script also removes the
  # optional server qdisc through the already-provisioned server key.
  ssh -T -q -o BatchMode=yes -o ConnectTimeout=10 "$client_target" \
    bash -s -- "$server_target" "$client_ssh_key" "$client_interface" "$server_interface" "$server_tc_sudo" <<'REMOTE_SHAPE_CLEANUP' >/dev/null 2>&1 || true
set +e
server=$1
key=$2
client_iface=$3
server_iface=$4
server_sudo=$5
tc qdisc del dev "$client_iface" root
if [ -n "$server_iface" ]; then
  if [ "$server_sudo" = 1 ]; then
    ssh -q -i "$key" -o BatchMode=yes -o ConnectTimeout=10 "$server" sudo -n tc qdisc del dev "$server_iface" root
  else
    ssh -q -i "$key" -o BatchMode=yes -o ConnectTimeout=10 "$server" tc qdisc del dev "$server_iface" root
  fi
fi
REMOTE_SHAPE_CLEANUP
}

# If the operator interrupts a trial while the worker is blocked in SSH, the
# worker may not get a chance to run its own trap. Remove any client/server
# qdiscs from the control host before returning, then let the EXIT trap remove
# staged files.
trap 'cleanup_client_shape; exit 130' INT TERM

collect_host_metadata() {
  # The version probe runs on the client so the server SSH key path and host
  # target stay in their intended trust domain. It produces JSON, not a shell
  # transcript, and is saved beside the final results as an audit/provenance
  # manifest.
  ssh -T -q \
    -o BatchMode=yes \
    -o ConnectTimeout=20 \
    -o ServerAliveInterval=5 \
    -o ServerAliveCountMax=3 \
    "$client_target" \
    bash -s -- "$client_asp" "$server_target" "$server_aspd" "$client_ssh_key" <<'REMOTE_METADATA'
set -euo pipefail
client_asp=$1
server_target=$2
server_aspd=$3
server_key=$4
first_line() { sed -n '1p'; }
client_asp_version=$("$client_asp" --version 2>&1 | first_line)
client_uname=$(uname -srmo 2>/dev/null || uname -s)
client_tc_version=$(tc -V 2>&1 | first_line)
client_ip_version=$(ip -V 2>&1 | first_line)
ssh_version=$(ssh -V 2>&1 | first_line)
server_aspd_version=$(ssh -q -i "$server_key" -o BatchMode=yes -o ConnectTimeout=15 "$server_target" "$server_aspd" --version 2>&1 | first_line)
server_uname=$(ssh -q -i "$server_key" -o BatchMode=yes -o ConnectTimeout=15 "$server_target" uname -srmo 2>/dev/null | first_line)
jq -cn \
  --arg client_asp_version "$client_asp_version" \
  --arg client_uname "$client_uname" \
  --arg client_tc_version "$client_tc_version" \
  --arg client_ip_version "$client_ip_version" \
  --arg ssh_version "$ssh_version" \
  --arg server_aspd_version "$server_aspd_version" \
  --arg server_uname "$server_uname" \
  '{client:{asp_version:$client_asp_version,uname:$client_uname,tc:$client_tc_version,iproute2:$client_ip_version,ssh:$ssh_version},server:{aspd_version:$server_aspd_version,uname:$server_uname}}'
REMOTE_METADATA
}

host_metadata=$(collect_host_metadata)
if ! jq -e '(.client.asp_version | type == "string" and length > 0) and (.server.aspd_version | type == "string" and length > 0)' <<<"$host_metadata" >/dev/null; then
  echo 'host metadata probe returned incomplete version information' >&2
  exit 1
fi

for trial in $(seq 1 "$trials"); do
  trial_file="$workdir/trial-$trial.jsonl"
  trial_stderr="$workdir/trial-$trial.stderr"
  remote_pcap=''
  if [[ -n "$pcap_dir" ]]; then
    remote_pcap="/tmp/asp-two-host-${run_id}-${trial}.pcap"
  fi
  # All arguments are validated above and contain no whitespace. Use the
  # client's own shell only to invoke bash -s; the worker validates again on
  # the client before touching tc, the server, or the workspaces.
  set +e
  ssh -T -q \
    -o BatchMode=yes \
    -o ConnectTimeout=20 \
    -o ServerAliveInterval=5 \
    -o ServerAliveCountMax=3 \
    "$client_target" \
    bash -s -- \
      "$server_target" "$server_root" "$endpoint" "$client_asp" "$client_cert" "$client_token" "$client_ssh_key" "$client_interface" "$server_interface" "$server_tc_sudo" "$server_metrics_port" "$trial" "$run_id" "$delay_ms" "$jitter_ms" "$loss_percent" "$rate_mbit" "$disconnect_seconds" "$log_mode" "$summary" "$summary_tail_bytes" "$remote_pcap" "$network_event_hook" "$network_event_kind" "$network_event_delay_seconds" \
    <"$worker" >"$trial_file" 2>"$trial_stderr"
  worker_status=$?
  set -e
  if [[ "$worker_status" -ne 0 ]]; then
    cleanup_client_shape
    echo "two-host agent trial $trial failed on client $client_target (status=$worker_status)" >&2
    sed -n '1,120p' "$trial_stderr" >&2 || true
    exit 1
  fi
  if ! jq -e -s --argjson trial "$trial" --arg network_event_kind "$network_event_kind" '
      length == 2
      and all(.[]; type == "object" and (.trial == $trial) and (.status == 0))
      and ([.[] | .system] | sort) == ["asp", "ssh-controlmaster"]
      and all(.[]; (.experiment == "agent-workload") and (.scenario | type == "string"))
      and all(.[]; .network_event_kind == $network_event_kind)
      and (if $network_event_kind == "none"
           then all(.[]; .network_event_completed == null and .network_event_duration_ms == null)
           else all(.[]; .network_event_completed == true and (.network_event_duration_ms | type == "number" and . >= 0))
           end)
    ' "$trial_file" >/dev/null; then
    cleanup_client_shape
    echo "two-host agent trial $trial did not produce exactly one valid ASP and SSH row" >&2
    sed -n '1,120p' "$trial_file" >&2 || true
    sed -n '1,120p' "$trial_stderr" >&2 || true
    exit 1
  fi
  if [[ -n "$pcap_dir" ]]; then
    command -v scp >/dev/null 2>&1 || die "scp is required when --pcap-dir is set"
    scp -q -o BatchMode=yes -o ConnectTimeout=20 \
      "$client_target:$remote_pcap" "$pcap_dir/trial-$trial.pcap"
    # The worker's cleanup removes its temporary path only after the pcap has
    # been stopped; remove the fetched copy on the client as well.
    ssh -T -q -o BatchMode=yes -o ConnectTimeout=20 "$client_target" rm -f -- "$remote_pcap"
    pcap_rewritten="$workdir/trial-$trial-with-pcap.jsonl"
    jq -c --arg pcap "$pcap_dir/trial-$trial.pcap" '.pcap_path = $pcap' "$trial_file" >"$pcap_rewritten"
    mv -f -- "$pcap_rewritten" "$trial_file"
  fi
  cat "$trial_file" >>"$staging"
  echo "completed two-host agent trial $trial/$trials" >&2
done

temporary=$(mktemp "$destination_parent/.asp-two-host-$destination_name.XXXXXX")
cp "$staging" "$temporary"
bash "$script_dir/qualify-results.sh" "$temporary" "$trials" agent-workload >/dev/null
metadata_temporary=$(mktemp "$destination_parent/.asp-two-host-$destination_name.meta.XXXXXX")
jq -cn \
  --arg generated_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg run_id "$run_id" \
  --arg client "$client_target" \
  --arg server "$server_target" \
  --arg endpoint "$endpoint" \
  --arg server_root "$server_root" \
  --arg server_aspd "$server_aspd" \
  --arg client_interface "$client_interface" \
  --arg server_interface "$server_interface" \
  --arg network_event_hook "$network_event_hook" \
  --arg network_event_kind "$network_event_kind" \
  --arg scenario "$scenario" \
  --arg log_mode "$log_mode" \
  --arg pcap_dir "$pcap_dir" \
  --argjson trials "$trials" \
  --argjson server_metrics_port "$server_metrics_port" \
  --argjson server_tc_sudo "$server_tc_sudo" \
  --argjson disconnect_seconds "$disconnect_seconds" \
  --argjson summary "$summary" \
  --argjson summary_tail_bytes "$summary_tail_bytes" \
  --argjson network_event_delay_seconds "$network_event_delay_seconds" \
  --argjson host_metadata "$host_metadata" \
  '{schema_version:1,experiment:"agent-workload",profile:"two-host",generated_at:$generated_at,run_id:$run_id,client:$client,server:$server,endpoint:$endpoint,server_root:$server_root,server_aspd:$server_aspd,client_interface:$client_interface,server_interface:(if $server_interface == "" then null else $server_interface end),server_tc_sudo:($server_tc_sudo == 1),server_metrics_port:$server_metrics_port,scenario:$scenario,log_mode:$log_mode,trials:$trials,disconnect_seconds:$disconnect_seconds,summary_output:($summary == 1),summary_tail_bytes:$summary_tail_bytes,pcap_dir:(if $pcap_dir == "" then null else $pcap_dir end),network_event_hook:(if $network_event_hook == "" then null else $network_event_hook end),network_event_kind:$network_event_kind,network_event_delay_seconds:$network_event_delay_seconds,hosts:$host_metadata}' >"$metadata_temporary"
mv -f -- "$metadata_temporary" "$destination_path.meta.json"
metadata_temporary=''
mv -f -- "$temporary" "$destination_path"
temporary=''
printf 'two-host ASP agent matrix written to %s (%s trials per system; scenario %s)\n' "$destination_path" "$trials" "$scenario"
printf 'run metadata written to %s.meta.json\n' "$destination_path"
