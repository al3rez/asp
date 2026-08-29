#!/usr/bin/env bash
set -euo pipefail

# Release-level smoke for the long-lived coding-agent adapter. This exercises
# the real QUIC daemon and deliberately keeps client cursor/output state out of
# the workspace so a semantic inspection can hit its digest fast path.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
aspd_bin=${ASPD_BIN:-"$repo_root/target/release/aspd"}
asp_bin=${ASP_BIN:-"$repo_root/target/release/asp"}
port=${ASP_AGENT_SMOKE_PORT:-4545}
health_port=${ASP_AGENT_SMOKE_HEALTH_PORT:-9445}
workspace=$(mktemp -d "${TMPDIR:-/tmp}/asp-agent-smoke.XXXXXX")
state_home=$(mktemp -d "${TMPDIR:-/tmp}/asp-agent-state.XXXXXX")
daemon_pid=""

cleanup() {
  if [[ -n "$daemon_pid" ]]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  rm -rf -- "$workspace" "$state_home"
}
trap cleanup EXIT INT TERM

{
  printf '%*s' 256 '' | tr ' ' a
  printf 'old body'
  printf '%*s' 256 '' | tr ' ' z
} >"$workspace/fixture.txt"
new_fixture="$state_home/new-fixture.txt"
{
  printf '%*s' 256 '' | tr ' ' a
  printf 'new body'
  printf '%*s' 256 '' | tr ' ' z
} >"$new_fixture"
if command -v shasum >/dev/null 2>&1; then
  fixture_sha=$(shasum -a 256 "$workspace/fixture.txt" | awk '{print $1}')
  new_fixture_sha=$(shasum -a 256 "$new_fixture" | awk '{print $1}')
else
  fixture_sha=$(sha256sum "$workspace/fixture.txt" | awk '{print $1}')
  new_fixture_sha=$(sha256sum "$new_fixture" | awk '{print $1}')
fi
new_fixture_base64=$(base64 <"$new_fixture" | tr -d '\n')
fixture_base64=$(base64 <"$workspace/fixture.txt" | tr -d '\n')
# A broad rewrite exercises the CLI's adaptive full-file fallback; repeating
# the same patch exercises the byte-identical no-op path.
printf '%*s' 1024 '' | tr ' ' a >"$workspace/broad.txt"
printf '%*s' 1024 '' | tr ' ' b >"$state_home/broad-new.txt"
make_range_fixture() {
  local output=$1
  local first=$2
  local second=$3
  local third=$4
  {
    printf '%*s' 512 '' | tr ' ' a
    printf '%s' "$first"
    printf '%*s' 15864 '' | tr ' ' a
    printf '%s' "$second"
    printf '%*s' 24568 '' | tr ' ' a
    printf '%s' "$third"
    printf '%*s' 8184 '' | tr ' ' a
  } >"$output"
}
make_range_fixture "$workspace/ranges.txt" ORIG-000 ORIG-001 ORIG-002
make_range_fixture "$state_home/ranges-new.txt" EDIT-001 EDIT-002 EDIT-003
make_range_fixture "$workspace/explicit-ranges.txt" ORIG-000 ORIG-001 ORIG-002
make_line_range_fixture() {
  local output=$1
  local mode=$2
  for line in $(seq 0 511); do
    case "$mode:$line" in
      old:*)
        printf 'fn item_%03d() { old_%03d(); }\n' "$line" "$line"
        ;;
      new:20)
        printf 'fn item_%03d() { new_%03d(); extra_%03d(); }\n' "$line" "$line" "$line"
        ;;
      new:240)
        printf 'fn item_%03d() { new_%03d(); }\ninserted_%03d();\n' "$line" "$line" "$line"
        ;;
      new:460)
        printf 'fn item_%03d() { new_%03d(); }\n' "$line" "$line"
        ;;
      new:*)
        printf 'fn item_%03d() { old_%03d(); }\n' "$line" "$line"
        ;;
    esac
  done >"$output"
}
make_line_range_fixture "$workspace/line-ranges.txt" old
make_line_range_fixture "$state_home/line-ranges-new.txt" new
if command -v shasum >/dev/null 2>&1; then
  ranges_sha=$(shasum -a 256 "$workspace/ranges.txt" | awk '{print $1}')
  explicit_ranges_sha=$(shasum -a 256 "$workspace/explicit-ranges.txt" | awk '{print $1}')
  line_ranges_sha=$(shasum -a 256 "$workspace/line-ranges.txt" | awk '{print $1}')
  line_ranges_new_sha=$(shasum -a 256 "$state_home/line-ranges-new.txt" | awk '{print $1}')
else
  ranges_sha=$(sha256sum "$workspace/ranges.txt" | awk '{print $1}')
  explicit_ranges_sha=$(sha256sum "$workspace/explicit-ranges.txt" | awk '{print $1}')
  line_ranges_sha=$(sha256sum "$workspace/line-ranges.txt" | awk '{print $1}')
  line_ranges_new_sha=$(sha256sum "$state_home/line-ranges-new.txt" | awk '{print $1}')
fi
ranges_new_base64=$(base64 <"$state_home/ranges-new.txt" | tr -d '\n')
line_ranges_new_base64=$(base64 <"$state_home/line-ranges-new.txt" | tr -d '\n')
if command -v base64 >/dev/null 2>&1; then
  explicit_one_base64=$(printf '%s' EDIT-001 | base64 | tr -d '\n')
  explicit_two_base64=$(printf '%s' EDIT-002 | base64 | tr -d '\n')
  explicit_three_base64=$(printf '%s' EDIT-003 | base64 | tr -d '\n')
else
  echo "base64 is required for the agent smoke" >&2
  exit 1
fi
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
for _ in $(seq 1 100); do
  if XDG_STATE_HOME="$state_home" "$asp_bin" \
      --cert "$workspace/.asp/server-cert.der" \
      --auth-token-file "$workspace/.asp/auth-token" \
      doctor "127.0.0.1:$port" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "$ready" != 1 ]]; then
  cat "$workspace/aspd.log" >&2
  echo "ASP agent smoke daemon did not become ready" >&2
  exit 1
fi

input="$state_home/agent-input.jsonl"
output="$state_home/agent-output.jsonl"
printf '%s\n' \
  '{"id":"exec-1","op":"exec_summary","command":"printf agent-adapter-ok"}' \
  '{"id":"summary-large","op":"exec_summary","command":"head -c 1048576 /dev/zero","tail_bytes":4096}' \
  '{"id":"spawn-1","op":"spawn","request_id":"00000000-0000-0000-0000-000000000009","command":"printf spawned-agent-prefix-TAIL"}' \
  '{"id":"inspect-1","op":"inspect","workspace":".","read_paths":["fixture.txt"]}' \
  '{"id":"inspect-ranges","op":"inspect","workspace":".","include_tree":false,"include_git_status":false,"read_paths":["ranges.txt"]}' \
  '{"id":"inspect-line-ranges","op":"inspect","workspace":".","include_tree":false,"include_git_status":false,"read_paths":["line-ranges.txt"]}' \
  '{"id":"inspect-2","op":"inspect","workspace":".","read_paths":["fixture.txt"]}' \
  '{"id":"inspect-no-tree","op":"inspect","workspace":".","include_tree":false,"searches":["old body"]}' \
  '{"id":"inspect-no-git","op":"inspect","workspace":".","include_tree":false,"include_git_status":false,"read_paths":["fixture.txt"]}' \
  "{\"id\":\"same-1\",\"op\":\"file_put\",\"path\":\"fixture.txt\",\"expected_sha256\":\"$fixture_sha\",\"data_base64\":\"$fixture_base64\"}" \
  "{\"id\":\"delta-1\",\"op\":\"file_put\",\"path\":\"fixture.txt\",\"expected_sha256\":\"$fixture_sha\",\"data_base64\":\"$new_fixture_base64\"}" \
  "{\"id\":\"explicit-range-1\",\"op\":\"file_patch_ranges\",\"path\":\"explicit-ranges.txt\",\"expected_sha256\":\"$explicit_ranges_sha\",\"ranges\":[{\"offset\":512,\"remove_len\":8,\"replacement_base64\":\"$explicit_one_base64\"},{\"offset\":16384,\"remove_len\":8,\"replacement_base64\":\"$explicit_two_base64\"},{\"offset\":40960,\"remove_len\":8,\"replacement_base64\":\"$explicit_three_base64\"}]}" \
  '{"id":"inspect-ranges-2","op":"inspect","workspace":".","include_tree":false,"include_git_status":false,"read_paths":["ranges.txt"]}' \
  '{"id":"inspect-line-ranges-2","op":"inspect","workspace":".","include_tree":false,"include_git_status":false,"read_paths":["line-ranges.txt"]}' \
  "{\"id\":\"range-delta-1\",\"op\":\"file_put\",\"path\":\"ranges.txt\",\"expected_sha256\":\"$ranges_sha\",\"data_base64\":\"$ranges_new_base64\"}" \
  "{\"id\":\"inspect-line-ranges-3\",\"op\":\"inspect\",\"workspace\":\".\",\"include_tree\":false,\"include_git_status\":false,\"read_paths\":[\"line-ranges.txt\"]}" \
  "{\"id\":\"line-range-delta-1\",\"op\":\"file_put\",\"path\":\"line-ranges.txt\",\"expected_sha256\":\"$line_ranges_sha\",\"data_base64\":\"$line_ranges_new_base64\"}" \
  '{"id":"ping-1","op":"ping"}' \
  '{"id":"close-1","op":"close"}' >"$input"

XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  agent "127.0.0.1:$port" <"$input" >"$output"

grep -q '"type":"ready"' "$output"
grep -q '"type":"spawned"' "$output"
grep -q '"stdout_tail_base64":"YWdlbnQtYWRhcHRlci1vaw==".*"type":"summary"' "$output"
grep -q '"id":"summary-large".*"stdout_bytes":1048576.*"type":"summary"' "$output"
grep -q '"id":"inspect-1".*"type":"workspace_state"' "$output"
grep -q '"id":"inspect-2".*"state_unchanged":true' "$output"
if ! jq -e 'select(.id == "inspect-no-tree" and (.tree | length) == 0 and .type == "workspace_state")' "$output" >/dev/null; then
  cat "$output" >&2
  exit 1
fi
if ! jq -e 'select(.id == "inspect-no-git" and (.git_status == null) and (.tree | length) == 0 and .type == "workspace_state")' "$output" >/dev/null; then
  cat "$output" >&2
  exit 1
fi
if ! grep -q '"id":"delta-1".*"transfer":"patch".*"type":"file_stored"' "$output" \
  || ! grep -q '"id":"delta-1".*"sha256":"'$new_fixture_sha'"' "$output"; then
  cat "$output" >&2
  exit 1
fi
if ! grep -q '"id":"range-delta-1".*"transfer":"patch_ranges".*"type":"file_stored"' "$output"; then
  cat "$output" >&2
  exit 1
fi
if ! grep -q '"id":"line-range-delta-1".*"transfer":"patch_ranges".*"type":"file_stored"' "$output"; then
  cat "$output" >&2
  exit 1
fi
if ! grep -q '"id":"line-range-delta-1".*"sha256":"'$line_ranges_new_sha'"' "$output"; then
  cat "$output" >&2
  exit 1
fi
if ! grep -q '"id":"explicit-range-1".*"transfer":"patch_ranges".*"type":"file_stored"' "$output"; then
  cat "$output" >&2
  exit 1
fi
if ! grep -q '"id":"same-1".*"transfer":"none".*"type":"file_unchanged"' "$output" \
  || ! grep -q '"id":"same-1".*"sha256":"'$fixture_sha'"' "$output"; then
  cat "$output" >&2
  exit 1
fi
grep -q '"id":"ping-1".*"type":"pong"' "$output"
grep -q '"id":"close-1".*"type":"closed"' "$output"

broad_patch_output=$(XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  patch "127.0.0.1:$port" "$state_home/broad-new.txt" broad.txt)
grep -q 'broad.txt version=' <<<"$broad_patch_output"
unchanged_patch_output=$(XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  patch "127.0.0.1:$port" "$state_home/broad-new.txt" broad.txt)
grep -q 'broad.txt version=.*(unchanged)' <<<"$unchanged_patch_output"

spawned_process_id=$(jq -r 'select(.type == "spawned" and .id == "spawn-1") | .process_id' "$output")
test -n "$spawned_process_id" && test "$spawned_process_id" != null
sleep 0.2
logs_input="$state_home/logs-input.jsonl"
logs_output="$state_home/logs-output.jsonl"
printf '%s\n' \
  "{\"id\":\"logs-1\",\"op\":\"logs\",\"process_id\":\"$spawned_process_id\",\"stream\":\"stdout\",\"tail_bytes\":4}" \
  '{"id":"logs-close","op":"close"}' >"$logs_input"
XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  agent "127.0.0.1:$port" <"$logs_input" >"$logs_output"
grep -q '"data_base64":"VEFJTA==".*"id":"logs-1".*"type":"log"' "$logs_output"
grep -q '"bytes":4.*"complete":true.*"id":"logs-1".*"type":"log_end"' "$logs_output"
cli_tail=$(XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  logs "127.0.0.1:$port" "$spawned_process_id" --stream stdout --tail 4)
test "$cli_tail" = "TAIL"
status_input="$state_home/status-input.jsonl"
status_output="$state_home/status-output.jsonl"
printf '%s\n' \
  "{\"id\":\"status-1\",\"op\":\"process_status\",\"process_id\":\"$spawned_process_id\"}" \
  '{"id":"status-close","op":"close"}' >"$status_input"
XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  agent "127.0.0.1:$port" <"$status_input" >"$status_output"
grep -q '"id":"status-1".*"process_id":"'$spawned_process_id'".*"type":"process_state"' "$status_output"
# The raw batch path can also opt into bounded summaries without paying a
# second connection for each command. Keep this in the release smoke so the
# fast scripted-agent path does not regress to full-log forwarding.
batch_output=$(XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  batch "127.0.0.1:$port" --summary --tail-bytes 4 \
  --command "printf batch-tail" 2>&1)
grep -q 'ASP summary: stdout_bytes=10 ' <<<"$batch_output"
grep -q 'tail' <<<"$batch_output"
# Independent status checks can overlap on one warm QUIC connection. The
# parallel path is intentionally zero-tail summary mode, so this smoke checks
# its input-ordered exit markers without making output ordering part of the
# contract.
parallel_output=$(XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  batch "127.0.0.1:$port" --summary --tail-bytes 0 --parallel 2 \
  --command true --command true 2>&1)
grep -q '^ASP_BATCH_RESULT 0 0$' <<<"$parallel_output"
grep -q '^ASP_BATCH_RESULT 1 0$' <<<"$parallel_output"
set +e
parallel_failure_output=$(XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  batch "127.0.0.1:$port" --summary --tail-bytes 0 --parallel 2 \
  --command true --command false 2>&1)
parallel_failure_status=$?
set -e
test "$parallel_failure_status" -eq 1
grep -q '^ASP_BATCH_RESULT 0 0$' <<<"$parallel_failure_output"
grep -q '^ASP_BATCH_RESULT 1 1$' <<<"$parallel_failure_output"
metrics=$(curl -fsS "http://127.0.0.1:$health_port/metrics")
printf '%s' "$metrics" | grep -Eq '^asp_quic_udp_tx_bytes_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_quic_path_rtt_us [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_response_memory_bytes [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_response_memory_limit [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_response_memory_rejections [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_pty_state_datagrams_sent_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_pty_state_datagram_bytes_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_pty_state_datagrams_compressed_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_pty_state_datagrams_skipped_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_pty_state_delta_datagrams_sent_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_pty_state_delta_datagram_bytes_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_pty_state_delta_datagrams_skipped_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_response_encode_gate_acquisitions_total [1-9][0-9]*$'
printf '%s' "$metrics" | grep -Eq '^asp_response_encode_gate_wait_us_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_response_encode_duration_us_total [1-9][0-9]*$'
printf '%s' "$metrics" | grep -Eq '^asp_process_log_sync_total [1-9][0-9]*$'
printf '%s' "$metrics" | grep -Eq '^asp_process_log_sync_bytes_total [1-9][0-9]*$'
printf '%s' "$metrics" | grep -Eq '^asp_process_log_sync_duration_us_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_process_log_sync_failures_total 0$'
printf '%s' "$metrics" | grep -Eq '^asp_storage_maintenance_runs_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_storage_maintenance_failures_total [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_storage_maintenance_last_failures [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_storage_maintenance_started_unix_ms [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_storage_maintenance_last_run_unix_ms [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_storage_maintenance_last_success_unix_ms [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_storage_maintenance_healthy [01]$'
printf '%s' "$metrics" | grep -Eq '^asp_request_duration_us_bucket\{operation="exec_summary",le="\+Inf"\} [0-9]+$'
printf '%s' "$metrics" | grep -Eq '^asp_request_duration_us_count\{operation="exec_summary"\} [1-9][0-9]*$'
if [[ "$(uname -s)" == "Linux" ]]; then
  printf '%s' "$metrics" | grep -Eq '^asp_process_no_new_privs 1$'
else
  printf '%s' "$metrics" | grep -Eq '^asp_process_no_new_privs 0$'
fi
printf '%s\n' "$metrics" | grep -Eq '^asp_workspace_digest_cache_hits_total [1-9][0-9]*$'

process_id=$(XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  spawn "127.0.0.1:$port" "sleep 30")
signal_input="$state_home/signal-input.jsonl"
signal_output="$state_home/signal-output.jsonl"
printf '%s\n' \
  "{\"id\":\"signal-1\",\"op\":\"signal\",\"process_id\":\"$process_id\",\"signal\":\"TERM\"}" \
  '{"id":"signal-close","op":"close"}' >"$signal_input"
XDG_STATE_HOME="$state_home" "$asp_bin" \
  --cert "$workspace/.asp/server-cert.der" \
  --auth-token-file "$workspace/.asp/auth-token" \
  agent "127.0.0.1:$port" <"$signal_input" >"$signal_output"
grep -q '"id":"signal-1".*"type":"signal_applied"' "$signal_output"
test -f "$state_home/asp/sessions.json"
test ! -e "$workspace/.asp-session"

printf 'ASP agent smoke passed\n'
