# Benchmarks

## Status

This document separates completed prototype checks from a publication-quality multi-trial, two-host benchmark. The current host is macOS, so Linux `tc netem`, OpenSSH, Mosh, and ASP were run in a reproducible Docker container with `NET_ADMIN`. A fresh command-latency matrix and agent-workload cell now have 30 paired trials per condition in one Linux container. Those captures are useful release-readiness evidence, but they are not a substitute for independently managed hosts, physical roaming, or confidence intervals.

Raw captured results live in `benchmarks/raw/`; harness scripts live beside them.
Use `bash benchmarks/summarize-results.sh RESULTS.jsonl` to produce grouped
failure counts and p50/p90/p99/min/max summaries while preserving the raw
rows. The summarizer uses the lower sorted sample
(`floor((n-1)*p)`); use the retained JSONL for confidence intervals or another
estimator.
Run `bash benchmarks/qualify-results.sh RESULTS.jsonl` before treating a
capture as release evidence. The qualification gate rejects malformed rows,
nonzero trial status, duplicate trial numbers, and cells with fewer than 30
trials (or an explicitly supplied lower local-smoke minimum).
For `agent-workload` captures it also requires finite, non-negative timing,
payload/count, persistence, interface-byte, and client CPU/RSS fields on every
row, plus ASP daemon CPU/RSS and Quinn transport counters on ASP rows.
Byte/RSS/count counters must be integers; this prevents a production comparison
from silently omitting or fabricating resource cost.
The release correctness checks are `benchmarks/smoke-persistence.sh`,
`benchmarks/smoke-pty-reconnect.sh`,
`benchmarks/smoke-agent.sh`, `benchmarks/smoke-concurrent-agents.sh`,
`benchmarks/smoke-capacity-rejection.sh`,
`benchmarks/smoke-capacity-soak.sh`,
`benchmarks/smoke-file-events.sh`,
`benchmarks/smoke-tls-reload.sh`,
`benchmarks/smoke-mtls-ca-rotation.sh`, `benchmarks/smoke-backup-restore.sh`,
`benchmarks/smoke-exec-timeout.sh`,
`benchmarks/smoke-exec-timeout-restart.sh`,
`benchmarks/smoke-security-policy.sh`,
`benchmarks/smoke-port-policy.sh`,
`benchmarks/smoke-production-policy.sh`,
`benchmarks/smoke-storage-headroom.sh`,
`benchmarks/smoke-process-launcher.sh`,
`benchmarks/smoke-reconnect-chaos.sh`, `benchmarks/smoke-legacy.sh`,
`benchmarks/smoke-artifacts.sh`, `benchmarks/smoke-transfer-restart.sh`,
`benchmarks/smoke-agent-socket.sh`, `benchmarks/smoke-packaged-runtime.sh`,
`benchmarks/smoke-upgrade-release.sh`, `benchmarks/smoke-mixed-release.sh`,
`benchmarks/smoke-protocol-fuzz.sh`,
and the Linux-only
`benchmarks/smoke-container.sh`; they cover persistence,
the persistent JSONL/digest and immutable-artifact paths, frame compression,
fail-closed
TLS/client-CA reload, checksummed state recovery, and the shipped container
image without claiming a WAN speedup. The file-events smoke additionally
mutates a workspace file outside ASP and requires one deduplicated durable
`FILE_CHANGED` event, proving the agent feed notices editor/build changes.
The PTY reconnect check drives a real
tmux-backed shell through a daemon restart; it skips explicitly when tmux is
not installed.

### Userspace impairment proxy

Linux runs can use `netem.sh` or the Docker harness. On macOS, where those
tools are not normally available, the release benchmark binary also includes
an explicit UDP proxy:

```sh
mise exec -- cargo run --locked --release -p asp-bench -- udp-proxy \
  --listen 127.0.0.1:5443 --target 127.0.0.1:4433 \
  --delay-ms 50 --jitter-ms 20 --loss-percent 1 --rate-mbit 10
```

The proxy applies one-way delay/jitter/loss and a per-direction serialization
rate, remembers the most recent client source (including a source-port
rebind), and bounds queued datagrams to 1,024 packets/32 MiB. Its pseudo-random
draws are deterministic so a local regression can be repeated. The process
prints a listening JSON record and a stop summary on Ctrl-C. It intentionally
has no authentication, NAT traversal, or relay behavior; it is benchmark
tooling only. The async loopback forwarding test and shaping unit tests run in
the normal `asp-bench` release test suite. The end-to-end
`benchmarks/smoke-udp-proxy.sh` additionally carries a real authenticated
`asp doctor` request through the proxy and keeps a JSONL agent alive across a
17-second proxy outage/restart. The outage is longer than ASP's configured
15-second QUIC idle bound, so the follow-up EXEC must redial and reuse the
durable session. Results collected through this proxy are still single-host
evidence and cannot satisfy the two-host release gate or a physical
roaming/SLO claim.

The legacy framing smoke first speaks v16 plain length-prefixed frames to a v17
daemon, then pins a fixture daemon to v16 and runs the current client against
it. It also carries a durable session/process through a v16-compatible daemon
restart into v17 and recovers the resulting status/log. This exercises both
directions of the rolling-upgrade handshake and durable state boundary. A
mixed-release binary matrix is still required before publishing a compatibility
SLO.

## Completed measurements

### Quinn transport smoke experiment

Command:

```sh
mise exec -- cargo run -q -p asp-bench -- quinn-smoke
```

The in-process test performs a TLS/QUIC handshake, reliable stream echo, DATAGRAM echo, client UDP socket rebind (exercising QUIC path migration), another stream echo, and reads Quinn UDP statistics.

Observed on this host, debug build, loopback, 2026-08-26:

| Metric | Value |
|---|---:|
| handshake | 6,102 µs |
| reliable stream RTT | 870 µs |
| DATAGRAM RTT | 344 µs |
| first stream RTT after rebind | 1,104 µs |
| client UDP datagrams tx/rx | 15 / 15 |

One sample is an implementation check, not a stable performance distribution.

A later optimized release run on the same host measured 2,203 µs handshake,
183 µs stream RTT, 70 µs DATAGRAM RTT, and 243 µs after UDP rebind. The five
retained release samples in the raw file have medians of 1,572 µs handshake,
202 µs stream RTT, 82 µs DATAGRAM RTT, and 205 µs after rebind, with handshakes
spanning 1,262–1,787 µs. These are loopback smoke samples, not a
network-latency claim; the small-sample DATAGRAM variance is expected in an
in-process loopback test.

After aligning `asp-bench` with the shared ASP transport profile, one fresh
release sample measured 1,349 µs handshake, 202 µs stream RTT, 71 µs
DATAGRAM RTT, and 193 µs after rebind. The raw row is retained in
`benchmarks/raw/quinn-smoke-2026-08-27.jsonl`; it is a profile-regression
check, not a distribution or a WAN performance claim.

The fifth release sample after the WAL/accounting hardening measured 1,787 µs
handshake, 181 µs stream RTT, 79 µs DATAGRAM RTT, and 205 µs after rebind; it
is appended to the same raw file. The variation is expected for an in-process
loopback smoke and does not change the qualification requirement for two-host
trials.

A fresh optimized run on 2026-08-28 measured 1,278 µs handshake, 321 µs stream
RTT, 87 µs DATAGRAM RTT, and 197 µs after rebind. Its raw row is retained in
`benchmarks/raw/quinn-smoke-2026-08-28.jsonl`; it is another transport
regression sample, not a WAN performance claim.

### Stream-frame compression experiment

Command:

```sh
mise exec -- cargo run --locked --release -p asp-bench -- frame-compression
```

The current release measured these deterministic 1 MiB samples:

| Input | Input bytes | Wire bytes | Wire/input |
|---|---:|---:|---:|
| repetitive `x` | 1,048,576 | 4,829 | 0.46% |
| xorshift pseudo-random | 1,048,576 | 1,048,583 | 100.0007% |

The second row confirms that incompressible data pays only the seven-byte
envelope and does not get sent through a larger compressed representation.
This is a codec regression check, not a source-repository distribution or a
claim about end-to-end QUIC bandwidth. The required two-host agent workload
must still record interface bytes and CPU for real files/logs.

The protocol test suite also exercises a deterministic malformed-frame corpus
across Postcard messages, stream envelopes, and both PTY datagram forms. The
zlib path specifically rejects a tiny body that advertises the 128 MiB logical
limit without eagerly reserving that entire amount. This is a bounded CI
regression, not a substitute for time-limited cargo-fuzz runs or an independent
security review. The release binary additionally exposes
`asp-bench protocol-fuzz`, which sends 10,000 deterministic mutations (10
decoder paths per input, 100,000 calls total by default) through the same
boundary under `benchmarks/smoke-protocol-fuzz.sh`. A seed and input-size cap
are printed in the JSON result so a failure can be reproduced. The smoke is a
fast panic/limit regression and remains intentionally distinct from a
coverage-guided or independently reviewed fuzzing campaign.

In a live daemon, compare `asp_response_frame_logical_bytes_total` with
`asp_response_frame_encoded_bytes_total`, alongside the compressed/plain frame
counts. This separates application-level codec savings from QUIC/IP overhead
and makes it possible to detect a workload where compression CPU is not paying
for itself.

### Persistence checks

Manual end-to-end procedure against `aspd` on loopback:

1. created session;
2. SPAWNed `sleep 30; printf thirty-second-resume-ok`;
3. SPAWN client exited immediately;
4. waited 31 seconds;
5. new client sent RESUME with saved event cursor;
6. received the missing output bytes and `PROCESS_EXITED(code=0)`; process snapshot reported `running=false`.

Result: pass. A separate 2-second version also passed. The Rust test suite and a manual restart test now also cover daemon-crash/restart persistence: `aspd` was stopped while a `sleep 5; printf restart-ok` child was running, restarted on the same workspace, and `RESUME` returned the output and exit event. A tmux-backed PTY was similarly reattached after daemon restart. These are correctness checks, not latency distributions.

### File checks

- whole-file PUT returned version 1 and SHA-256;
- hash-guarded prefix/suffix PATCH returned version 2 and the expected new SHA-256;
- unit test verifies the patch isolates a middle edit;
- path traversal rejection is unit tested.

The deterministic `asp-bench file-sync` experiment serializes the exact v17
frames for five edit shapes and compares full `FILE_PUT`, contiguous
`FILE_PATCH`, and the negotiated `FILE_PATCH_RANGES` form. The raw row is
`benchmarks/raw/file-sync-wire-2026-08-28.jsonl`:

| Fixture | Full | Contiguous patch | Range count | Ranges | Current policy |
|---|---:|---:|---:|---:|---|
| localized source edit | 647 B | 131 B | 1 | 131 B | PATCH |
| three scattered source edits (length-changing) | 2,686 B | 2,558 B | 3 | 228 B | RANGES |
| three scattered source edits (equal-length) | 571 B | 489 B | 3 | 162 B | RANGES |
| compressible broad rewrite | 680 B | 679 B | 1 | 683 B | PUT |
| incompressible middle rewrite | 98,769 B | 230 B | 1 | 232 B | PATCH |

These are encoded request sizes, including the stream envelope and length
prefix, not network measurements. The equal-length scattered case saves 409 B
versus a full PUT, while the bounded line-aware matcher saves 2,458 B for the
length-changing source case. Both use the capability-gated range path; edits
that exceed the matcher budget still fall back to a contiguous replacement.
The broad rewrite demonstrates why the client retains its conservative PUT
fallback. A shaped, multi-trial end-to-end file workload remains required
before changing the threshold or claiming a general bandwidth improvement.

### Large-output integrity

An automated server test executes `yes x | head -c 10485760`, consumes structured stdout chunks, and then independently re-reads the retained journal from the process-start cursor. Both the live stream and retained replay contain exactly 10,485,760 bytes and the process exits zero. Result: pass. The agent workload below also transfers 10 MiB through `netem` and records interface bytes.

### Hardening and reconnect checks

The live-output monitor has a regression test for shell pipelines: `yes x | head -c 1048576` delivers exactly 1,048,576 bytes before its `PROCESS_EXITED` response, even though the wrapper's exit marker can race inherited pipeline descriptors. The test is a correctness guard, not a throughput measurement.

The release hardening suite also verifies that a queued process-output response
holds its aggregate-memory permit until the response is consumed, preventing a
slow reader from creating an uncharged second copy in the per-process channel.
If the shared budget remains exhausted, the monitor detaches that live response
attachment after a bounded 250 ms wait; the durable cursor remains the recovery
path rather than allowing a persistent log writer to stall forever.

The release build was smoke-tested with Quinn stream/DATAGRAM echo and client UDP rebind. An authenticated `asp doctor` call reported protocol version 17, session/process counts, active connections, event-log bytes, uptime, request/failure counters, and the tmux PTY backend. A forced daemon-kill test during `printf first; sleep 2; printf second` reconnected with the same EXEC request ID and emitted `firstsecond` exactly once; the persisted event log contained one process start, two output chunks, and one exit. High-rate journal frames now use a bounded 256 KiB/25 ms group-commit window while lifecycle/file events remain synchronous, reducing unnecessary `fsync` calls without changing the recovery contract for persistent process logs. Synchronous append/fsync work also yields the normal multi-thread Tokio worker through `block_in_place`, while synchronous startup and current-thread tests retain the direct path. PTY screen-generation markers are additionally coalesced to one event per 100 ms, avoiding a WAL transaction for every terminal output chunk while exact PTY bytes remain immediate. Version-17 stream frames use a bounded zlib envelope for large compressible messages and keep incompressible payloads plain; response/request byte counters measure the actual encoded wire bytes. Resume recovery now uses bounded stream frames and a release end-to-end test. A release subscription smoke followed a live EXEC and received `PROCESS_STARTED` and `PROCESS_EXITED` without polling; `--no-output` avoided forwarding the command's byte stream. A second smoke killed/restarted `aspd` between two EXECs; `asp events` automatically reconnected and observed both process starts. The release `EXEC_SUMMARY` smoke ran a 10 MiB-output command and emitted an 8 KiB bounded tail, demonstrating the no-full-transcript request contract; a release `asp logs` smoke also fetched an exact persisted stdout range after the process exited. The artifact smoke uploads a multi-frame immutable object, verifies full and range downloads, restarts the daemon, and exercises JSONL agent PUT/GET; the server unit suite additionally verifies retention tombstoning cannot resurrect metadata. `benchmarks/compare-agent-contracts.sh` performs the paired exact-vs-summary interface-byte comparison after each capture has passed the strict qualification gate.
Durable replay has a separate regression: when a WAL tail exceeds 100,000 events or 64 MiB of estimated event data, the server returns the current snapshot with `compacted=true` without collecting the remaining history or truncating the active log. Live replay validates the WAL but does not rebuild the startup process/request/artifact maps, so the bounded tail is also a bounded temporary state allocation. This protects reconnect memory on a busy, not-yet-compacted session; it is a bounded correctness check rather than a substitute for a retention/backup policy.
The `/metrics` counter `asp_resume_replay_limited_total` makes this fallback
operationally distinguishable from ordinary journal compaction, so a benchmark
or alert can detect when consumers are routinely too far behind.

The durable EXEC/SPAWN and tmux-backed PTY launch paths now include private
intent/log preparation, metadata checks, fsyncs, terminal setup, and fork/exec
inside the same multi-thread Tokio
blocking handoff as WAL work. This keeps a slow disk or saturated process
table from pinning the QUIC reactor while the session commit boundary remains
held; the single-worker regression test verifies a timer remains runnable
during the operation.
Resumable file/artifact staging checks and cleanup use that handoff as well;
streaming body writes remain asynchronous, and storage headroom is sampled
before upload plus at durable 1 MiB boundaries rather than on every 64 KiB
chunk. The 64 MiB restart-transfer smoke passes for both file and artifact
uploads without Quinn receive-buffer gaps.
The optional rich-PTY DATAGRAM path has a standalone MTU smoke: a deterministic 7,777-byte ANSI redraw encoded as a 348-byte `PZ`/`AF` payload for a 1,200-byte budget and decoded byte-for-byte. This demonstrates that an oversized replaceable screen can fit without retransmitting stale intermediate state; it does not substitute for a two-host PTY latency/loss measurement. Synchronous portable-pty input writes are serialized per PTY, offloaded to the blocking pool, and bounded by a timeout, so PTY backpressure does not stall the QUIC reactor or multiply blocked tasks during reconnect storms. Raw output is `benchmarks/raw/pty-rich-datagram-compression-2026-08-28.jsonl`.

The plain PTY state-delta codec smoke is `mise exec -- cargo run --locked
--release -p asp-bench -- pty-state-delta`. On a deterministic 80x120 screen,
the complete plain snapshot encoded to 5,703 bytes, while a cursor-only update
was 26 bytes and a one-row update was 99 bytes. An 80-row rewrite was 10,506
bytes, so the sender correctly selected the full snapshot. These are codec
measurements, not WAN latency claims; the periodic full checkpoint and
loss/reorder behavior still require the two-host impairment matrix. Raw output
is `benchmarks/raw/pty-state-delta-2026-08-28.jsonl`.

The dedicated event-subscription reconnect smoke keeps `asp events` alive while
the daemon is replaced, then verifies that one pre-restart and one post-restart
lifecycle event arrive exactly once. It also exercises the warm bearer-token
endpoint path used to retain the UDP socket and TLS session cache across the
reconnect.
Response streams now use Quinn's native priority scheduler (PTY, then bounded
summary/control, then bulk logs/files/workspace state); large legacy PUT/PATCH
requests also move to the bulk class. This is a transport-level contention
optimization and still needs to be measured in the shaped two-host matrix.

The release agent smoke now exercises both JSONL `tail_bytes` and CLI
`asp logs --tail`, verifying that only a bounded final suffix is transferred
after a process exits. `benchmarks/compare-agent-contracts.sh` compares
qualified exact and summary captures by paired trial and reports application
and interface-byte reductions without conflating the two contracts.

The warm-agent reconnect smoke keeps one JSONL adapter process alive while the
daemon is replaced three times, then verifies that point-in-time workspace and
process-log reads reconnect without replaying the event journal. It also kills
the daemon after an `EXEC_SUMMARY` process is admitted but before its final
response, then verifies the stable request ID replays exactly one durable
result after a direct HELLO retry; the resulting read/log/side-effect resume
deltas are emitted as a machine-readable row. The latest local row is
`benchmarks/raw/agent-reconnect-direct-retry-2026-08-29.jsonl`. This is a
reliability check, not a remote latency claim.

The release concurrent-agent smoke launches independent JSONL adapters against
one shared workspace and overlaps semantic inspection, bounded EXEC summaries,
and file commits. It verifies each response, zero request failures, and zero
residual frame/response memory. Local runs pass at the daemon-advertised
32-connection per-principal ceiling; the harness rejects larger all-success
inputs with an explicit capacity message. This is a bounded contention
regression, not a capacity SLO or an intentional-rejection test.

The release capacity-rejection smoke fills that same 32-connection
per-principal ceiling with held JSONL adapters, verifies that the next HELLO is
rejected with `principal_connection_limit`, then closes the holders and checks
that active connection leases return to zero. It is an admission and cleanup
regression check; it does not establish a sustained multi-agent capacity SLO.

The bounded sustained-load check is `bash benchmarks/smoke-capacity-soak.sh`.
It keeps independent JSONL adapters warm for a configurable interval while
repeating `ping`, `EXEC_SUMMARY`, no-tree/no-Git workspace reads, and guarded
file mutations. It verifies that every worker closes cleanly, that no adapter
errors occur, and that active connections, request streams, decoded-frame
memory, and encoded-response memory all return to zero. The default is eight
workers for 15 seconds; CI uses a shorter four-worker run. The command emits a
JSON row with worker count, duration, response count, request/response-byte
deltas, and daemon CPU-time delta. This is a bounded leak/contention
regression, not a capacity SLO: production still needs
longer soaks, independent workspaces, disk/WAL/audit exhaustion, and
supervisor/cgroup measurements.

The soak has a bounded drain phase: `ASP_CAPACITY_SOAK_DRAIN_GRACE_SECONDS`
defaults to 60 seconds (1--600 is accepted). If writers or adapters remain
backlogged beyond `duration + drain_grace`, the watchdog terminates them and
the run fails. This keeps an overloaded local harness from turning into an
unbounded process tree; a passing short soak is still not a capacity SLO.

An extended local run on 2026-08-28 kept eight workers active for 60 seconds
at a 100 ms request interval. It completed 5,116 adapter responses with zero
worker errors and returned all connection, request-stream, decoded-frame, and
encoded-response gauges to zero. The raw result is retained in
`benchmarks/raw/capacity-soak-2026-08-28-final.jsonl`; this is stronger leak
evidence than the CI smoke, but it remains single-host evidence rather than a
capacity SLO or multi-tenant isolation qualification.

A release loopback smoke for the new CLI fast path ran five `true` commands as five separate invocations in 301.8 ms versus `asp batch` over one connection in 262.7 ms (1.15x for this single trial). This is deliberately not a latency claim: process startup dominates loopback, and the expected benefit grows with RTT and a warm agent process. The proper comparison remains the multi-trial matrix below.

The CLI now also exposes an explicit concurrent batch path for independent
status/check commands: `asp batch --parallel N --summary --tail-bytes 0` opens
up to `N` idempotent EXEC requests concurrently over one authenticated QUIC
connection (bounded to 32) and emits input-ordered exit markers. It suppresses
command output by contract, so output-dependent scripts remain sequential.
This removes serialized application gates when an agent has genuinely
independent checks; measure it with the same paired two-host fixture before
turning it into a workload-wide claim.

The 2026-08-28 local capacity soak also ran 16 warm adapters for 30 seconds
with a 200-ms command interval. It completed 5,432 responses with zero request
failures and zero residual connection/request/frame/response memory, but the
queued `EXEC_SUMMARY` process launches drained over 127 seconds and consumed
9.18 seconds of daemon CPU. The raw row is
`raw/capacity-soak-2026-08-28-16x30.jsonl`; this is useful contention evidence,
not a production SLO or a two-host capacity result.

After moving `EXEC_SUMMARY` tail accumulation into the process monitor (so
summary attachments no longer queue every durable output chunk), the same
rebuilt 16-worker/30-second profile completed 5,474 responses with zero request
failures and zero residual connection/request/frame/response memory. Its
coarse wall-clock drain fell to 120 seconds; the raw row is
`raw/capacity-soak-2026-08-28-summary-fastpath.jsonl`. This is a local
contention comparison, not a capacity SLO: process launch and synchronous
lifecycle durability remain the next bottleneck to profile and qualify.

With the command-metadata and committed-intent cleanup fsyncs removed from the
short-lived launch path, a second rebuilt run completed the same 5,474 responses
in a coarse 116 seconds and used 8.86 seconds of daemon CPU. The raw row is
`raw/capacity-soak-2026-08-28-summary-fastpath-relaxed.jsonl`. Treat the
directional change as evidence for fewer durability barriers, not as a claimed
percentage improvement: these local runs were not randomized or isolated from
filesystem variance.

The daemon now exports `asp_process_launch_duration_us` (fixed buckets, count,
and sum) plus `asp_process_launch_failures_total` on `/metrics`. The histogram
covers admitted durable preparation and spawn/bookkeeping, excluding response
draining and child lifetime; future capacity and WAN captures should record it
alongside request latency to identify whether launch contention or transport is
the limiting phase.

A rebuilt 8-worker/15-second local run recorded 2,740 adapter responses, 454
process launches, zero launch failures, 39.27 seconds of cumulative launch
time (86.5 ms per launch), 4.00 seconds of daemon CPU, and zero residual
resource gauges. The raw JSONL row is
`raw/capacity-soak-2026-08-29-launch-metrics.jsonl`; this is a telemetry
regression check, not a cross-host performance claim.

The short-command launch path now creates and syncs one exact process-wrapper
template per session and hard-links it into each process record. Two rebuilt
8-worker/15-second captures recorded 456 launches in 32.72 seconds (71.8
ms/launch) and 453 launches in 30.97 seconds (68.4 ms/launch), with no launch
failures and no residual resources. The paired raw rows are retained in
`raw/capacity-soak-2026-08-29-wrapper-reuse.jsonl`. Relative to the earlier
86.5 ms/launch capture, this is directionally faster, but the runs are not
controlled host-isolated experiments and must not be promoted as a WAN SLO.

The follow-up launch transaction removes four redundant parent-directory
`fsync` calls: each pending intent/metadata write already uses
`write_atomic_file`, which syncs the renamed file and its parent before it
returns. A paired 8-worker/15-second run kept 2,734 responses, 453 launches,
zero launch failures, and zero residual resources while reducing cumulative
launch time from 32.37 s (71.5 ms/launch) to 29.45 s (65.0 ms/launch), about
9% in this local pair. The raw rows are in
`raw/capacity-soak-2026-08-29-atomic-dir-sync.jsonl`; this is a durability-
preserving local signal, not a production SLO or universal speed claim.

The same launch path now creates fresh stdout/stderr logs with exclusive,
0600 descriptor modes, avoiding a follow-up metadata open and `fchmod` while
retaining no-follow and stale-path rejection. The persisted-output test checks
both log modes. A follow-up soak remained clean at 442 launches and 65.4
ms/launch; the small difference from the preceding pair is within local
filesystem variance, so no additional percentage claim is made.

The persistent `asp agent` adapter uses the same one-connection path as
`batch`, but emits structured offset/base64 events for a long-lived caller.
Its local smoke is a protocol/usability check rather than a speed claim; a
credible result needs a persistent agent process under the shaped two-host
matrix and must include the process-start/handshake cost of the competing
adapter.

`benchmarks/smoke-consumer-cursors.sh` is a separate correctness smoke for
concurrent event consumers. It verifies that named consumers can bootstrap
from one legacy session cursor and then advance independent local replay
points; it does not claim that v0 server ACKs retain per-consumer leases.

One release loopback trial sent five `true` requests through the adapter in
295.245 ms versus 747.651 ms for five separate `asp exec` processes (2.53x
lower wall time, with all five exit events observed). A later 20-command
loopback trial measured 1,727 ms through the adapter versus 1,838 ms for
separate clients (1.06x), showing how server process-spawn and local scheduling
can dominate when propagation delay is negligible. These are regression
signals—not a universal warm-path or remote-network latency claim. Raw rows:
`benchmarks/raw/agent-adapter-local-2026-08-26.jsonl`.

After removing attachment progress from transport retries (so short commands
neither fsync the session sidecar nor replay the journal), a fresh three-round
loopback release check measured 1,280–1,350 ms for twenty warm JSONL-agent
`EXEC_SUMMARY true` requests, versus 3,030–3,220 ms for twenty cold CLI
invocations. The fixture is intentionally small and unshaped; it demonstrates
the hot-path cost direction, not a WAN SLO or an isolated causal percentage.
Raw rows: `benchmarks/raw/agent-adapter-local-2026-08-29-attachment-memory.jsonl`.

The release adapter smoke also completed detached `spawn`, resumable durable
`logs`, `file_put`, `file_get`, hash-guarded `file_patch`, and `inspect` on one
connection (with a second attach for the log range), returning the expected
SHA-256 values and base64 file bytes. Its inspect-then-`file_put` case now
asserts the cached-base `transfer:"patch"` path and the exact resulting hash;
uncached or non-beneficial edits remain full PUTs. This is a protocol
integration check, not a shaped-network performance distribution. Large files continue
to use the resumable binary `FILE_*_STREAM` commands rather than the 128 KiB
JSONL request envelope.

A second loopback trial sent five `inspect --read server.log` calls through
five one-shot clients in about 330 ms versus about 80 ms through one warm
adapter (4.1x lower wall time). These `/usr/bin/time` values are coarse,
single-trial process-start measurements; the raw row is retained only as a
warm-path regression signal, not as a remote-network claim.

The current release also ran a two-identical-inspection cache smoke over one
adapter connection. The second request hit the tree and repeated-search
caches; the small Git metadata queries hit twice (status and log), while the
watcher remained healthy. Occupancy was 64 KiB for search results and 128 KiB
for Git results. This validates invalidation/visibility and memory bounds, not
a statistically meaningful latency improvement. The raw row is retained in
`benchmarks/raw/agent-adapter-local-2026-08-26.jsonl`.

A release wire-byte smoke with three files and a `TODO` search measured roughly
469 bytes for the first complete `WORKSPACE_STATE` response and 99 bytes for
the identical digest-validated response (4.7x smaller on the response frame).
The adapter reconstructed the full local JSONL result and reported
`state_unchanged=true`; this is the intended semantic optimization, measured
on loopback in one trial rather than a general bandwidth guarantee.
The daemon now keeps a bounded watcher-invalidated digest index as well, so the
same validator can bypass the server-side scan/Git/file work during the short
freshness window. `benchmarks/smoke-agent.sh` covers this path for correctness;
server CPU/latency savings still require the two-host, multi-trial benchmark.

A current release `agent-workload` smoke on loopback (1-second deliberate
disconnect, no `netem`) completed in 3,073 ms, used 12 application gates and
two QUIC connections, and resumed in 2.75 ms with the persistent process
observed. Its JSON result now includes the per-connection-summed Quinn
transport counters (`quic_{tx,rx}_{datagrams,bytes}`, lost packets,
congestion events, and the last path RTT), alongside the logical
`application_payload_bytes`; these fields make interface overhead and semantic
payload cost separable in the two-host run. The raw row is
`benchmarks/raw/agent-workload-local-2026-08-27.jsonl`; it is a regression
check, not evidence for a real-network speedup.

The latest release validation also covered `aspd --validate-config`, the
platform guard that rejects address-space limits on macOS, and the daemon-wide
workspace commit path after body staging was moved outside the commit gate.
These are correctness/operational checks, not throughput claims.

The current release also completed an authenticated health/file/EXEC smoke,
then stopped and restarted `aspd`; a saved session resumed a detached process
and recovered its output. Successful streamed downloads leave no partial data
sidecars, while an interrupted download retains a locked checkpoint for a
later invocation. These are correctness checks, not a throughput benchmark.

The release daemon now accepts optional inherited command-tree limits: Linux/
Android address-space via `--process-memory-bytes` and Unix CPU time via
`--process-cpu-seconds`. A CPU-limited live smoke completed a normal
`EXEC_SUMMARY`; a nonzero address-space limit is rejected on macOS because the
host does not permit lowering `RLIMIT_AS`, rather than being silently ignored.
The systemd template enables 2 GiB/24 hours and reports both configured limits
through health metrics. These are per-command guardrails, not a replacement
for cgroup/VM enforcement or a benchmark of resource isolation.

A 256 MiB zero-filled upload was intentionally interrupted after a 2 MiB
server-durable prefix, the client process was killed, and `aspd` was restarted.
The second `asp put` reused the locked `<local>.asp-upload` request checkpoint,
received `FILE_UPLOAD_READY` at the staged offset, and completed with a
matching SHA-256; a streamed GET verified all 256 MiB byte-for-byte. This is a
single release smoke, not a resumable-transfer throughput distribution.

The current release artifact smoke uploads a multi-frame object, repeats the
same digest in the original session and a fresh same-principal session to
exercise pre-body hard-link deduplication, verifies complete and ranged
downloads, restarts `aspd`, verifies retrieval again, and exercises JSONL agent
PUT/GET. It is a correctness and semantic-bandwidth check; no throughput
claim is made until duplicate-upload bytes and large-object throughput are
recorded under the two-host matrix. The smoke does enforce that the
same-principal duplicate stays below half of the source object's admitted
request bytes, which catches a regression to full retransmission without
pretending to be a distributional benchmark.

### Linux `netem` command-start sweep

Raw file: `benchmarks/raw/docker-command-latency-2026-08-26.jsonl` (39 rows, all status zero). Image: Rust 1.88/Bookworm, OpenSSH 9.2p1, Mosh 1.4.0. The qdisc shapes loopback; configured `delay` is one-way and the scenario name treats twice that value as approximate RTT. This sweep predates compact Postcard framing, so its ASP byte counters are not measurements of the final encoding; its latency rows remain preliminary transport/startup evidence.

Each measurement starts a client and runs remote `true`. ASP reconnects an already-created session and then EXECs. SSH makes a fresh key-authenticated connection. Mosh performs its normal fresh SSH bootstrap and UDP session startup inside a 24×80 pseudo-terminal. Thus this measures **cold client/startup pathways**, not keystroke latency and not equally warm sessions.

| Condition | ASP ms | SSH ms | Mosh ms |
|---|---:|---:|---:|
| approx. RTT 0, no loss | 7.10 | 177.59 | 505.91 |
| approx. RTT 100, no loss | 338.66 | 1,500.52 | 1,996.26 |
| approx. RTT 300, no loss | 926.95 | 3,909.04 | 4,688.79 |
| RTT 100, loss 5% | 342.32 | 2,531.28 | 2,404.46 |
| RTT 100, loss 10% | 359.64 | 4,234.25 | 2,670.16 |
| RTT 100, jitter 100 ms | 590.38 | 2,100.02 | 2,777.45 |
| RTT 100, 1 Mbps | 389.52 | 1,567.53 | 2,046.88 |
| RTT 300, jitter 100 ms, loss 10%, 1 Mbps | 1,213.93 | 5,474.12 | 8,884.49 |

The ASP result scales at roughly three application/handshake RTT gates in the no-loss sweep. SSH and Mosh cold setup pay substantially more. At 10% loss Mosh starts faster than SSH, consistent with the reason Mosh exists, while ASP/QUIC is fastest for this structured no-output operation. These are single samples; loss outcomes vary substantially and need distributions.

Loopback transmitted-byte counters for the selected conditions ranged roughly 9–17 KiB across all three systems. ASP did **not** demonstrate a consistent bandwidth advantage. Its resumed snapshot grows as process records accumulate, while SSH/Mosh setup has different authentication/bootstrap contents; the counters are diagnostic, not an application-payload comparison.

The result supports continued investigation of structured resume/EXEC, but it does not prove ASP beats a warm SSH ControlMaster, a persistent Mosh/RoSE session, or an agent daemon tunneled through either.

### 30-trial command-latency matrix (Linux container)

Raw file: `benchmarks/raw/docker-command-latency-2026-08-27-30trials.jsonl`.
The strict qualifier passed all 1,170 rows (39 experiment/system/scenario
cells, 30 trials each, zero failures). The complete-matrix profile was used:

```sh
bash benchmarks/qualify-results.sh \
  benchmarks/raw/docker-command-latency-2026-08-27-30trials.jsonl 30 \
  command-latency
```

That profile rejects a capture that omits any of the 13 impairment scenarios or
one of the `asp`, `ssh`, or `mosh` systems. The image was rebuilt from the
current release source on an aarch64 Linux container under OrbStack. `tc netem` shaped
the container loopback; the `rtt-*` labels therefore use twice the configured
one-way delay as an approximate RTT. The rows measure cold client/process
startup for `true`, not keystroke latency or an equally warm Mosh/RoSE session.
Each cell below reports wall time in milliseconds as `p50/p90/p99`; the full
resource and byte statistics remain in the raw JSONL and are reproducible with
`bash benchmarks/summarize-results.sh`.

| Condition | ASP ms | SSH ms | Mosh ms |
|---|---:|---:|---:|
| RTT 0, no loss | 62/67/69 | 167/170/171 | 456/502/533 |
| RTT 20, no loss | 132/139/139 | 514/526/528 | 878/891/896 |
| RTT 100, no loss | 405/414/420 | 1,537/1,572/1,582 | 2,042/2,064/2,070 |
| RTT 200, no loss | 705/716/720 | 2,776/2,800/2,802 | 3,435/3,460/3,466 |
| RTT 300, no loss | 1,010/1,022/1,026 | 3,987/4,020/4,025 | 4,792/4,819/4,823 |
| RTT 100, loss 1% | 402/416/428 | 1,541/1,764/2,567 | 2,057/2,393/2,533 |
| RTT 100, loss 5% | 412/627/703 | 2,043/2,460/3,242 | 2,361/3,457/4,373 |
| RTT 100, loss 10% | 619/1,721/3,157 | 2,197/3,825/8,084 | 3,301/5,059/6,105 |
| RTT 100, jitter 20 ms | 429/463/464 | 1,598/1,680/1,683 | 2,047/2,166/2,210 |
| RTT 100, jitter 100 ms | 653/777/795 | 1,900/2,089/2,180 | 2,661/2,882/2,957 |
| RTT 100, 1 Mbps | 462/470/472 | 1,608/1,634/1,648 | 2,117/2,144/2,146 |
| RTT 100, 10 Mbps | 406/417/422 | 1,550/1,576/1,578 | 2,049/2,079/2,081 |
| RTT 300, jitter 100 ms, loss 10%, 1 Mbps | 1,382/3,431/4,264 | 6,096/8,492/9,198 | 8,341/11,232/12,641 |

ASP is faster in every measured cold-start cell, including the harsh corner
condition, but this is still a same-container comparison. It is evidence that
the structured QUIC/session path removes startup gates under shaping; it does
not establish a universal advantage over warm SSH+agent, Mosh/RoSE terminal
sessions, or an agent daemon tunneled through either. Interface bytes for these
small `true` requests are roughly 10--18 KiB per direction and are not an
application-bandwidth claim.

## Required network matrix

The matrix above is a single-container qualification pass. Repeat on two Linux hosts/containers with `CAP_NET_ADMIN` using `benchmarks/netem.sh`:

- RTT: 0, 20, 100, 200, 300 ms (configure half delay per direction when shaping both ends);
- loss: 0, 1, 5, 10%;
- jitter: 0, 20, 100 ms;
- bandwidth: 1, 10, 100 Mbps;
- abrupt 30-second disconnect, interface/address rebind, and sleep/wake.

For each cell run at least 30 trials after warm-up and report median, p90, p99, confidence intervals, raw rows, exact versions/configuration, and failure count.

The split-host agent runner is `benchmarks/two-host-agent-matrix.sh` with
`benchmarks/two-host-agent-worker.sh`. It executes the same structured
agent/SSH fixture from a client host against a separately managed server host,
reads ASP daemon CPU/RSS from the server's loopback metrics endpoint, can apply
the netem tuple on both egress interfaces, and optionally fetches a client-side
UDP pcap per trial. The server must already be running a production-shaped
`aspd`; the runner never provisions credentials, disables SSH host-key
checking, or restarts the service. Paths passed with `--client-*` are paths on
the client host. The runner stages output and invokes the strict qualifier
before replacing the destination, so incomplete or unpaired trials cannot be
published. A `<results>.meta.json` sidecar records the run ID, shaping tuple,
endpoint, and independently probed client/server binary and kernel versions.
`benchmarks/smoke-two-host-contract.sh` exercises only the argument and
dry-run contract in CI and is not performance evidence.

To make the migration/sleep portion reproducible without pretending that a
container rebind is a laptop roaming event, pass an operator-owned executable
with `--network-event-hook`, select `--network-event-kind migration` or
`sleep-wake` (or `custom`), and optionally set `--network-event-delay`. The
matrix invokes that hook once during the ASP leg and once at the corresponding
point in the SSH leg, waits for it to finish, and records
`network_event_kind`, `network_event_completed`, and
`network_event_duration_ms` in every row and metadata sidecar. The hook is
called on the client host with `ASP_NETWORK_EVENT_KIND`,
`ASP_NETWORK_EVENT_SYSTEM` (`asp` or `ssh-controlmaster`),
`ASP_NETWORK_EVENT_TRIAL`, `ASP_NETWORK_EVENT_RUN_ID`,
`ASP_NETWORK_EVENT_ENDPOINT`, `ASP_NETWORK_EVENT_INTERFACE`, and
`ASP_NETWORK_EVENT_SERVER_INTERFACE`. It must be a regular non-group/world-
writable executable, perform an operator-approved route/interface/sleep
transition, restore connectivity, and exit only after the transition is
complete. A non-zero exit fails the trial. Without a hook, rows explicitly
record `network_event_kind: none` and null completion/duration; they must not
be described as physical migration or sleep/wake evidence.

For a full Cartesian sweep, `benchmarks/two-host-agent-grid.sh` wraps that
runner across the configured RTT/loss/jitter/rate lists (the defaults are
5 × 4 × 3 × 3 = 180 cells). It records grid coordinates and the rounded
one-way delay used for odd RTT targets, then qualifies the combined JSONL a
second time before atomically publishing it with a grid metadata sidecar. The
default 30 trials and 30-second disconnect make this a deliberately
long-running release qualification; inspect the manifest with `--dry-run` and
bound the run with `--max-cells` before starting it. To make that long run
restartable, provide an empty operator-owned `--checkpoint-dir`. Each cell is
qualified and atomically marked with SHA-256 digests for its JSONL and metadata
sidecar before it becomes resumable; a later `--resume` revalidates the marker
digests, manifest, host-version provenance, exact shaping scenario, and cell
capture before skipping it, and reruns incomplete or corrupt cells. A pcap-
enabled checkpoint is invalid unless every row carries a regular pcap below the
requested capture directory. The run ID is recovered
from the checkpoint manifest when `--resume` is used without `--run-id`.
This helper remains an orchestrator: hosts, credentials, Linux `tc`, packet
capture, and supervisor controls must already be provisioned by the operator.

## Metrics and instrumentation

- time to first interaction: process start to usable prompt/accepted request;
- keystroke perceived latency: input capture to authoritative/predicted render;
- command latency: request write to accepted/output/exit separately;
- request SLOs: use ASP's fixed-cardinality
  `asp_request_duration_us_bucket{operation=...,le=...}` plus `_count`/`_sum`
  series to calculate p50/p90/p99 without introducing request-ID labels;
- reconnect: link return/new connection to state reconstructed;
- bytes: interface packet capture plus Quinn/SSH counters, application payload separately; ASP `/metrics` also exposes encoded response-frame and port-forward payload counters plus decoded-request/encoded-response memory occupancy, limits, response-capacity refusals, response encode-gate wait/codec totals, and PTY input-write timeouts;
- transport: ASP `/metrics` exports Quinn UDP datagrams/bytes, lost packets,
  congestion events, and the last path RTT/MTU for each completed attachment;
- CPU/RSS: client, daemon, child, and any Tailscale process; daemon health
  exposes `asp_process_cpu_time_us_total` and peak
  `asp_process_max_rss_bytes`, while child/cgroup values still come from the
  supervisor or host metrics;
- The Linux Docker command-start and agent-workload harnesses additionally
  record cumulative per-trial daemon deltas as `aspd_user_cpu_ms` and
  `aspd_system_cpu_ms`, plus the post-trial `aspd_rss_kb`. The agent fixture
  also records aggregate client CPU/RSS for each system. Missing `/proc` data
  is represented as zero and must not be interpreted as a measurement.
- output integrity: hash/byte count, gaps/duplicates;
- packet loss behavior and maximum control-input latency during 10 MB output.

## Agent workload

Fixture steps:

1. inspect repository tree/metadata;
2. git status;
3. three searches and several file reads;
4. modify three files;
5. run tests;
6. produce/consume 10 MiB output;
7. inspect diff;
8. run follow-up command;
9. disconnect 30 seconds;
10. resume and continue.

The reproducible Docker fixture compares ASP with a warm SSH ControlMaster at approximately 100 ms RTT. ASP uses structured EXEC/FILES and combines tree, git status, three searches, and three reads into one `WORKSPACE_STATE` request. Both modes edit three files, run tests, consume an exact 10 MiB output, inspect the diff, detach during a 30-second process, reconnect, verify its completion, and continue.

Set `ASP_AGENT_SUMMARY=1` for a paired run that uses `EXEC_SUMMARY` with a
bounded tail instead of forwarding the full command transcript. Compare its
`application_payload_bytes` with the exact-output run only as a semantic
tradeoff: both modes retain the complete log durably, but they intentionally
deliver different response contracts.

After both captures pass `benchmarks/qualify-results.sh`, generate a paired
JSON report with:

```sh
bash benchmarks/compare-agent-contracts.sh exact.jsonl summary.jsonl
```

The report matches ASP rows by scenario/trial and includes p50/p90/p99
application-payload and interface-byte deltas plus reduction ratios. It fails
closed when either capture is incomplete, uses the wrong summary flag, or has
different trial IDs. If migration/sleep metadata is present, it also requires
the same event kind and successful completion in both captures. Use a third
argument of `1` only for a local smoke; keep the default 30-trial threshold for
release evidence.

For the required multi-trial agent evidence, use
`benchmarks/docker-agent-matrix.sh RESULTS.jsonl 30`. It starts a fresh
container per trial, injects a positive `trial` number into both the ASP and
warm-SSH rows, stages the complete capture, and renames it only after every
trial has produced exactly one successful row for each system. Run
`benchmarks/qualify-results.sh` before summarizing. Summary mode should be
captured into a separate file because it intentionally transfers only bounded
tails; it is not the same workload contract as exact-output mode.

The matrix passes Linux `tc netem` parameters through
`ASP_AGENT_DELAY_MS`, `ASP_AGENT_JITTER_MS`, `ASP_AGENT_LOSS_PERCENT`, and
`ASP_AGENT_RATE_MBIT`; `ASP_AGENT_DISCONNECT_SECONDS` controls the durable
outage. The scenario label records the complete shaping tuple. Use one output
file per condition and qualify each (or merge only after preserving the
scenario labels) so a 30-trial cell cannot be mistaken for a mixed population.
The `ASP_AGENT_LOG_MODE` dimension is `compressible` (10 MiB of zero bytes,
the historical default), `incompressible` (10 MiB from `/dev/urandom`), or
`mixed` (5 MiB of each). Exact-output byte claims should include an
incompressible or mixed capture; zero-byte output is useful for codec
regression testing but is not representative of arbitrary test logs.

### 30-trial agent workload cell (Linux container)

Raw file: `benchmarks/raw/docker-agent-workload-2026-08-27-rtt100.jsonl`.
The capture passed the strict qualifier with 30 successful ASP rows and 30
successful warm-SSH ControlMaster rows. It used a fresh container per trial,
one-way `tc netem` delay of 50 ms (approximately 100 ms RTT), 100 Mbps, zero
loss, zero jitter, and `disconnect_seconds=0`; it is therefore a performance
cell, not the 30-second sleep/reconnect gate. Percentiles use the lower sorted
sample (`floor((n-1)*p)`).

| Metric (p50; p90/p99 in parentheses) | ASP | SSH ControlMaster | ASP change at p50 |
|---|---:|---:|---:|
| wall time | 3,236 ms (3,269 / 3,285) | 9,484 ms (9,545 / 9,558) | 66.1% lower |
| network-blocked time | 2,212 ms (2,242 / 2,253) | 8,359 ms (8,414 / 8,420) | 73.7% lower |
| reconnect/state recovery | 341 ms (352 / 353) | 1,569 ms (1,592 / 1,594) | 78.7% lower |
| application request gates | 12 (12 / 12) | 18 (18 / 18) | 33.3% fewer |
| interface bytes per direction | 104,996 (106,280 / 106,546) | 10,566,544 (10,569,461 / 10,569,974) | 99.0% lower |
| ASP daemon CPU (user + system) | 90 + 160 ms (110 + 180 / 110 + 180) | not captured | — |
| ASP daemon RSS | 23,108 KiB (24,236 / 25,148) | not captured | — |

The fixture writes/reads a 10 MiB all-`x` log, so the very large interface-byte
reduction is expected from ASP's strict-win v17 compression envelope. It must
not be generalized to random or otherwise incompressible logs: the exact-output
contract still delivers every requested byte. The capture also records
`application_payload_bytes` (ASP p50 10,485,971 B; SSH p50 10,487,585 B) and
`persistent_process_observed=true` for every row. The two systems used two
transport connections per trial; the measured difference is primarily the
structured request gate count, codec behavior, and resumable session path.

This is the first 30-trial cell, not the production matrix. Repeat it on two
independently managed Linux hosts and add the required RTT/loss/jitter/bandwidth,
address-migration, abrupt-disconnect, and sleep/wake conditions before making a
general performance or reliability claim.

### 30-trial exact-vs-summary contract comparison

Raw summary file:
`benchmarks/raw/docker-agent-workload-2026-08-27-rtt100-summary.jsonl`.
It passed the same strict 30-trial-per-system qualifier as the exact-output
capture, with `summary_output=true`, an 8 KiB tail, and the same single-container
100 ms-approximate-RTT/no-loss shaping. The paired report from
`benchmarks/compare-agent-contracts.sh` measured these ASP-only contract
changes (SSH continues to run the exact shell fixture):

| Metric (p50; p90/p99 in parentheses) | Exact output | `EXEC_SUMMARY` | Change |
|---|---:|---:|---:|
| application payload | 10,485,971 B | 8,403 B | 99.92% lower |
| interface bytes per direction | 104,996 B | 39,981 B | 62.2% lower |
| wall time | 3,236 ms (3,269 / 3,285) | 3,165 ms (3,213 / 3,215) | 2.2% lower |
| network-blocked time | 2,212 ms (2,242 / 2,253) | 2,137 ms (2,189 / 2,191) | 3.4% lower |
| reconnect/state recovery | 341 ms (352 / 353) | 342 ms (351 / 357) | effectively equal |

`EXEC_SUMMARY` changes the delivery contract: the complete stdout/stderr log
is still retained durably and remains available through `asp logs`, while the
request response carries only a verdict, byte count, and bounded final tail.
The payload reduction therefore applies when an agent does not need every log
byte immediately; it is not a way to make exact-output delivery smaller than
the bytes requested. The all-zero comparison is still one container and one
no-loss condition; the separate incompressible capture below removes the
compression confounder, while packet loss and two independently managed hosts
remain necessary before publishing a general bandwidth SLO.

After the server response-serialization reactor hardening, a five-trial
regression smoke at the same approximately 100 ms RTT cell remained lossless:
ASP p50 wall time was 3,206 ms, network-blocked time 2,185 ms, and recovery
344 ms, with 105,179 interface bytes per direction. This is a regression check,
not a replacement for the qualified 30-trial capture above.

### 30-trial incompressible exact-output cell

Raw file: `benchmarks/raw/docker-agent-workload-2026-08-27-rtt100-incompressible.jsonl`.
This capture passed the strict 30-trial qualifier for both systems and uses the
same fresh-container, 50 ms one-way delay (approximately 100 ms RTT), 100 Mbps,
zero-loss/zero-jitter performance cell as the historical capture, but the
10 MiB command output is read from `/dev/urandom`. `log_mode=incompressible`
is recorded on every row, and `disconnect_seconds=0` keeps this a performance
cell rather than a 30-second outage test.

| Metric (p50; p90/p99 in parentheses) | ASP | SSH ControlMaster | ASP change at p50 |
|---|---:|---:|---:|
| wall time | 4,475 ms (4,501 / 4,512) | 9,261 ms (9,329 / 9,370) | 51.7% lower |
| network-blocked time | 3,456 ms (3,483 / 3,497) | 8,177 ms (8,251 / 8,292) | 57.7% lower |
| reconnect/state recovery | 340 ms (349 / 353) | 1,514 ms (1,542 / 1,543) | 77.6% lower |
| application request gates | 12 (12 / 12) | 18 (18 / 18) | 33.3% fewer |
| application payload | 10,485,971 B | 10,487,585 B | effectively equal |
| interface bytes per direction | 10,853,696 (10,855,642 / 10,856,049) | 10,584,889 (10,590,758 / 10,592,794) | 2.5% higher |
| ASP daemon CPU (user + system) | 70 + 130 ms (80 + 140 / 80 + 150) | not captured | — |
| ASP daemon RSS | 33,576 KiB (33,908 / 34,192) | not captured | — |

This result is the important qualification for the earlier all-zero capture:
when exact output is required, ASP transfers essentially the same 10 MiB as
SSH and is slightly larger on the loopback interface. The latency improvement
survives because the structured fixture uses six fewer application gates and a
faster durable resume path; it is not a compression artifact. The one-container
and no-loss limitations still apply.

### 30-trial incompressible exact-vs-summary contract comparison

Raw summary file:
`benchmarks/raw/docker-agent-workload-2026-08-27-rtt100-incompressible-summary.jsonl`.
It passed the same strict qualifier, uses `log_mode=incompressible`, and keeps
the complete 10 MiB process log durable while returning only an 8 KiB bounded
tail through `EXEC_SUMMARY`. The paired report from
`benchmarks/compare-agent-contracts.sh` produced:

| Metric (p50; p90/p99 in parentheses) | Exact output | `EXEC_SUMMARY` | Change at p50 |
|---|---:|---:|---:|
| application payload | 10,485,971 B | 8,403 B | 99.92% lower |
| interface bytes per direction | 10,853,696 B | 47,694 B | ~99.56% lower |
| wall time | 4,475 ms (4,501 / 4,512) | 3,045 ms (3,081 / 3,085) | 32.0% lower |
| network-blocked time | 3,456 ms (3,483 / 3,497) | 2,023 ms (2,060 / 2,069) | 41.4% lower |
| reconnect/state recovery | 340 ms (349 / 353) | 340 ms (349 / 351) | effectively equal |

The summary reduction is a semantic choice: agents that need a verdict and
diagnostic tail avoid moving the full transcript, while `asp logs` can fetch an
exact durable range later. This is a real bandwidth result even for
incompressible output, but it must not be presented as exact-output compression.
The paired capture remains a single-container/no-loss result and still needs
two independently managed hosts and the full impairment/migration matrix.

Raw file: `benchmarks/raw/docker-agent-workload-postcard-2026-08-26.jsonl` (two successful rows).

That raw run predates the current one-shot fast path: the current CLI no longer
does a redundant `RESUME_SESSION` before every operation and retries
idempotent requests across a reconnect. Re-run the fixture before treating the
historical gate counts below as current release numbers.

| Metric | ASP | SSH ControlMaster | ASP change |
|---|---:|---:|---:|
| application request gates | 13 | 18 | 27.8% fewer |
| network-blocked time | 3.082 s | 7.996 s | 61.5% lower |
| wall time including 31 s deliberate wait | 34.414 s | 39.036 s | 11.8% lower |
| reconnect + state verification | 223.7 ms | 1,471.7 ms | 84.8% lower |
| application payload consumed | 10,485,971 B | 10,487,585 B | effectively equal |
| loopback interface bytes | 10,865,064 B | 10,609,369 B | 2.4% higher |
| detached process observed | yes | yes | — |

Subtracting the fixed 31-second wait gives approximately 3.41 s active wall time for ASP and 8.04 s for SSH. The result demonstrates the central latency mechanism on this fixture: five fewer serialized application gates plus cheaper cursor resume. It does **not** isolate propagation delay from SSH channel/process overhead, nor establish a population distribution.

ASP did not win bandwidth when both callers demanded every log byte. Its compact Postcard row was 2.4% larger at the interface than SSH. A preceding JSON/base64 build used 21,755,521 B; switching framing reduced that ASP row to 10,865,064 B, but only removed an implementation mistake rather than creating a semantic advantage. Those prior rows are retained as `docker-agent-workload-json-2026-08-26.jsonl`.

Mosh is not assigned a synthetic full-workload number: it has no structured file mutation, workspace query, exit/result journal, or exact resumable-output API. Its applicable cold terminal bootstrap is included in the command-start sweep. A future comparison should include RoSE for terminal-only steps and an agent daemon over SSH/Tailscale as the strongest semantic baseline.

## Remaining unproven effects

- EXEC over a fresh 1-RTT QUIC connection should not be assumed faster than a warm multiplexed SSH connection.
- QUIC migration should reduce interruption for short path changes; durable resume handles longer loss.
- PTY latest-state datagrams carry parsed screen/cursor state and can supersede old generations, but perceived latency and loss behavior have not been compared with Mosh/RoSE.
- Structured file patches should save bytes for localized edits; the current CLI/agent path falls back to a full PUT for broad edits and for small files where metadata may exceed compressed whole-file transfer. A guarded agent `file_put` that repeats the exact inspected bytes now short-circuits the mutation locally after a zero-byte metadata/hash check (`file_unchanged`) instead of sending a redundant PUT and creating a journal event. End-to-end encoded-byte savings for changed files still need a shaped multi-trial measurement.
- Semantic aggregation saved five application gates in the small fixture; repository-scale CPU, cache behavior, and response bytes remain unmeasured.

Per-keystroke perceived latency, authoritative PTY latency, daemon CPU/RSS, physical laptop sleep/wake, real Wi-Fi→cellular migration, and two-host packet captures remain unmeasured. A PTY transcript attempt was discarded because it did not yield a trustworthy marker timestamp; cold startup time is not relabeled as keystroke latency.
