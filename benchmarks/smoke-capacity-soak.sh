#!/usr/bin/env bash
set -euo pipefail

# Sustained local capacity regression for independent coding-agent adapters.
# This is deliberately bounded and deterministic: it does not claim a
# production capacity SLO, but it catches connection/request/response-memory
# leaks that a one-shot concurrency test cannot see.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
workers=${ASP_CAPACITY_SOAK_WORKERS:-8}
duration_seconds=${ASP_CAPACITY_SOAK_SECONDS:-15}
interval_ms=${ASP_CAPACITY_SOAK_INTERVAL_MS:-100}
drain_grace_seconds=${ASP_CAPACITY_SOAK_DRAIN_GRACE_SECONDS:-60}
port=${ASP_CAPACITY_SOAK_PORT:-4599}
health_port=${ASP_CAPACITY_SOAK_HEALTH_PORT:-9499}

if ! [[ "$workers" =~ ^[1-9][0-9]*$ ]] || ((workers > 32)); then
  echo 'ASP_CAPACITY_SOAK_WORKERS must be an integer from 1 to 32' >&2
  exit 2
fi
if ! [[ "$duration_seconds" =~ ^[1-9][0-9]*$ ]] || ((duration_seconds > 300)); then
  echo 'ASP_CAPACITY_SOAK_SECONDS must be an integer from 1 to 300' >&2
  exit 2
fi
if ! [[ "$interval_ms" =~ ^[0-9]+$ ]] || ((interval_ms > 60000)); then
  echo 'ASP_CAPACITY_SOAK_INTERVAL_MS must be an integer from 0 to 60000' >&2
  exit 2
fi
if ! [[ "$drain_grace_seconds" =~ ^[1-9][0-9]*$ ]] || ((drain_grace_seconds > 600)); then
  echo 'ASP_CAPACITY_SOAK_DRAIN_GRACE_SECONDS must be an integer from 1 to 600' >&2
  exit 2
fi
if ! [[ "$port" =~ ^[1-9][0-9]*$ ]] || ((port > 65535)); then
  echo 'ASP_CAPACITY_SOAK_PORT must be a valid TCP/UDP port' >&2
  exit 2
fi
if ! [[ "$health_port" =~ ^[1-9][0-9]*$ ]] || ((health_port > 65535)); then
  echo 'ASP_CAPACITY_SOAK_HEALTH_PORT must be a valid TCP port' >&2
  exit 2
fi
if [[ "$port" == "$health_port" ]]; then
  echo 'ASP_CAPACITY_SOAK_PORT and ASP_CAPACITY_SOAK_HEALTH_PORT must differ' >&2
  exit 2
fi
if [[ ! -x "$aspd_bin" || ! -x "$asp_bin" ]]; then
  echo "release binaries are required: $aspd_bin and $asp_bin" >&2
  exit 2
fi
if ! command -v curl >/dev/null 2>&1; then
  echo 'smoke-capacity-soak.sh requires curl' >&2
  exit 2
fi

workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-capacity-soak.XXXXXX")
state=$(mktemp -d "${TMPDIR:-/tmp}/asp-capacity-soak-state.XXXXXX")
daemon_pid=""
client_pids=()
writer_pids=()
watchdog_pid=""
watchdog_timeout_file=""

cleanup() {
  if [[ -n "$watchdog_pid" ]]; then
    kill "$watchdog_pid" 2>/dev/null || true
    wait "$watchdog_pid" 2>/dev/null || true
  fi
  for pid in "${writer_pids[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in "${client_pids[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in "${writer_pids[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  for pid in "${client_pids[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -r -- "$workspace" "$state"
}
trap cleanup EXIT INT TERM

printf 'soak fixture\n' >"$workspace/fixture.txt"
"$aspd_bin" \
  --listen "127.0.0.1:$port" \
  --root "$workspace" \
  --cert "$workspace/.asp/server-cert.der" \
  --key "$workspace/.asp/server-key.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  --health-listen "127.0.0.1:$health_port" \
  >"$workspace/aspd.log" 2>&1 &
daemon_pid=$!

ready=0
for _ in $(seq 1 160); do
  if "$asp_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      --session-file "$state/bootstrap-session.json" \
      doctor "127.0.0.1:$port" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd.log" >&2
  echo 'ASP capacity-soak daemon did not become ready' >&2
  exit 1
fi

wait_for_zero_connections() {
  local active=1
  for _ in $(seq 1 240); do
    active=$(curl -fsS "http://127.0.0.1:$health_port/metrics" \
      | awk '$1 == "asp_active_connections" { print $2; found = 1 } END { if (!found) exit 1 }')
    if [[ "$active" == 0 ]]; then
      break
    fi
    sleep 0.05
  done
  test "$active" = 0
}
wait_for_zero_connections

metrics_before=$(curl -fsS "http://127.0.0.1:$health_port/metrics")
metric_value() {
  local name=$1
  local body=$2
  awk -v name="$name" '$1 == name { print $2; found = 1 } END { if (!found) exit 1 }' <<<"$body"
}
requests_before=$(metric_value asp_requests_total "$metrics_before")
response_bytes_before=$(metric_value asp_response_bytes_total "$metrics_before")
process_cpu_before=$(metric_value asp_process_cpu_time_us_total "$metrics_before")
process_launches_before=$(metric_value asp_process_launch_duration_us_count "$metrics_before")
process_launch_failures_before=$(metric_value asp_process_launch_failures_total "$metrics_before")
process_launch_sum_before=$(metric_value asp_process_launch_duration_us_sum "$metrics_before")

started_epoch=$(date +%s)
for i in $(seq 1 "$workers"); do
  worker_state="$state/worker-$i"
  mkdir -p "$worker_state"
  output="$worker_state/output.jsonl"
  mkfifo "$worker_state/in"
  XDG_STATE_HOME="$worker_state" "$asp_bin" \
    --cert "$workspace/.asp/server-cert.der" \
    --auth-token-file "$workspace/.asp/auth-token" \
    agent "127.0.0.1:$port" \
    <"$worker_state/in" >"$output" 2>"$worker_state/stderr" &
  client_pids+=("$!")
  # Open the FIFO only after the adapter exists. Each writer holds its own
  # stream open, so every worker keeps one authenticated QUIC connection
  # alive for the full soak instead of repeatedly measuring cold startup.
  (
    exec 3>"$worker_state/in"
    deadline=$(( $(date +%s) + duration_seconds ))
    sequence=0
    while (( $(date +%s) < deadline )); do
      sequence=$((sequence + 1))
      printf '{"id":"ping-%s-%s","op":"ping"}\n' "$i" "$sequence" >&3
      printf '{"id":"summary-%s-%s","op":"exec_summary","command":"printf worker-%s-%s"}\n' \
        "$i" "$sequence" "$i" "$sequence" >&3
      printf '{"id":"inspect-%s-%s","op":"inspect","include_tree":false,"include_git_status":false,"read_paths":["fixture.txt"]}\n' \
        "$i" "$sequence" >&3
      value=$(printf 'worker-%s-%s' "$i" "$sequence" | base64 | tr -d '\n')
      printf '{"id":"put-%s-%s","op":"file_put","path":"soak-%s-%s.txt","data_base64":"%s"}\n' \
        "$i" "$sequence" "$i" "$sequence" "$value" >&3
      if ((interval_ms > 0)); then
        sleep "$(awk -v ms="$interval_ms" 'BEGIN { printf "%.3f", ms / 1000 }')"
      fi
    done
    printf '{"id":"close-%s","op":"close"}\n' "$i" >&3
    exec 3>&-
  ) &
  writer_pids+=("$!")
done

# A FIFO provides useful backpressure, but it also means a deliberately
# aggressive workload can leave writers blocked behind a slow adapter. Bound
# the whole drain phase so a capacity regression cannot turn this smoke into
# an unbounded process tree. The timeout is a test failure, not a successful
# partial sample.
soak_deadline=$((started_epoch + duration_seconds + drain_grace_seconds))
watchdog_timeout_file="$state/watchdog-timeout"
(
  while :; do
    now=$(date +%s)
    if ((now >= soak_deadline)); then
      printf 'capacity-soak deadline exceeded (duration=%ss, drain grace=%ss)\n' \
        "$duration_seconds" "$drain_grace_seconds" >"$watchdog_timeout_file"
      for pid in "${writer_pids[@]}" "${client_pids[@]}"; do
        kill "$pid" 2>/dev/null || true
      done
      break
    fi
    sleep 1
  done
) &
watchdog_pid=$!

failed=0
for index in "${!writer_pids[@]}"; do
  if ! wait "${writer_pids[$index]}"; then
    echo "capacity-soak writer $((index + 1)) failed" >&2
    failed=1
  fi
done
for index in "${!client_pids[@]}"; do
  if ! wait "${client_pids[$index]}"; then
    echo "capacity-soak client $((index + 1)) failed" >&2
    cat "$state/worker-$((index + 1))/stderr" >&2 || true
    failed=1
  fi
done

# Stop and join the watchdog before inspecting its marker. This removes a
# race where the main process could read the marker just before the watchdog
# writes it at the deadline. The wall-clock check below also catches a test
# that finished after the deadline but was lucky enough to beat that write.
if [[ -n "$watchdog_pid" ]]; then
  kill "$watchdog_pid" 2>/dev/null || true
  wait "$watchdog_pid" 2>/dev/null || true
  watchdog_pid=""
fi
if [[ -f "$watchdog_timeout_file" ]] || (( $(date +%s) >= soak_deadline )); then
  if [[ -f "$watchdog_timeout_file" ]]; then
    cat "$watchdog_timeout_file" >&2
  else
    printf 'capacity-soak deadline exceeded (duration=%ss, drain grace=%ss)\n' \
      "$duration_seconds" "$drain_grace_seconds" >&2
  fi
  failed=1
fi
if [[ "$failed" != 0 ]]; then
  exit 1
fi

finished_epoch=$(date +%s)
metrics=$(curl -fsS "http://127.0.0.1:$health_port/metrics")
printf '%s\n' "$metrics" | grep -Eq '^asp_request_failures 0$'
printf '%s\n' "$metrics" | grep -Eq '^asp_active_connections 0$'
printf '%s\n' "$metrics" | grep -Eq '^asp_active_request_streams 0$'
printf '%s\n' "$metrics" | grep -Eq '^asp_frame_memory_bytes 0$'
printf '%s\n' "$metrics" | grep -Eq '^asp_response_memory_bytes 0$'
requests_after=$(metric_value asp_requests_total "$metrics")
response_bytes_after=$(metric_value asp_response_bytes_total "$metrics")
process_cpu_after=$(metric_value asp_process_cpu_time_us_total "$metrics")
process_launches_after=$(metric_value asp_process_launch_duration_us_count "$metrics")
process_launch_failures_after=$(metric_value asp_process_launch_failures_total "$metrics")
process_launch_sum_after=$(metric_value asp_process_launch_duration_us_sum "$metrics")
if ((requests_after < requests_before || response_bytes_after < response_bytes_before || process_cpu_after < process_cpu_before || process_launches_after < process_launches_before || process_launch_failures_after < process_launch_failures_before || process_launch_sum_after < process_launch_sum_before)); then
  echo 'capacity-soak daemon counters regressed during the run' >&2
  exit 1
fi
if ((process_launches_after == process_launches_before)); then
  echo 'capacity-soak did not record any process launches' >&2
  exit 1
fi
if ((process_launch_failures_after != process_launch_failures_before)); then
  echo 'capacity-soak observed a process-launch failure' >&2
  exit 1
fi
request_delta=$((requests_after - requests_before))
response_bytes_delta=$((response_bytes_after - response_bytes_before))
process_cpu_delta_us=$((process_cpu_after - process_cpu_before))
process_launch_delta=$((process_launches_after - process_launches_before))
process_launch_sum_delta=$((process_launch_sum_after - process_launch_sum_before))

total_lines=0
for i in $(seq 1 "$workers"); do
  output="$state/worker-$i/output.jsonl"
  lines=$(wc -l <"$output" | tr -d ' ')
  if ((lines < 5)); then
    echo "capacity-soak worker $i produced too few adapter responses ($lines)" >&2
    cat "$output" >&2 || true
    exit 1
  fi
  if grep -q '"type":"error"' "$output"; then
    echo "capacity-soak worker $i returned an adapter error" >&2
    grep '"type":"error"' "$output" >&2 || true
    exit 1
  fi
  total_lines=$((total_lines + lines))
done

jq -cn \
  --argjson workers "$workers" \
  --argjson duration_seconds "$duration_seconds" \
  --argjson interval_ms "$interval_ms" \
  --argjson drain_grace_seconds "$drain_grace_seconds" \
  --argjson responses "$total_lines" \
  --argjson wall_ms "$(((finished_epoch - started_epoch) * 1000))" \
  --argjson request_delta "$request_delta" \
  --argjson response_bytes_delta "$response_bytes_delta" \
  --argjson process_cpu_delta_us "$process_cpu_delta_us" \
  --argjson process_launch_delta "$process_launch_delta" \
  --argjson process_launch_sum_delta_us "$process_launch_sum_delta" \
  '{experiment:"capacity-soak",workers:$workers,duration_seconds:$duration_seconds,interval_ms:$interval_ms,drain_grace_seconds:$drain_grace_seconds,responses:$responses,wall_ms:$wall_ms,request_delta:$request_delta,response_bytes_delta:$response_bytes_delta,process_cpu_delta_us:$process_cpu_delta_us,process_launch_delta:$process_launch_delta,process_launch_sum_delta_us:$process_launch_sum_delta_us,status:0}'
printf 'ASP capacity-soak smoke passed (workers=%s duration=%ss responses=%s drain_grace=%ss)\n' \
  "$workers" "$duration_seconds" "$total_lines" "$drain_grace_seconds"
