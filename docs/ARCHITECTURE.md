# ASP architecture

## Boundaries

ASP is an application protocol over IETF QUIC. Quinn provides QUIC, rustls provides TLS, and Tailscale can provide private reachability/NAT traversal. ASP owns durable session/resource semantics, idempotency, state synchronization, event replay, and authorization.

```text
 Agent / CLI / IDE
        |
        | typed ASP operations
        v
 +---------------- asp client ----------------+
 | session cursor(s) | file bases | agent UI  |
 +--------------------+-----------------------+
                      |
              QUIC/TLS connection (attachment)
        +-------------+-------------------------------+
        | streams: CONTROL / EXEC / PTY input+bytes   |
        |          FILES / events / ports / artifacts  |
        | datagrams: latest terminal/status state      |
        +-------------+-------------------------------+
                      |
 +------------------- aspd ---------------------------+
 | authenticated connection -> authorized session     |
 |                                                    |
 | SessionStore                                       |
 |  session UUID                                      |
 |   +-- EventJournal (monotonic IDs, bounded memory) |
 |   +-- Session WAL (durable lifecycle frames,       |
 |       bounded group-commit output frames, age/size  |
 |       compaction, and process-start intents)       |
 |   +-- ProcessTable + per-process output logs       |
 |   +-- PTY (tmux-owned child, parsed screen+bytes)  |
 |   +-- LatestState (replaceable PTY/status hints)   |
 |   +-- FileVersions (hash, optimistic writes)       |
 |   +-- Workspace query (tree/git/search/read batch) |
 |   +-- immutable artifact objects + agent adapter   |
+----------------------------------------------------+
          | filesystem / OS processes / ports
          v
      remote workspace
```

Admission is layered above transport flow control: authenticated request
payload bytes are charged to a bounded per-principal/per-operation rolling
window (4 GiB/minute by default), including streamed file and PTY continuation
frames. The server exposes the admitted-byte and rejection counters through
the loopback health endpoint. Encoded response-frame and port-forward payload
bytes are also counted process-wide, while encoded responses are charged to a
bounded per-principal rolling egress budget (4 GiB/minute by default). Host/
Quinn telemetry is still needed for packet overhead. This limits a noisy
identity without trying to replace QUIC's congestion controller.

The optional `--min-free-bytes` policy is a filesystem circuit breaker. Below
the configured headroom, readiness becomes false and new durable mutations are
rejected with `storage_headroom`; read-only inspection and recovery remain
available. It is intentionally independent of per-session/object quotas and
must be paired with off-host backups and supervisor disk alerts.

Quinn flow control is configured symmetrically with an 8 MiB per-stream
receive window, 32 MiB connection receive window, and 32 MiB send window. The
windows are deliberately bounded: they cover the expected high-latency
development link without allowing the 128-stream limit to turn into an
unbounded per-connection buffer. Congestion control, loss recovery, and pacing
remain Quinn responsibilities. ASP explicitly enables Quinn's server-side
migration policy so NAT rebinding and Wi-Fi/cellular path changes can keep an
authenticated connection alive. Both endpoints use five-second
keepalives and a 15-second maximum idle timeout; healthy idle PTY/event
attachments therefore persist, while a dead path is noticed promptly enough
for the client to reconnect/resume. This is a tunable transport policy, not a
session timeout. Client bidirectional request/attachment stream creation is
bounded to ten seconds, so peer stream-limit/flow-control exhaustion fails as a
retryable transport error rather than pinning an agent call indefinitely. Each
client request-frame write has a size-aware 64 KiB/s minimum-rate deadline
(10-second floor, five-minute cap), matching the server's receive policy and
preventing a stalled peer from holding an upload forever. Server response-frame
writes use the matching per-frame minimum-rate bound, releasing the encoded
response permit and request task when a reader stops consuming data. A request
that cannot
acquire a decoded-frame permit within
250 ms fails fast and increments `asp_frame_memory_rejections`, so memory
pressure cannot leave request tasks waiting indefinitely behind a stalled
large frame.

## Connection is not session

An ASP session has a UUID and owner identity. A QUIC connection is one temporary authenticated attachment. QUIC migration preserves an attachment across many path changes; if it fails or expires, retryable operations reconnect with the session UUID and their own idempotency/range state, while an explicit event consumer sends `RESUME_SESSION(session_id,last_event_id)` when it needs journal replay. The daemon continues processes/PTYs without a client.

The client cursor is local attachment metadata, not the session identity. The
default per-server cursor is retained for compatibility; concurrent consumers
can pass distinct `--consumer-id` values to store independent cursors in the
same lock-protected file. A named consumer first falls back to the legacy
per-server UUID, then materializes its own cursor on the first save. This
prevents one subscriber from advancing another subscriber's replay point while
keeping the wire protocol session UUID unchanged. Filtered request/result
streams carry their own request ID, byte offset, or immutable digest for retry;
they never advance the durable consumer cursor. Only an explicit
replay/subscription boundary can do that. A retry reconnect therefore starts
with HELLO and the original operation, without an implicit full-journal replay
or an atomic sidecar write/fsync for a non-durable attachment hint.
The ordinary `asp connect` command performs only the QUIC/authentication
handshake and reuses that saved UUID, so reconnecting does not replay a large
journal; `asp resume` is the explicit snapshot/event catch-up operation.

The session UUID and event journal are persisted below `.asp/sessions/<uuid>/`. `aspd` validates and truncates a partial event-log tail at startup, rebuilds materialized process/file state, and starts monitors for still-running children. Symlinked or UUID-named non-directory entries in the sessions directory fail recovery instead of being silently omitted. If a durable running-process record cannot be mapped to a safe process-artifact root during the final monitor sweep, startup fails closed rather than serving with an unobserved child; a supervisor can alert and retry after state repair. EXEC/SPAWN stdout and stderr are redirected to append-only per-process files, so output written while the daemon is down is folded back into the journal after restart. The wrapper records an atomic exit-status file, while a durable process-start intent makes an ambiguous spawn fail closed instead of adopting an unknown PID. Recovered PIDs are validated against the ASP wrapper command before monitoring and periodically while detached; on Linux the command check reads `/proc/<pid>/cmdline` with exact argument matching and, when present, compares `/proc/<pid>/stat` start-time ticks. Legacy WAL records without that identity begin unverified and are never signaled until the wrapper check succeeds; an ambiguous recovery is recorded as an unknown exit rather than risking a recycled-PID kill. BSD/macOS use an absolute-path `ps` fallback. Background maintenance applies configured event/process-log age retention in addition to size bounds. Each maintenance pass reports run/failure counters and timestamps through `/ready` and `/metrics`; a stale scheduler or a pass with failed cleanup turns readiness false instead of silently allowing retention drift. This is deliberately a small supervisor; production deployments should still run `aspd` under systemd/launchd and enforce disk quotas/backup policy.

Recovered process monitors keep their status-file and log polling asynchronous. A missing exit marker may still require an operating-system liveness probe; on POSIX the probe uses direct `kill(2)` signal-zero semantics rather than spawning a helper, so it is constant-time and does not depend on `PATH`. If an append-only log is shorter than its durable cursor, recovery fails closed and publishes a terminal monitor-failure event rather than rewinding the cursor and duplicating output.

The read-only `PROCESS_STATE` operation exposes one materialized process record
(running flag, exit code, and durable stdout/stderr byte counts) without
constructing or replaying the complete session snapshot. This keeps detached
agent status checks cheap while leaving the journal and range log streams as
the authoritative recovery paths.

Live EXEC output is also bounded independently of the request-frame budget. A 128 MiB aggregate semaphore covers queued output chunks across processes and keeps each permit until the corresponding bounded response item has been consumed by the QUIC writer, so a slow reader cannot create a second uncharged copy in the response channel. If the aggregate budget stays exhausted for 250 ms, the live attachment detaches; durable logs/events remain authoritative and a reconnect resumes from its cursor, which prevents a disk-backed monitor from waiting indefinitely. Each process uses a small response channel, and persistent log reads are capped at 64 KiB. Event fan-out retains at most 256 events per session; a lagged subscriber reconnects from its durable cursor. When a client cannot consume output, backpressure reaches the child process or the attachment is detached; the daemon does not retain an unbounded per-stream transcript in memory.

The PTY backend uses a named, initially detached `tmux` session
(`asp-<session-id>`). The PTY master and `attach-session` process are temporary
attachments owned by `aspd`, while tmux owns the shell and survives daemon/client
loss. A clean attachment drop explicitly detaches the tmux client before
portable-pty closes its master, avoiding an EOF/hangup being interpreted by the
shell during restart. On restart the daemon reattaches and rebuilds the parsed
screen state. The resolver checks standard system/Homebrew locations or an
explicit `ASP_TMUX_PATH` before `PATH`, keeping service-manager environments
deterministic; it accepts only regular executables without group/world-write
bits, so a replaceable supervisor cannot silently change the durable PTY
boundary. If tmux is unavailable, PTY_OPEN fails clearly; the server does not
silently downgrade a supposedly durable session to an orphanable direct PTY.
The supplied systemd unit uses `KillMode=process` so a daemon restart does not
terminate session children and deliberately leaves `PrivateTmp` disabled so
the new daemon can see tmux's default `/tmp` socket; deployments using another
supervisor must provide equivalent child-lifecycle semantics or configure tmux
to use a persistent socket path. Because portable-pty exposes a synchronous
master writer, PTY input is serialized per backend, dispatched to Tokio's
blocking pool, and bounded by a ten-second write timeout; a child that stops
reading cannot pin a QUIC reactor worker or multiply blocked tasks during
reconnect storms.

## Resource consistency classes

| Class | Examples | Delivery/recovery |
|---|---|---|
| Side-effect command | EXEC, SIGNAL, FILE_PATCH | reliable stream + idempotency key/result cache |
| Ordered durable event | process lifecycle/output, file changed | journal + reliable replay cursor |
| Replaceable state | terminal screen, progress, presence | numbered DATAGRAM; reliable snapshot fallback |
| Bulk immutable bytes | artifacts, file GET, process-log ranges | dedicated reliable uni/bi stream, bounded offset/range resume |
| Live byte tunnel | ports, PTY raw history | reliable stream with reset/half-close |

## Prototype crate layout

- `asp-protocol`: versioned serde wire schema and length framing.
- `asp-server` (`aspd`): Quinn listener, session store, event journal, processes, PTY and files.
- `asp-client` (`asp`): pinned-cert client, saved per-server cursor, EXEC/SPAWN/resume/shell/file/inspect commands, a persistent JSONL `agent` adapter that reuses one QUIC connection across EXEC, workspace, and small file tool calls, keeps that attachment's Quinn endpoint alive so reconnects reuse its UDP socket and TLS session cache, and applies the same endpoint reuse to warm `batch` retries, long-lived event subscriptions, interactive shells, and forwarding listeners when bearer-token authentication is in use. A supervisor-friendly private Unix-socket `agent-listen`/`agent-connect` endpoint has a bounded four-connection idle pool; discarded, stale, or aborted adapter connections explicitly close and remove their endpoint handles so a local client error cannot hold a remote lease until QUIC idle timeout. The client also includes the agent workload driver.
- `asp-pty`: connection-independent `portable-pty` wrapper with broadcast output, a `vt100` parsed screen, an optional ANSI-formatted rich snapshot that preserves cell attributes, and bounded raw tail.
- `asp-bench`: Quinn streams/DATAGRAM/rebind/stats experiment; benchmark expansion point.

## Security posture

The client sends TLS SNI `localhost` by default to match generated
certificates; `--server-name` selects the DNS name or IP SAN for an
operator-issued certificate.

The server generates a self-signed rustls certificate; the client pins its DER file or a bounded directory of DER pins for rollover. By default the server also creates a random bearer token in `<workspace>/.asp/auth-token` (mode `0600`) and requires it in `HELLO_FEATURES`; relative certificate/key/token/principal/lock paths are scoped below `--root`, and absolute paths are explicit opt-outs. Existing private keys are tightened to mode `0600` through a no-follow descriptor before they are loaded. A running Unix daemon accepts SIGHUP to reload an already-provisioned certificate/key pair for new handshakes and keeps the previous config if the pair is absent or invalid; this avoids dropping durable sessions while leaving PKI issuance and client pin rollout to the operator. Comparison is constant-time. The daemon re-reads the token file for each request, so an atomic replacement invalidates old credentials even on an existing connection. For multiple identities, `--auth-principals-file` accepts a JSON map of principal names to `{token,scopes}` records; sessions persist their owner and every resource request checks both scope and owner. Shared deployments can instead use `--client-ca` plus `--auth-certificates-file`: rustls requires a client certificate signed by the configured DER CA, and ASP maps the leaf certificate SHA-256 fingerprint to an explicit owner/scopes record. The fingerprint mapping is re-read on requests, so removing a certificate principal revokes it without a daemon restart. Parsed token/principal maps are cached by file identity, length, and mtime after the first request, avoiding synchronous JSON parsing on the hot path while still reloading when an atomic rotation replaces a file with equal-sized content. `HELLO_FEATURES` returns the negotiated capability intersection, while legacy `HELLO` remains accepted for older clients. `aspd` refuses non-loopback listeners unless `--allow-non-loopback` explicitly acknowledges a private overlay/firewall. `--insecure-no-auth` is an explicit localhost-development escape hatch. Bearer tokens remain weaker than mTLS/SSH-certified or Tailscale identity, and shell execution is not a sandbox. No custom cryptography is used; Cargo enables Quinn/rustls' mature ring provider only (the unused default AWS-LC/platform-verifier features are disabled to keep the production dependency surface smaller). The daemon emits structured tracing records for request operation labels and authenticated principals without logging bearer tokens, client key material, command bodies, or file contents. It also writes the same safe labels to a private rotating JSONL audit sink (`.asp/audit.log`) from a bounded writer queue; dropped entries and writer failures are counted. Audit, event-WAL, lock, and persistent process-log append/create opens use Unix descriptor-level no-follow checks as well. The loopback metrics endpoint exposes those counters, but an operator still needs to export, retain, and alert on the audit stream.
The authentication path commits the connection's principal identity and
per-principal lease atomically across concurrent HELLO streams. Credential
rotation may re-authenticate the same principal after revocation, but a
different identity cannot take over that QUIC attachment.
The loopback `/ready` probe also validates the live credential source and
reports `auth_config_healthy`, so a missing or malformed rotated secret takes
the daemon out of service before a supervisor routes new work to it.
Failed probes include a bounded `ready_reasons` array with stable policy
codes, allowing a supervisor to distinguish credential, audit, storage,
launcher, Git-helper, and planned-drain remediation without scraping
unrelated counters.

The process boundary is also explicit: EXEC/SPAWN, semantic Git helpers, and
PTY creation may use an operator-supplied absolute launcher, and
`--require-process-launcher` makes it fail closed when the deployment needs one.
Health exposes whether the hook is configured. ASP canonicalizes the launcher
path once at startup, binds that canonical executable to its filesystem
identity, and rechecks it before every spawn, so replacement of the path or an
ancestor directory cannot silently redirect commands. The launcher must
`exec` the final shell, Git executable, and tmux command so durable PID identity
and process-group signals remain valid; PTY/tmux children still require the
host/container/supervisor boundary because ASP does not implement a sandbox.

The client and server apply one shared bounded Quinn transport profile (flow
control, keepalive, datagram buffers, and fair same-priority scheduling), so a
deployment cannot accidentally tune the two ends differently. Each
request/response stream also receives a Quinn-native priority class: PTY
interactive traffic first, bounded `EXEC_SUMMARY`/control acknowledgements
next, and bulk logs/files/workspace snapshots last. Exact `EXEC` remains bulk
because its response may contain a large transcript; legacy whole-file PUT and
large PATCH bodies switch to bulk at the codec offload threshold. This is only
a scheduler hint for bytes already buffered locally; ASP does not implement
its own retransmission or congestion control.

For mTLS rotation, `--client-ca` also accepts a bounded directory of up to eight regular `.der` CA certificates (16 MiB aggregate, symlinks rejected). The daemon reloads that bundle together with the server certificate/key on SIGHUP, so old and replacement client CAs can overlap while existing sessions remain connected.

An unauthenticated connection has a ten-second deadline to complete `HELLO`;
otherwise it is closed and counted in `asp_auth_handshake_timeouts_total`.
This prevents pre-auth peers from exhausting the bounded connection pool while
leaving authenticated idle PTY and event streams persistent.

When an attachment closes, a drop guard snapshots Quinn's UDP byte/datagram
counts, loss, congestion events, and last path RTT/MTU into process metrics.
ASP does not implement a second transport or congestion layer; these counters
make real network behavior visible for SLOs and benchmark analysis.

The daemon can require Quinn's stateless retry on unvalidated Initial packets
with `--stateless-retry` (or `ASP_STATELESS_RETRY=1`). The fail-closed
`--production` profile enables it automatically. Quinn owns the retry-token
cryptography and address validation; ASP only exposes the effective setting and
counts retry attempts and failures as `asp_quic_stateless_retry_enabled`,
`asp_quic_stateless_retries_total`, and
`asp_quic_stateless_retry_failures_total`. This protects a publicly reachable
UDP listener from spoofed handshake amplification at the cost of one extra
initial handshake flight; it does not alter connection-independent sessions,
QUIC migration, or reconnect semantics.

`asp_process_output_attachment_detaches_total` is a process-level counter for
attachments that close after a reader disappears or the shared output-memory
budget remains exhausted. It lets an operator distinguish a normal completed
process from a live-stream observer that needs to resume from durable logs.

## PTY design

When a plain PTY attachment negotiates `pty_state_delta`, the server may send
`PD`-prefixed base-relative row updates instead of a full screen when the
changed rows are smaller. Each attachment owns its last-sent base; the client
applies a delta only on an exact generation/dimension match. A full checkpoint
is emitted at least every 16 updates (and after resize/send failure), so a lost
or reordered best-effort datagram cannot strand the view indefinitely. Rich
state takes precedence, and reliable PTY output remains authoritative.

The PTY attachment, output reader thread, `vt100` screen parser, 64 KiB raw tail, and broadcast channel live in the session store. The actual shell is inside tmux. An attached shell gets exact reliable output frames. The server emits the newest parsed screen/dimensions/cursor as a QUIC DATAGRAM when it fits the peer's maximum datagram size; publication is throttled to roughly 60 Hz and only the newest pending snapshot is rebuilt, so a compiler/test flood does not serialize a full screen for every output chunk. The PTY owner also keeps generation-keyed plain and rich screen caches, so concurrent shell/agent attachments share one parser render per generation; output and resize invalidate both caches. By default the snapshot is the compact plain-text shape. Peers that negotiate the optional `pty_rich_state` capability receive an ANSI-formatted full redraw instead, preserving colors and text attributes while retaining the same replaceable-state semantics. If `pty_rich_compression` is also negotiated, a rich state that would otherwise exceed the path's DATAGRAM budget is zlib-compressed into a bounded `PZ`/`AF` payload; this closes the common MTU-sized-screen gap without changing reliable output semantics. Quinn may evict older queued datagrams. On subscription lag/reconnect the reliable stream sends the newest snapshot in the negotiated form. PTY input uses per-attachment sequence/ACK frames so retries cannot duplicate shell bytes; a reconnect starts a fresh input epoch. The synchronous master writer is serialized per PTY, so a timed-out write retains the single blocking slot instead of allowing reconnect storms to create unbounded blocked tasks; `asp_pty_input_write_timeouts_total` exposes the timeout. The CLI keeps the terminal in raw mode while it retries reconnect/resume with bounded handshakes, forwards Unix window-size signals, then opens a fresh PTY attachment and renders the authoritative snapshot; input typed while disconnected is intentionally discarded because its delivery is ambiguous.

On daemon startup, recovered tmux sessions are queried for their existing pane
geometry before the replacement PTY is attached. The bounded query preserves a
user's last window size instead of resetting a live shell to 24×80; a missing
or stalled session falls back safely and the next client attachment supplies
the authoritative dimensions.

This is real current terminal state, so obsolete intermediate frames can be skipped. The row-delta path reduces bytes for localized plain-screen changes, but it is deliberately conservative and periodically resets to a full snapshot. The rich path closes the previous attribute-loss gap, and the optional negotiated scrollback page preserves a bounded amount of history when a client process is recreated. For tmux-backed sessions, that page uses a bounded `capture-pane` query through the validated tmux/launcher path and is executed off the Tokio reactor; a missing or stalled tmux server degrades to parser-local history. Neither path is a complete Mosh/RoSE replacement: there is no parser/terminal-engine parity with wezterm or speculative local echo. A production integration should evaluate wezterm-term or RoSE rather than expand this small parser into a terminal project.

The short-lived tmux metadata/history helpers run in private Unix process
groups. If a launcher or helper descendant inherits the capture pipe, timeout
cleanup kills the group before joining the reader, preserving the bounded
500-ms control-path deadline instead of allowing a leaked child to pin a
blocking worker.

Workspace Git queries resolve a canonical executable from service-manager-safe
standard paths before `PATH`; deployments with a custom installation can set
the absolute `ASP_GIT_PATH` environment variable. The selected helper's file
identity is retained at startup and revalidated before every invocation, so a
package replacement or path substitution fails closed rather than silently
changing the executable run for an agent. If Git is absent, a non-Git
workspace remains inspectable and the Git fields are omitted rather than
failing the whole semantic query.

## Process and event design

On Linux/Android, optional `--process-memory-bytes` address-space and Unix
`--process-cpu-seconds` limits are installed in each command child before exec
and inherited by descendants; zero disables an individual RLIMIT. A supervisor
cgroup remains required for aggregate daemon-plus-child enforcement and for
non-Linux memory isolation. Linux children also set `PR_SET_NO_NEW_PRIVS` by
default, preventing setuid/file-capability privilege gains; a reviewed trusted
launcher can opt out explicitly with `--allow-process-privilege-gain`. The
configured limits are exposed as `asp_process_memory_limit_bytes` and
`asp_process_cpu_seconds_limit` in health metrics.

Age/size compaction drops exited process records from snapshots after retention while retaining compact command-hash tombstones. Long-lived sessions therefore do not retain every historical command body, and an old request ID cannot silently launch a second process after its result has compacted. The three durable idempotency tables share a 65,536-record per-session budget; existing IDs remain replayable, while a full table rejects new side effects before execution and increments a visible capacity counter rather than weakening exactly-once behavior.

EXEC/SPAWN launches a bounded `/bin/sh` wrapper in the workspace. An
operator may set `--process-launcher /absolute/path` (and repeat
`--process-launcher-arg`) to put that shell behind a reviewed supervisor,
`bwrap`, or site wrapper; `--require-process-launcher` makes the boundary
mandatory at startup. ASP validates the launcher as a non-symlink executable,
canonicalizes it once, binds its filesystem identity, and rechecks that identity
before each spawn; it appends `/bin/sh` itself, while the launcher must `exec`
the child so PID identity and process-group signals remain observable. When a
PTY is opened, the same launcher receives the absolute `tmux` command and its
arguments, so the interactive shell cannot bypass the configured boundary.
This is an integration hook, not a built-in sandbox: the operator must verify
that the launcher handles the detached `new-session -d` and attached
`attach-session` command shapes and preserves tmux children over daemon
restarts. A bounded retry window around `capture-pane` handles the brief
empty/failure interval while a tmux server or old client is recovering.
Stdout/stderr are chunked in 64 KiB source units with independent offsets, written to durable per-process logs, and appended as journal events. Persistent monitors batch up to four source chunks (256 KiB) before one log-file sync; the corresponding journal event is appended only after that sync, preserving crash recovery while reducing sync and event overhead. One process is capped at 512 MiB of aggregate stdout+stderr; an over-limit or unreadable log is terminated and produces `PROCESS_EXITED(code=null)` so a subscription has a terminal lifecycle signal. If the response stream disappears, the child and monitor continue. A later resume replays retained chunks or returns a compact snapshot when the cursor is too old. Repeating the same `(session,request_id,command)` returns the original process and replays its retained event range; reusing an ID for another command is rejected. The request mapping is rebuilt from the persisted `PROCESS_STARTED` event after restart. `EXEC_SUMMARY` consumes the same durable output without forwarding each chunk: the monitor keeps only a bounded tail and exact byte counters, then emits one summary before `PROCESS_EXITED`; duplicate summaries use the same journal-to-tail path. This keeps test/build diagnostics useful without making a 10 MiB (or larger) log a mandatory agent response payload or a per-chunk live queue allocation. The short-lived command body is written without an extra fsync because the durable start intent and `PROCESS_STARTED` event carry the recovery contract; the committed intent is removed lazily and startup clears a stale committed entry. `PROCESS_OUTPUT_STREAM` reads a snapshot-length, at-most-64 MiB range from the private stdout/stderr file under `process:read`, so exact diagnostics remain addressable after journal compaction without replaying every output event. Long-lived PTY, event-subscription, and port streams revalidate the authenticated principal at a bounded one-second cadence; revocation closes the stream rather than allowing an already-open attachment to outlive policy.

An optional `--exec-timeout-seconds` policy applies only to attached
`EXEC`/`EXEC_SUMMARY` requests. The absolute deadline is persisted beside the
process artifacts, survives daemon restart, terminates the process group after
a short SIGTERM grace period, and reports conventional exit code `124`;
detached `SPAWN` remains intentionally long-lived. The default is disabled for
compatibility, so unattended deployments should set it explicitly or enforce
an equivalent supervisor worker policy.

The `/metrics` endpoint also publishes a bounded
`asp_process_launch_duration_us` histogram and
`asp_process_launch_failures_total`. The launch timer covers the admitted
process-start transaction (durable preparation, policy recheck, and
spawn/bookkeeping) while excluding response draining and child lifetime, so
operators can separate launch contention from transport/request latency
without adding labels or allocations to the hot path.

The durable process wrapper is written and synced once per session, then
hard-linked into each process record. Each record still has its own wrapper
pathname for PID identity and recovery checks; only the repeated wrapper
payload write/sync is removed. Existing per-process wrappers remain valid for
mixed-release recovery, and filesystems without hard-link support use the
previous private-file path. The shared template is exact-byte and private-mode
validated before a new process is admitted, so this optimization cannot turn a
tampered wrapper into an accepted launch.

The in-memory journal is bounded to 20,000 events/64 MiB, while the disk WAL remains the crash-recovery source of truth. Startup replays frames incrementally, so a large WAL does not require a second full-file allocation. The WAL uses CRC32 frames, 64 MiB segments, a 4 GiB per-session safety quota, atomic snapshots, and a background compactor; a snapshot-prefix replay keeps events written after a crash boundary. Startup also validates nonzero, strictly increasing event IDs and requires a contiguous sequence after a snapshot boundary; a gap or out-of-order frame is quarantined instead of being silently skipped. An incomplete tail is recoverable only in the active append log; a torn frame in an already-rotated segment is quarantined and blocks startup so acknowledged history is never silently dropped, and a symlink masquerading as a rotated segment is rejected. Lifecycle/file events are synchronously durable; high-rate output and PTY-state frames use a bounded 256 KiB/25 ms group-commit window and are recovered from per-process logs where available. On the normal multi-thread Tokio runtime, the synchronous append/fsync section yields its reactor worker through `block_in_place`; event numbering, commit ordering, and the current-thread startup/test fallback remain unchanged. Process launch preparation (private intent/log files, metadata/fsyncs, and fork/exec) uses the same bounded blocking path, so a slow disk or host process table cannot pin a QUIC reactor worker while the session commit lock is held. File mutations, signals, and session opens carry persisted request IDs/hashes so safe retries return the original result. Current clients use bounded `RESUME_BEGIN`/`RESUME_EVENT`/`RESUME_END` frames, while the legacy one-frame form remains for compatibility. `SUBSCRIBE_EVENTS` takes a cursor while holding the same commit boundary as journal append, sends the retained backlog, then follows the live broadcast queue. With the optional `event_consumer_leases` capability, an additive `SUBSCRIPTION_CAUGHT_UP` marker ends the captured backlog so a filtered client can safely ACK the full boundary. A lagged subscriber receives a recoverable error and must use resume. When the optional `event_consumer_leases` capability is negotiated, cumulative named ACKs are persisted in a checksummed sidecar with a seven-day lease; compaction defers while an unexpired consumer is behind, then resumes after it catches up or expires. Durable replay itself is bounded at 100,000 events or 64 MiB of event payloads; if a tail exceeds either limit, the server leaves the WAL untouched and returns the current snapshot with `compacted=true` instead of materializing an unbounded replay vector. Live replay validates framing, checksums, and contiguous IDs without rebuilding the startup process/request/artifact maps, so a busy long-lived session cannot turn one reconnect into unbounded state allocation. The daemon bounds total sessions, per-principal sessions, active connections, per-principal active connections, in-flight request streams, active subscriptions, and aggregate decoded frame memory (256 MiB) so one identity cannot consume all long-lived slots or multiply the 128 MiB frame ceiling into unbounded allocations. Request bodies have a size-aware minimum-rate deadline after their length prefix (10 seconds for small frames, roughly 64 KiB/s for larger ones, five-minute cap), while an idle PTY can remain open. Synchronous compaction, retention, and staging cleanup run on Tokio's blocking pool so large housekeeping passes do not stall QUIC I/O.

The maintenance interval skips missed ticks after a slow pass instead of
issuing catch-up sweeps back-to-back. Housekeeping therefore stays low
priority under disk pressure while live QUIC requests and process monitors
retain reactor capacity.

PTY attachment creation uses the same multi-thread blocking handoff as
process launch. Opening/attaching tmux and setting up a PTY can therefore
take a slow filesystem or fork/exec path without pinning a QUIC reactor
worker; the durable session commit boundary remains unchanged.

Per-principal request streams are bounded independently of the process-wide
4,096-stream semaphore: each authenticated identity may hold at most 512
concurrent streams by default. This protects long-lived PTY, event, and port
forwarding streams from one identity monopolizing the daemon; lease release is
RAII-backed and therefore also covers abrupt task or connection teardown.

## File design

Paths are workspace-relative and reject absolute/parent traversal, the reserved `.asp` subtree, and symlink escapes (existing reads canonicalize and use a final-component `O_NOFOLLOW` open; writes validate the nearest existing parent). PUT and PATCH use temporary-file rename. PATCH verifies SHA-256 of the exact base and replaces the middle between a shared prefix/suffix; its bounded old-file read and patch construction happen before the workspace gate, while the final commit rechecks the base hash so a concurrent writer becomes an explicit conflict. Optional `FILE_PATCH_RANGES` performs several sorted, non-overlapping original-file replacements in one pass under the same hash guard and atomic commit; the client only sends it after negotiating `file_patch_ranges` and only when its conservative estimate beats a full PUT. Whole-file PUT now accepts an optional expected base hash and an explicit blind-overwrite bit; absent a hash, an existing target is create-only and returns `precondition_required` unless the caller opts into `allow_blind`. The same precondition is checked for streamed uploads at the final commit. Conflict is explicit. Replacements preserve an existing regular file's ordinary Unix mode bits, including executable bits; new files remain private (`0600`) because v0 does not carry a mode field. File commits take a daemon-wide workspace mutation gate before the per-session transaction lock, so two agents sharing a workspace cannot interleave rename/journal boundaries; the gate is held only for the final commit, not while a streamed upload transfers bytes. A workspace-shared monotonic file-version clock is rebuilt from all session snapshots/WALs and advanced at that same commit boundary, so `FileStored.version` and subsequent reads are consistent across sessions. Direct PUT staging has a cancellation guard that removes an abandoned `asp-tmp-*` file if its request task is aborted. This serializes commit ordering but does not merge edits: agents that may race must carry a base hash or use an external workspace policy. The persistent JSONL agent adapter can reuse an inspected, hash-matching base to derive a smaller prefix/suffix PATCH or multi-range PATCH without another FILE_GET; uncached or non-beneficial edits retain full PUT semantics. `asp patch` makes the same adaptive choice after its base GET and treats a byte-identical local file as a no-op, avoiding a mutation event and a second request stream. Legacy file reads/patches remain bounded at 16 MiB; `FILE_GET_STREAM`/`FILE_PUT_STREAM` provide bounded 64 KiB chunks, full-file SHA-256 validation, atomic installation, a durable resumable-upload prefix, and a 1 GiB v0 ceiling. Stream frames apply transparent bounded zlib to large compressible messages; client and server move large-frame compression to Tokio's blocking pool so codec work does not stall interactive control traffic. Content-defined deltas, sparse-file/mode negotiation, and measured codec calibration remain future work.
Large frame encoding also samples three windows of payloads at least 64 KiB
before dispatching zlib; near-uniform high-diversity data skips the codec pass,
while source/log text keeps the strict byte-win compression path. This is a
CPU optimization only and does not weaken framing or decompression bounds.

The artifact store is a separate private subtree below each persisted session.
An artifact is addressed by the SHA-256 of its complete bytes, so uploads can
be retried by request ID and downloads can use exact offset/length ranges
without inventing a second consistency model. Uploads land in a `create_new`
staging prefix, are fsynced at bounded intervals, then rename atomically under
an artifact mutation lock before the `ARTIFACT_CREATED` event is appended.
The live materialized map is updated in the same commit transaction and rebuilt
from the WAL/snapshot on restart. A per-session 8 GiB quota and 1 GiB object
limit bound storage. A duplicate digest already present in a session is
acknowledged before body transfer, avoiding repeated agent/build output bytes.
Committed objects are retained for `--artifact-retention-hours` (30 days by
default). Maintenance leases active readers, appends an `ARTIFACT_DELETED`
tombstone before unlinking, and retries safe orphan digest files on later
passes. Unknown-age records from pre-v15 snapshots are retained. A fresh
session for the same authenticated principal can reuse a verified object from
another session through a local hard link; the destination still gets its own
journal record, name, retention lease, and quota charge. Discovery is filtered
by principal and re-hashes the source while leased, so a digest cannot become a
cross-owner read oracle. Filesystem/link failures fall back to the normal
streamed upload. Artifact metadata queries remain future work.

`WORKSPACE_STATE` batches a bounded tree walk, fixed git queries, literal searches, and selected file reads into one response. It is the prototype's semantic-latency experiment, not a general remote filesystem. The server skips the repository-wide scan when a request asks only for git fields or selected files; scans/searches run on blocking workers so they do not stall QUIC I/O. The scan, selected-file reads, and independent git commands overlap inside the request, while selected-file reads reserve two 16 MiB slots, preserving the 32 MiB aggregate read budget even if files grow after metadata validation. Git subprocesses run through the same validated operator process launcher and inherited command limits as EXEC/SPAWN when configured, disable process-wide credential/config discovery, and are killed/reaped after a 60-second deadline, so a broken repository or helper cannot pin a request forever or bypass the worker boundary. The helper is placed in a dedicated process group; timeout, output-limit, and read-error paths kill that verified group before reaping the direct child. Repository-local Git configuration remains available for repository semantics and is therefore covered by that boundary. Guarded file replacement hashes an existing target with bounded asynchronous I/O before the commit boundary; blind overwrites do not perform that full read, and a target-presence race is rejected before install. Legacy upload-body and download-body digesting, plus selected-file and complete semantic-result hashing, are moved to the blocking pool, so maximum-sized `FILE_PUT`/`FILE_GET`/`WORKSPACE_STATE` requests do not consume a Tokio worker for their CPU pass. Resumable file/artifact staging metadata, symlink checks, private-permission changes, storage-headroom probes, and cleanup are dispatched through the same blocking handoff; body writes and reads remain Tokio file I/O, and the check repeats only at durable 1 MiB boundaries instead of once per 64 KiB chunk. Tree metadata, repeated searches, and small Git results may use bounded watcher-invalidated caches; the native watcher hand-off is also bounded and an overflow permanently disables the cache until restart, preventing an event storm from becoming an unbounded allocation. The complete-result digest index additionally lets a matching recent validator skip the server-side scan/Git/file work entirely. Stale, oversized, or unreliable-watcher paths always fall back to fresh work.

Selected-file buffers now draw from a daemon-wide 32 MiB semaphore rather
than a per-request-only budget. Each read reserves a 16 MiB growth-safe slot,
then retains only its rounded actual size until the `WORKSPACE_STATE` response
has been written. This prevents concurrent agents from multiplying the
selected-file cap while still allowing many small files without deadlocking a
single query.
The response's size-aware encoding buffer also borrows from the daemon-wide
256 MiB response budget until Quinn consumes it, so large tree/git responses
cannot multiply transient allocations across every request stream. Potentially
large response shapes are serialized before the exact permit is acquired; this
closes the short window where a set of concurrent RESUME/workspace requests
could each allocate an uncharged 128 MiB buffer. Bounded control and
interactive responses bypass that serialization gate, avoiding head-of-line
blocking for PTY or session control while retaining the same response-memory
charge before the payload is held for QUIC. The decoded-request and
encoded-response budgets are separate because handlers retain their request
values while they build a response. If either workspace construction or
response capacity stays occupied for 250 ms, a new query fails instead of
pinning a request stream indefinitely; the caller can retry after backoff.

The daemon also maintains a bounded per-workspace tree index for up to two
seconds behind a native filesystem watcher. A stable scan returns an
epoch/generation validator, and a caller that supplies the same validator can
omit an unchanged tree. The same watcher sends regular-file paths through a
bounded/coalescing worker that appends durable `FILE_CHANGED(path,version)`
events to every attached session, including changes made by editors and build
tools outside ASP. ASP-owned atomic writes seed the worker's observation under
the mutation gate, so rename callbacks do not duplicate their `FILE_MUTATION`
event. Watcher errors, event loss, invalidation races, or an old entry disable
the cache for that query and force a fresh scan; this is a latency optimization
and an event hint, not an authority over the filesystem. The JSONL agent
adapter also recognizes a hash-matching byte-identical `file_put` from its
bounded inspection cache as a local no-op (`file_unchanged`) after a zero-byte
metadata/hash check; it leaves the cache valid and avoids a remote
mutation/event entirely.

The tree, repeated-search, and small Git metadata paths now use bounded index
caches; selected-file reads and Git output larger than the cache threshold
remain on demand. Cache entries are invalidated with the same watcher
generation, capped in count and aggregate bytes, and never replace the
fresh-scan fallback.

Large request frames are decoded on Tokio's blocking pool after their bounded
wire/decoded permits are acquired; small control frames stay inline, while
high-entropy plain request bodies at or above 64 KiB use the same off-reactor
path. The client applies that threshold to request-frame encoding; compressed
responses and genuinely large plain responses (at least 256 KiB) are decoded
off-reactor, while the ordinary uncompressed 64 KiB stream-transfer chunks stay
inline to avoid one blocking task per chunk. These choices apply on both
negotiated versions. On the server, Postcard response serialization
uses `block_in_place` on the normal multi-thread runtime (with a current-thread
fallback), and v17 compression still crosses the blocking pool at the same
threshold. This keeps bulk file/log transfers from delaying interactive control
traffic without changing framing or memory admission.

## Port forwarding

`PORT_OPEN` requires the session owner's `ports:open` scope and connects only to a literal loopback target on the server (`localhost`, `127.0.0.1`, or `::1`). The client binds a local listener and opens one QUIC bidirectional stream per accepted TCP flow; after `PORT_READY`, the stream carries raw bytes with independent half-close/reset behavior. The listener reconnects and resumes the session for new flows after a transport loss; an already-open TCP flow is closed because replaying arbitrary bytes would be unsafe. Operators may install an exact loopback target allowlist with repeated `--port-target HOST:PORT` flags; unlisted targets are rejected before the daemon dials them and policy occupancy/denials are exported as metrics. Omitting the option preserves the development default. This deliberately leaves non-loopback policy, NAT traversal, and relaying to Tailscale or another network layer. Reverse forwards and port leases remain v1 work. Forward payloads are charged to the principal's rolling request/response byte budgets, and the credential is revalidated at a bounded one-second cadence even while a flow is idle.

## Failure handling

- QUIC path change: Quinn migration/path validation.
- QUIC connection loss: new connection + ASP resume.
- duplicate EXEC/SPAWN: request mapping is journaled with `PROCESS_STARTED` and rebuilt on restart; repeated requests replay the original process/result range.
- old event cursor: current snapshot + `compacted=true`.
- wrong state-delta base: discard and await/request snapshot.
- client lag: keep process/PTY alive; bound buffers and compact.
- daemon crash: event WAL tail recovery, process log tail reconciliation, tmux PTY reattachment, and explicit shutdown handling are implemented; the WAL has bounded segments/quota and background compaction, while multi-user authorization and external supervisor integration remain v1 work.

`HEALTH` is an authenticated control operation surfaced as `asp doctor`. It reports protocol version, server implementation, persisted session count, running-process count, active connections, event-log bytes, uptime, request/failure counters, authentication state, and PTY backend. The optional loopback `/ready` and `/metrics` endpoint additionally reports active request streams, resume counts/replay volume/maximum lag, admitted request bytes, encoded response-frame bytes, port-forward payload bytes, byte-budget limits/rejections, global and per-principal connection-limit rejections/limits, per-source failed-HELLO rate limiting, process-output queue occupancy/limit, selected-file and encoded-workspace frame-memory occupancy/limits plus decoded-frame admission rejections, workspace tree/search/Git cache hit/miss and occupancy/limit metrics, complete-result digest hits plus daemon-side digest-cache fast-path hits, PTY plain/rich snapshot-cache hits and renders, audit-sink status, queue drops, writer failures, storage-maintenance pass/failure counters and last-success timestamps, and whether the daemon is draining after SIGTERM/SIGINT. A maintenance pass that fails cleanup or has not completed within the bounded scheduler interval turns `/ready` into 503 with `storage_maintenance_unhealthy`, so retention failures are visible to a supervisor instead of remaining log-only. During a drain `/live` remains available but `/ready` is 503, allowing a supervisor to stop routing new work before the grace timer expires. On Linux with cgroup v2 mounted, it also exposes current/limit memory, aggregate CPU time, and process-count gauges for the daemon's cgroup (including descendants); absent controllers report zero rather than pretending to enforce a limit. Its concurrent probe tasks are capped at 64. It is a readiness diagnostic, not a replacement for service-manager liveness/metrics integration. `aspd` also takes a mandatory filesystem lock so two daemons cannot supervise one state directory concurrently and bounds the total number of in-flight request streams across connections.

Local maintenance uses the same lock: `aspd --list-sessions` loads and prints
bounded session summaries, while `aspd --delete-session UUID` removes only a
quiescent UUID-derived directory. Running children, persisted PTYs, and active
subscriptions block deletion, so cleanup cannot silently orphan a durable
resource or race a serving daemon.
