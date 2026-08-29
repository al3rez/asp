# Conclusions and v1 decision

The current production target is a Linux/macOS Unix host behind a private
overlay and supervisor. CI now checks Windows compilation and protocol tests,
including a bounded deterministic mutation smoke over the public decoders, but
Windows PTY, service-manager, and network-failure behavior remain unqualified
and are not part of the v0 support promise. An independent security review and
coverage-guided protocol/codec fuzzing campaign are still required before
treating the implementation as a broad production service; the local
malformed-input, mutation, and resource-bound checks are necessary but not
sufficient.

## 1. Is ASP necessary?

Not as a new remote shell, transport, or VPN. QUIC, RoSE, Mosh, SSH, tmux, and Tailscale already cover those layers. ASP is justified only as a reusable application/session layer when multiple coding agents need typed operations, durable cross-resource cursors, exact output, optimistic file writes, and semantic workspace aggregation. A small agent daemon over SSH remains the strongest simpler alternative and must stay in future comparisons.

## 2. What does it add beyond RoSE + Tailscale?

RoSE + Tailscale provides a strong roaming terminal path. ASP adds connection-independent multi-resource sessions, structured EXEC results, point-in-time `PROCESS_STATE` reads for detached agents, per-stream output offsets, a replay/compaction journal, idempotent process requests, version/hash-checked file mutation, and `WORKSPACE_STATE`. These could be implemented beside or inside RoSE; ASP does not need to replace its terminal engine or Tailscale connectivity.

## 3. What does it add beyond Mosh?

Reliable exact logs, typed process lifecycle and exit status, file transfer/patching with workspace-shared monotonic versions, resumable event cursors, multiple QUIC streams, loopback dev-server forwarding, immutable content-addressed artifacts with range resume, and extensible resource objects. Mosh remains better-proven for predicted interactive echo and terminal-specific synchronization.

## 4. Where are latency improvements measured?

At approximately 100 ms RTT, the historical coding fixture used 13 ASP gates versus 18 warm SSH ControlMaster channels, 3.08 s versus 8.00 s network-blocked time, and 224 ms versus 1.47 s recovery. A fresh single-container cell repeated the structured fixture for 30 paired trials at the same nominal delay (50 ms one-way `tc netem`, 100 Mbps, zero loss/jitter): ASP p50 wall time was 3,236 ms versus 9,484 ms for SSH, p50 network-blocked time was 2,212 ms versus 8,359 ms, and p50 recovery was 341 ms versus 1,569 ms. ASP used 104,996 interface bytes per direction at p50 versus 10,566,544 for SSH because this exact-output fixture's 10 MiB all-`x` log compresses extremely well. These are stronger distributional measurements than the historical one-trial result, but they still come from one container and one no-loss condition; they are not a two-host or universal WAN claim. The fresh 30-trial cold command-start matrix also favored ASP in every tested condition: at nominal RTT 0/100/300 ms its p50 was 62/405/1,010 ms versus SSH 167/1,537/3,987 ms and Mosh 456/2,042/4,792 ms; at the harsh 300-ms-jitter/10%-loss/1-Mbps corner it was 1,383 ms versus SSH 6,096 ms and Mosh 8,341 ms. Those rows are same-container startup measurements, not keystroke or warm-session SLOs. Wall time including the same deliberate 31-second wait in the historical run was 34.41 s versus 39.04 s. A current loopback release smoke sent five `true` requests through the persistent JSONL `asp agent` adapter in 295.245 ms versus 747.651 ms for five separate CLI processes; a separate five-inspect trial was roughly 80 ms warm versus 330 ms one-shot. A later 20-command loopback trial was only 1,727 ms warm versus 1,838 ms one-shot, because local process spawn dominated. These loopback figures include process startup and are not remote-network claims. The current CLI additionally skips a redundant preflight resume and retries idempotent requests. The cold `true` sweep also favored ASP across the tested matrix, but it is not an equal warm-session or keystroke comparison.
A corrected 30-trial incompressible-output cell at the same shaping point measured ASP p50 wall time 4,475 ms versus SSH 9,261 ms, network-blocked time 3,456 ms versus 8,177 ms, and recovery 340 ms versus 1,514 ms. Exact interface bytes were 10,853,696 versus 10,584,889 per direction, so the latency result survives without compression but exact output is not a bandwidth win. Both captures remain single-container/no-loss evidence rather than a universal WAN claim.

Removing attachment progress from transport retries (and therefore avoiding
both a sidecar fsync and an implicit journal replay) produced a fresh local
hot-path signal: twenty warm summary commands completed in 1.28–1.35 s over
three rounds, versus 3.03–3.22 s for twenty cold CLI invocations. This is a
small, unshaped loopback fixture and should not be read as a WAN or causal
percentage; raw rows are retained in
`benchmarks/raw/agent-adapter-local-2026-08-29-attachment-memory.jsonl`.

The ambiguous-response retry proof is retained in
`benchmarks/raw/agent-reconnect-direct-retry-2026-08-29.jsonl`; it records one
admitted side effect, one replayed result, and zero implicit resume requests.

## 5. Where are bandwidth improvements measured?

They are workload-dependent rather than universal for an exact-output contract. In the historical full 10 MiB-log fixture ASP used 10.87 MB per loopback direction versus SSH's 10.61 MB, 2.4% more. In the all-`x` 30-trial cell, v17 strict-win compression reduced ASP's p50 interface bytes to 104,996 per direction versus SSH's 10,566,544 (99.0% lower); random/incompressible output does not get this benefit. The qualified incompressible exact-output cell confirms that: ASP used 10,853,696 B per direction versus SSH's 10,584,889 (2.5% higher). A paired incompressible 30-trial `EXEC_SUMMARY` capture reduced ASP's application payload from 10,485,971 B to 8,403 B (99.92%) and interface bytes from 10,853,696 B to 47,694 B per direction (about 99.56%), while retaining the complete log for later `asp logs` retrieval. That is a deliberate bounded-tail contract, not exact-output compression. The deterministic v17 file-sync codec capture measures 516 B saved for a localized edit, 2,458 B for three scattered length-changing source edits, and 409 B for three equal-length scattered edits; a broad rewrite stays on the conservative full-PUT path. End-to-end file-transfer savings on shaped WAN links still need measurement. Full-output delivery still cannot be smaller than the requested bytes.

## 6. What remains QUIC work?

TLS, packet protection, handshake, connection IDs, migration/path validation, reliable streams, DATAGRAM delivery, loss recovery, congestion control, pacing, flow control, and transport statistics. ASP must not recreate any of these. Tailscale or another connectivity layer should continue to own NAT traversal and relay fallback for v1.

## 7. What is genuinely novel?

Not Rust + QUIC + terminal diffs; RoSE already demonstrates that. The potentially useful contribution is the combination of a durable connection-independent resource session, event replay-or-snapshot semantics, delivery classes for exact versus replaceable state, and agent-oriented semantic operations whose value is evaluated in application RTTs eliminated. Novelty is currently a hypothesis, not a literature-complete academic claim.

## 8. Is this a useful open-source project?

Yes, if scoped as an embeddable agent-session daemon/protocol and benchmark corpus that interoperates with SSH/Tailscale/RoSE. It is less useful as another standalone shell. The current dual MIT/Apache prototype is small enough to invite experimentation and contains a runnable falsification benchmark.

## 9. Is there enough for an academic paper?

Not yet. A credible paper needs broader related-work coverage of remote IDE/agent protocols, formal resume/idempotency semantics, a general versioned object model, several real repositories and agents, strong baselines including an agent daemon over SSH and RoSE, two-host/roaming tests, at least 30 trials per condition, statistical analysis, and measured CPU/memory/bytes. The semantic-gate result is a promising pilot.

## Production-readiness audit (after the hardening pass)

Same-principal cross-session artifact reuse is intentionally not counted as a
universal bandwidth win yet: the release smoke proves the verified hard-link
path and owner isolation, while the two-host benchmark still needs to measure
duplicate-upload bytes, hash-validation cost, and the filesystem fallback.

The supervised local adapter endpoint (`asp agent-listen`/`asp agent-connect`) now keeps adapter logic in one warm supervisor process and reuses a bounded four-connection idle pool, removing a new remote adapter process and handshake from sequential tool calls while retaining bounded asynchronous output buffering. Concurrent local clients still lease separate QUIC connections; the pool does not multiplex unrelated callers onto one transport.

The release warm-agent reconnect smoke now keeps a JSONL adapter alive across an
`aspd` restart and verifies that both a point-in-time workspace read and a
following idempotent EXEC reconnect directly without a redundant journal
replay. It also kills the daemon after EXEC admission but before the final
response, proving that the stable request ID replays exactly one durable result.
Quinn stream-open failures are classified as transport errors for this retry
path; application errors remain fail-fast.

Release installation now has a standalone archive verifier that checks the
SHA-256 sidecar, required binaries/templates/schema, unsafe archive paths and
all non-regular tar entries (links, FIFOs, devices, and sockets), accidental
credential/state inclusion, and the declared dual-license
texts. The package builder runs the
same verifier and includes the research/operations documentation in the
archive. The archive also carries `deploy/install-release.sh`, which verifies
the archive, takes a bounded private snapshot of the archive and sidecars,
installs an immutable versioned directory, atomically switches a `current`
pointer, and retains a validated `previous` pointer for rollback without
overwriting a running binary. The standalone verifier and signature helper use
the same snapshot boundary, so pathname replacement cannot change the bytes
being listed, authenticated, or extracted. The installer also rejects
untrusted symlink or group/world-writable existing prefix components before
creating directories, while allowing sticky shared ancestors and root-owned
system aliases. Supervisor integration,
signing, SBOM publication, and promotion remain external release controls; the
archive's deterministic `SBOM.spdx.json` is generated from the locked Cargo
resolve graph for those systems to review or attest. Workspace Git queries resolve a canonical helper from standard
service-manager paths (or `ASP_GIT_PATH`), and the container image installs Git
so semantic inspections do not depend on an interactive `PATH`.

The archive also carries `deploy/upgrade-release.sh`, which composes the
atomic install with an explicit supervisor restart and a loopback `/ready`
gate. A non-mutating prefix trust preflight rejects unsafe existing ancestors
before the lock and is repeated after locking; the prefix lock then serializes
concurrent upgrade transactions. A failed readiness check restores `previous`
automatically and returns failure even when rollback is healthy. The packaged
rollback/lock/unsafe-prefix smoke exercises this operator workflow;
independent-host supervisor and historical-binary qualification remain release
gates.

Daily single-user use is now substantially safer than the original v0: the server authenticates clients by default, session/event/process state is durable, process output is reconciled after daemon restart, PTYs reattach through a detached tmux owner (with an explicit clean client detach so daemon shutdown cannot inject EOF into the shell), writes are atomic and workspace-confined, streams are bounded (including size-aware request-body and response-frame deadlines), configurable filesystem headroom fails readiness and new durable mutations before a volume fills, and SIGTERM/CTRL-C shuts down the listener without deleting durable state.

The release concurrency smoke additionally runs independent agent adapters
against one shared workspace, overlapping semantic inspections, bounded command
summaries, and file commits; the current run passes at the daemon-advertised
32-connection per-principal ceiling with zero request failures and no residual
frame/response memory. This is a bounded contention regression, not evidence
of multi-tenant capacity.

The bounded capacity-soak smoke now keeps independent warm adapters active for
repeated summaries, semantic reads, and guarded writes, and requires all
connection, request-stream, decoded-frame, and encoded-response gauges to
return to zero afterward. The default local run (eight workers, 15 seconds)
and the shorter CI profile pass. This closes a local leak/contention gap but
does not establish a production capacity SLO; longer independent-workspace
soaks and supervisor/cgroup/disk exhaustion tests remain required.
The harness also enforces a duration-plus-drain-grace deadline (default 60
seconds) and fails closed when blocked writers/adapters exceed it, preventing
an overloaded local run from creating an unbounded process tree.

The in-flight transfer restart smoke now covers the failure mode that ordinary
artifact restart retrieval did not: a client is paused after a durable upload
prefix exists, the daemon is killed and restarted, and the same FILE_PUT and
artifact requests resume from that prefix without a byte mismatch. A bounded
post-resume scheduler yield and four-frame (256 KiB) pacing burst are limited
to continuation streams; each burst is followed by a 10 ms pause and fresh
uploads retain normal QUIC pacing. This is a local restart invariant, not a
WAN loss or roaming result.

`benchmarks/smoke-mixed-release.sh` now composes that invariant for two
independently supplied release archives with explicit SHA-256 sidecars and runs
both old-to-new and new-to-old directions, including a timeout-bound EXEC
across each daemon replacement. Same-digest archives are rejected unless an
explicit mechanics-only opt-in is supplied and are labeled in the JSON result;
the historical-binary compatibility claim still requires an actual separately
built v16 artifact.

The daemon now exposes fixed-cardinality per-operation request-duration
histograms through `/metrics`, with long-lived PTY/subscription/port streams
excluded so normal p50/p90/p99 request SLOs remain meaningful. Release archives
also normalize metadata and gzip headers; CI rebuilds an archive twice and
requires byte-for-byte equality before deployment. These improve operational
measurement and supply-chain repeatability, but do not substitute for the
external two-host performance or signing gates.

Storage compaction, process-log pruning, resumable-upload cleanup, and artifact
retention now publish bounded pass/failure/last-success telemetry. A failed pass
or a scheduler that goes stale makes `/ready` return
`storage_maintenance_unhealthy`, giving supervisors a concrete signal for
retention drift instead of relying on warning logs alone.

The final hardening pass also adds incremental bounded-memory WAL replay (the live validator checks frames without rebuilding startup process/request/artifact maps), CRC32-protected 64 MiB segments, atomic snapshots/background compaction, bounded group-commit for high-rate output, authenticated `asp doctor` request/failure counters, bounded streaming resume, reconnect-safe EXEC retries with offset de-duplication, idempotent first-session creation with a stable `OPEN_SESSION` request ID, `EXEC_SUMMARY` bounded-tail results, durable process-log range reads, an adapter-level detached SPAWN and resumable LOGS path, overlapped workspace query work, a watcher-backed versioned workspace tree index with conditional tree omission plus bounded repeated-search and small-Git-metadata caches, a v13 complete workspace-result digest validator, a watcher-invalidated daemon-side digest fast path, a bounded agent-side semantic cache, immutable content-addressed artifact PUT/GET streams with resumable prefixes and exact ranges, and indefinitely retrying PTY reattach and event subscriptions across network loss, token-file rotation/revocation (including the atomic `aspd --rotate-auth-token` maintenance command), metadata-cached principal/certificate maps that still reload on rotation, optional CA-validated mTLS identity binding, SIGHUP TLS reload with bounded client pin sets for overlap, atomic file-write rollback/version serialization, precondition-aware whole-file and streamed file writes with explicit blind-overwrite opt-in, a daemon-wide workspace commit gate with cancellation cleanup for abandoned direct uploads, input/path/query limits, active-connection/request deadlines, bounded Git-query subprocesses, a 256 MiB aggregate decoded-frame memory budget plus a separate 256 MiB encoded-response memory budget (with serialized response encoding before permit acquisition), per-principal/per-operation request-byte admission, per-principal active-connection and request-stream leases, a per-session 65,536-record idempotency budget that refuses new side effects when full, lock-protected client checkpoints (including cross-process uploads), a machine-readable schema registry, a persistent JSONL agent adapter for EXEC, workspace inspection, durable process signaling, small file mutations, and artifact transfers, a systemd deployment baseline, separate fail-closed systemd/launchd production templates that require an operator-owned process launcher, end-to-end output-queue permits that stay charged until the QUIC writer consumes each item, Quinn-native stream priorities for interactive/control traffic ahead of bulk payloads, a roughly 60 Hz latest-wins PTY screen datagram path that avoids full-screen reconstruction for every output chunk, and an explicit validated process-launcher hook with a fail-closed `--require-process-launcher` mode for both EXEC/SPAWN and tmux-backed PTYs. PTY generation markers are persisted on a 100 ms coalescing cadence instead of once per output chunk, tmux discovery is deterministic under launchd/systemd (`ASP_TMUX_PATH` or standard system paths), and named `--consumer-id` cursors isolate concurrent local event subscribers without changing durable session IDs. These changes make the single-user pilot materially more usable and safer for concurrent edits, but do not remove the v1 blockers below.

The negotiated multi-range file patch path now covers scattered equal-length
and bounded line-oriented length-changing source edits. It is included in the
real agent smoke plus deterministic wire-size evidence; older peers continue
to use existing patch/PUT paths.

For the response-memory accounting described above, the current implementation
serializes only potentially large response shapes before acquiring the exact
encoded permit. Bounded control and interactive responses bypass that gate so
large workspace/log encodes cannot delay PTY or session control; all retained
payloads remain charged to the aggregate response semaphore.

It is not yet a zero-operator multi-tenant production service. The server now has an identity-bound mTLS mode (CA validation plus certificate-fingerprint owner/scope mapping), a private rotating JSONL audit sink, aggregate decoded-frame/file-read memory budgets, size-aware body receive deadlines, per-principal/per-operation request-byte admission (including streamed continuations), per-principal response-byte admission, per-principal active-connection and active-request-stream leases, a cross-session per-principal running-process quota, process-level response-frame/port-payload counters, active request-stream telemetry, resume replay/compaction/lag counters, and best-effort Linux cgroup-v2 usage/limit gauges. Broad exposure still needs externally managed PKI/secret distribution, dedicated least-privilege or sandboxed workers, monitored audit/retention export, validated process-supervisor deployment (systemd and macOS launchd starting points are supplied), supervisor-enforced child/cgroup policy and alerting, PTY backend portability, and two-host security/failure testing. Checksummed backup/verify/restore, corruption quarantine, age-based compaction, retention-aware process metadata pruning, process-start intent recovery, resumable large uploads, immutable artifact range retrieval, durable artifact tombstones/GC, same-principal cross-session artifact hard-link reuse, and the filesystem headroom circuit breaker now exist. Large GET/PUT/artifact transfers are streamed with SHA-256 validation and client/server checkpoints, while the current command surface intentionally executes shell code as the service account and is not a sandbox. The running daemon can reload a complete server certificate/key pair on SIGHUP; this avoids connection churn but still requires an operator-controlled trust rollout. The client cursor writer now merges same-session updates monotonically under its lock, so concurrently running adapters cannot regress the shared resume point; named `--consumer-id` entries additionally give independent subscribers separate local cursors. With the optional `event_consumer_leases` capability, the server ACK path now persists named cursors and seven-day lease heartbeats and defers compaction behind unexpired consumers; older peers retain advisory ACK semantics.

The container deployment now ships an immutable exec-only worker wrapper and
starts ASP with the fail-closed `--production` profile by default. The wrapper
anchors the process-policy identity for EXEC, SPAWN, Git helpers, and tmux PTYs;
the container's cgroup, read-only root, and no-new-privileges settings remain
the aggregate boundary. Host systemd/launchd deployments still require a
site-owned sandbox or supervisor wrapper, and this does not change the
two-host qualification or external PKI/audit/backup gates.

The latest local speed hardening keeps large (at least 64 KiB) request and
response codec work off the Tokio reactor even when high-entropy payloads stay
uncompressed; ordinary uncompressed 64 KiB transfer chunks are decoded inline
to avoid one scheduler task per chunk. Server-side Postcard response
serialization still yields the multi-thread Tokio worker before the bounded
frame/compression path, with a safe current-thread fallback. The CLI `patch`
path also chooses a guarded full PUT for broad rewrites or returns a no-op for
identical bytes. These choices reduce avoidable reactor stalls and mutation
events; their real-network impact still requires the external qualification
matrix.

The durable launch transaction also stopped repeating parent-directory
`fsync` calls that `write_atomic_file` had already performed. In a paired local
8-worker/15-second soak this reduced launch time from 71.5 to 65.0 ms per
launch (about 9%) with zero failures or residual resources. The capture is
retained as an optimization signal; storage and cross-host variance mean it is
not a universal SLO claim.

Fresh persisted stdout/stderr logs are also created exclusively with 0600
descriptor modes, removing a follow-up metadata open/`fchmod` while preserving
no-follow and stale-path rejection. The persisted-output test verifies the
private modes; the follow-up soak stayed clean but was statistically unchanged,
so this is recorded as a syscall reduction rather than a measured speed win.

The optional `pty_rich_state` capability now preserves ANSI terminal attributes
in reconnect snapshots for new peers while retaining the plain mixed-release
fallback. A separately negotiated `pty_rich_compression` marker lets oversized
rich screen state fit a QUIC DATAGRAM when fast zlib produces a strict win;
malformed or oversized compressed state is rejected before allocation. It
remains a full redraw rather than a RoSE-quality terminal engine. Plain peers
may also negotiate `pty_state_delta`: changed rows are sent relative to an
exact screen generation and a full checkpoint is emitted at least every 16
updates, bounding recovery after a lost datagram without making the lossy path
reliable. A bounded negotiated scrollback page now preserves recent plain-text
history when a client process is recreated. On tmux-backed sessions it is
sourced via a bounded `capture-pane` call through the validated launcher path;
full terminal-engine parity and speculative local echo still require a
dedicated RoSE/wezterm-quality integration.
The client also advances that replaceable-state generation from reliable
`PTY_OUTPUT` frames and lag-recovery `PTY_READY` snapshots, so a delayed
DATAGRAM or reliable snapshot cannot repaint an older screen over newer live
output.

Long-lived bearer-token agent adapters and event subscribers now retain the
configured Quinn endpoint across reconnects, reusing its UDP socket and rustls
session cache instead of rebuilding transport state after every daemon restart
or network flap. mTLS client-certificate adapters deliberately rebuild so
rotated key material is reloaded; the optimization does not change QUIC
migration or durable session resume semantics.

The client and server now derive their flow-control, keepalive, datagram-buffer,
and fair-scheduling settings from one shared Quinn transport profile. This keeps
the low-latency PTY/control priorities and replaceable datagram path consistent
across both endpoints; Quinn still owns loss recovery, pacing, migration, and
congestion control.

The SSH bootstrap helper now bounds both credential transfers with a
20-second connection timeout and five-second server-alive probes. This avoids
an asleep host leaving a daily setup command stuck forever while preserving the
operator's normal SSH authentication policy.

Decoded-frame admission now fails fast after 250 ms when the aggregate memory
budget is occupied, and `asp_frame_memory_rejections` makes that pressure
visible instead of leaving request tasks waiting behind a stalled large frame.

`EXEC`/`EXEC_SUMMARY` now have an optional server-side wall-clock policy
(`--exec-timeout-seconds`) that persists the deadline and reports exit code 124
after terminating the process group. The default remains disabled for
compatibility, and supervisor/worker policy is still required as the stronger
boundary for untrusted workloads; durable `SPAWN` remains intentionally
long-lived.

The same SIGHUP path now reloads a bounded server-side client-CA bundle, so
identity-bound deployments can overlap old/new trust roots before revocation.

Bounded semantic Git helpers now run in a dedicated process group; timeout,
output-limit, and read-error paths terminate the verified group before reaping
the direct child, avoiding lingering credential/helper descendants.

The process-level `asp_process_output_attachment_detaches_total` metric now
makes slow-reader detachments observable, and daemon-level CPU time/peak RSS
gauges come from `getrusage`; Linux cgroup-v2 current/limit gauges are also
exported when available. Supervisor policy, central audit export, and alert
wiring remain deployment gates.

Quinn UDP/loss/congestion/path RTT/MTU counters are now captured when each
attachment closes and exported for transport SLOs; ASP still delegates loss
recovery, pacing, migration, and congestion control entirely to Quinn.

The daemon's fail-closed production profile now enables Quinn stateless retry
automatically. Shared-interface development daemons can opt in with
`--stateless-retry`; retry attempts and failures are exported separately. This
adds an initial address-validation flight to protect a reachable UDP listener
from spoofed amplification without introducing ASP-owned cryptography or a
second transport algorithm.

The release smoke suite now also exercises bounded mTLS client-CA overlap and
SIGHUP rotation, retired-CA rejection, checksummed backup/verify/tamper/
force-restore behavior, attached-EXEC wall-clock termination, and timeout
recovery after an abrupt daemon restart. Linux CI additionally builds the non-root,
read-only-root container and exercises authenticated EXEC, detached SPAWN, and
durable log retrieval inside it. Those tests validate local invariants; PKI ownership,
off-host backup retention, and two-host failure qualification remain external
production gates.

The split-host benchmark now accepts an operator-owned network-event executable
for the two cases that cannot be honestly emulated by loopback shaping: address
migration and laptop sleep/wake. It runs the same hook in the ASP and SSH legs,
records completion/duration in each paired row, and refuses to publish a cell
when the hook fails or its metadata does not match the run configuration. This
improves auditability of the eventual measurement; it does not claim that a
physical roaming experiment has happened until an operator runs it on two
independent hosts.
Selected-file workspace responses also use a daemon-wide bounded memory budget
and retain permits through response serialization, so concurrent agents cannot
multiply the per-request file cap. Encoded responses borrow from a separate
daemon-wide 256 MiB budget until the QUIC write completes; a single encoding
gate closes the transient uncharged-buffer window, with request/response
occupancy, limits, and temporary refusal counts exposed as metrics.

Workspace and durable-log reads also use final-component no-follow opens after
path confinement checks, closing the common symlink replacement race; a host
sandbox is still required for stronger intermediate-directory isolation.

The current release also preserves ordinary Unix mode bits when replacing an
existing file (new files default to `0600`), revalidates idle port forwards on
the same one-second revocation bound as active flows, bounds JSONL adapter input
without allocating an unterminated line, and lets operator-issued certificates
select their DNS/IP-SAN through `--server-name`. These are deployment-safety
details rather than claims of multi-tenant isolation.

## 10. What should v1 contain?

1. identity-based client authentication and SSH/mTLS bootstrap;
2. backup/restore and policy-driven retention around the checksummed segmented WAL/snapshots, plus an external process supervisor;
3. stable versioned binary schema/feature negotiation and streamed/range semantics for patches and artifacts;
4. versioned workspace state, subscriptions, and cache/invalidation rules;
5. generic snapshot/diff/apply contracts with reliable fallback;
6. generalized adaptive whole-file/patch/delta selection (server-side and beyond the current CLI/agent prefix/suffix choice) and exact byte telemetry;
7. log summaries, filters, ranges, and immutable artifacts;
8. policy-configured ports, quotas, backpressure, authorization, and multi-agent conflicts;
9. RoSE/wezterm-quality terminal integration rather than a new emulator;
10. repeated two-host, real-roaming, CPU/RSS, keystroke, sleep/wake, and agent-daemon baseline measurements.
