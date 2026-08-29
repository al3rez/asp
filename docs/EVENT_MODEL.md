# ASP event model

## Invariants

Each session has one monotonically increasing `u64` event ID space. IDs are assigned when the daemon commits an event to that session journal. Gaps are permitted after compaction but IDs are never reused within a session incarnation. Every event includes session context implicitly, timestamp for diagnostics, kind, and kind-specific payload.

Network delivery order does not define event order; `event_id` does. Clients apply only IDs greater than their durable cursor and tolerate duplicates. A
filtered request/result stream (such as one process's `EXEC` output) may expose
event IDs with gaps; observing its highest ID is not an event ACK and must not
advance a shared consumer past events that stream did not deliver. Retry paths
use the operation's stable request ID, byte offset, or immutable digest instead
of an attachment cursor. Only a complete replay, snapshot boundary, or
subscription catch-up marker advances the durable consumer cursor. This avoids
an atomic sidecar write and fsync on every short command and keeps transport
retries independent from event-consumer progress.

## Event versus state

Events describe durable facts needed for replay/audit:

```text
PROCESS_STARTED
PROCESS_OUTPUT(offset,data)
PROCESS_EXITED
FILE_CHANGED(path,version)
FILE_MUTATION(request_id,request_hash,version,hash)
SIGNAL_APPLIED(request_id,request_hash,result)
TEST_COMPLETED
ARTIFACT_CREATED
ARTIFACT_DELETED
```

Snapshots describe reconstructible current state:

```text
process table + running/exit/output lengths
current file versions and immutable artifact metadata
current PTY screen/tail generation
active subscriptions/ports as appropriate
```

High-rate replaceable samples such as CPU percentage or intermediate terminal screens are not individual durable events by default. The journal may record a coarse `PTY_STATE_ADVANCED(generation)` while storing only the latest snapshot.

### Workspace file observations

`FILE_CHANGED(path,version)` is emitted for regular files changed by an
editor, compiler, formatter, or another process in the workspace. ASP-owned
writes retain their richer `FILE_MUTATION` request/result event instead of
creating a second `FILE_CHANGED` notification. Native filesystem
notifications invalidate the semantic workspace index immediately, then pass
a bounded, coalesced path batch to the journal worker. The worker observes
metadata and, for files up to 16 MiB, a SHA-256 digest so duplicate
rename/delete notifications do not become duplicate events. The event carries
the workspace-shared monotonic path version; it does not carry file contents
or claim that a particular intermediate version was observed.

ASP-owned `FILE_PUT`/`FILE_PATCH` commits record their observation while the
workspace mutation gate is held. This prevents the subsequent atomic-rename
notification from producing a second event. Private `.asp` state and ASP
staging names are ignored. If the bounded hand-off overflows, the daemon
increments `asp_workspace_file_event_drops_total`, disables the watcher-backed
cache, and relies on fresh semantic scans; clients must treat a drop as a
resync condition and use `RESUME_SESSION`/`WORKSPACE_STATE` rather than assume
that every path notification was retained.

Fan-out to multiple sessions is retry-safe: if a WAL accepts a path/version
before a later session fails, the next worker pass completes the missing
journals at that same workspace version and skips the already durable event.
This avoids duplicate `FILE_CHANGED` notifications when one workspace is
shared by several agents and a disk/WAL error briefly interrupts delivery.

## Resume algorithm

Client sends `(session_id,last_event_id)`.

1. Authenticate and authorize session ownership.
2. Read a consistent snapshot boundary `S` and journal retention start `R`.
3. If `last_event_id + 1 >= R`, return events `(last_event_id,S]` plus snapshot metadata.
4. Otherwise return `compacted=true`, a snapshot at boundary `S`, and any events after `S` that raced with snapshot creation.
5. Client installs snapshot, applies events by ascending ID, then ACKs its new cursor when it has a named durable consumer identity.

Snapshot and event reads must have a defined boundary; otherwise an event can be missed between them. v0 takes the session commit lock while reading both the materialized snapshot and the journal cursor, and compaction writes the snapshot before resetting the WAL. A multi-process or highly available store would still need transactional metadata or copy-on-write snapshot cursors; the single-daemon deployment does not expose that race.

## Retention

The materialized in-memory view is bounded to 20,000 events or approximately 64 MiB per session, whichever is reached first. The crash-recovery WAL below `.asp/sessions/<session-id>/` uses CRC32-protected 64 MiB segments plus an active `events.log`, with a 4 GiB per-session safety quota. Oldest in-memory events are evicted, but a cursor at or after the latest snapshot boundary is recovered from the durable WAL before the server reports `compacted=true`; only cursors older than that boundary require the current snapshot. A background compactor writes an atomic snapshot and removes obsolete segments. Durable replay is independently capped at 100,000 events or 64 MiB of event payloads; an over-budget resume returns the current snapshot with `compacted=true` and leaves the WAL untouched. The live validator still checks every scanned frame, checksum, and event-ID boundary, but does not rebuild the startup process/request/artifact maps while collecting a replay tail. The daemon also compacts on the configured `--event-retention-hours` age and prunes exited process logs after `--process-log-retention-hours`; operators should choose these values for their recovery and audit requirements. File versions are the exception to the per-session materialization boundary: all sessions attached to one workspace share a monotonic path-version clock rebuilt from every session snapshot/WAL and advanced under the workspace mutation gate. This makes hash-guarded writes and `FileStored.version` responses comparable across concurrent agents, while event replay itself remains session-scoped.

The server stores named consumer acknowledgements in a checksummed
`event-consumers.bin` sidecar. A lease heartbeat is refreshed by cumulative
ACKs and expires after seven days without contact. Compaction advances the
snapshot boundary only when every unexpired named consumer has acknowledged the
current journal head; an abandoned consumer therefore cannot silently lose
events, while a permanently abandoned lease cannot pin storage forever.

Proposed production tiers:

- lifecycle/file metadata: days or session lifetime;
- process output: size/time quota, then content-addressed compressed segments;
- terminal current states: one/few snapshots only;
- audit/security events: administrator policy;
- acknowledged events: eligible for earlier compaction once no active consumer needs them.

Retention responses must disclose `retained_from_event_id` (v0 now includes it in `RESUMED`). A too-old cursor is normal, not a transport error.

## Snapshots and compaction

Create a snapshot after event-count/byte/time thresholds or before dropping events required to reconstruct current state. Snapshot contains schema version, session epoch, `through_event_id`, resource versions, active process summaries, and content hashes/references for large data.

Compaction rules are kind-specific:

- repeated `FILE_CHANGED` for one path may compact to current version if historical output is not subscribed;
- process start + exit compact to final process summary, but private stdout/stderr logs remain addressable through bounded `PROCESS_OUTPUT_STREAM` ranges while retained;
- terminal states compact to newest full snapshot;
- side-effect requests/results remain for the session lifetime. A per-session
  65,536-record safety budget bounds metadata growth; once full, new
  side-effecting requests fail with `idempotency_capacity` rather than
  silently allowing a retry to execute twice. Future versions may add an
  explicitly negotiated, durable expiry policy.

## Duplicate and idempotency handling

Client event application is `if id <= cursor: ignore; if id == cursor+1: apply; if id > cursor+1: request replay/snapshot`. Events must be designed so applying once is deterministic. `SUBSCRIBE_EVENTS` uses the same cursor: the server snapshots the backlog boundary while holding the session commit lock, sends retained events, then follows a bounded live queue. With the optional `event_consumer_leases` capability, an additive `SUBSCRIPTION_CAUGHT_UP` marker follows the captured backlog so filtered consumers can durably acknowledge the boundary even when no matching event was delivered. A `subscription_lagged` error is an explicit signal to resume from the last locally durable cursor rather than silently losing events. Long-lived subscriptions periodically revalidate the authenticated principal (at most one second between checks); a revoked credential closes the stream and the client must authenticate again before resuming.

Side-effect requests carry `request_id`. The server stores `(session_id,request_id,request_hash,status,result_event_range)` before or atomically with execution:

- same ID + same request returns prior/in-progress outcome;
- same ID + different body is `IDEMPOTENCY_CONFLICT`;
- v0 does not silently expire a record. If the bounded table is full, the
  server rejects new side effects before execution and exposes the limit and
  rejection counter through health/metrics.

EXEC/SPAWN request mappings are journaled in `PROCESS_STARTED` events. File mutations and signals use durable request/result events as well. The same ID and request hash returns the original result; the same ID with a different body is rejected, including after a daemon restart. Session-open request mappings are kept in an atomic `.asp/open-requests.json` table. File writes use a durable intent plus hash-guarded rollback/recovery, and process starts use a durable pre-spawn intent. Whole-file and streamed PUTs also enforce a create-only or explicit base-hash precondition at the final workspace commit; a blind replacement is opt-in and is included in the request hash. Large streamed uploads additionally persist a private request manifest and acknowledged prefix outside the event journal; retries negotiate the prefix offset and only publish a `FILE_CHANGED` event after final hash verification and atomic install. Artifact uploads use the same durable request manifest/prefix pattern, append `ARTIFACT_CREATED` only after an atomic content-addressed install, and rebuild their metadata from the event journal on restart. Retention appends `ARTIFACT_DELETED` before unlinking an object; replay removes its metadata and idempotency mapping, so a collected artifact cannot resurrect after restart. Port leases remain future work.

## Acknowledgement

Application ACK is cumulative per consumer: `(consumer_id,through_event_id)`. It says the client durably applied state, not that QUIC delivered bytes. The optional `event_consumer_leases` capability adds `ACK_EVENTS_CONSUMER`; the daemon persists the cursor and lease heartbeat in a checksummed sidecar, ignores regressions, rejects cursors beyond the journal head, and defers compaction while an unexpired consumer is behind. A seven-day lease expiry bounds abandoned state. Older v17 peers simply omit this capability and retain the advisory `ACK_EVENTS` behavior. The CLI's `--consumer-id ID` stores a separate local cursor under the same locked cursor file, and the background ACK worker coalesces high-rate updates without adding an RTT to event delivery. Without that flag, the historical shared cursor remains suitable for one consumer but not independent subscribers that need every event.

State-datagram ACK is a separate advisory value naming an installed object state. It selects a compact diff base and may be lost. Do not use it to delete durable journal events.

## Crash recovery

Current append/recovery sequence:

1. allocate the next session event ID under the journal lock;
2. append a length-prefixed Postcard event frame to `events.log`;
3. flush the active event file; lifecycle, file, and session events call `sync_data` immediately, while high-rate process-output events use a bounded 256 KiB/25 ms group-commit window and PTY screen generations are coalesced to at most one `PTY_STATE_ADVANCED` marker per 100 ms. Persistent process-log monitors read at most a 256 KiB batch (from 64 KiB source reads) and sync that batch before appending its `PROCESS_OUTPUT` event, so a power loss cannot leave the durable cursor ahead of the source file;
4. apply/update materialized process/file state;
5. publish to the attached stream.

On restart, the server incrementally validates a partial tail after an explicit `ASPLOG`/version header and truncates only bytes proven incomplete at EOF. A complete frame with a bad CRC, impossible length, undecodable payload, zero ID, out-of-order ID, or post-snapshot gap is quarantined under a unique `*.corrupt-*.log` name and startup fails rather than silently discarding committed history. The bounded replay window is retained in memory while every segment is consumed. New logs use a CRC32-protected frame format; legacy v1 logs remain readable without checksums and should be migrated before relying on corruption detection. An unknown log format/version fails startup rather than silently discarding history. A shell wrapper writes an atomic exit-status file; the monitor reconciles an OS PID when no status exists. Recovered PIDs are checked against the private wrapper and, on Linux, the exact `/proc/<pid>/cmdline` arguments plus a persisted `/proc/<pid>/stat` start-time identity before monitoring or signaling; BSD/macOS use an absolute-path `ps` fallback. PTYs use tmux as the external shell supervisor. Process-start intents are durable before spawning; an uncommitted live child is terminated only when its command line and optional start-time identity match ASP's wrapper, while an ambiguous intent blocks startup for operator recovery. Clean daemon shutdown flushes every journal, and persistent process logs provide recovery for output written during the short group-commit window. Identity-bound mTLS exists for multi-principal deployments; certificate lifecycle, per-tenant quotas, and an external child supervisor remain deployment/v1 responsibilities.

## Overflow and backpressure

Subscribers have bounded queues. Durable subscribers that lag switch to journal reads by cursor; replaceable-state subscribers drop intermediate values; a cursor older than retention gets `SNAPSHOT_REQUIRED`. Process output is bounded at 512 MiB per process across stdout and stderr; when the limit is reached ASP terminates the process group and records a terminal `PROCESS_EXITED(code=null)` event rather than allowing an unbounded producer to block the daemon. The `asp_process_output_limit_terminations_total` metric makes this safety termination observable. Quotas are visible events/errors.
