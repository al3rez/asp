#!/usr/bin/env bash
set -euo pipefail
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
export TERM=xterm-256color

destination=${1:-/work/docker-results.jsonl}
results=/tmp/asp-docker-results.jsonl
temporary_destination=""
# The production comparison gate requires at least 30 independent samples per
# condition. Set ASP_BENCH_REPEATS=1 (or another explicit value) only for a
# quick smoke; the raw output records every trial number.
repeats=${ASP_BENCH_REPEATS:-30}
if ! [[ "$repeats" =~ ^[1-9][0-9]*$ ]] || ((repeats > 1000)); then
  echo "ASP_BENCH_REPEATS must be an integer from 1 to 1000" >&2
  exit 2
fi
# Linux `/proc` exposes the daemon's cumulative CPU counters and resident set
# size. Capture these alongside client `/usr/bin/time` values so a faster
# protocol path is not declared a win if it merely moves work into aspd.
cpu_ticks_per_second=$(getconf CLK_TCK 2>/dev/null || printf '100\n')
page_size_bytes=$(getconf PAGESIZE 2>/dev/null || printf '4096\n')
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
  kill "$aspd_pid" "$sshd_pid" 2>/dev/null || true
  if [[ -n "$temporary_destination" ]]; then
    rm -f -- "$temporary_destination"
  fi
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

/work/target/release/asp \
  --cert /work/.asp/server-cert.der \
  --auth-token-file /work/.asp/auth-token \
  --session-file /work/.asp/session.json \
  connect 127.0.0.1:4433 >/dev/null

measure() {
  local system=$1
  local scenario=$2
  local trial=$3
  shift 3
  local rx_before tx_before started finished rx_after tx_after status user_cpu system_cpu max_rss
  local aspd_user_before aspd_system_before aspd_rss_before
  local aspd_user_after aspd_system_after aspd_rss_after
  local aspd_user_cpu_ms aspd_system_cpu_ms aspd_rss_kb
  read -r aspd_user_before aspd_system_before aspd_rss_before < <(aspd_proc_stats)
  rx_before=$(cat /sys/class/net/lo/statistics/rx_bytes)
  tx_before=$(cat /sys/class/net/lo/statistics/tx_bytes)
  started=$(date +%s%N)
  set +e
  /usr/bin/time -f '%U %S %M' -o /tmp/bench-time -- \
    timeout --kill-after=2s 60s "$@" >/tmp/bench-command.out 2>/tmp/bench-command.err
  status=$?
  set -e
  finished=$(date +%s%N)
  rx_after=$(cat /sys/class/net/lo/statistics/rx_bytes)
  tx_after=$(cat /sys/class/net/lo/statistics/tx_bytes)
  read -r aspd_user_after aspd_system_after aspd_rss_after < <(aspd_proc_stats)
  read -r user_cpu system_cpu max_rss </tmp/bench-time
  aspd_user_cpu_ms=$(awk -v before="${aspd_user_before:-0}" \
    -v after="${aspd_user_after:-0}" -v hz="$cpu_ticks_per_second" \
    'BEGIN { delta=after-before; if (delta < 0) delta=0; printf "%.6f", (delta / hz) * 1000 }')
  aspd_system_cpu_ms=$(awk -v before="${aspd_system_before:-0}" \
    -v after="${aspd_system_after:-0}" -v hz="$cpu_ticks_per_second" \
    'BEGIN { delta=after-before; if (delta < 0) delta=0; printf "%.6f", (delta / hz) * 1000 }')
  aspd_rss_kb=$(awk -v pages="${aspd_rss_after:-0}" -v size="$page_size_bytes" \
    'BEGIN { if (pages < 0) pages=0; printf "%d", (pages * size) / 1024 }')
  jq -cn \
    --arg experiment command-latency \
    --arg system "$system" \
    --arg scenario "$scenario" \
    --argjson trial "$trial" \
    --argjson status "$status" \
    --argjson wall_ns "$((finished - started))" \
    --argjson rx_bytes "$((rx_after - rx_before))" \
    --argjson tx_bytes "$((tx_after - tx_before))" \
    --arg user_cpu "$user_cpu" \
    --arg system_cpu "$system_cpu" \
    --argjson client_max_rss_kb "$max_rss" \
    --argjson aspd_user_cpu_ms "$aspd_user_cpu_ms" \
    --argjson aspd_system_cpu_ms "$aspd_system_cpu_ms" \
    --argjson aspd_rss_kb "$aspd_rss_kb" \
    '{experiment:$experiment,system:$system,scenario:$scenario,trial:$trial,status:$status,wall_ns:$wall_ns,rx_bytes:$rx_bytes,tx_bytes:$tx_bytes,client_user_cpu_ms:(($user_cpu|tonumber)*1000),client_system_cpu_ms:(($system_cpu|tonumber)*1000),client_max_rss_kb:$client_max_rss_kb,aspd_user_cpu_ms:$aspd_user_cpu_ms,aspd_system_cpu_ms:$aspd_system_cpu_ms,aspd_rss_kb:$aspd_rss_kb}' \
    | tee -a "$results"
}

aspd_proc_stats() {
  # `aspd` has no spaces in its comm field, so these are stable `/proc/stat`
  # offsets: utime=14, stime=15, resident pages=24. Missing procfs (or a
  # process exiting during cleanup) yields zeros rather than invalid JSON.
  if [[ -r "/proc/$aspd_pid/stat" ]]; then
    awk '{print $14, $15, $24}' "/proc/$aspd_pid/stat"
  else
    printf '0 0 0\n'
  fi
}

run_scenario() {
  local name=$1 delay=$2 jitter=$3 loss=$4 rate=$5
  local descriptor="${name}:delay=${delay}ms,jitter=${jitter}ms,loss=${loss}%,rate=${rate}mbit"
  tc qdisc replace dev lo root netem \
    delay "${delay}ms" "${jitter}ms" \
    loss "${loss}%" \
    rate "${rate}mbit"

  for trial in $(seq 1 "$repeats"); do
    measure asp "$descriptor" "$trial" \
      /work/target/release/asp \
        --cert /work/.asp/server-cert.der \
        --auth-token-file /work/.asp/auth-token \
        --session-file /work/.asp/session.json \
        exec 127.0.0.1:4433 true

    measure ssh "$descriptor" "$trial" \
      ssh -q -i /tmp/asp-bench-id -p 2222 \
        -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null 127.0.0.1 true

    measure mosh "$descriptor" "$trial" \
      script -qefc \
        "stty rows 24 cols 80; mosh --predict=never --ssh='ssh -q -i /tmp/asp-bench-id -p 2222 -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null' 127.0.0.1 -- bash -lc true" \
        /dev/null
  done
}

: >"$results"
run_scenario rtt-0 0 0 0 100
run_scenario rtt-20 10 0 0 100
run_scenario rtt-100 50 0 0 100
run_scenario rtt-200 100 0 0 100
run_scenario rtt-300 150 0 0 100
run_scenario loss-1 50 0 1 100
run_scenario loss-5 50 0 5 100
run_scenario loss-10 50 0 10 100
run_scenario jitter-20 50 20 0 100
run_scenario jitter-100 50 100 0 100
run_scenario bandwidth-1 50 0 0 1
run_scenario bandwidth-10 50 0 0 10
run_scenario corner 150 100 10 1

tc qdisc del dev lo root 2>/dev/null || true
# Do not leave qualification as a documentation-only step.  The matrix is
# complete only when every expected impairment/system cell has the requested
# trial count and paired IDs.  A caller may deliberately use repeats=1 for a
# smoke, but the resulting file is still structurally complete and is clearly
# not a 30-trial production capture.
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
destination_parent=$(dirname -- "$destination")
mkdir -p -- "$destination_parent"
temporary_destination=$(mktemp "$destination_parent/.asp-benchmark.XXXXXX")
cp "$results" "$temporary_destination"
bash "$script_dir/qualify-results.sh" "$temporary_destination" "$repeats" command-latency >/dev/null
mv -f -- "$temporary_destination" "$destination"
temporary_destination=""
