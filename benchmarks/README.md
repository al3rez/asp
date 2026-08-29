# Benchmark harness

The packaged-release runtime gate is `bash
benchmarks/smoke-packaged-runtime.sh RELEASE.tar.gz [RELEASE.sha256]`. It
verifies and extracts the exact archive, starts its `aspd` under the
fail-closed production profile, runs a strict doctor/readiness check, performs
a file PUT/GET round trip, kills and restarts the packaged daemon, and verifies
that a detached process and its durable log survive. This catches packaging,
launcher, and deployment-path regressions that source-tree smokes cannot. It
uses loopback only and is not a substitute for the two-host WAN or supervisor
qualification matrix.

The transactional rollout gate is `bash
benchmarks/smoke-upgrade-release.sh RELEASE.tar.gz [RELEASE.sha256]`. It runs
the packaged `deploy/upgrade-release.sh` against a real daemon and a generated
supervisor shim, forces one failed readiness transition, verifies automatic
pointer rollback and recovery, then verifies a normal upgrade. It validates the
operator workflow locally; it does not replace independent-host rollback,
supervisor, or historical-binary qualification.

The release provenance gate is `bash
benchmarks/smoke-release-signature.sh RELEASE.tar.gz [RELEASE.sha256]`. It
extracts the packaged GnuPG signing/verifying helpers, creates an ephemeral
test key, verifies the checksum signature, and requires an unexpected
fingerprint to fail. It checks the helper wiring only; production key custody,
trust distribution, rotation, and attestations remain operator-owned.
Production promotion should pass `--require-signature` (and an exact
`--fingerprint`) to both the atomic installer and readiness-gated upgrader. The
smoke also replaces the source archive from injected verifier/tar shims and
requires both the standalone verifier and installer to succeed from private
bounded snapshots, guarding against archive-path replacement between
verification and extraction.

The cursor-integrity regression is `bash
benchmarks/smoke-event-cursor-safety.sh`. It interleaves detached `SPAWN` and
filtered `EXEC` attachments, then requires a full `RESUME` to recover every
durable event; this protects the boundary between attachment retry progress
and named-consumer replay cursors.

`netem.sh` applies one-direction Linux `tc netem` shaping to an explicitly named interface, runs one command, and always removes the qdisc. To model a target RTT, apply half the delay on each host/interface or document that a one-sided delay is being used.

The standalone frame-codec check is `mise exec -- cargo run --locked
--release -p asp-bench -- frame-compression`. It reports encoded ratios for a
repetitive and a deterministic pseudo-random 1 MiB payload; use it to catch
codec regressions without confusing local compression CPU with network
latency. The checked-in sample is
`raw/frame-compression-2026-08-27.jsonl`.

The rich-terminal MTU check is `mise exec -- cargo run --locked --release -p
asp-bench -- pty-datagram-compression`. It encodes a deterministic ANSI screen,
checks it against a 1,200-byte DATAGRAM budget, and round-trips the `PZ`/`AF`
payload. The 2026-08-28 sample reduced a 7,777-byte plain redraw to 348 bytes
and decoded it byte-for-byte; this is a codec/fit smoke, not a network latency
claim. The raw row is `raw/pty-rich-datagram-compression-2026-08-28.jsonl`.

The plain PTY state-delta check is `mise exec -- cargo run --locked
--release -p asp-bench -- pty-state-delta`. It compares the complete plain
screen with cursor-only, localized-row, and broad-screen deltas and verifies
that the sender chooses the delta only when it is a wire win. The deterministic
80x120 fixture measured 5,703 bytes for a full screen versus 99 bytes for one
changed row; an 80-row rewrite measured 10,506 bytes and correctly fell back
to the full snapshot. The raw row is
`raw/pty-state-delta-2026-08-28.jsonl`.

The file mutation codec check is `mise exec -- cargo run --locked --release -p
asp-bench -- file-sync`. It serializes deterministic localized, scattered,
compressible, and incompressible edits with the exact v17 envelope, then reports
the full `FILE_PUT`, contiguous `FILE_PATCH`, and negotiated
`FILE_PATCH_RANGES` wire sizes plus the current raw selection decision. The
checked-in 2026-08-28 row is
`raw/file-sync-wire-2026-08-28.jsonl`. It measures framing/codec behavior only;
end-to-end WAN transfer and CPU costs still belong to the paired agent matrix.

When Linux `tc netem` or Docker is unavailable, `asp-bench udp-proxy` provides
a small userspace UDP shaper for local Quinn experiments. It binds an explicit
listener, forwards only to the explicit target, and applies the same delay in
both directions (so `--delay-ms 50` is approximately 100 ms RTT). Jitter is
uniform and deterministic, loss is packet-based, and `--rate-mbit` serializes
each direction; the queue is capped at 1,024 packets/32 MiB. For example:

```sh
mise exec -- cargo run --locked --release -p asp-bench -- udp-proxy \
  --listen 127.0.0.1:5443 --target 127.0.0.1:4433 \
  --delay-ms 50 --jitter-ms 20 --loss-percent 1 --rate-mbit 10
```

The command prints a JSON listening record followed by a JSON stop summary on
Ctrl-C. Use a separate process to run `aspd` on the target address and point
the normal ASP client at the proxy listener. This is useful for repeatable
single-host regression runs and macOS development, but it is not a NAT relay,
security boundary, or substitute for the required two-host qualification
matrix; packet captures and independently managed hosts are still needed for
release SLOs. The bidirectional loopback forwarding and deterministic shaper
are covered by `asp-bench`'s async tests. The end-to-end smoke
`benchmarks/smoke-udp-proxy.sh` runs a real authenticated `asp doctor` through
the proxy, then keeps a JSONL agent alive across a 17-second proxy outage and
same-address restart. That proves request-level recovery after the configured
15-second QUIC idle bound on both Linux and macOS; it does not prove physical
Wi-Fi/cellular roaming or a two-host path migration.

The v16 compatibility smoke is `bash benchmarks/smoke-legacy.sh`. It first
sends HELLO/HEALTH using legacy plain framing to a v17 daemon, then restarts a
deterministic v16-only compatibility fixture and verifies that the current
client prefers v17 and automatically retries v16 when the old peer rejects the
v17 envelope. It creates a durable session/process through that v16 mode,
rolls forward to v17, recovers the finished process status/log, then rolls back
to the v16 ceiling and recovers the same state again. A mixed-binary release
matrix with historical artifacts is still required to publish a compatibility
SLO.

Never run it on an interface you have not verified. Capture exact commands, system versions, ASP git revision, network route, and raw JSON/pcap files. `docs/BENCHMARKS.md` defines the matrix and metrics.

Summarize a captured JSONL file without discarding failed trials with:

```sh
bash benchmarks/summarize-results.sh benchmarks/raw/docker-command-latency.jsonl
```

The output is grouped by experiment/system/scenario and includes trial and
failure counts plus deterministic p50/p90/p99/min/max statistics for the
fields present in the raw rows (including `wall_ns` for command latency and
fractional `wall_ms` for the agent workload). Percentiles use the lower sorted sample
(`floor((n-1)*p)`); retain the JSONL rows for confidence intervals or a
different estimator.

Before using a capture as a production comparison, run the strict qualification
gate:

```sh
bash benchmarks/qualify-results.sh /path/to/captured-results.jsonl
```

It fails on malformed rows, nonzero trial status, duplicate or non-contiguous
trial numbers, mismatched trial sets between systems, or fewer than 30 trials
in any experiment/system/scenario cell. Pass a second argument (or set
`ASP_BENCH_MIN_TRIALS`) for a deliberately smaller local smoke; do not use
that reduced sample as production evidence. For `agent-workload` captures it
also requires finite, non-negative timing, payload/count, persistence,
interface-byte, and client CPU/RSS fields on every row, plus ASP daemon CPU/RSS
and Quinn transport counters on ASP rows. Byte/RSS/count counters must be
integers, so resource cost cannot be silently omitted or fabricated.

For the complete command-latency impairment matrix, pass the explicit
`command-latency` profile:

```sh
bash benchmarks/qualify-results.sh \
  benchmarks/raw/docker-command-latency-2026-08-27-30trials.jsonl 30 \
  command-latency
```

That profile additionally requires all 13 RTT/loss/jitter/bandwidth scenarios
emitted by `docker-benchmark.sh` and the `asp`, `ssh`, and `mosh` systems. The
`agent-workload` profile requires both `asp` and `ssh-controlmaster`; its
scenario list is intentionally supplied by the matrix runner because exact
output and `EXEC_SUMMARY` are separate contracts.

`Dockerfile` and `docker-benchmark.sh` provide a reproducible single-container comparison with ASP, OpenSSH, Mosh, and Linux `netem`. The deployment image is separate: `deploy/container/Dockerfile` ships an immutable exec-only worker wrapper and enables ASP's fail-closed `--production` profile by default; the container's cgroup/read-only/no-new-privileges settings remain the aggregate boundary. The benchmark image pins its Rust base image by digest. It shapes loopback, so configured one-way delay approximates half the named RTT but is not a substitute for two real hosts. The harness defaults to 30 trials per condition; set `ASP_BENCH_REPEATS=1` explicitly for a quick smoke and do not use that output as the production comparison. Run:

```sh
docker build -f benchmarks/Dockerfile -t asp-bench .
docker run --rm --cap-add NET_ADMIN -v "$PWD/benchmarks/raw:/results" \
  asp-bench /results/docker-command-latency-2026-08-26.jsonl
```

Run the coding-agent workload at approximately 100 ms RTT (50 ms loopback
delay). It deliberately includes a 30-second disconnect:

```sh
docker run --rm --cap-add NET_ADMIN --entrypoint /usr/local/bin/asp-agent-workload \
  -v "$PWD/benchmarks/raw:/results" asp-bench \
  /results/docker-agent-workload-postcard-2026-08-26.jsonl
```

Set `ASP_AGENT_SUMMARY=1` (and optionally
`ASP_AGENT_SUMMARY_TAIL_BYTES=8192`) for a second run that uses
`EXEC_SUMMARY` for command output. Keep that capture in a separate file and
compare it with the exact-output capture using:

```sh
bash benchmarks/compare-agent-contracts.sh \
  benchmarks/raw/agent-workload-exact.jsonl \
  benchmarks/raw/agent-workload-summary.jsonl
```

The helper re-runs the strict qualification gate, pairs ASP rows by scenario
and trial, and emits JSON with p50/p90/p99 application-payload and interface
byte deltas/reduction ratios. Pass `1` as a third argument for a local smoke;
the default 30-trial requirement remains the production evidence gate. The
full log remains durable and can be retrieved by range afterward. When the
rows include a migration/sleep hook, the helper also requires both captures to
use the same event kind and every ASP row to report successful completion. This
is a semantic-contract comparison, not a claim that exact output can be
compressed below the requested bytes.

For a publication-quality agent comparison, run the fixture in fresh
containers with trial IDs and atomic aggregation:

```sh
bash benchmarks/docker-agent-matrix.sh \
  benchmarks/raw/agent-workload-30trials.jsonl 30
bash benchmarks/qualify-results.sh \
  benchmarks/raw/agent-workload-30trials.jsonl
bash benchmarks/summarize-results.sh \
  benchmarks/raw/agent-workload-30trials.jsonl
```

Each trial starts a new container and records one ASP row and one warm-SSH
ControlMaster row with the same `trial` number. Set
`ASP_AGENT_SUMMARY=1` for a separate summary-mode capture; do not combine
exact-output and summary-mode rows in one qualification file because they are
different application contracts. The matrix stages the complete JSONL and
renames it only after every trial succeeds, so an interrupted run cannot look
like a complete comparison.

The agent matrix accepts the same Linux `netem` knobs as the command harness:
`ASP_AGENT_DELAY_MS` (one-way delay), `ASP_AGENT_JITTER_MS`,
`ASP_AGENT_LOSS_PERCENT`, and `ASP_AGENT_RATE_MBIT`, plus
`ASP_AGENT_DISCONNECT_SECONDS`. The `ASP_AGENT_LOG_MODE` fixture dimension is
`compressible` (10 MiB of zero bytes, the historical default), `incompressible`
(10 MiB from `/dev/urandom`), or `mixed` (5 MiB of each). Use the latter two
when measuring transport bytes; an all-zero log is intentionally compression-
friendly and must not be treated as representative of arbitrary test output.
For example, a 100 ms approximate RTT cell
with 5% loss is:

```sh
ASP_AGENT_DELAY_MS=50 ASP_AGENT_LOSS_PERCENT=5 \
  bash benchmarks/docker-agent-matrix.sh benchmarks/raw/agent-rtt100-loss5.jsonl 30
```

An incompressible capture can be collected with:

```sh
ASP_AGENT_LOG_MODE=incompressible \
  bash benchmarks/docker-agent-matrix.sh \
    benchmarks/raw/agent-rtt100-incompressible.jsonl 30
```

Run each RTT/loss/jitter/bandwidth cell into a distinct output file (or merge
only after qualification); the scenario string records all four shaping
parameters, so the summarizer and gate keep cells separate.

For the required independently managed-host run, use
`benchmarks/two-host-agent-matrix.sh`. The control machine only orchestrates;
the client host runs `two-host-agent-worker.sh`, and the server host owns the
daemon and workspaces. The server must already be running a production-shaped
`aspd` with a loopback `/metrics` endpoint, and the client must already have
the release `asp` binary, pinned certificate, bearer token (or mTLS files),
and an SSH key for the server. The runner never disables SSH host-key
checking, provisions credentials, or restarts the daemon.

Example (paths after `--client-*` are on the client host):

```sh
bash benchmarks/two-host-agent-matrix.sh \
  --output benchmarks/raw/agent-two-host-rtt100-loss1.jsonl \
  --client bench-client --server asp-server \
  --endpoint 100.64.0.2:4433 --server-root /srv/asp/workspace \
  --client-asp /usr/local/bin/asp \
  --client-cert /home/bench/.config/asp/server-cert.der \
  --client-auth-token /home/bench/.config/asp/auth-token \
  --client-ssh-key /home/bench/.ssh/id_ed25519 \
  --client-interface eth0 --server-interface eth0 \
  --delay-ms 50 --loss-percent 1 --rate-mbit 100 \
  --disconnect-seconds 30 --trials 30 --pcap-dir "$PWD/benchmarks/pcap" \
  --network-event-hook /usr/local/bin/asp-network-event \
  --network-event-kind migration --network-event-delay 5
```

`--server-interface` applies the same netem tuple on the server; omit it when
the operator intentionally wants one-sided shaping and record that choice.
`--server-tc-sudo` uses non-interactive `sudo -n tc`. `--pcap-dir` fetches one
client-side UDP capture per trial; capture the server side separately when the
release report needs both directions. `--summary` creates a separate
`EXEC_SUMMARY` contract capture and must not be merged with exact-output rows.
For a real address migration or laptop sleep/wake, add an operator-owned
`--network-event-hook` and choose `--network-event-kind migration` or
`sleep-wake` (use `custom` for another explicitly documented transition).
The executable runs on the client host once in each paired ASP/SSH leg with
`ASP_NETWORK_EVENT_KIND`, `ASP_NETWORK_EVENT_SYSTEM`,
`ASP_NETWORK_EVENT_TRIAL`, `ASP_NETWORK_EVENT_RUN_ID`,
`ASP_NETWORK_EVENT_ENDPOINT`, `ASP_NETWORK_EVENT_INTERFACE`, and
`ASP_NETWORK_EVENT_SERVER_INTERFACE` in its environment. It must perform the
operator-approved transition, restore connectivity, and exit only when the
event is complete; a non-zero exit fails the trial. The worker rejects
symlinks and group/world-writable hooks. Rows record the event kind,
completion, and duration. Without a hook the rows use `network_event_kind:
none` with null completion/duration and are not migration or sleep evidence.
The runner writes a sidecar `<output>.meta.json` containing the run ID, shaping
tuple, endpoint, and client/server binary and kernel versions, then stages the
JSONL and invokes `qualify-results.sh` before publishing, so an interrupted or
unpaired trial cannot replace a previous capture. Run
`bash benchmarks/smoke-two-host-contract.sh` for the no-network CI contract
check; its dry-run manifest is not benchmark evidence.

For the complete Cartesian RTT/loss/jitter/rate qualification, use
`benchmarks/two-host-agent-grid.sh`. It invokes the split-host runner once per
cell (default: 5 RTT values × 4 loss values × 3 jitter values × 3 rates = 180
cells), annotates every row with nominal and configured shaping values, and
re-runs the strict qualifier over the combined capture before atomically
publishing it. A 30-trial grid is intentionally a long-running release gate;
start with `--dry-run` to inspect the cell count, use `--max-cells` to prevent
an accidental combinatorial run, and keep exact-output and `--summary` grids
in separate files. For a run that must survive a terminal/host interruption,
pass an empty operator-owned `--checkpoint-dir`; each qualified cell is copied
and marked with SHA-256 digests for its JSONL and metadata sidecar only after
its own qualifier passes. Re-run with the same arguments and `--resume` (the
run ID is recovered from the manifest when omitted). Marker digests, the
manifest, host-version provenance, exact shaping scenario, and completed cell
captures are validated before reuse; mismatched shaping, credentials, hosts,
or output paths fail closed. A pcap-enabled checkpoint is invalid unless every
row carries a regular pcap below the requested capture directory. The grid
wrapper does not provision hosts, credentials,
traffic-shaping privileges, or supervisors; those remain operator-owned.

The comparison uses one persistent ASP connection and a warm SSH ControlMaster,
then tears each transport down for the reconnect step. Mosh is excluded from the
full workload because it has no structured file or exact resumable-output API;
its applicable terminal startup path remains in the command-latency sweep.

The strict agent-workload qualifier also requires finite non-negative timing,
payload/count, persistence, interface-byte, and client CPU/RSS fields on both
systems, plus ASP daemon CPU/RSS and Quinn transport counters on ASP rows.
The ASP workload JSON includes both logical `application_payload_bytes` and
summed Quinn `quic_tx_bytes`/`quic_rx_bytes` (plus datagrams, loss, congestion,
and path RTT), so later trials can distinguish semantic savings from transport
overhead instead of inferring them from wall time alone. The fixture also
records the ASP daemon's cumulative user/system CPU delta and resident set
size, plus aggregate SSH client CPU/RSS across its command invocations. A
zero daemon value means procfs was unavailable, not that the daemon used no
CPU; child/cgroup accounting still belongs to the supervisor.

The command-latency harness also records Linux `/proc` deltas for the shared
daemon (`aspd_user_cpu_ms`, `aspd_system_cpu_ms`, and post-trial
`aspd_rss_kb`). These make daemon-side work visible when comparing a structured
ASP request with SSH/Mosh; a zero value means procfs was unavailable, not that
the daemon used no resources.

The harness stages output in `/tmp` and copies the completed two-row result to
the requested destination, preventing partial raw files. Its cleanup bounds the
ControlMaster shutdown so a failed trial cannot hold the container open.

For a fast local/CI durability check (without `tc`, SSH, or Tailscale), run
`bash benchmarks/smoke-persistence.sh` after a release build. It starts a
private loopback daemon, spawns a delayed process, terminates only `aspd`,
restarts it, verifies that a second client can bootstrap the durable session
from an explicit UUID/cursor, and then verifies the process log through the
reconnecting client.

The connect-lifecycle check is
`bash benchmarks/smoke-connect-idempotency.sh`. It proves that repeated
`asp connect` calls reuse the saved durable session without creating an
additional journal identity, that `--new` is required for an intentional
replacement, and that no hidden third session is created.

The client error-cleanup check is
`bash benchmarks/smoke-client-failure-cleanup.sh`. It intentionally exits a
short-lived CLI after a successful QUIC connection but before the normal
success-path close, then verifies that the daemon releases the connection
lease promptly instead of waiting for the 15-second idle timeout. This keeps
bursts of failed agent calls from looking like a principal-capacity outage.

The interactive PTY restart check is `bash benchmarks/smoke-pty-reconnect.sh`.
It drives a real `asp shell` through a FIFO, stops and restarts only `aspd`,
and verifies that the tmux-owned shell accepts commands before and after
reconnect. It skips with an explicit message when no supported tmux executable
is installed; tmux is a runtime prerequisite for durable PTYs.

The persistent adapter smoke is `bash benchmarks/smoke-agent.sh`. It exercises
the release JSONL adapter, semantic workspace digest reuse, the no-tree/no-Git
fast paths, automatic cached-base contiguous, equal-length multi-range, and
bounded line-aware length-changing patches, an explicit `file_patch_ranges`
request, and the default per-user cursor location against a real loopback
daemon. Both smoke scripts and the legacy framing smoke
are run in CI; they are correctness gates, not network performance claims.

The warm-agent outage check is
`bash benchmarks/smoke-agent-reconnect.sh`. It keeps one JSONL adapter process
and its input FIFO alive while replacing `aspd` three times, then verifies that
workspace and process-log reads reconnect without replaying the event journal,
and that a following command reconnects, resumes the same durable session, and
returns without an adapter error.

The concurrent-agent smoke is
`bash benchmarks/smoke-concurrent-agents.sh`. It launches independent JSONL
adapters against one workspace, overlaps semantic inspection, bounded EXEC
summaries, and file commits, and verifies every result plus zero request
failures and zero residual frame/response memory. Set
`ASP_CONCURRENT_AGENTS` up to the daemon-advertised per-principal connection
limit for a larger all-success run; inputs above that limit fail early with an
explicit message rather than being mistaken for worker instability. This
remains a bounded contention check rather than a capacity or multi-tenant SLO.

The sustained local regression is
`bash benchmarks/smoke-capacity-soak.sh`. It keeps independent adapters and
their QUIC connections warm for a bounded duration, repeatedly exercises
`ping`, `EXEC_SUMMARY`, semantic reads, and guarded file mutations, then checks
that workers exit without adapter errors and all connection/request/memory
gauges return to zero. Defaults are eight workers for 15 seconds; tune
`ASP_CAPACITY_SOAK_WORKERS`, `ASP_CAPACITY_SOAK_SECONDS`, and
`ASP_CAPACITY_SOAK_INTERVAL_MS` for a local stress run. CI uses a shorter
four-worker profile. The emitted JSON row includes response count, request and
response-byte deltas, and daemon CPU-time delta, making the load signal useful
for comparing releases without claiming a full resource profile. It remains a
leak/contention regression, not a production capacity SLO; retain longer runs with disk/WAL/audit,
cgroup, and supervisor metrics for release qualification.
`ASP_CAPACITY_SOAK_DRAIN_GRACE_SECONDS` (default 60, maximum 600) bounds the
post-duration drain; a watchdog terminates blocked writers/adapters and fails
the run when that deadline is exceeded.

The admission-boundary smoke is
`bash benchmarks/smoke-capacity-rejection.sh`. It holds one authenticated
adapter per principal connection slot, requires the next HELLO to fail with
`principal_connection_limit`, and then closes the holders to verify lease
cleanup. It is a quota regression check, not a sustained capacity SLO.

The concurrent-consumer cursor smoke is
`bash benchmarks/smoke-consumer-cursors.sh`. It attaches two named consumers
to one durable session, verifies a newly named consumer bootstraps from the
legacy cursor, and proves that subsequent cursor writes remain independent.
It is a local cursor-safety gate; when the optional `event_consumer_leases`
capability is available, it also runs a filtered subscriber through the
backlog-boundary marker and verifies that the client advances across hidden
event IDs while persisting server-side ACK retention leases without adding an
RTT to event delivery.

The supervised local-adapter smoke is `bash benchmarks/smoke-agent-socket.sh`.
It starts `asp agent-listen` on a private Unix socket, connects with two
short-lived `asp agent-connect` clients, verifies that the second client
reuses the first client's pooled QUIC connection, checks the same JSONL
operations, and verifies clean SIGTERM socket removal. It is a local
endpoint/lifecycle gate; concurrent clients still use separate connections.

The immutable artifact smoke is `bash benchmarks/smoke-artifacts.sh`. It
uploads a multi-frame SHA-256 object, verifies same-session and same-principal
cross-session hard-link deduplication, full and bounded-range downloads,
daemon-restart recovery, and JSONL agent artifact PUT/GET. It is a
correctness/semantic-bandwidth gate, not a throughput distribution; it also
checks the duplicate upload's admitted request bytes stay below half of the
source object.

The stronger in-flight continuation smoke is
`bash benchmarks/smoke-transfer-restart.sh`. It pauses a client after the
daemon has persisted a nonzero FILE_PUT or artifact prefix, kills and restarts
the daemon, then resumes the same request and byte-compares the result. The
source is bounded random data so an accidental retransmit cannot be hidden by
compression. A one-time scheduler yield follows the resumable handshake, then
every four 64-KiB continuation frames are followed by a ten-millisecond pause;
this avoids overflowing Quinn's bounded out-of-order assembler on a fast
loopback sender while avoiding a per-frame sleep across the whole transfer.
Fresh uploads retain normal QUIC pacing. The smoke is still a single-host
restart test, not evidence for WAN loss or roaming behavior.

Set `ASP_TRANSFER_RESTART_INITIAL_MAX_PROTOCOL_VERSION=16` and
`ASP_TRANSFER_RESTART_RESTARTED_MAX_PROTOCOL_VERSION=17` to run the same
in-flight transfer check across the v16 plain-framing ceiling and the current
v17 envelope. The client remains pinned to v16 for that connection and must
still resume correctly after the v17 daemon replaces it; this validates the
additive framing boundary, not compatibility with an independently built
historical v16 binary.

For a real mixed-release check, set `ASPD_INITIAL_BIN` and
`ASPD_RESTARTED_BIN` to distinct, independently built `aspd` binaries and set
`ASP_BIN` to the client that should remain connected. The harness then runs the
same FILE_PUT and artifact continuation against the first daemon, kills it,
restarts the second binary, and verifies byte-for-byte completion. It also
repeats the daemon replacement during a durable timeout-bound EXEC, proving
that an in-flight command deadline survives the version boundary. The
`smoke-mixed-release.sh` wrapper below runs both upgrade directions from two
verified archives:

```sh
ASP_MIXED_RELEASE_SIZE_MB=8 \
bash benchmarks/smoke-mixed-release.sh \
    /srv/asp/releases/asp-v16-linux.tar.gz \
    /srv/asp/releases/asp-v17-linux.tar.gz \
    /srv/asp/releases/asp-v16-linux.sha256 \
    /srv/asp/releases/asp-v17-linux.sha256
```

Both checksum sidecars are required: the old archive may predate the current
deployment-helper manifest, but the wrapper still requires its explicit digest,
safe tar paths/no links, and executable `asp`/`aspd` before extracting it. A
same-archive run is rejected unless `ASP_MIXED_RELEASE_ALLOW_SAME_ARCHIVE=1`
is explicitly set; it checks harness mechanics only and must not be reported as
historical compatibility evidence. The JSON result marks that mechanics-only
case with `same_archive:true`.

`bash benchmarks/smoke-tls-reload.sh` verifies that a running release daemon
accepts SIGHUP for TLS reload and keeps the last known-good configuration when
the certificate is temporarily absent. It is also run in CI; production
certificate issuance and client pin rollout remain deployment responsibilities.

`bash benchmarks/smoke-mtls-ca-rotation.sh` exercises identity-bound mTLS
against a real QUIC handshake, stages an old/new client-CA overlap, reloads it
with SIGHUP, and verifies the retired CA is rejected. It uses short-lived
OpenSSL credentials on loopback; production PKI issuance and distribution
remain deployment responsibilities.

`bash benchmarks/smoke-backup-restore.sh` exercises the lock-protected
checksummed backup, tamper detection, verification, and force-restore flow
against a temporary workspace. It is a durability drill, not a substitute for
scheduled off-host backups.

`bash benchmarks/smoke-exec-timeout.sh` starts a release daemon with a one-
second attached-EXEC deadline, verifies the process returns exit code 124, and
checks the timeout metric. Detached `SPAWN` remains intentionally long-lived.

`bash benchmarks/smoke-exec-timeout-restart.sh` kills the daemon while an
attached `EXEC` is running, starts it again with the same policy, and verifies
that the recovered process still observes its persisted deadline and returns
exit code 124. This is the crash/restart durability check for EXEC timeouts.

`bash benchmarks/smoke-security-policy.sh` verifies that the daemon rejects
unauthenticated non-loopback listeners and non-loopback health endpoints before
it initializes TLS/state. It is a configuration-safety gate, not a substitute
for firewall, PKI, or worker isolation.

`bash benchmarks/smoke-port-policy.sh` verifies the exact loopback target
allowlist for `PORT_OPEN`: an allowed service is reachable through the QUIC
bridge, an unlisted service is rejected before ASP dials it, and the policy
occupancy/denial metrics are exported. It then replaces only `aspd` and checks
that the local forwarding listener reconnects and can carry a new TCP flow
without restarting `asp forward`. Existing TCP flows remain tied to their
original QUIC stream and may fail across a daemon restart; transparent flow
resume, reverse forwarding, and non-loopback targets remain outside the v0
surface.

`bash benchmarks/smoke-process-launcher.sh` verifies the optional absolute
process-launcher boundary for both `EXEC` and durable `SPAWN`. Its wrapper only
execs the child and is not a sandbox; production deployments should substitute
a reviewed per-workspace supervisor/`bwrap` policy and add
`--require-process-launcher`. It also replaces the launcher after startup and
checks that `/ready` returns 503 before new work can be routed to a drifted
boundary.

`bash benchmarks/smoke-production-policy.sh` verifies the explicit
`--production` profile. It checks that missing authentication, health/metrics,
launcher, privilege, or command-limit controls are rejected before startup,
then launches a complete profile. This is a configuration acceptance test,
not evidence that the supplied no-op wrapper provides isolation.

`bash benchmarks/smoke-storage-headroom.sh` verifies the filesystem circuit
breaker. It starts a private daemon below its configured free-space threshold,
checks that `/live` and authenticated health remain available while `/ready`
returns 503, and verifies that `OPEN_SESSION` is rejected with the stable
`storage_headroom` code before durable state is created.

`bash benchmarks/smoke-reconnect-chaos.sh` repeatedly SIGKILLs and restarts
the daemon while a detached process is writing output, then checks every
durable marker and its exact count. It is a short abrupt-failure drill; the
30-second persistence smoke remains the longer disconnect qualification.

`bash benchmarks/smoke-session-admin.sh` exercises the lock-protected local
`aspd --list-sessions`/`--delete-session` lifecycle commands. Deletion refuses
running processes and persisted PTYs, then succeeds only after the session is
quiescent and the daemon has been restarted.

`bash benchmarks/smoke-container.sh` builds `deploy/container/Dockerfile` and
runs the image with a read-only root, dropped capabilities, and a writable
workspace volume. It verifies generated credentials, authenticated `EXEC`,
detached `SPAWN`, and durable log retrieval. Linux CI runs this smoke; a local
Docker engine is required.
cgroup, and supervisor metrics for release qualification.
