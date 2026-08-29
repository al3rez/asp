# RoSE: architectural assessment

## Revision, license, and sources

Repository: [nikhiljha/rose](https://github.com/nikhiljha/rose), revision `f145dfc383d925d9703d500d24f8f95bf8edcdfd` (inspected 2026-08-26). Read in full: `README.md`, `AGENTS.md`, `doc/spec.md`, workspace and crate manifests, then `ssp.rs`, `terminal.rs`, `transport.rs`, `protocol.rs`, `session.rs`, `pty.rs`, `scrollback.rs`, client/server orchestration, STUN/bootstrap code, tests, and benchmarks.

RoSE is GPL-3.0-or-later and contains tests ported from Mosh. ASP uses it only as architectural prior art; no RoSE source is copied.

## Component diagram

```text
                       SSH bootstrap (optional)
 RoSE client -------------------------------------------- RoSE server
     |                cert + UDP port + STUN hints              |
     |                                                          |
     +==== QUIC/TLS connection (Quinn/rustls, mutual TLS) ======+
     |       bi control stream: hello/reconnect/resize           |
     | <---- QUIC DATAGRAM: SSP screen diff / ACK -------------->|
     | <---- uni stream: oversized SSP frame --------------------|
     | <---- uni stream: append-only scrollback -----------------|
     |                                                          |
 wezterm-term                                              portable-pty
 predicted screen <--- ScreenState rows/diffs ---> authoritative wezterm-term
                                                               |
                                                       detached SessionStore
                                                (PTY + terminal + SspSender)
```

## What RoSE already solves

RoSE is a credible modern Mosh replacement, not merely a sketch. It provides Rust, Quinn QUIC, rustls certificates and mutual TLS, a `portable-pty` shell, dual `wezterm-term` emulators, screen-state diffs over QUIC DATAGRAM, ACK-based rebaselining, oversized-frame fallback to reliable uni streams, a lower-priority reliable scrollback stream, detach/reattach, client exponential reconnect, SSH bootstrap, certificate TOFU/authorization, and best-effort STUN hole punching.

Its `SspSender` retains up to 32 numbered `ScreenState` snapshots, generates a diff from the ACKed state to the latest, and can choose a smaller initialization diff. `SspReceiver` ignores stale targets and wrong bases. Row diffs carry cursor position and terminal height. The server coalesces PTY output, snapshots at a short interval, retransmits from the current ACK base, and resets the sender on reattach so the client gets an immediate full state.

Quinn’s `send_datagram` semantics are a particularly good fit: when its outgoing datagram buffer fills, older unsent datagrams can be evicted to make room for the new one.

## What ASP should not reinvent

- QUIC/TLS, loss recovery, congestion control, migration, datagram framing, stream flow control, and statistics.
- Mature PTY management.
- Terminal emulation; if ASP promises exact screen state it should embed or integrate a maintained emulator.
- The acknowledged-base/latest-state screen-diff pattern.
- Reliable-stream fallback for a state frame larger than path MTU.
- SSH as an optional installation/bootstrap mechanism.
- A separate scrollback/log policy rather than pretending the visible screen is history.

## Can RoSE be the foundation?

Technically, yes for a terminal-centric GPL project. Its modules and tests offer a working base, and Quinn/wezterm/portable-pty choices align with ASP. Legally and strategically, not by default. ASP is currently dual MIT/Apache and aims to be a multi-resource agent protocol; importing RoSE code would require GPL-compatible distribution or a separate process/protocol boundary plus careful legal review.

A cleaner path is to depend on the same permissively licensed building blocks and independently implement the higher-level ASP schema. Interoperating with or contributing an ASP control/EXEC extension to RoSE remains possible if license goals change.

## Extractable architectural ideas

The following are ideas rather than copied implementation:

- separate control, latest-screen, oversized-state, and scrollback channels;
- keep detached process/PTY/terminal objects in a daemon-owned store;
- treat reconnection as a new QUIC connection that attaches to an existing application session;
- reset per-connection state synchronization while preserving authoritative application state;
- couple state datagrams with reliable recovery paths;
- authorize session reattach using the authenticated client identity, not possession of UUID alone.

## What prevents RoSE from being agent-native

RoSE’s top-level abstraction is one interactive terminal. It lacks structured EXEC requests/results, stable process objects, resumable per-process output offsets, a general event journal, idempotency keys, file versions and optimistic patches, watches/subscriptions, artifacts, semantic workspace queries, resource quotas, and multi-agent concurrency controls.

An agent issuing `git status`, search, reads, edits, and tests through RoSE still serializes commands into terminal input and parses human-formatted bytes. QUIC makes that terminal faster and more resilient but does not eliminate semantic round trips or redundant repository scans. RoSE also reconnects screen state by starting fresh SSP state rather than replaying a durable cross-resource event history.

The specification’s claim that lost keystrokes are “naturally retried by the user” is unsuitable for autonomous agents and questionable for humans. ASP input, EXEC, signals, and edits must be reliable/idempotent. Datagrams should carry replaceable presentation state, not commands with side effects.

## Is its SSP general-purpose?

Conceptually yes; concretely no. `SspSender`, `SspReceiver`, `ScreenState`, and `ScreenDiff` are terminal-specific concrete types. Diffs are changed row strings plus cursor and total rows. There is no trait expressing arbitrary snapshot/diff/apply/size semantics, no object ID namespace, and no generic compaction policy.

The mechanism could be generalized by introducing an object key, epoch, state number, base state number, codec identifier, and a trait such as `diff(base,current)`, `apply`, `full_snapshot`, and `supersedes`. That still would not make every ASP resource eligible: file mutations, process lifecycle, and logs need durable ordered events or reliable streams, not latest-wins state.

## Bottom line

RoSE substantially invalidates any claim that “Mosh over QUIC in Rust” is novel. ASP is justified only if measurements show that structured, durable, cross-resource operations improve real coding-agent workloads beyond what RoSE over Tailscale provides.

