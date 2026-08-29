#!/usr/bin/env bash
set -euo pipefail

# Run the complete paired two-host agent workload matrix.  This is an
# orchestration layer around two-host-agent-matrix.sh: each cell is staged and
# qualified independently, then all cells are combined and qualified again
# before the final JSONL (and its provenance manifest) is published.  The
# script never provisions a daemon, credentials, or a supervisor.

export LANG=C.UTF-8
export LC_ALL=C.UTF-8
# Agent fixtures can contain command output and source paths. Keep local
# captures/private packet traces private even when the caller's umask is lax.
umask 077

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cell_runner="$script_dir/two-host-agent-matrix.sh"
qualifier="$script_dir/qualify-results.sh"

usage() {
  cat >&2 <<'USAGE'
usage: two-host-agent-grid.sh --output RESULTS.jsonl --client SSH_TARGET \
  --server SSH_TARGET --endpoint HOST:PORT --server-root ABS_PATH \
  --client-asp PATH --client-cert PATH --client-auth-token PATH \
  --client-ssh-key PATH --client-interface IFACE [options]

Run one paired two-host agent workload for every RTT/loss/jitter/rate cell.
Paths after --client-* are interpreted on the client host. The server must
already run a production-shaped aspd with a loopback metrics endpoint.

Options:
  --rtt-ms LIST              target RTTs, comma-separated (default: 0,20,100,200,300)
  --loss-percent LIST        packet-loss percentages (default: 0,1,5,10)
  --jitter-ms LIST           one-way jitter values (default: 0,20,100)
  --rate-mbit LIST           per-direction rates, 0 means unlimited (default: 1,10,100)
  --max-cells N              refuse grids larger than N cells (default: 1000)
  --trials N                 trials per cell (default: 30)
  --server-aspd PATH         aspd path on the server (default: /usr/local/bin/aspd)
  --server-interface IFACE   shape server egress too (recommended for RTT cells)
  --server-tc-sudo           run server tc through sudo -n
  --server-metrics-port N    server loopback /metrics port (default: 9443)
  --disconnect-seconds N     deliberate outage in each workload (default: 30)
  --log-mode MODE            compressible, incompressible, or mixed (default: compressible)
  --summary                  use EXEC_SUMMARY with a bounded tail for ASP
  --summary-tail-bytes N     summary tail (default: 8192)
  --pcap-dir DIR             fetch client UDP pcaps below this directory
  --network-event-hook PATH  client-host executable for migration/sleep hook
  --network-event-kind KIND  migration, sleep-wake, or custom (default: none)
  --network-event-delay N    seconds before invoking the event hook (default: 5)
  --checkpoint-dir DIR       durable per-cell capture directory (retained for resume)
  --resume                   resume qualified cells from --checkpoint-dir
  --run-id ID                stable safe run identifier (default: generated)
  --dry-run                  validate and print the planned cell manifest
USAGE
}

die() {
  echo "two-host-agent-grid.sh: $*" >&2
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
rtt_list=${ASP_TWO_HOST_RTT_MS:-0,20,100,200,300}
loss_list=${ASP_TWO_HOST_LOSS_PERCENT:-0,1,5,10}
jitter_list=${ASP_TWO_HOST_JITTER_MS:-0,20,100}
rate_list=${ASP_TWO_HOST_RATE_MBIT:-1,10,100}
max_cells=${ASP_TWO_HOST_MAX_CELLS:-1000}
trials=${ASP_TWO_HOST_TRIALS:-30}
disconnect_seconds=${ASP_TWO_HOST_DISCONNECT_SECONDS:-30}
log_mode=${ASP_TWO_HOST_LOG_MODE:-compressible}
summary=0
summary_tail_bytes=${ASP_TWO_HOST_SUMMARY_TAIL_BYTES:-8192}
pcap_dir=''
network_event_hook=''
network_event_kind=none
network_event_delay_seconds=5
checkpoint_dir=${ASP_TWO_HOST_CHECKPOINT_DIR:-}
resume=0
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
    --rtt-ms) require_value "$1" "${2-}"; rtt_list=$2; shift 2 ;;
    --loss-percent) require_value "$1" "${2-}"; loss_list=$2; shift 2 ;;
    --jitter-ms) require_value "$1" "${2-}"; jitter_list=$2; shift 2 ;;
    --rate-mbit) require_value "$1" "${2-}"; rate_list=$2; shift 2 ;;
    --max-cells) require_value "$1" "${2-}"; max_cells=$2; shift 2 ;;
    --trials) require_value "$1" "${2-}"; trials=$2; shift 2 ;;
    --disconnect-seconds) require_value "$1" "${2-}"; disconnect_seconds=$2; shift 2 ;;
    --log-mode) require_value "$1" "${2-}"; log_mode=$2; shift 2 ;;
    --summary) summary=1; shift ;;
    --summary-tail-bytes) require_value "$1" "${2-}"; summary_tail_bytes=$2; shift 2 ;;
    --pcap-dir) require_value "$1" "${2-}"; pcap_dir=$2; shift 2 ;;
    --network-event-hook) require_value "$1" "${2-}"; network_event_hook=$2; shift 2 ;;
    --network-event-kind) require_value "$1" "${2-}"; network_event_kind=$2; shift 2 ;;
    --network-event-delay) require_value "$1" "${2-}"; network_event_delay_seconds=$2; shift 2 ;;
    --checkpoint-dir) require_value "$1" "${2-}"; checkpoint_dir=$2; shift 2 ;;
    --resume) resume=1; shift ;;
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

# The values are later passed through two SSH/bash boundaries. Keep the grid
# wrapper at least as strict as the single-cell runner so a malformed list
# cannot become shell syntax on either host.
for name in output client_target server_target endpoint server_root server_aspd client_asp client_cert client_token client_ssh_key client_interface server_interface pcap_dir network_event_hook network_event_kind checkpoint_dir run_id rtt_list loss_list jitter_list rate_list; do
  value=${!name}
  if [[ "$value" == *[[:space:]]* || "$value" == *$'\n'* || "$value" == *$'\r'* || "$value" == *';'* || "$value" == *'|'* || "$value" == *'&'* || "$value" == *'`'* || "$value" == *'$('* || "$value" == *'>'* || "$value" == *'<'* ]]; then
    die "$name contains whitespace or an unsafe shell character"
  fi
done
[[ "$output" != / ]] || die "--output must not be the filesystem root"
[[ "$client_target" != -* && "$client_target" =~ ^[A-Za-z0-9._@:-]+$ ]] || die "--client must be a host/user target without SSH option syntax"
[[ "$server_target" != -* && "$server_target" =~ ^[A-Za-z0-9._@:-]+$ ]] || die "--server must be a host/user target without SSH option syntax"
[[ "$endpoint" =~ ^[A-Za-z0-9._:-]+:[0-9]{1,5}$ ]] || die "--endpoint must be HOST:PORT"
endpoint_port=${endpoint##*:}
((endpoint_port >= 1 && endpoint_port <= 65535)) || die "--endpoint port must be 1..65535"
[[ "$server_root" =~ ^/[A-Za-z0-9._/@+-]+$ && "$server_root" != / ]] || die "--server-root must be a non-root absolute safe path"
[[ "$client_interface" =~ ^[A-Za-z0-9_.:-]+$ ]] || die "--client-interface contains unsafe characters"
[[ -z "$server_interface" || "$server_interface" =~ ^[A-Za-z0-9_.:-]+$ ]] || die "--server-interface contains unsafe characters"
for name in client_asp client_cert client_token client_ssh_key; do
  [[ "${!name}" =~ ^[A-Za-z0-9._/@+-]+$ ]] || die "--${name//_/-} must be a shell-safe client path"
done
[[ "$server_aspd" =~ ^[A-Za-z0-9._/@+-]+$ ]] || die "--server-aspd must be a shell-safe path"
[[ -z "$pcap_dir" || "$pcap_dir" =~ ^/[A-Za-z0-9._/@+-]+$ ]] || die "--pcap-dir must be an absolute safe directory"
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
[[ -z "$checkpoint_dir" || "$checkpoint_dir" =~ ^/[A-Za-z0-9._/@+-]+$ ]] || die "--checkpoint-dir must be an absolute safe directory"
[[ -z "$checkpoint_dir" || "$checkpoint_dir" != / ]] || die "--checkpoint-dir must be a non-root absolute path"
[[ -z "$run_id" || "$run_id" =~ ^[A-Za-z0-9._-]+$ ]] || die "--run-id contains unsafe characters"
if ((resume == 1)) && [[ -z "$checkpoint_dir" ]]; then
  die "--resume requires --checkpoint-dir"
fi

for name in max_cells trials server_metrics_port disconnect_seconds summary_tail_bytes network_event_delay_seconds; do
  value=${!name}
  [[ "$value" =~ ^[0-9]+$ ]] || die "$name must be a non-negative integer"
done
((max_cells >= 1 && max_cells <= 10000)) || die "--max-cells must be between 1 and 10000"
((trials >= 1 && trials <= 1000)) || die "--trials must be between 1 and 1000"
((server_metrics_port >= 1 && server_metrics_port <= 65535)) || die "--server-metrics-port must be 1..65535"
((summary_tail_bytes >= 1 && summary_tail_bytes <= 1048576)) || die "--summary-tail-bytes must be 1..1048576"
((network_event_delay_seconds <= 600)) || die "--network-event-delay must be at most 600 seconds"
case "$log_mode" in
  compressible|incompressible|mixed) ;;
  *) die "--log-mode must be compressible, incompressible, or mixed" ;;
esac

validate_list_shape() {
  local name=$1
  local input=$2
  [[ -n "$input" && "$input" != *, && "$input" != ,* && "$input" != *,,* ]] || die "$name must be a comma-separated non-empty list"
}

validate_numeric_list() {
  local name=$1
  shift
  local item
  (($# > 0)) || die "$name must not be empty"
  for item in "$@"; do
    [[ "$item" =~ ^(0|[1-9][0-9]*)$ ]] || die "$name entries must be canonical non-negative integers: $item"
    awk -v value="$item" 'BEGIN { exit !(value <= 2147483647) }' || die "$name entry is too large: $item"
  done
}

validate_percentage_list() {
  local name=$1
  shift
  local item
  (($# > 0)) || die "$name must not be empty"
  for item in "$@"; do
    [[ "$item" =~ ^(0|[1-9][0-9]*)([.][0-9]+)?$ ]] || die "$name entries must be canonical percentages: $item"
    awk -v value="$item" 'BEGIN { exit !(value >= 0 && value <= 100) }' || die "$name entry is outside 0..100: $item"
  done
}

validate_unique_list() {
  local name=$1
  shift
  local candidate prior
  local seen=()
  for candidate in "$@"; do
    if ((${#seen[@]} > 0)); then
      for prior in "${seen[@]}"; do
        [[ "$candidate" != "$prior" ]] || die "$name entries must be unique: $candidate"
      done
    fi
    seen+=("$candidate")
  done
}

validate_list_shape --rtt-ms "$rtt_list"
validate_list_shape --loss-percent "$loss_list"
validate_list_shape --jitter-ms "$jitter_list"
validate_list_shape --rate-mbit "$rate_list"
IFS=',' read -r -a rtt_values <<<"$rtt_list"
IFS=',' read -r -a loss_values <<<"$loss_list"
IFS=',' read -r -a jitter_values <<<"$jitter_list"
IFS=',' read -r -a rate_values <<<"$rate_list"
validate_numeric_list --rtt-ms "${rtt_values[@]}"
validate_percentage_list --loss-percent "${loss_values[@]}"
validate_numeric_list --jitter-ms "${jitter_values[@]}"
validate_numeric_list --rate-mbit "${rate_values[@]}"
validate_unique_list --rtt-ms "${rtt_values[@]}"
validate_unique_list --loss-percent "${loss_values[@]}"
validate_unique_list --jitter-ms "${jitter_values[@]}"
validate_unique_list --rate-mbit "${rate_values[@]}"

cell_count=$(( ${#rtt_values[@]} * ${#loss_values[@]} * ${#jitter_values[@]} * ${#rate_values[@]} ))
((cell_count <= max_cells)) || die "grid has $cell_count cells, exceeding --max-cells $max_cells"

run_id_supplied=0
if [[ -n "$run_id" ]]; then
  run_id_supplied=1
else
  run_id="$(date -u +%Y%m%dT%H%M%SZ)-$$"
fi

if [[ -n "$pcap_dir" ]]; then
  if [[ -e "$pcap_dir" || -L "$pcap_dir" ]]; then
    [[ -d "$pcap_dir" && ! -L "$pcap_dir" ]] || die "--pcap-dir must be a regular directory, not a symlink"
    chmod go-rwx "$pcap_dir" || die "cannot make --pcap-dir private: $pcap_dir"
  else
    mkdir -p -m 700 -- "$pcap_dir"
  fi
  pcap_dir=$(cd -- "$pcap_dir" && pwd)
fi

if ((dry_run == 1)); then
  command -v jq >/dev/null 2>&1 || die "jq is required on the control host"
  jq -cn \
    --arg output "$output" \
    --arg client "$client_target" \
    --arg server "$server_target" \
    --arg endpoint "$endpoint" \
    --arg server_root "$server_root" \
    --arg run_id "$run_id" \
    --arg checkpoint_dir "$checkpoint_dir" \
    --arg network_event_hook "$network_event_hook" \
    --arg network_event_kind "$network_event_kind" \
    --arg log_mode "$log_mode" \
    --argjson trials "$trials" \
    --argjson cells "$cell_count" \
    --argjson server_metrics_port "$server_metrics_port" \
    --argjson server_tc_sudo "$server_tc_sudo" \
    --argjson disconnect_seconds "$disconnect_seconds" \
    --argjson summary_tail_bytes "$summary_tail_bytes" \
    --argjson summary "$summary" \
    --argjson resume "$resume" \
    --argjson network_event_delay_seconds "$network_event_delay_seconds" \
    --argjson symmetric "$([[ -n "$server_interface" ]] && echo true || echo false)" \
    --argjson rtts "$(printf '%s\n' "${rtt_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
    --argjson losses "$(printf '%s\n' "${loss_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
    --argjson jitters "$(printf '%s\n' "${jitter_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
    --argjson rates "$(printf '%s\n' "${rate_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
    '{experiment:"agent-workload",profile:"two-host-grid",output:$output,client:$client,server:$server,endpoint:$endpoint,server_root:$server_root,run_id:$run_id,checkpoint_dir:(if $checkpoint_dir == "" then null else $checkpoint_dir end),resume:($resume == 1),trials:$trials,cells:$cells,server_metrics_port:$server_metrics_port,server_tc_sudo:($server_tc_sudo == 1),disconnect_seconds:$disconnect_seconds,summary_output:($summary == 1),summary_tail_bytes:$summary_tail_bytes,network_event_hook:(if $network_event_hook == "" then null else $network_event_hook end),network_event_kind:$network_event_kind,network_event_delay_seconds:$network_event_delay_seconds,symmetric_shaping:$symmetric,rtt_ms:$rtts,loss_percent:$losses,jitter_ms:$jitters,rate_mbit:$rates,log_mode:$log_mode,requires:["independent client and server hosts","one qualified result per cell","Linux tc/iproute2 on the client","aspd loopback /metrics","normal SSH host-key verification"]}'
  exit 0
fi

command -v jq >/dev/null 2>&1 || die "jq is required on the control host"
[[ -x "$cell_runner" ]] || die "cell runner is missing or not executable: $cell_runner"
[[ -x "$qualifier" ]] || die "result qualifier is missing or not executable: $qualifier"

destination_parent=$(dirname -- "$output")
mkdir -p -- "$destination_parent"
destination_parent=$(cd -- "$destination_parent" && pwd)
destination_name=$(basename -- "$output")
destination_path="$destination_parent/$destination_name"
for destination_candidate in "$destination_path" "$destination_path.meta.json"; do
  if [[ -L "$destination_candidate" || ( -e "$destination_candidate" && ! -f "$destination_candidate" ) ]]; then
    die "output path must be a regular file or absent, not a symlink/special file: $destination_candidate"
  fi
done
workdir=$(mktemp -d "$destination_parent/.asp-two-host-grid.XXXXXX")
staging="$workdir/results.jsonl"
metadata_staging="$workdir/cells.meta.jsonl"
: >"$staging"
: >"$metadata_staging"

checkpoint_explicit=0
qualification_temporary=''
cleanup() {
  # A signal can arrive while a cell is being copied into the durable
  # checkpoint. Remove only this process's temporary names; completed cells
  # and the manifest are intentionally retained for --resume.
  if ((checkpoint_explicit == 1)); then
    rm -f -- \
      "$checkpoint_dir"/.cell-*.tmp."$$" \
      "$checkpoint_dir"/.cell-*.complete."$$"
  fi
  if [[ -n "$qualification_temporary" ]]; then
    rm -f -- "$qualification_temporary"
  fi
  rm -rf -- "$workdir"
}
trap cleanup EXIT INT TERM

# A long WAN grid can run for many hours.  With an explicit checkpoint
# directory, each qualified cell is published independently and a later
# invocation can resume without repeating completed trials.  The default
# remains an ephemeral directory so a normal run does not leave benchmark
# state behind on interruption.
if [[ -n "$checkpoint_dir" ]]; then
  checkpoint_explicit=1
  if [[ -e "$checkpoint_dir" || -L "$checkpoint_dir" ]]; then
    [[ -d "$checkpoint_dir" && ! -L "$checkpoint_dir" ]] || die "--checkpoint-dir must be a regular directory, not a symlink"
    chmod go-rwx "$checkpoint_dir" || die "cannot make --checkpoint-dir private: $checkpoint_dir"
  else
    mkdir -p -m 700 -- "$checkpoint_dir"
  fi
  checkpoint_dir=$(cd -- "$checkpoint_dir" && pwd)
else
  checkpoint_dir="$workdir/checkpoint"
  mkdir -p -- "$checkpoint_dir"
fi
checkpoint_manifest="$checkpoint_dir/manifest.json"
checkpoint_hosts="$checkpoint_dir/hosts.json"
if ((resume == 0)); then
  if [[ -e "$checkpoint_manifest" || -L "$checkpoint_manifest" ]]; then
    die "checkpoint already exists at $checkpoint_dir; use --resume or choose a new directory"
  fi
  if ((checkpoint_explicit == 1)); then
    checkpoint_entries=$(find "$checkpoint_dir" -mindepth 1 -maxdepth 1 -print -quit)
    [[ -z "$checkpoint_entries" ]] || die "--checkpoint-dir must be empty for a new run: $checkpoint_dir"
  fi
else
  [[ -f "$checkpoint_manifest" && ! -L "$checkpoint_manifest" ]] || die "checkpoint manifest is missing or unsafe: $checkpoint_manifest"
  if ((run_id_supplied == 0)); then
    run_id=$(jq -r '.run_id // empty' "$checkpoint_manifest") || die "checkpoint manifest is malformed: $checkpoint_manifest"
    [[ -n "$run_id" && "$run_id" =~ ^[A-Za-z0-9._-]+$ ]] || die "checkpoint manifest has an unsafe run_id: $checkpoint_manifest"
  fi
fi

# The manifest is a canonical configuration identity.  It prevents a resume
# from silently combining cells captured with different hosts, shaping, or
# trial counts.  The output path is included deliberately: operators should
# choose a new checkpoint for a new publication target rather than mixing
# evidence into an existing run.
checkpoint_config="$workdir/checkpoint-config.json"
jq -n \
  --arg output "$destination_path" \
  --arg client "$client_target" \
  --arg server "$server_target" \
  --arg endpoint "$endpoint" \
  --arg server_root "$server_root" \
  --arg server_aspd "$server_aspd" \
  --arg client_asp "$client_asp" \
  --arg client_cert "$client_cert" \
  --arg client_token "$client_token" \
  --arg client_ssh_key "$client_ssh_key" \
  --arg client_interface "$client_interface" \
  --arg server_interface "$server_interface" \
  --arg pcap_dir "$pcap_dir" \
  --arg network_event_hook "$network_event_hook" \
  --arg network_event_kind "$network_event_kind" \
  --arg run_id "$run_id" \
  --arg log_mode "$log_mode" \
  --argjson server_tc_sudo "$server_tc_sudo" \
  --argjson server_metrics_port "$server_metrics_port" \
  --argjson trials "$trials" \
  --argjson disconnect_seconds "$disconnect_seconds" \
  --argjson summary "$summary" \
  --argjson summary_tail_bytes "$summary_tail_bytes" \
  --argjson network_event_delay_seconds "$network_event_delay_seconds" \
  --argjson rtts "$(printf '%s\n' "${rtt_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
  --argjson losses "$(printf '%s\n' "${loss_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
  --argjson jitters "$(printf '%s\n' "${jitter_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
  --argjson rates "$(printf '%s\n' "${rate_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
  '{schema_version:1,experiment:"agent-workload",profile:"two-host-grid",output:$output,client:$client,server:$server,endpoint:$endpoint,server_root:$server_root,server_aspd:$server_aspd,client_asp:$client_asp,client_cert:$client_cert,client_token:$client_token,client_ssh_key:$client_ssh_key,client_interface:$client_interface,server_interface:$server_interface,pcap_dir:$pcap_dir,network_event_hook:(if $network_event_hook == "" then null else $network_event_hook end),network_event_kind:$network_event_kind,network_event_delay_seconds:$network_event_delay_seconds,run_id:$run_id,log_mode:$log_mode,server_tc_sudo:($server_tc_sudo == 1),server_metrics_port:$server_metrics_port,trials:$trials,disconnect_seconds:$disconnect_seconds,summary:($summary == 1),summary_tail_bytes:$summary_tail_bytes,rtt_ms:$rtts,loss_percent:$losses,jitter_ms:$jitters,rate_mbit:$rates}' \
  >"$checkpoint_config"
if ((resume == 1)); then
  if ! cmp -s "$checkpoint_config" "$checkpoint_manifest"; then
    die "checkpoint configuration does not match this invocation: $checkpoint_dir"
  fi
else
  mv -f -- "$checkpoint_config" "$checkpoint_manifest"
fi

summary_args=()
if ((summary == 1)); then
  summary_args=(--summary --summary-tail-bytes "$summary_tail_bytes")
fi

record_or_validate_hosts() {
  local metadata_path=$1
  local observed expected temporary_hosts
  [[ -f "$metadata_path" && ! -L "$metadata_path" ]] || die "cell metadata is missing or unsafe: $metadata_path"
  jq -e '(.hosts | type == "object")' "$metadata_path" >/dev/null 2>&1 || die "cell metadata has no host provenance: $metadata_path"
  observed=$(jq -c -e '.hosts' "$metadata_path") || die "cell metadata has no host provenance: $metadata_path"
  if [[ -f "$checkpoint_hosts" && ! -L "$checkpoint_hosts" ]]; then
    expected=$(jq -c -e '.' "$checkpoint_hosts") || die "checkpoint host provenance is malformed: $checkpoint_hosts"
    [[ "$observed" == "$expected" ]] || die "host versions changed while resuming checkpoint $checkpoint_dir"
  elif [[ -e "$checkpoint_hosts" || -L "$checkpoint_hosts" ]]; then
    die "checkpoint host provenance is unsafe: $checkpoint_hosts"
  else
    temporary_hosts="$workdir/hosts.json"
    printf '%s\n' "$observed" >"$temporary_hosts"
    mv -f -- "$temporary_hosts" "$checkpoint_hosts"
  fi
}

publish_checkpoint_file() {
  local source=$1
  local destination=$2
  local temporary_path="$checkpoint_dir/.$(basename -- "$destination").tmp.$$"
  cp -- "$source" "$temporary_path"
  mv -f -- "$temporary_path" "$destination"
}

sha256_file() {
  local file=$1
  local digest
  if command -v sha256sum >/dev/null 2>&1; then
    digest=$(sha256sum -- "$file" | awk 'NR == 1 { print $1 }')
  elif command -v shasum >/dev/null 2>&1; then
    digest=$(shasum -a 256 -- "$file" | awk 'NR == 1 { print $1 }')
  else
    die "a SHA-256 utility (sha256sum or shasum) is required for checkpoint integrity"
  fi
  [[ "$digest" =~ ^[[:xdigit:]]{64}$ ]] || die "failed to compute SHA-256 for checkpoint file: $file"
  printf '%s\n' "$digest"
}

validate_cell_pcaps() {
  local results_path=$1
  local pcap_path pcap_paths
  [[ -n "$pcap_dir" ]] || return 0
  # A requested pcap is evidence, not an optional annotation.  Treat a
  # missing/null path as a failed capture instead of allowing a checkpoint to
  # resume without the packet trace it was configured to collect.
  pcap_paths=$(jq -r '.[].pcap_path // "__asp_missing_pcap__"' "$results_path") || return 1
  while IFS= read -r pcap_path; do
    [[ -n "$pcap_path" && "$pcap_path" != "__asp_missing_pcap__" ]] || return 1
    [[ "$pcap_path" == "$pcap_dir"/* && -f "$pcap_path" && ! -L "$pcap_path" ]] || return 1
  done <<<"$pcap_paths"
}

validate_cell_network_event() {
  local results_path=$1
  jq -e -s \
    --arg expected_kind "$network_event_kind" \
    'length > 0
     and all(.[]; .network_event_kind == $expected_kind)
     and (if $expected_kind == "none"
          then all(.[]; .network_event_completed == null and .network_event_duration_ms == null)
          else all(.[]; .network_event_completed == true and (.network_event_duration_ms | type == "number" and . >= 0))
          end)' \
    "$results_path" >/dev/null
}

validate_cell_network_event_metadata() {
  local metadata_path=$1
  jq -e \
    --arg expected_hook "$network_event_hook" \
    --arg expected_kind "$network_event_kind" \
    --argjson expected_delay "$network_event_delay_seconds" \
    '.network_event_kind == $expected_kind
     and .network_event_delay_seconds == $expected_delay
     and .network_event_hook == (if $expected_hook == "" then null else $expected_hook end)' \
    "$metadata_path" >/dev/null
}

validate_cell_capture() {
  local results_path=$1
  local metadata_path=$2
  local expected_scenario=$3
  local expected_run_id=$4
  local expected_pcap_dir=$5

  [[ -f "$results_path" && ! -L "$results_path" ]] || return 1
  [[ -f "$metadata_path" && ! -L "$metadata_path" ]] || return 1
  bash "$qualifier" "$results_path" "$trials" agent-workload >/dev/null 2>&1 || return 1
  jq -e -s \
    --arg expected_scenario "$expected_scenario" \
    --arg expected_log_mode "$log_mode" \
    --argjson expected_summary "$summary" \
    --argjson expected_tail "$summary_tail_bytes" \
    --argjson expected_disconnect "$disconnect_seconds" \
    --argjson expected_trials "$trials" \
    'length == (2 * $expected_trials)
     and ([.[] | .system] | sort) == ["asp", "ssh-controlmaster"]
     and all(.[];
       .experiment == "agent-workload"
       and .status == 0
       and .scenario == $expected_scenario
       and .log_mode == $expected_log_mode
       and .disconnect_seconds == $expected_disconnect
       and ((.system != "asp") or (.summary_output == ($expected_summary == 1)
         and .summary_tail_bytes == $expected_tail))
     )' \
    "$results_path" >/dev/null || return 1
  validate_cell_pcaps "$results_path" || return 1
  validate_cell_network_event "$results_path" || return 1
  validate_cell_network_event_metadata "$metadata_path" || return 1
  jq -e \
    --arg expected_scenario "$expected_scenario" \
    --arg expected_run_id "$expected_run_id" \
    --arg expected_log_mode "$log_mode" \
    --arg expected_pcap_dir "$expected_pcap_dir" \
    --argjson expected_summary "$summary" \
    --argjson expected_tail "$summary_tail_bytes" \
    --argjson expected_disconnect "$disconnect_seconds" \
    --argjson expected_trials "$trials" \
    --arg expected_client "$client_target" \
    --arg expected_server "$server_target" \
    --arg expected_endpoint "$endpoint" \
    --arg expected_server_root "$server_root" \
    --arg expected_server_aspd "$server_aspd" \
    --arg expected_client_interface "$client_interface" \
    --arg expected_server_interface "$server_interface" \
    --argjson expected_server_tc_sudo "$server_tc_sudo" \
    --argjson expected_metrics_port "$server_metrics_port" \
    --arg expected_network_event_kind "$network_event_kind" \
    --arg expected_network_event_hook "$network_event_hook" \
    --argjson expected_network_event_delay "$network_event_delay_seconds" \
    '.schema_version == 1
     and .experiment == "agent-workload"
     and .profile == "two-host"
     and .run_id == $expected_run_id
     and .client == $expected_client
     and .server == $expected_server
     and .endpoint == $expected_endpoint
     and .server_root == $expected_server_root
     and .server_aspd == $expected_server_aspd
     and .client_interface == $expected_client_interface
     and .server_interface == (if $expected_server_interface == "" then null else $expected_server_interface end)
     and .server_tc_sudo == ($expected_server_tc_sudo == 1)
     and .server_metrics_port == $expected_metrics_port
     and .scenario == $expected_scenario
     and .log_mode == $expected_log_mode
     and .trials == $expected_trials
     and .disconnect_seconds == $expected_disconnect
     and .summary_output == ($expected_summary == 1)
     and .summary_tail_bytes == $expected_tail
     and .pcap_dir == (if $expected_pcap_dir == "" then null else $expected_pcap_dir end)
     and .network_event_kind == $expected_network_event_kind
     and .network_event_delay_seconds == $expected_network_event_delay
     and .network_event_hook == (if $expected_network_event_hook == "" then null else $expected_network_event_hook end)
     and (.hosts | type == "object")' \
    "$metadata_path" >/dev/null || return 1
}

cell_index=0
resumed_cells=0
for rtt in "${rtt_values[@]}"; do
  # With symmetric shaping, each egress receives half the desired RTT. For an
  # odd target, round up and retain both values in provenance rather than
  # silently claiming exact timing.
  delay_ms=$(( (rtt + 1) / 2 ))
  for loss in "${loss_values[@]}"; do
    for jitter in "${jitter_values[@]}"; do
      for rate in "${rate_values[@]}"; do
        cell_index=$((cell_index + 1))
        cell_output="$checkpoint_dir/cell-$cell_index.jsonl"
        cell_metadata="$cell_output.meta.json"
        cell_complete="$checkpoint_dir/cell-$cell_index.complete"
        cell_work_output="$workdir/cell-$cell_index.jsonl"
        expected_scenario="delay=${delay_ms}ms,jitter=${jitter}ms,loss=${loss}%,rate=${rate}mbit"
        cell_pcap=''
        if [[ -n "$pcap_dir" ]]; then
          cell_pcap="$pcap_dir/cell-$cell_index"
          mkdir -p -- "$cell_pcap"
        fi
        cell_run_id="$run_id-cell-$cell_index"
        cell_ready=0
        if ((resume == 1)) && [[ -f "$cell_complete" && ! -L "$cell_complete" && -f "$cell_output" && ! -L "$cell_output" && -f "$cell_metadata" && ! -L "$cell_metadata" ]]; then
          checkpoint_result_sha256=$(sha256_file "$cell_output")
          checkpoint_metadata_sha256=$(sha256_file "$cell_metadata")
          if jq -e \
              --arg expected_result_sha256 "$checkpoint_result_sha256" \
              --arg expected_metadata_sha256 "$checkpoint_metadata_sha256" \
              --argjson expected_cell "$cell_index" \
              '.schema_version == 1
               and .status == "qualified"
               and .cell == $expected_cell
               and .result_sha256 == $expected_result_sha256
               and .metadata_sha256 == $expected_metadata_sha256' \
              "$cell_complete" >/dev/null 2>&1 \
            && validate_cell_capture "$cell_output" "$cell_metadata" "$expected_scenario" "$cell_run_id" "$cell_pcap"; then
            record_or_validate_hosts "$cell_metadata"
            cell_ready=1
            resumed_cells=$((resumed_cells + 1))
            echo "resuming qualified two-host agent cell $cell_index/$cell_count (rtt=${rtt}ms, loss=${loss}%, jitter=${jitter}ms, rate=${rate}mbit)" >&2
          else
            echo "discarding invalid checkpoint for cell $cell_index; rerunning" >&2
            rm -f -- "$cell_complete" "$cell_output" "$cell_metadata"
          fi
        fi
        cell_args=(
          --output "$cell_work_output"
          --client "$client_target"
          --server "$server_target"
          --endpoint "$endpoint"
          --server-root "$server_root"
          --server-aspd "$server_aspd"
          --client-asp "$client_asp"
          --client-cert "$client_cert"
          --client-auth-token "$client_token"
          --client-ssh-key "$client_ssh_key"
          --client-interface "$client_interface"
          --server-metrics-port "$server_metrics_port"
          --trials "$trials"
          --delay-ms "$delay_ms"
          --jitter-ms "$jitter"
          --loss-percent "$loss"
          --rate-mbit "$rate"
          --disconnect-seconds "$disconnect_seconds"
          --log-mode "$log_mode"
          --run-id "$cell_run_id"
        )
        if [[ -n "$server_interface" ]]; then
          cell_args+=(--server-interface "$server_interface")
        fi
        if ((server_tc_sudo == 1)); then
          cell_args+=(--server-tc-sudo)
        fi
        if [[ -n "$cell_pcap" ]]; then
          cell_args+=(--pcap-dir "$cell_pcap")
        fi
        cell_args+=(--network-event-kind "$network_event_kind" --network-event-delay "$network_event_delay_seconds")
        if [[ -n "$network_event_hook" ]]; then
          cell_args+=(--network-event-hook "$network_event_hook")
        fi
        if ((summary == 1)); then
          cell_args+=("${summary_args[@]}")
        fi

        if ((cell_ready == 0)); then
          echo "starting two-host agent cell $cell_index/$cell_count (rtt=${rtt}ms, loss=${loss}%, jitter=${jitter}ms, rate=${rate}mbit)" >&2
          bash "$cell_runner" "${cell_args[@]}"
          validate_cell_capture "$cell_work_output" "$cell_work_output.meta.json" "$expected_scenario" "$cell_run_id" "$cell_pcap" \
            || die "cell $cell_index output or provenance does not match the requested contract"
          record_or_validate_hosts "$cell_work_output.meta.json"
          publish_checkpoint_file "$cell_work_output" "$cell_output"
          publish_checkpoint_file "$cell_work_output.meta.json" "$cell_metadata"
          cell_marker_tmp="$checkpoint_dir/.cell-$cell_index.complete.$$"
          result_sha256=$(sha256_file "$cell_output")
          metadata_sha256=$(sha256_file "$cell_metadata")
          jq -cn \
            --argjson cell "$cell_index" \
            --arg result_sha256 "$result_sha256" \
            --arg metadata_sha256 "$metadata_sha256" \
            '{schema_version:1,status:"qualified",cell:$cell,result_sha256:$result_sha256,metadata_sha256:$metadata_sha256}' \
            >"$cell_marker_tmp"
          mv -f -- "$cell_marker_tmp" "$cell_complete"
        fi
        # The single-cell runner has already qualified its pair. Add explicit
        # grid coordinates so a later report can distinguish a rounded odd RTT
        # or a deliberately one-sided shape from another scenario string.
        jq -c \
          --argjson rtt "$rtt" \
          --argjson delay "$delay_ms" \
          --arg loss "$loss" \
          --argjson jitter "$jitter" \
          --argjson rate "$rate" \
          --argjson symmetric "$([[ -n "$server_interface" ]] && echo true || echo false)" \
          --argjson cell "$cell_index" \
          '. + {grid_cell:$cell,nominal_rtt_ms:$rtt,configured_one_way_delay_ms:$delay,grid_loss_percent:($loss|tonumber),grid_jitter_ms:$jitter,grid_rate_mbit:$rate,symmetric_shaping:$symmetric}' \
          "$cell_output" >>"$staging"
        jq -c \
          --argjson cell "$cell_index" \
          --argjson rtt "$rtt" \
          --argjson delay "$delay_ms" \
          --arg loss "$loss" \
          --argjson jitter "$jitter" \
          --argjson rate "$rate" \
          --argjson symmetric "$([[ -n "$server_interface" ]] && echo true || echo false)" \
          --slurpfile cell_meta "$cell_metadata" \
          '{cell:$cell,nominal_rtt_ms:$rtt,configured_one_way_delay_ms:$delay,loss_percent:($loss|tonumber),jitter_ms:$jitter,rate_mbit:$rate,symmetric_shaping:$symmetric,metadata:$cell_meta[0]}' \
          <<<"{}" >>"$metadata_staging"
      done
    done
  done
done

# The second qualification pass catches accidental cross-cell omissions or
# duplicate trial IDs even if every individual cell was valid on its own.
temporary=$(mktemp "$destination_parent/.asp-two-host-grid-$destination_name.XXXXXX")
cp "$staging" "$temporary"
# The strict qualifier groups by scenario.  A caller may intentionally use
# nominal RTT values that round to the same configured one-way delay (for
# example 1ms and 2ms with symmetric shaping); keep the published scenario
# untouched, but qualify a stream-local copy with the grid cell in its key so
# those independent cells do not look like duplicate trial IDs.
qualification_temporary=$(mktemp "$destination_parent/.asp-two-host-grid-$destination_name.qualify.XXXXXX")
jq -c 'if (.grid_cell | type) == "number" then .scenario = ((.scenario | tostring) + ",grid_cell=" + (.grid_cell | tostring)) else . end' "$staging" >"$qualification_temporary"
bash "$qualifier" "$qualification_temporary" "$trials" agent-workload >/dev/null

metadata_temporary=$(mktemp "$destination_parent/.asp-two-host-grid-$destination_name.meta.XXXXXX")
jq -n \
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
  --argjson server_tc_sudo "$server_tc_sudo" \
  --argjson server_metrics_port "$server_metrics_port" \
  --arg checkpoint_dir_metadata "$([[ "$checkpoint_explicit" == 1 ]] && printf '%s' "$checkpoint_dir" || true)" \
  --arg log_mode "$log_mode" \
  --arg pcap_dir "$pcap_dir" \
  --argjson trials "$trials" \
  --argjson cells "$cell_count" \
  --argjson disconnect_seconds "$disconnect_seconds" \
  --argjson summary "$summary" \
  --argjson summary_tail_bytes "$summary_tail_bytes" \
  --argjson network_event_delay_seconds "$network_event_delay_seconds" \
  --argjson resumed_cells "$resumed_cells" \
  --argjson rtts "$(printf '%s\n' "${rtt_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
  --argjson losses "$(printf '%s\n' "${loss_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
  --argjson jitters "$(printf '%s\n' "${jitter_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
  --argjson rates "$(printf '%s\n' "${rate_values[@]}" | jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)')" \
  --slurpfile cell_metadata "$metadata_staging" \
  '{schema_version:1,experiment:"agent-workload",profile:"two-host-grid",generated_at:$generated_at,run_id:$run_id,client:$client,server:$server,endpoint:$endpoint,server_root:$server_root,server_aspd:$server_aspd,client_interface:$client_interface,server_interface:(if $server_interface == "" then null else $server_interface end),network_event_hook:(if $network_event_hook == "" then null else $network_event_hook end),network_event_kind:$network_event_kind,network_event_delay_seconds:$network_event_delay_seconds,server_tc_sudo:($server_tc_sudo == 1),server_metrics_port:$server_metrics_port,symmetric_shaping:($server_interface != ""),cells:$cells,trials:$trials,resumed_cells:$resumed_cells,disconnect_seconds:$disconnect_seconds,log_mode:$log_mode,summary_output:($summary == 1),summary_tail_bytes:$summary_tail_bytes,rtt_ms:$rtts,loss_percent:$losses,jitter_ms:$jitters,rate_mbit:$rates,pcap_dir:(if $pcap_dir == "" then null else $pcap_dir end),checkpoint_dir:(if $checkpoint_dir_metadata == "" then null else $checkpoint_dir_metadata end),cell_metadata:$cell_metadata}' >"$metadata_temporary"
mv -f -- "$metadata_temporary" "$destination_path.meta.json"
mv -f -- "$temporary" "$destination_path"
printf 'two-host ASP agent grid written to %s (%s cells, %s trials per system; %s resumed)\n' "$destination_path" "$cell_count" "$trials" "$resumed_cells"
printf 'run metadata written to %s.meta.json\n' "$destination_path"
if ((checkpoint_explicit == 1)); then
  printf 'cell checkpoint retained at %s\n' "$checkpoint_dir"
fi
