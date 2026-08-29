# ASP protocol v0

## Status and encoding

This document defines intended semantics; the Rust prototype implements the listed v0 subset with wire protocol version 17. The binary accepts the explicit `supported_protocol_versions` list (`[16, 17]`); unknown versions and untested enum shapes are rejected. Version 16 uses the same Postcard request/response shapes with a plain length-prefixed payload, while version 17 adds the `AF` envelope. A QUIC connection is pinned to one framing mode after its first decodable request, and the server rejects a version/mode mismatch. Version-17 frames carry an encoding marker and decoded length; frames at least 1 KiB use fast zlib compression only when it produces a strict byte win, while incompressible frames remain plain. Decompression is bounded by the advertised length and the server charges both wire and decoded buffers to its aggregate memory budget. The binary encoding keeps log/file byte strings compact even before compression. `HELLO_FEATURES` negotiates required capabilities (`resume_stream`, `file_stream`, `file_upload_resume`, `workspace_state`, `pty_datagram`, `principal_scopes`, `event_subscriptions`, `port_forward`, `exec_summary`, `process_log_stream`, `file_preconditions`, `workspace_index`, `workspace_digest`, and `artifact_stream`) plus the optional `event_consumer_leases`, `pty_rich_state`, `pty_rich_compression`, and `file_patch_ranges` capabilities; the legacy `HELLO` form remains available in both tested versions. The `AF` framing is mandatory for all version-17 stream messages and is not an optional feature bit. Version 12 added a watcher-backed workspace tree index validator and conditional tree omission. Version 13 added a digest validator for the complete requested semantic result: when it matches, the server omits repeated tree/Git/search/file payloads and returns `state_unchanged=true`. Version 14 added immutable content-addressed artifact upload, resumable upload staging, and bounded range retrieval. Version 15 added durable artifact-deletion tombstones so retention/GC cannot resurrect collected metadata after restart. Version 16 added a point-in-time `PROCESS_STATE` read for detached process checks without replaying a complete session snapshot. Version 17 adds bounded transparent stream-frame compression; peers that cannot negotiate it remain on the tested v16 plain framing. The current client prefers v17 and retries the handshake with v16 when an older daemon cannot parse the v17 envelope. The initial version/deprecation rules are in [`docs/SCHEMA.md`](SCHEMA.md), and the current machine-readable registry is [`docs/schema.json`](schema.json).

## Common rules

The optional capability registry also includes `pty_state_delta` and
`pty_scrollback`; both are negotiated extensions and never change the legacy
response sequence when a peer does not advertise them.

For version-17 frames at least 64 KiB, the encoder takes a conservative sample
of three payload windows and skips the zlib invocation only for near-uniform
high-diversity bytes. This keeps binary/artifact transfers from paying codec
CPU while preserving the existing strict byte-win check for source and log
payloads.

- `session_id` is a UUID independent of any QUIC connection.
- The CLI's ordinary `connect` path is idempotent against its local cursor: it
  performs the connection/authentication handshake and reuses the saved session
  UUID without replaying the journal. The explicit `resume` command performs
  journal/snapshot recovery when missed events are needed. A fresh durable
  identity requires an explicit `--new`; this prevents repeated daily
  invocations from silently orphaning processes. The replaced session remains
  durable until an operator removes it through the session-admin command.
- `request_id` identifies a side-effect request. EXEC/SPAWN mappings are rebuilt from the durable process journal; OPEN_SESSION, FILE_PUT, FILE_PATCH, FILE_PATCH_RANGES, and SIGNAL also persist request hashes/results so safe retries remain idempotent after a daemon restart.
- `event_id` is monotonically increasing within one session, never global.
- Paths are UTF-8 workspace-relative names in v0.
- `HELLO` carries the bearer token when token authentication is enabled; a connection must authenticate before any other request. A server may map tokens to named principals and scopes using its JSON principals file; each durable session records its owner and every resource request is authorized against it. In mTLS mode, the TLS handshake authenticates the client certificate and `HELLO` carries no secret; the server maps the leaf certificate SHA-256 fingerprint to a named principal/scopes record.
- Commands are shell commands by design and execute as the authenticated service account inside the configured workspace. ASP is not a sandbox.
- Errors have a stable machine code plus diagnostic text; request-level
  validation errors finish their stream so callers can classify them without
  treating a reset as an ambiguous side effect. Common codes include `authentication_required`,
  `forbidden`, `server_busy`, `principal_byte_budget`, `invalid_cursor`,
  `invalid_ack`, `invalid_signal`, `unknown_process`,
  `process_not_running`, `process_spawn_failed`, `file_not_found`,
  `invalid_path`, `invalid_range`, `file_changed`, `file_too_large`,
  `invalid_sha256`, `invalid_patch`, `precondition_required`,
  `version_conflict`, `idempotency_conflict`, `idempotency_capacity`,
  `artifact_not_found`, `artifact_too_large`, `artifact_hash_mismatch`,
  `artifact_quota`, `invalid_artifact_id`, and `invalid_artifact_name`.
  During graceful daemon shutdown, newly opened request streams receive
  `server_draining`; already-admitted long-lived PTY/event/port streams may
  finish within the supervisor's grace period and reconnect from durable state.
  Clients may retry only when
  the code's operation contract says the request is safe or idempotent; a
  typed application error is not evidence that a side effect is unknown.
  Each session also has a bounded durable idempotency table (65,536 request
  IDs across EXEC/SPAWN, file mutations, SIGNAL, and artifact uploads). Existing IDs remain
  replayable; once the table is full, new side effects fail before execution
  with `idempotency_capacity` so the server never trades exactly-once
  behavior for an unbounded metadata map. The limit, aggregate occupancy, and
  rejection counter are exported through health/metrics.
  PTY attachment validation uses `invalid_pty_size` and reports unavailable
  tmux/backend setup as `pty_unavailable`.
  Streamed uploads additionally use `invalid_chunk`, `invalid_offset`,
  `incomplete_upload`, and `upload_staging_failed` for caller-actionable
  failures; artifact streams use the corresponding artifact-specific codes.
- A server may reject a new stream with `server_busy` when its global in-flight request budget is exhausted; clients should retry after backoff without assuming the operation ran. The daemon also bounds aggregate decoded request-frame memory to 256 MiB and refuses a frame with a retryable temporary error if it cannot acquire capacity within 250 ms; `asp_frame_memory_rejections` exposes that pressure. After a frame length prefix, bodies must arrive at roughly 64 KiB/s: small frames get a 10-second floor, larger frames get a proportional deadline capped at five minutes. An idle PTY prefix remains allowed to wait, while the initial request prefix itself has a 60-second deadline.
- Response frames use the same roughly 64 KiB/s minimum-rate contract (10-second floor, five-minute cap) for each encoded frame. If a peer stops consuming, the stream is detached and its response-memory permit is released; `asp_response_frame_write_timeouts_total` records the event. This is a per-frame backpressure bound, not a lifetime limit for a healthy PTY/event attachment.
- An unauthenticated stream uses the shorter HELLO deadline for its initial
  request prefix; a connection that does not authenticate is closed after ten
  seconds. Authenticated long-lived PTY/event/port streams use the normal
  request-header policy and periodic credential revalidation.
- The daemon enforces both a process-wide active-connection cap and a per-principal active-connection lease (32 by default). A principal that has exhausted its lease receives `principal_connection_limit` during HELLO; the lease is released when the connection and all of its request streams have ended. This prevents one identity from starving other identities while retaining connection-independent sessions.
- After authentication, each principal is also limited to 512 concurrent request streams by default. This is intentionally separate from the process-wide 4,096-stream cap because PTY, event, and forwarding streams can stay open for a long time. A rejected stream receives `principal_request_stream_limit`; the lease is released when its handler exits, including abrupt disconnects. `/ready` and `/metrics` expose the rejection counter and limit.
- After authentication, decoded request-frame bytes are charged to a rolling per-principal/per-operation budget (4 GiB per minute by default, configurable on `aspd` with `--principal-request-bytes-per-minute`). This includes continuation frames for streamed uploads and PTY input/resize. A rejected frame returns `principal_byte_budget`; clients should back off or wait for the window rather than retrying immediately. The budget is an admission guard, not a replacement for QUIC flow control or congestion control.
- Transport arrival is not application acknowledgement. An event ACK means the client has durably advanced its resume cursor.
- The CLI uses a saved session UUID directly for ordinary one-shot operations to avoid a redundant preflight `RESUME`; callers that need missed journal events invoke `RESUME_SESSION_STREAM` explicitly. On a transport retry, every retryable request reconnects after `HELLO` and retries directly. Point-in-time reads carry their own range/digest, while mutating requests carry stable request IDs and durable idempotency records; process output retries also carry byte offsets. Replaying the whole journal before each retry would add a round trip and can be unbounded relative to the requested result. Only explicit event consumers (`asp resume`/`asp events`) replay the journal.
- Filtered EXEC/SPAWN/file result streams never advance the durable event-consumer cursor. They are response attachments, not acknowledgements; the durable cursor moves only after a complete replay, snapshot boundary, or subscription catch-up marker. This keeps reconnect retries independent from event-consumer progress and removes an atomic sidecar write/fsync from every short command.
- One-shot client operations also have a five-minute response-read deadline. This is separate from long-lived PTY/event/port attachments, which remain open while QUIC is healthy and reconnect/resume after transport loss; a timed-out one-shot with a stable request ID is safe to retry because the server's idempotency table deduplicates the side effect.
- Request-level reconnect retries use a bounded 90-second recovery window by default (the client can tune it with global `--reconnect-timeout-ms`, capped at ten minutes) before returning a transport error. This covers normal laptop sleep and path handoff outages without making a failed peer pin an agent forever; interactive PTY and event subscriptions retain their separate cancellable/indefinite loops. A successful retry begins with HELLO and the original operation, not an implicit full-journal replay.
- Durable replay is bounded independently of the on-disk WAL: when a resume or subscription would exceed 100,000 events or 64 MiB of event payloads, the server stops collecting the tail and returns the current materialized snapshot with `compacted=true`. The WAL is not modified, and clients must treat the snapshot as authoritative rather than expecting every intermediate event. This is the same recoverable state-synchronization contract used when the in-memory journal has evicted old events.

### Persistent agent adapter

The `asp agent SERVER` CLI mode is a local JSONL adapter (adapter API version
1) over the binary ASP
protocol. It keeps one authenticated QUIC connection and one durable session
open across many requests, which removes a client process and handshake from
the common coding-agent path. The adapter is deliberately not a second wire
protocol: it translates each input object into the existing `EXEC` or
`EXEC_SUMMARY` request and emits structured local output:

```json
{"id":"t1","op":"exec","request_id":"<uuid>","command":"git status"}
{"type":"started","id":"t1","request_id":"<uuid>","process_id":"<uuid>","event_id":101}
{"type":"output","id":"t1","process_id":"<uuid>","stream":"stdout","offset":0,"data_base64":"..."}
{"type":"exit","id":"t1","process_id":"<uuid>","event_id":102,"code":0}
```

Supported operations are `ping`, `exec`, `exec_summary`, `spawn`, `status` (also
`process_status`), `logs` (also `process_logs`), `inspect` (also `workspace_state`), `signal`, `file_get`,
`file_put`, `file_patch`, and `close`. `spawn` returns a durable
`process_id`; `logs` takes that ID plus `stream`, `offset`, and optional
`length`, or `tail_bytes` to fetch only the final bounded bytes from a
point-in-time `PROCESS_STATE` observation. The tail form cannot be combined
with `offset`/`length` and emits offset-addressed base64 `log` chunks plus a
`log_end` marker that can be retried without duplicating chunks. `signal` takes a
`process_id`, an optional `request_id`, and a signal name/number (`HUP`, `INT`,
`KILL`, or `TERM`) and emits `signal_applied` after the durable acknowledgement.
`inspect` maps to the semantic `WORKSPACE_STATE` request and accepts optional
`workspace`, `include_tree` (default `true`), `include_git_status` (default
`true`), `searches`, `read_paths`, `diff`, and `recent_commits` fields. Set
`include_tree:false` when a repository-wide tree is not needed, or set
`include_git_status:false` when Git status is not needed. With no search terms,
`include_tree:false` also skips the repository-wide scan; a search still needs
a bounded file walk even when its tree payload is omitted. The Git subprocess
is skipped when `include_git_status:false`, while the other requested fields
remain available. It
emits one `workspace_state` object; selected file bytes are represented as
`data_base64` instead of JSON integer arrays. `file_get` emits `file_data`,
while a mutating `file_put` or `file_patch` emits `file_stored`; their bodies
are base64-encoded and mutating calls may provide a caller-generated
`request_id` for cross-process idempotency. Output and file data are
base64-encoded and carry absolute per-stream offsets or content hashes where
applicable; after a connection loss the adapter resumes the same ASP request
and emits only the unseen suffix. Input objects are capped at 128 KiB and
unknown fields/operations are rejected with a structured error. The adapter is
an integration convenience; authorization, durability, and retry semantics
remain those of ASP itself. When a guarded `file_put` follows an inspection
whose exact hash-matching base is still in the adapter cache, the adapter first
performs a zero-length `FILE_GET_STREAM` metadata/hash check. A matching
byte-identical replacement then emits `file_unchanged` locally and sends no
mutation request or body. The event includes the caller's `id`, optional
`request_id`, path, SHA-256, and current file version;
because no server-side mutation occurs, it has no new workspace version or
journal event. If the bytes differ, the adapter chooses the cached contiguous
PATCH only when it is materially smaller; otherwise it sends the ordinary
guarded PUT.

`asp agent-listen SERVER SOCKET` runs that adapter under a supervisor and
serves the same JSONL contract over a private Unix socket; `asp agent-connect
SOCKET` bridges a caller's stdin/stdout to it. The listener removes the socket
on clean shutdown and bounds queued local output to 64 MiB. It keeps a bounded
four-connection idle pool: sequential socket clients reuse an authenticated
transport, while concurrent clients lease separate connections. Checkout
validates the durable cursor/session before reuse, so an explicit session
replacement cannot inherit an old connection; this is pooling, not
multiplexing one QUIC connection across unrelated clients.
If all four leases remain occupied for the bounded checkout window, the local
caller receives `agent_connection_pool_busy` and may retry.
The listener admits at most 32 local client tasks; excess callers receive
`agent_client_limit`. On SIGTERM it removes the socket, drains active clients
for up to ten seconds, then aborts only remaining local bridges.

When a token file is rotated while an idle pooled connection remains open,
the adapter reconnects once after the server returns `authentication_required`.
An explicitly supplied bearer token is not reloaded.

The adapter keeps a bounded local cache (16 query shapes, 32 MiB) of the last
complete workspace results. Repeating an identical inspection sends its
`state_digest` validator; if the server returns `state_unchanged=true`, the
adapter reconstructs the complete JSONL result from that cache while the
network carries only the compact acknowledgement. When its native watcher is
healthy, the daemon also keeps a bounded digest-only index for recent query
shapes; a matching validator can therefore skip the server-side scan/Git/file
work as well as the response payload. The index is invalidated on filesystem
events and expires with the same short freshness window as the workspace
tree cache, so a miss always falls back to fresh bounded work. EXEC and
successful file mutations invalidate the cache because arbitrary shell
commands can change
workspace or Git state. Callers that provide `known_state_digest` without a
matching local cache are intentionally fetched in full so the adapter never
emits an incomplete result.

## Channel map

| Abstraction/operation | QUIC mechanism | Notes |
|---|---|---|
| Version/CONTROL request | bi stream | one or long-lived control stream |
| EXEC/SPAWN/SIGNAL/process-log ranges | bi stream | output frames on same EXEC stream; process survives reset; durable log ranges are bounded |
| Event replay/subscription | uni server stream or control bi stream | reliable, ordered by event ID |
| PTY input/resize | reliable bi stream | never DATAGRAM |
| PTY raw output/history | reliable stream | exact bytes when requested |
| PTY screen/current state | DATAGRAM | numbered latest-wins state, snapshot fallback |
| FILE_GET/PUT/PATCH/PATCH_RANGES/WORKSPACE_STATE | bi stream; `FILE_GET_STREAM`/`FILE_PUT_STREAM` for large bodies | version/hash checked; semantic aggregation |
| PORT_OPEN data | one bi stream per connection | reliable byte stream, half-close/reset |
| Artifacts | uni/bi stream | hash + offset resume |
| Ephemeral status/presence | DATAGRAM | loss-safe only |

The prototype applies one shared Quinn transport profile: bounded QUIC
flow-control windows (8 MiB per stream and 32 MiB per connection/send
direction), one-megabyte application-datagram buffers, and fair scheduling
among streams at the same priority. The windows avoid throttling large
logs/files on the expected 100–300 ms development links. These are transport
tuning knobs, not ASP reliability semantics; deployments should remeasure them
against their bandwidth-delay product and memory budget rather than treating
the values as a universal optimum. Both endpoints also use a 15-second maximum
QUIC idle timeout with five-second keepalives: a healthy idle attachment
remains open, while a dead path is declared lost promptly enough for
connection-independent session resume. A deployment with unusually long
sleep/NAT intervals should retest this trade-off before shipping; changing the
bound is a transport-tuning release decision rather than a reason to weaken
session durability.

An ASP daemon may require Quinn stateless retry for unvalidated QUIC Initial
packets with `--stateless-retry`; the fail-closed `--production` profile
enables it automatically. Quinn generates and validates the retry token, so
ASP does not define cryptography or congestion control for this exchange. The
first connection pays one extra handshake flight, while spoofed UDP sources
cannot make the daemon allocate TLS/application resources before proving they
can receive traffic. The effective setting, retry attempts, and failures are
exported as `asp_quic_stateless_retry_enabled`,
`asp_quic_stateless_retries_total`, and
`asp_quic_stateless_retry_failures_total`.

ASP also assigns Quinn's native per-stream priorities: PTY traffic is
interactive, bounded `EXEC_SUMMARY`/control acknowledgements are next, and
bulk logs/files/workspace snapshots are lower priority. Exact `EXEC` remains
bulk because its response can contain a large transcript. Small legacy file
mutations stay on the control lane, while a whole-file PUT or PATCH at/above
the codec offload threshold is classified as bulk. This keeps a large test
transcript or file body from delaying a resize, signal, or session-control
response on the same QUIC connection. Priorities affect only locally buffered
scheduling; Quinn still owns packet loss recovery, pacing, congestion control,
and flow control.

## CONTROL

### `HELLO(version,auth_token?)`

Negotiates protocol version/features and authenticates the connection when the server has a token configured. It returns server implementation/capabilities. No side effect; safe to retry. The token is compared as a bearer credential over the already encrypted QUIC connection. The optional server principals file maps that credential to an owner and explicit scopes; the legacy single-token mode uses the `legacy` owner with all scopes. With `--client-ca` and `--auth-certificates-file`, rustls requires a CA-signed client certificate and the certificate map binds its leaf fingerprint to an owner/scopes record. External SSH/Tailscale identity can still be used to provision or transport those credentials.
When a client uses `--auth-token-file`, each newly established connection reads the file again; an atomic rotation can therefore be picked up by reconnecting a long-lived adapter. An explicit `--auth-token` is static and must be replaced by the caller.

### `HELLO_FEATURES(version,auth_token?,features)`

The extended handshake has the same authentication and version semantics and returns the intersection of the client's requested list with the server's supported feature list. Unknown features are ignored, malformed/oversized lists are rejected, and a client must not use a feature it did not receive in the response. Current clients require the stream/workspace capabilities they depend on and opportunistically use optional capabilities such as `event_consumer_leases`, `pty_rich_state`, `pty_rich_compression`, and `pty_state_delta`.

The optional `pty_scrollback` capability adds one bounded reliable
`PTY_READY_SCROLLBACK` response immediately after `PTY_READY` on a new
attachment. It contains only the newest plain-text history rows (up to 256
rows and 256 KiB), never terminal control sequences. Older peers do not
advertise the capability and therefore receive the original response sequence
unchanged.

### `HEALTH`

Returns an authenticated readiness snapshot: protocol version, server implementation, persisted session count, running process count, active QUIC connection count, event-log bytes, daemon uptime, request/failure counters, workspace digest-hit/cache counters, whether client authentication is required, and the PTY backend (`tmux` when the executable is currently available, otherwise `unavailable`). The CLI exposes this as `asp doctor`; `asp doctor --strict SERVER` fails unless the protocol is supported, authentication is enabled, and the durable tmux backend is available. Passing `--ready-url http://LOOPBACK:PORT/ready` additionally performs a bounded literal-loopback HTTP readiness check, so the client can surface storage headroom, storage-maintenance failures/staleness, audit, launcher identity, authentication-source, and drain failures in the same preflight. A failed `/ready` response includes a bounded `ready_reasons` array with stable policy codes; this field is diagnostic and does not change the authenticated wire-level `HEALTH` shape. Filesystem headroom, audit, maintenance, launcher identity, and supervisor health remain on the loopback `/ready` probe; they are not inferred from this authenticated response. HEALTH is intentionally not an unauthenticated information endpoint.

### `OPEN_SESSION(request_id)`

Creates a durable application session below the server's configured state directory and returns `SESSION_OPENED(session_id,event_id)`. The session UUID is not a credential. In production, owner identity is taken from authenticated connection context. Not safe for unguarded 0-RTT.

### `RESUME_SESSION(session_id,last_event_id)`

Attaches a new connection. Returns `RESUMED(snapshot,events,compacted,retained_from_event_id)`:

- if the cursor is retained, `events` contains every event with ID greater than the cursor;
- if not, `compacted=true`, events may be empty, and snapshot is sufficient to reconstruct current resource state;
- `retained_from_event_id` advertises the oldest event still available in the in-memory replay window;
- duplicate events are allowed across retries; clients deduplicate by ID.

Resume never restarts processes or creates a new session.

### `RESUME_SESSION_STREAM(session_id,last_event_id)`

The production client path uses a bounded stream: `RESUME_BEGIN(snapshot,compacted,retained_from_event_id,event_count)`, exactly `event_count` `RESUME_EVENT(event)` frames, and `RESUME_END(through_event_id)`. Events are ordered by ID and the end cursor is the snapshot boundary. A client can discard an incomplete stream and retry from its previous durable cursor after a transport loss; it only advances its saved cursor after `RESUME_END`.

### `ACK_EVENTS(session_id,through_event_id)`

Declares all event IDs up to the value applied/durable at the client. ACKs are cumulative and idempotent. The legacy form is advisory. When `event_consumer_leases` is negotiated, clients use the additive `ACK_EVENTS_CONSUMER(session_id,consumer_id,through_event_id)` form; the server persists each named cursor/lease heartbeat, rejects a cursor beyond the journal head, ignores regressions, and defers age/size compaction while an unexpired consumer is behind. Leases expire after seven days without an ACK. The CLI uses distinct `--consumer-id` values when following one session concurrently; its background ACK worker coalesces updates for up to 25 ms without adding an RTT to event delivery.

## EXEC

### `EXEC(session_id,request_id,command)`

Starts a process and keeps the request stream open. For persisted sessions, stdout/stderr are redirected to per-process append-only logs and the event WAL; the child and its output can be reconciled after an `aspd` restart. Responses are:

1. `PROCESS_ACCEPTED(process_id,event_id)` after spawn;
2. zero or more `PROCESS_OUTPUT(process_id,event_id,stream,offset,data)`;
3. exactly one retained `PROCESS_EXITED(process_id,event_id,code)`.

If the child cannot be created (including an explicitly configured process
resource limit that the host rejects), the server returns
`ERROR(code=process_spawn_failed)` and finishes the request stream. Clients
must not retry that application error as if it were a transport failure.

Stdout and stderr offsets are independent, byte-based, and monotonic. A stream reset detaches the observer; it does not kill the process. A client may retry the same request ID; the server replays the original process and the client suppresses already-rendered offsets. Resume recovers journaled output. High-rate output persistence is group-committed within a bounded 256 KiB/25 ms window and reconciled from the per-process log after a daemon restart. The aggregate retained output of one process is capped at 512 MiB across stdout and stderr; reaching the cap terminates the process and emits a terminal `PROCESS_EXITED` event with an unknown exit code so subscribers cannot wait forever on an unbounded producer. Live response attachments share a 128 MiB output-memory budget; if it remains exhausted for 250 ms, the attachment may close while the process and durable logs continue, and the client resumes from its last offset/cursor.

Deployments may configure an `aspd --exec-timeout-seconds` wall-clock budget for
attached `EXEC`/`EXEC_SUMMARY` requests. The deadline is persisted with the
process metadata, survives daemon restart, and terminates the command's
process group after a short SIGTERM grace period (with an identity-guarded
SIGKILL fallback). A timed-out command reports the conventional exit code
`124`; `SPAWN` is intentionally unaffected so development servers can remain
long-lived. The default is disabled for compatibility, so unattended
deployments should set it explicitly or enforce an equivalent supervisor
worker policy.

For worker isolation, deployments may configure an absolute
`--process-launcher` executable and repeated `--process-launcher-arg` values.
ASP invokes the launcher followed by `/bin/sh` and the command/wrapper path for
EXEC/SPAWN, and by the absolute `tmux` command plus its arguments for PTY
attachments. The launcher must replace itself with its arguments so durable PID
identity and group-signal semantics remain valid. `--require-process-launcher`
rejects startup without the configured boundary. ASP does not implement or
attest to the launcher's sandbox policy; the operator must verify that the
launcher supports both shell and tmux commands and preserves durable children
across a daemon restart.

### `EXEC_SUMMARY(session_id,request_id,command,tail_bytes)`

Uses the same process, durability, and idempotency semantics as `EXEC`, but
does not send every `PROCESS_OUTPUT` frame to the request stream. The server
still consumes and journals the complete output, then emits one bounded
`PROCESS_SUMMARY(process_id,event_id,stdout_bytes,stderr_bytes,stdout_tail,stderr_tail,stdout_truncated,stderr_truncated)` immediately before the normal `PROCESS_EXITED`. `tail_bytes` is capped at 1 MiB per stream. This is the preferred contract for coding-agent commands such as tests and builds where the verdict, counts, and final diagnostics matter more than retransmitting megabytes of already-durable logs; a later `SUBSCRIBE_EVENTS` or resume can fetch retained bytes when exact output is needed.

### `PROCESS_OUTPUT_STREAM(session_id,process_id,stream,offset,length?)`

When `process_log_stream` is negotiated, this read-only operation fetches a
bounded range from the process's durable stdout or stderr log. It returns
`PROCESS_OUTPUT_STREAM_BEGIN(process_id,stream,total_size,offset,length)`,
64 KiB-or-smaller `PROCESS_OUTPUT_STREAM_CHUNK(offset,data)` frames, and
`PROCESS_OUTPUT_STREAM_END(bytes,complete)`. `total_size` and `length` are a
snapshot boundary captured before BEGIN; bytes appended later are requested by
the next range. v0 permits at most 64 MiB per request and requires the
`process:read` scope. A short read sets `complete=false`, allowing a client to
retry from the returned offset. The files follow the configured process-log
retention policy, so this API remains usable after output events have been
compacted from the event journal. The CLI exposes it as `asp logs`.

### `PROCESS_STATE(session_id,process_id)`

Returns a point-in-time `PROCESS_STATE(snapshot)` for one materialized process.
The snapshot includes the command, running flag, exit code, and durable
stdout/stderr byte counts. It is a read-only operation and does not create an
idempotency record. An unknown or retention-pruned process returns
`ERROR(code=unknown_process)`. This narrow status read lets detached agents
poll a long-running process without replaying the complete session snapshot;
the process-log range API remains the source for exact output bytes.

### `SPAWN`

Same creation semantics but returns after `PROCESS_ACCEPTED`; observation continues through events/subscriptions.

### `SIGNAL(session_id,request_id,process_id,signal)`

Requests a POSIX signal (INT, HUP, KILL, or TERM in v0). On Unix, ASP signals the process group created for the command so descendants do not continue silently. The request ID/hash and durable `SIGNAL_APPLIED` event make retries idempotent, including after daemon restart; the server uses direct `kill(2)` probes/signals rather than spawning a host utility and returns an ACK/error.

### `SUBSCRIBE_EVENTS(session_id,after_event_id,process_id?,include_output)`

Opens a reliable event stream. The server sends `SUBSCRIPTION_READY(snapshot,through_event_id,retained_from_event_id,compacted)` with a consistent current-state snapshot and journal boundary, then retained `EVENT_NOTIFICATION(event)` frames followed by live events. When the optional `event_consumer_leases` capability is negotiated, it sends an additive `SUBSCRIPTION_CAUGHT_UP(through_event_id)` marker after the captured backlog; a client may ACK that boundary even when a process/output filter hid every matching event. `process_id` filters process lifecycle/output/signal events; `include_output=false` drops high-rate output while preserving lifecycle events. `FILE_CHANGED(path,version)` notifications cover external regular-file changes observed by the workspace watcher; ASP-owned writes use the richer `FILE_MUTATION` request/result event and are seeded into the observer so the atomic-rename callback does not duplicate them. File-change notifications carry no file bytes, and clients should refresh semantic state or use a guarded `FILE_GET` when they need contents. The bounded live queue is best-effort: `SUBSCRIPTION_LAGGED` is a recoverable error, and the client must resubscribe from its last durable cursor. Closing the stream detaches without affecting the session or processes. The server revalidates the authenticated principal at most once per second while the stream is open; revocation closes it with `authentication_revoked`. The `asp events` CLI persists its cursor and automatically reconnects/resubscribes after transport loss.

## PTY

### `PTY_OPEN(session_id,rows,cols)`

Creates (detached, on first use) or attaches the named tmux-backed session PTY,
and resizes it if present. The tmux session is the durable owner; the PTY and
`attach-session` process are replaceable views. On clean shutdown ASP first
detaches that client so closing the PTY cannot inject EOF into or hang up the
shell. Returns `PTY_READY(snapshot)`, then reliable `PTY_OUTPUT(generation,data)`
frames. Closing the QUIC stream detaches, not terminates. A client may open a
new connection, resume the session, and issue `PTY_OPEN` again; the current CLI
does this automatically with bounded handshake timeouts and forwards local Unix
window-size changes as `PTY_RESIZE`. While attached, the server revalidates the
authenticated principal at most once per second and closes a revoked stream.
`tmux` is a runtime prerequisite for daemon-restart durability.

When `pty_scrollback` is negotiated, the server sends one additional
`PTY_READY_SCROLLBACK(snapshot)` response immediately after `PTY_READY`.
The snapshot contains only the newest bounded plain-text history rows; it is
not a second live output stream and does not change PTY generation or input
acknowledgement semantics.
For tmux-backed sessions the page comes from a bounded `capture-pane` query
through the same validated executable/launcher used to own the session. A
newly attached or recovering tmux server may briefly return only the viewport;
ASP retries empty/failed captures inside a short total window, then falls back
to parser-local history (when any). History recovery is executed off the QUIC
reactor and cannot block control traffic indefinitely.

The CLI's global `--prefer-pty-delta` option requests the compact plain
snapshot plus `pty_state_delta` capability instead of ANSI rich-state markers;
it is a bandwidth/latency preference, not a different session or reliability
contract.

### `PTY_INPUT`

Legacy reliable ordered bytes are accepted for compatibility. Current clients use `PTY_INPUT_SEQUENCED(session_id,sequence,data)`, with sequence zero at the start of each PTY attachment stream. The server ACKs each accepted or duplicate sequence and rejects gaps; a duplicate is never written twice. A reconnect starts a fresh input epoch rather than replaying an unacknowledged byte whose delivery is uncertain.

### `PTY_STATE` DATAGRAM

The v0 wire form is `(session_id,generation,rows,cols,screen,cursor)` and is
sent only when the complete parsed plain-text screen fits one DATAGRAM; a
reliable `PTY_READY` carries the same current screen after attach/lag. Old
results may be discarded, so this latest-wins snapshot already avoids
retransmitting obsolete intermediate screens. Peers that negotiate optional
`pty_rich_state` instead receive an ANSI-formatted full redraw
(`PtyRichSnapshot`/a `PR`-prefixed DATAGRAM), preserving cell colors and text
attributes without changing the durable session shape. When
`pty_rich_compression` is also negotiated and the rich state is larger than the
path's DATAGRAM budget, the server may send a `PZ`-prefixed zlib `AF` payload;
peers that do not advertise the compression capability continue to receive
only the plain rich form.

Peers that negotiate `pty_state_delta` and do not negotiate `pty_rich_state`
may receive a `PD`-prefixed `PtyStateDeltaDatagram`. It carries
`base_generation`, `generation`, dimensions, cursor position, and a strictly
ordered list of complete replacement rows. A client applies it only when its
current plain screen has exactly `base_generation` and matching dimensions;
otherwise it ignores the replaceable packet and waits for a full checkpoint or
reliable `PTY_READY`. The server keeps a base per attachment, chooses the row
delta only when it is smaller than the full snapshot, and emits a full plain
snapshot at least every 16 accepted updates (and after a resize or send
failure). This periodic checkpoint bounds recovery after a lost or reordered
QUIC DATAGRAM; the feature never treats DATAGRAM delivery as reliable and does
not alter the lossless `PTY_OUTPUT` stream. Rich state takes precedence over
the row-delta form. The client also advances its monotonic replaceable-state
guard from reliable `PTY_OUTPUT(generation, data)` frames and lag-recovery
`PTY_READY` snapshots, so a delayed DATAGRAM or reliable snapshot cannot
repaint an older screen over newer live output. Terminal-engine parity with
wezterm and speculative local echo remain later work; the optional bounded
history page now covers the common reconnect case without attempting to replace
a terminal emulator. All three replaceable datagram decoders enforce an 8 MiB
payload bound and stream sequence fields through bounded visitors (4,096 rows
for plain/delta state), so a forged Postcard count cannot reserve unbounded
memory before malformed input is rejected.

## FILES

### `FILE_GET(session_id,path)`

Returns path, version, SHA-256, and bytes. Reads have no side effect. The legacy whole-frame form is capped at 16 MiB; use `FILE_GET_STREAM` for larger bodies or offset/range recovery.

### `FILE_GET_STREAM(session_id,path,offset,length?)`

Returns `FILE_STREAM_BEGIN(path,version,total_size,offset,length,sha256)`, bounded `FILE_STREAM_CHUNK(offset,data)` frames, and `FILE_STREAM_END(bytes,sha256)`. The server validates the range and hashes the file; a client may reconnect with the same local partial length and request the remaining range. The CLI uses this path for `asp get`, keeps a locked `<local>.asp-download`/`.meta` checkpoint across process crashes, and atomically renames the completed file.

### `FILE_PUT(session_id,request_id,path,expected_sha256,allow_blind,data)`

Atomically stores a whole file and returns the new version/hash. If
`expected_sha256` is present, the server compares it with the target's current
SHA-256 while holding the workspace mutation gate and rejects a mismatch with
`VERSION_CONFLICT`. If it is absent, an existing target is rejected with
`PRECONDITION_REQUIRED` unless `allow_blind=true`; a missing target remains a
safe create-only operation. Repeating the same request ID and body returns the
original result, while reusing it with a different precondition/body is an
`IDEMPOTENCY_CONFLICT`. This avoids an extra read round trip for agents that
already have a workspace-file hash. The CLI exposes this as
`asp put --expected-sha256 <digest>`; resumable upload checkpoints persist the
base hash and `allow_blind` policy so a reconnect cannot change the write
semantics midway through a transfer.

### `FILE_PUT_STREAM(session_id,request_id,path,total_size,sha256,expected_sha256,allow_blind)`

The client sends ordered `FILE_PUT_STREAM_CHUNK(offset,data)` frames followed by `FILE_PUT_STREAM_END`. Chunks are bounded, the server reserves an idempotency record before receiving the body, writes a private durable staging file, verifies the declared size/SHA-256, applies the same `expected_sha256`/`allow_blind` precondition at the final commit, atomically installs it, and records the mutation under the request ID. Repeating the same request ID and digest/precondition returns the original `FILE_STORED` result. The streamed v0 limit is 1 GiB; the legacy whole-frame operation remains capped at 16 MiB. A full session idempotency table is rejected before any upload bytes are consumed.

### `FILE_PUT_STREAM_RESUME_BEGIN(session_id,request_id,path,total_size,sha256,expected_sha256,allow_blind)`

When the `file_upload_resume` feature is negotiated, a retry may use this begin form. The server validates the request hash and returns `FILE_UPLOAD_READY(...,offset)` before accepting chunks. The client seeks to that offset and continues with matching chunk offsets; the prefix is hashed again before commit, so a crash or torn write cannot silently corrupt the result. The CLI persists the request ID in a locked `<local>.asp-upload` checkpoint, allowing a later process invocation to resume as well as an in-process reconnect. A completed request may return `FILE_STORED` immediately, avoiding a second transfer after a lost final response. Staged prefixes are retained through transient disconnects and pruned by the process-log retention policy when abandoned.

### `FILE_PATCH(session_id,request_id,...)`

v0 fields: expected SHA-256, common prefix length, common suffix length, replacement bytes. The server verifies the exact base, rejects overlapping/out-of-range spans, writes atomically, and returns version/hash. `VERSION_CONFLICT` never triggers fuzzy application.

### `FILE_PATCH_RANGES(session_id,request_id,path,expected_sha256,ranges)`

This optional operation requires the `file_patch_ranges` capability. Each range
contains an original-file `offset`, `remove_len`, and replacement bytes. The
server requires a nonempty list of sorted, non-overlapping ranges, verifies the
single expected base SHA-256, materializes all changes in one pass, and commits
the result through the same atomic workspace/version/idempotency transaction as
`FILE_PATCH`. A malformed range, stale base, or output over the 16 MiB legacy
file limit is rejected without a partial mutation. The adapter derives ranges
only from an already cached semantic inspection and sends them only when the
estimated encoded body beats `FILE_PUT`; unsupported peers receive the existing
contiguous patch or full PUT path instead. Equal-length byte runs are found
directly, while a bounded line-aware matcher can keep independent
length-changing source edits separate. If matching exceeds the client's
explicit CPU/memory bounds, the edit collapses to one contiguous range so
later offsets remain unambiguous.

## ARTIFACTS

Artifacts are immutable content-addressed objects intended for test results,
build outputs, and other bytes that should outlive a process. The
`artifact_stream` feature is negotiated before use. An `artifact_id` is the
lower-case SHA-256 digest of the complete object; the server never accepts an
object whose final digest differs from its requested ID. Objects are private to
the owning session and require `artifact:read` or `artifact:write`.

### `ARTIFACT_PUT_STREAM_BEGIN(session_id,request_id,artifact_id,total_size,name?)`

The client opens a bidirectional reliable stream. The server first returns
`ARTIFACT_UPLOAD_READY(...,offset=0)` (or immediately returns
`ARTIFACT_STORED` when the content-addressed object is already present), then
the client sends bounded `ARTIFACT_PUT_STREAM_CHUNK(offset,data)` frames and
`ARTIFACT_PUT_STREAM_END`. The server writes a private staging file, validates
ordered offsets, size, and SHA-256, atomically installs the object under its
digest, and appends `ARTIFACT_CREATED` to the session journal. Reusing a
request ID with the same digest/size/name replays the stored response; a
different body is an `IDEMPOTENCY_CONFLICT`. A duplicate digest in the same
session is naturally deduplicated without retransmitting its bytes. The
maximum object is 1 GiB and the default aggregate per-session artifact quota
is 8 GiB. If the digest is not present in the destination session, the server
may acknowledge it before body transfer by linking a verified object from
another session owned by the same authenticated principal. This cross-session
fast path still appends a destination-local `ARTIFACT_CREATED` event and
charges the destination quota; it never exposes objects across principals.
The source is leased and re-hashed immediately before linking. If the source
is missing, changed, or cannot be hard-linked, the client receives the normal
`ARTIFACT_UPLOAD_READY` response and streams the body.

### `ARTIFACT_PUT_STREAM_RESUME_BEGIN(...)`

After a transport or client restart, the same request ID and metadata ask the
server for `ARTIFACT_UPLOAD_READY(artifact_id,total_size,offset)`. The client
seeks to the returned durable prefix and continues with matching offsets. A
completed request is acknowledged without retransmitting its body. Abandoned
staging is private and bounded; operators should monitor the staging directory
and align `--artifact-retention-hours` with their backup policy. Committed
objects are expired by the daemon's age-based retention pass.

### `ARTIFACT_GET_STREAM(session_id,artifact_id,offset,length?)`

The server returns `ARTIFACT_STREAM_BEGIN(artifact_id,total_size,offset,length,
sha256,name)`, bounded chunk frames, and `ARTIFACT_STREAM_END(bytes,sha256)` on
a reliable stream. `length` defaults to the remaining object and may not
exceed the object or the 1 GiB stream limit. A complete range beginning at zero
is verified against the content address; partial ranges retain the object hash
and exact offset/length metadata so callers can retry without replaying an
already written prefix. The CLI persists locked checkpoint sidecars for full
downloads and atomically renames the verified result. JSONL agent transfers
are capped at 16 MiB and use base64; larger objects use the binary CLI path.

Committed artifacts are retained according to the daemon's
`--artifact-retention-hours` policy (30 days by default). Expiration appends an
`ARTIFACT_DELETED` journal event before unlinking the object; active range
readers are leased and skipped. Unknown-age records from pre-v15 snapshots are
retained, and orphan digest files are collected only after the same age
window. A failed unlink leaves a safe orphan for a later maintenance pass.

### Future `FILE_WATCH`

The existing `SUBSCRIBE_EVENTS` feed already delivers the v0
`FILE_CHANGED(path,version)` form for regular-file changes. A future dedicated
`FILE_WATCH` operation may add explicit create/delete/rename kinds, content
hashes, and a coalescing marker. Queue overflow remains a resync condition:
clients must refresh workspace state rather than assume an event stream is a
complete filesystem journal.

## PORTS

### `PORT_OPEN(session_id,host,port)`

When the `port_forward` feature is negotiated, this opens one reliable
bidirectional byte tunnel to a service on the server host and returns
`PORT_READY(host,port)`. v0 accepts only `localhost`, `127.0.0.1`, or `::1` and
requires the `ports:open` scope; an invalid or unavailable target returns a
typed error. An operator may additionally repeat `--port-target HOST:PORT` on
the daemon to install an exact loopback allowlist. When that option is absent,
the historical development behavior allows any loopback port; when it is
present, only listed normalized addresses are dialed and an unlisted target
returns `port_target_not_allowed` before any TCP connection is attempted.
After `PORT_READY`, the stream carries raw bytes in both directions, with QUIC
stream reset/half-close semantics. ASP does not provide NAT traversal, a VPN,
or reverse forwarding/leases in v0; use Tailscale or a firewall for those
responsibilities. Payload bytes are admitted to the
principal's rolling request/response byte budgets (the same budgets used by
other streamed operations), and the server rechecks the credential at most
once per second while data is flowing. A revoked principal or exhausted budget
terminates the bridge; `asp_port_forward_bytes_total` counts bytes written to
either side after successful admission. `asp_port_target_policy_entries` and
`asp_port_target_rejections_total` expose policy occupancy and rejected target
attempts for deployment alerting.

The client-side forwarding listener reconnects its authenticated session after a
daemon or network loss and accepts new local TCP flows without restarting the
listener. An already-open TCP flow has no durable flow identity or replay buffer
in v0, so it may fail when its QUIC stream is lost; transparent flow resume,
reverse forwarding, and relay semantics are future work.

## WORKSPACE_STATE

`WORKSPACE_STATE(session_id,workspace,include_tree,include_git_status,include_diff,recent_commits,searches,read_paths,known_tree_version,known_state_digest)` returns one coordinated structured response containing requested tree entries, git text, search hits, commit summaries, and selected file bytes with SHA-256 bases. The response includes `state_digest`, a SHA-256 validator over the complete requested semantic result, and `state_unchanged`; when the supplied digest matches, all repeated payload fields are omitted and only the digest/metadata remain. The v0 server excludes `.git`, `target`, and `.asp`, rejects symlink escapes, bounds scans to 20,000 files and search results to 2,000 hits, and invokes fixed git subcommands without interpolating a shell command.

Paths and search terms are capped at 4 KiB, individual file bodies at 16 MiB, selected file bodies at 32 MiB per request, and the aggregate response at 64 MiB. Search lines are bounded before serialization. Large trees, logs, and artifacts require dedicated streamed/range forms; `FILE_GET_STREAM`/`FILE_PUT_STREAM` cover large file bodies in v0.

Selected-file buffers also draw from a daemon-wide 32 MiB memory budget. The
server retains each rounded permit through response serialization, while the
encoded response borrows from a separate daemon-wide 256 MiB response budget
until its QUIC write completes. Potentially large response shapes are
serialized before their exact permit is acquired, closing the transient
uncharged-buffer window for concurrent large responses. Bounded control and
interactive responses bypass that gate so a large workspace/log encode cannot
head-of-line-block PTY or session control; their retained payloads remain
charged to the response semaphore. A new reader fails after a bounded 250 ms
wait when a slow client holds either budget with a retryable `SERVER_BUSY`
response and increments `asp_response_memory_rejections`, so concurrent agents
cannot multiply the per-request cap or stall the reactor.

This operation is intentionally an experiment: it trades one network gate for server-side batching. It does not promise one physical repository scan because stock `git` operations remain separate. `FileStored.version` is a workspace-shared monotonic file clock. When tree/search work is requested, the server maintains bounded per-workspace tree and repeated-search caches behind a native filesystem watcher. Small Git status/diff/log results use the same invalidation generation; large Git output is returned but not retained. The response includes `tree_version` (`epoch`, `generation`) and `tree_unchanged`; a client may send `known_tree_version` to omit an unchanged tree and `known_state_digest` to omit every unchanged semantic field. The digest is recomputed from fresh/cached server-side inputs on each request, is scoped by the caller's identical query shape, and is not an authorization token. Watcher errors, queue overflow, invalidation races, and entries older than two seconds disable the cache path for that query and force a fresh bounded scan, so a cache failure degrades to latency rather than silently serving known-stale metadata. Selected-file fields are still evaluated for every request before a digest match is declared.

## Future abstractions

- `EVENTS`: lease handoff, explicit consumer deletion, and versioned
  workspace/file invalidation on top of the v0 subscription stream. Basic
  durable named cursors are implemented when `event_consumer_leases` is
  negotiated.
- `PORTS`: add policy-configured non-loopback targets, reverse forwards, port leases, and connection quotas around the v0 loopback `PORT_OPEN` stream. The v0 exact loopback target allowlist is implemented; non-loopback and reverse/lease semantics remain future work.
- `ARTIFACTS`: principal-scoped cross-session hard-link reuse is implemented on
  the v0 immutable stream; artifact metadata queries and a shared global object
  index remain future work. Retention leases and garbage collection are
  implemented by the daemon's age-based policy.
- `GIT`: native versioned status/diff/log objects beyond v0's combined `WORKSPACE_STATE` query.
- `AGENT`: presence, leases, intent, conflict notifications, quotas—not autonomous execution policy.

## Idempotency matrix

| Operation | Natural idempotency | Required protection |
|---|---|---|
| HELLO/RESUME/ACK/GET | yes | cursor/version bounds |
| OPEN_SESSION | no | persisted request ID/result mapping |
| EXEC/SPAWN | no | request ID/result cache |
| PROCESS_OUTPUT_STREAM | yes | offset/length bounds; retry from a later offset |
| SIGNAL | sometimes | persisted request ID/result mapping |
| FILE_PUT/PATCH | create-only or hash-guarded; blind overwrite is explicit | persisted request ID + request hash/base version |
| ARTIFACT_PUT_STREAM | content-addressed and immutable | persisted request ID + digest/size/name; durable prefix on resume |
| ARTIFACT_GET_STREAM | yes | immutable digest + bounded offset/length |
| PTY_INPUT | no | ordered input sequence/ack epoch |
| latest-state DATAGRAM | yes | object epoch/result number |
