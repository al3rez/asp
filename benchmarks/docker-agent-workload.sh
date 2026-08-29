#!/usr/bin/env bash
set -euo pipefail
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

destination=${1:-/work/agent-workload.jsonl}
results=/tmp/asp-agent-workload.jsonl
delay_ms=${ASP_AGENT_DELAY_MS:-50}
jitter_ms=${ASP_AGENT_JITTER_MS:-0}
loss_percent=${ASP_AGENT_LOSS_PERCENT:-0}
rate_mbit=${ASP_AGENT_RATE_MBIT:-100}
disconnect_seconds=${ASP_AGENT_DISCONNECT_SECONDS:-30}
trial=${ASP_AGENT_TRIAL:-1}
log_mode=${ASP_AGENT_LOG_MODE:-compressible}
if ! [[ "$trial" =~ ^[1-9][0-9]*$ ]]; then
  echo "ASP_AGENT_TRIAL must be a positive integer" >&2
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
case "$log_mode" in
  compressible) log_command='head -c 10485760 /dev/zero' ;;
  incompressible) log_command='head -c 10485760 /dev/urandom' ;;
  mixed) log_command='head -c 5242880 /dev/zero; head -c 5242880 /dev/urandom' ;;
  *)
    echo "ASP_AGENT_LOG_MODE must be compressible, incompressible, or mixed" >&2
    exit 2
    ;;
esac
summary_args=()
if [[ "${ASP_AGENT_SUMMARY:-0}" == 1 ]]; then
  summary_tail_bytes=${ASP_AGENT_SUMMARY_TAIL_BYTES:-8192}
  if ! [[ "$summary_tail_bytes" =~ ^[1-9][0-9]*$ ]] || ((summary_tail_bytes > 1048576)); then
    echo "ASP_AGENT_SUMMARY_TAIL_BYTES must be an integer from 1 to 1048576" >&2
    exit 2
  fi
  summary_args=(--summary-output --tail-bytes "$summary_tail_bytes")
fi
# Linux `/proc` exposes cumulative CPU counters and resident set size for the
# daemon. Keep these alongside logical/interface byte counts so a semantic
# speedup is not reported without its server-side cost. Missing procfs is
# represented as zero and documented as unavailable rather than measured.
cpu_ticks_per_second=$(getconf CLK_TCK 2>/dev/null || printf '100\n')
page_size_bytes=$(getconf PAGESIZE 2>/dev/null || printf '4096\n')
aspd_proc_stats() {
  # `aspd` has no spaces in its comm field, so these are stable `/proc/stat`
  # offsets: utime=14, stime=15, resident pages=24.
  if [[ -n "${aspd_pid:-}" && -r "/proc/$aspd_pid/stat" ]]; then
    awk '{print $14, $15, $24}' "/proc/$aspd_pid/stat"
  else
    printf '0 0 0\n'
  fi
}
ticks_to_ms() {
  awk -v before="${1:-0}" -v after="${2:-0}" -v hz="$cpu_ticks_per_second" \
    'BEGIN { delta=after-before; if (delta < 0) delta=0; printf "%.6f", (delta / hz) * 1000 }'
}
pages_to_kb() {
  awk -v pages="${1:-0}" -v size="$page_size_bytes" \
    'BEGIN { if (pages < 0) pages=0; printf "%d", (pages * size) / 1024 }'
}
mkdir -p /run/sshd /root/.ssh /work/.asp

if [[ ! -f /tmp/asp-bench-id ]]; then
  ssh-keygen -q -t ed25519 -N '' -f /tmp/asp-bench-id
fi
cp /tmp/asp-bench-id.pub /root/.ssh/authorized_keys
chmod 700 /root/.ssh
chmod 600 /root/.ssh/authorized_keys

/usr/sbin/sshd -D -e -p 2222 >/tmp/sshd.log 2>&1 &
sshd_pid=$!
/work/target/release/aspd \
  --listen 127.0.0.1:4433 \
  --root /work \
  --cert /work/.asp/server-cert.der \
  --key /work/.asp/server-key.der \
  --auth-token-file /work/.asp/auth-token >/tmp/aspd.log 2>&1 &
aspd_pid=$!

cleanup() {
  tc qdisc del dev lo root 2>/dev/null || true
  if [[ -S /tmp/asp-control ]]; then
    timeout 2 ssh -q -p 2222 -S /tmp/asp-control -O exit 127.0.0.1 2>/dev/null || true
  fi
  kill "$aspd_pid" "$sshd_pid" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

for _ in $(seq 1 100); do
  if ssh -q -i /tmp/asp-bench-id -p 2222 \
      -o BatchMode=yes -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile=/dev/null 127.0.0.1 true; then
    break
  fi
  sleep 0.05
done

make_fixture() {
  local workspace=$1
  mkdir -p "/work/$workspace/src"
  printf 'alpha function\nTODO: improve alpha\n' >"/work/$workspace/src/alpha.txt"
  printf 'beta function calls alpha\n' >"/work/$workspace/src/beta.txt"
  printf 'gamma function\nalpha integration\n' >"/work/$workspace/src/gamma.txt"
  printf '#!/bin/sh\nset -eu\ngrep -q alpha src/alpha.txt\ngrep -q beta src/beta.txt\ngrep -q gamma src/gamma.txt\n' >"/work/$workspace/test.sh"
  chmod +x "/work/$workspace/test.sh"
  git -C "/work/$workspace" init -q
  git -C "/work/$workspace" config user.email benchmark@example.invalid
  git -C "/work/$workspace" config user.name 'ASP benchmark'
  git -C "/work/$workspace" add .
  git -C "/work/$workspace" commit -qm fixture
}

make_fixture agent-fixture-asp
make_fixture agent-fixture-ssh

/work/target/release/asp \
  --cert /work/.asp/server-cert.der \
  --auth-token-file /work/.asp/auth-token \
  --session-file /work/.asp/agent-session.json \
  connect 127.0.0.1:4433 >/dev/null

ssh_base=(
  ssh -q -i /tmp/asp-bench-id -p 2222
  -o BatchMode=yes -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null -o ControlPath=/tmp/asp-control
  127.0.0.1
)

tc qdisc replace dev lo root netem \
  delay "${delay_ms}ms" "${jitter_ms}ms" \
  loss "${loss_percent}%" \
  rate "${rate_mbit}mbit"
: >"$results"

rx_before=$(cat /sys/class/net/lo/statistics/rx_bytes)
tx_before=$(cat /sys/class/net/lo/statistics/tx_bytes)
read -r aspd_user_before aspd_system_before aspd_rss_before < <(aspd_proc_stats)
set +e
/usr/bin/time -f '%U %S %M' -o /tmp/asp-agent-time -- \
  timeout --kill-after=5s 120s /work/target/release/asp \
  --cert /work/.asp/server-cert.der \
  --auth-token-file /work/.asp/auth-token \
  --session-file /work/.asp/agent-session.json \
  agent-workload 127.0.0.1:4433 \
  --workspace agent-fixture-asp \
  --disconnect-seconds "$disconnect_seconds" \
  --log-mode "$log_mode" \
  "${summary_args[@]}" >/tmp/asp-agent.json
asp_status=$?
set -e
rx_after=$(cat /sys/class/net/lo/statistics/rx_bytes)
tx_after=$(cat /sys/class/net/lo/statistics/tx_bytes)
read -r aspd_user_after aspd_system_after aspd_rss_after < <(aspd_proc_stats)
if [[ "$asp_status" -ne 0 || ! -s /tmp/asp-agent.json ]]; then
  echo "ASP agent workload failed (status=$asp_status)" >&2
  tail -n 40 /tmp/aspd.log >&2 || true
  exit 1
fi
read -r asp_client_user asp_client_system asp_client_rss </tmp/asp-agent-time
aspd_user_cpu_ms=$(ticks_to_ms "$aspd_user_before" "$aspd_user_after")
aspd_system_cpu_ms=$(ticks_to_ms "$aspd_system_before" "$aspd_system_after")
aspd_rss_kb=$(pages_to_kb "$aspd_rss_after")
jq -c \
  --arg scenario "delay=${delay_ms}ms,jitter=${jitter_ms}ms,loss=${loss_percent}%,rate=${rate_mbit}mbit" \
  --arg log_mode "$log_mode" \
  --argjson trial "$trial" \
  --argjson rx_bytes "$((rx_after - rx_before))" \
  --argjson tx_bytes "$((tx_after - tx_before))" \
  --argjson aspd_user_cpu_ms "$aspd_user_cpu_ms" \
  --argjson aspd_system_cpu_ms "$aspd_system_cpu_ms" \
  --argjson aspd_rss_kb "$aspd_rss_kb" \
  --arg client_user_cpu_ms "$asp_client_user" \
  --arg client_system_cpu_ms "$asp_client_system" \
  --argjson client_max_rss_kb "$asp_client_rss" \
  '. + {scenario:$scenario,log_mode:$log_mode,trial:$trial,interface_rx_bytes:$rx_bytes,interface_tx_bytes:$tx_bytes,status:0,aspd_user_cpu_ms:$aspd_user_cpu_ms,aspd_system_cpu_ms:$aspd_system_cpu_ms,aspd_rss_kb:$aspd_rss_kb,client_user_cpu_ms:(($client_user_cpu_ms|tonumber)*1000),client_system_cpu_ms:(($client_system_cpu_ms|tonumber)*1000),client_max_rss_kb:$client_max_rss_kb}' \
  /tmp/asp-agent.json | tee -a "$results"

ssh_started=$(date +%s%N)
rx_before=$(cat /sys/class/net/lo/statistics/rx_bytes)
tx_before=$(cat /sys/class/net/lo/statistics/tx_bytes)
master_started=$(date +%s%N)
ssh -MNf -i /tmp/asp-bench-id -p 2222 \
  -o BatchMode=yes -o StrictHostKeyChecking=no \
  -o UserKnownHostsFile=/dev/null -o ControlPath=/tmp/asp-control \
  127.0.0.1
master_finished=$(date +%s%N)

ssh_blocked_ns=$((master_finished - master_started))
ssh_round_trips=0
ssh_payload_bytes=0
ssh_client_user_cpu_ms=0
ssh_client_system_cpu_ms=0
ssh_client_max_rss_kb=0
ssh_timed() {
  local started finished size user_cpu system_cpu max_rss
  started=$(date +%s%N)
  /usr/bin/time -f '%U %S %M' -o /tmp/ssh-agent-time -- \
    "${ssh_base[@]}" "$@" >/tmp/ssh-agent.out
  finished=$(date +%s%N)
  ssh_blocked_ns=$((ssh_blocked_ns + finished - started))
  ssh_round_trips=$((ssh_round_trips + 1))
  size=$(wc -c </tmp/ssh-agent.out)
  ssh_payload_bytes=$((ssh_payload_bytes + size))
  read -r user_cpu system_cpu max_rss </tmp/ssh-agent-time
  ssh_client_user_cpu_ms=$(awk -v total="$ssh_client_user_cpu_ms" -v value="$user_cpu" \
    'BEGIN { printf "%.6f", total + (value * 1000) }')
  ssh_client_system_cpu_ms=$(awk -v total="$ssh_client_system_cpu_ms" -v value="$system_cpu" \
    'BEGIN { printf "%.6f", total + (value * 1000) }')
  if ((max_rss > ssh_client_max_rss_kb)); then
    ssh_client_max_rss_kb=$max_rss
  fi
}

ssh_workspace=/work/agent-fixture-ssh
ssh_timed "find $ssh_workspace -maxdepth 2 -type f -print | sort"
ssh_timed "git -C $ssh_workspace status --short"
ssh_timed "rg -n TODO $ssh_workspace"
ssh_timed "rg -n alpha $ssh_workspace"
ssh_timed "rg -n function $ssh_workspace"
for path in alpha beta gamma; do
  ssh_timed "cat $ssh_workspace/src/$path.txt"
  ssh_timed "printf '\\nagent edit src/$path.txt\\n' >>$ssh_workspace/src/$path.txt"
done
ssh_timed "cd $ssh_workspace && ./test.sh"
ssh_timed "$log_command"
ssh_timed "git -C $ssh_workspace diff --stat && git -C $ssh_workspace diff"
ssh_timed "wc -l $ssh_workspace/src/*.txt"
ssh_timed "nohup sh -c 'sleep $disconnect_seconds; printf persistent-agent-work-complete >$ssh_workspace/.complete' >/dev/null 2>&1 &"

ssh -q -S /tmp/asp-control -O exit 127.0.0.1 >/dev/null 2>&1 || true
sleep "$((disconnect_seconds + 1))"
recovery_started=$(date +%s%N)
master_started=$(date +%s%N)
ssh -MNf -i /tmp/asp-bench-id -p 2222 \
  -o BatchMode=yes -o StrictHostKeyChecking=no \
  -o UserKnownHostsFile=/dev/null -o ControlPath=/tmp/asp-control \
  127.0.0.1
master_finished=$(date +%s%N)
ssh_blocked_ns=$((ssh_blocked_ns + master_finished - master_started))
ssh_timed "test \"\$(cat $ssh_workspace/.complete)\" = persistent-agent-work-complete"
recovery_finished=$(date +%s%N)
ssh_timed "git -C $ssh_workspace status --short"

ssh_finished=$(date +%s%N)
rx_after=$(cat /sys/class/net/lo/statistics/rx_bytes)
tx_after=$(cat /sys/class/net/lo/statistics/tx_bytes)
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
  --argjson interface_rx_bytes "$((rx_after - rx_before))" \
  --argjson interface_tx_bytes "$((tx_after - tx_before))" \
  --argjson client_user_cpu_ms "$ssh_client_user_cpu_ms" \
  --argjson client_system_cpu_ms "$ssh_client_system_cpu_ms" \
  --argjson client_max_rss_kb "$ssh_client_max_rss_kb" \
  '{experiment:$experiment,system:$system,scenario:$scenario,log_mode:$log_mode,trial:$trial,application_round_trips:$application_round_trips,transport_connections:$transport_connections,application_payload_bytes:$application_payload_bytes,wall_ms:$wall_ms,network_blocked_ms:$network_blocked_ms,recovery_ms:$recovery_ms,disconnect_seconds:$disconnect_seconds,interface_rx_bytes:$interface_rx_bytes,interface_tx_bytes:$interface_tx_bytes,persistent_process_observed:true,status:0,client_user_cpu_ms:$client_user_cpu_ms,client_system_cpu_ms:$client_system_cpu_ms,client_max_rss_kb:$client_max_rss_kb}' \
  | tee -a "$results"

cp "$results" "$destination"
