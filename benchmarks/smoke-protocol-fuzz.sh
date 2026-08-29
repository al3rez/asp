#!/usr/bin/env bash
set -euo pipefail

# Run the deterministic bounded protocol-decoder corpus. This catches panics
# and accidental unbounded work in CI/release builds; it is deliberately not
# described as an independent security review or a coverage-guided fuzzing
# campaign.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bench_bin=${ASP_BENCH_BIN:-"$repo_root/target/release/asp-bench"}
iterations=${ASP_PROTOCOL_FUZZ_ITERATIONS:-10000}
max_bytes=${ASP_PROTOCOL_FUZZ_MAX_BYTES:-4096}
# Clap's u64 parser accepts decimal CLI values; this is the decimal form of
# the stable `0x4153505f46555a5a` default used by the Rust command.
seed=${ASP_PROTOCOL_FUZZ_SEED:-4707194405664414298}

if [[ ! -x "$bench_bin" ]]; then
  echo "asp-bench release binary is not executable: $bench_bin" >&2
  exit 2
fi
command -v jq >/dev/null 2>&1 || {
  echo "jq is required for the protocol-fuzz smoke" >&2
  exit 2
}

result=$("$bench_bin" protocol-fuzz \
  --iterations "$iterations" \
  --max-bytes "$max_bytes" \
  --seed "$seed")
jq -e \
  --argjson iterations "$iterations" \
  --argjson max_bytes "$max_bytes" \
  '.experiment == "protocol-fuzz"
   and .iterations == $iterations
   and .inputs == $iterations
   and .max_bytes == $max_bytes
   and .decoder_calls == ($iterations * 10)
   and .panics == 0' <<<"$result" >/dev/null
printf '%s\n' "$result"
