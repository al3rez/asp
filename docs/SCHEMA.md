# ASP wire-schema and compatibility policy

ASP v0 currently uses protocol version **17** and explicitly accepts the
version list published in `supported_protocol_versions` (`[16, 17]`). Version
16 has the same Postcard request/response shapes with a plain length-prefixed
payload; version 17 adds the fixed `AF` frame envelope and optional fast-zlib
representation. A connection is pinned to one framing mode after its first
decodable request, and a version/mode mismatch fails closed. The advertised
decoded length is bounded by the 128 MiB logical message limit. This is a
deliberately small prototype schema. The canonical registry is checked in and
tested, but the wire format is not yet a public interoperable standard.

## Compatibility rules

1. A change to an enum discriminant, variant fields, required framing, or the
   meaning of an existing field increments `PROTOCOL_VERSION`. The sole
   additive exception is a strictly appended request or response variant
   gated by an optional feature: it may be sent only after that feature is
   negotiated, so peers that do not know the variant continue to receive the
   old shape. The sender must still ensure that the receiver has advertised
   the capability before emitting the appended enum value.
2. Peers compare the version during `HELLO`/`HELLO_FEATURES` against the
   explicit supported-version list and fail closed on a mismatch. The server
   detects the v16/v17 frame mode once, then requires every stream on that
   connection to use the negotiated mode; it never guesses an unknown enum
   shape.
3. Additive capabilities are listed in `SUPPORTED_FEATURES` (required) or
   `OPTIONAL_FEATURES` (opportunistic) and must be negotiated before use. A
   peer may ignore an unknown feature name, but a client must not send an
   optional operation or rely on an optional response marker the server did
   not return. The `event_consumer_leases` capability covers durable named
   ACKs and the `SUBSCRIPTION_CAUGHT_UP` backlog boundary marker. The
   `pty_rich_state` capability selects the additive ANSI-formatted PTY
   snapshot/datagram; peers that do not negotiate it retain the plain
   attribute-free snapshot shape. `pty_rich_compression` is a second,
   independent opt-in for `PZ`-prefixed zlib rich-state datagrams when the
   plain datagram exceeds the path MTU; peers that do not negotiate it never
   receive that marker. The `file_patch_ranges` capability gates the appended
   `FILE_PATCH_RANGES` request; clients derive it only from a cached base and
   fall back to `FILE_PATCH`/`FILE_PUT` when the peer does not advertise it.
   The `pty_state_delta` capability gates `PD`-prefixed base-relative plain
   PTY row datagrams. A delta is usable only with an exact generation and
   matching dimensions; periodic full snapshots provide loss recovery, and
   `pty_rich_state` takes precedence when both are negotiated.
   The `pty_scrollback` capability adds a bounded reliable
   `PTY_READY_SCROLLBACK` response after `PTY_READY` for a fresh attachment.
   It contains only plain-text history rows (up to 256 rows/256 KiB), so
   clients that do not advertise it see the original response sequence.
4. A request carrying a `request_id` is replay-safe only when the operation's
   idempotency contract says so. Schema compatibility never implies side-effect
   safety.
5. A response/event field that is not needed to reconstruct durable state may
   be omitted or replaced by a newer snapshot only after the operation's
   documented recovery contract permits it.

## Current versions

| Version | State | Notable wire change |
|---:|---|---|
| 7 | retired | event subscriptions; no port-forward variant |
| 8 | retired | loopback `PORT_OPEN`/`PORT_READY` |
| 9 | retired | `EXEC_SUMMARY`/`PROCESS_SUMMARY` bounded-tail results |
| 10 | retired | durable `PROCESS_OUTPUT_STREAM` range reads |
| 11 | retired | hash-guarded/explicit-blind `FILE_PUT` and streamed upload preconditions |
| 12 | retired | watcher-backed workspace tree index validator and conditional tree omission |
| 13 | retired | complete `WORKSPACE_STATE` semantic-result digest validator and compact unchanged responses |
| 14 | retired | immutable content-addressed artifact streams, resumable upload prefixes, and bounded range retrieval |
| 15 | retired | durable artifact-deletion tombstones and retention/GC-safe artifact metadata |
| 16 | compatibility | point-in-time `PROCESS_STATE` reads; plain length-prefixed frames |
| 17 | current | bounded transparent zlib stream-frame compression, the `AF` envelope, and additive optional consumer-lease, PTY-rich-state, scrollback, and multi-range file-patch operations |

The version numbers above describe the prototype history; independent
implementations should support only versions they have tested. A v1 release
publishes a machine-readable schema (for example, canonical JSON or
CBOR/Protobuf descriptors alongside the binary encoding), a registry entry for
each version/feature, and a compatibility test matrix covering mixed client
and server releases. The current prototype publishes the canonical registry at
[`docs/schema.json`](schema.json) and bundles the same file in `asp-protocol`;
workspace and packaged-crate tests check its version, feature list, and complete
operation surface against the Rust wire constants.
The registry also lists common request-level error codes and their retry
classification; diagnostic text remains implementation-specific.

## Deprecation and rollout

- Keep old enum variants readable for at least one deprecation window when the
  implementation can do so without ambiguity.
- Prefer introducing a new operation/feature over changing the meaning of an
  existing field.
- During a rolling upgrade, deploy servers that accept the previous version
  before clients that require the new version. If a downgrade would lose
  durable events or idempotency records, reject it rather than silently
  truncating state.
- Record negotiated version/features in safe operational logs and expose them
  in health diagnostics; do not log credentials or command/file payloads.

The current implementation satisfies the fail-closed version check and
feature negotiation rules, publishes the supported-version list, and checks
the registry against the Rust wire surface in CI. The server accepts the
tested v16 peer during a rolling deployment, and the current client prefers
v17 but retries the handshake with v16 when an older daemon cannot parse the
v17 envelope. The release legacy smoke also creates a session and process
through the v16 compatibility mode, restarts the same workspace under v17,
and recovers the finished process status/log through the saved session UUID.
A bounded five-minute endpoint hint avoids repeating that failed probe on
every reconnect and is refreshed after expiry. A real mixed-binary matrix
(including an independently built old release, persisted in-progress/result
states, and rollback) and a formal v1 deprecation window remain release work.
No version is admitted merely because it is numerically adjacent to the
current one.
