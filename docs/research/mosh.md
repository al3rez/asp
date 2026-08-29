# Mosh paper: findings for ASP

## Scope and sources

Primary source: Keith Winstein and Hari Balakrishnan, [“Mosh: An Interactive Remote Shell for Mobile Clients”](https://www.usenix.org/system/files/conference/atc12/atc12-final32.pdf), USENIX ATC 2012. The longer contemporary [USENIX ;login: article](https://www.usenix.org/publications/login/august-2012-volume-37-number-4/mosh-state-art-good-old-fashioned-mobile-shell) was used as supplementary context.

## The important abstraction

SSH transports an ordered byte stream. Mosh instead places a terminal emulator at each end and synchronizes an object: the current terminal screen. That change lets the sender skip obsolete intermediate output. If states 10–99 were never rendered and state 100 is available, the useful action is to transform the receiver’s last known state directly into 100—not faithfully deliver every byte that once produced 10–99.

SSP runs in both directions over authenticated UDP datagrams:

- server → client state is the visible terminal object;
- client → server state is an append-only history of input events.

An SSP instruction names a source state, a target state, and an object-defined diff. It is therefore self-contained and idempotent: duplicates can be ignored, reordering is harmless unless the named base is unavailable, and a later update can supersede a lost one. The protocol maintains numbered states, acknowledges received states, and communicates a “throwaway” boundary so peers can discard reference snapshots that will no longer be used.

## Timing and control behavior

SSP does not retransmit an obsolete screen update. It periodically regenerates a diff from the latest receiver state it can reasonably assume to the sender’s current state. Mosh adapts its frame interval to roughly half the smoothed RTT, caps the frame rate at 50 Hz, and uses an 8 ms collection interval so clustered application writes can coalesce. Delayed acknowledgements are normally piggybacked; a 3-second heartbeat discovers roaming and keeps NAT state alive.

This is application-level backpressure. A process producing terminal output faster than the link does not force every byte into a reliable queue. The newest screen remains small, so interrupt input is not stuck behind megabytes of obsolete output.

## Roaming

The server accepts an authenticated packet with a sequence number newer than any previously seen and adopts its source IP/port as the client’s current address. Roaming is therefore immediate and does not depend on an old TCP connection timing out. SSP’s packet sequence also supplies a nonce/replay ordering function for its custom authenticated-encryption layer.

## Speculative local echo

The client predicts ordinary insertion/backspace behavior against a local screen image. Predictions are grouped into confidence epochs; control/navigation keys lower confidence. The server includes an “echo acknowledgement” only after a keystroke has been exposed to the application for 50 ms, avoiding client-side timeout errors caused by server scheduling and network jitter. In the reported traces Mosh rendered about 70% of keystrokes immediately; on the tested EV-DO link median perceived response was under 5 ms versus 503 ms for SSH. Wrong predictions occurred for about 0.9% of keystrokes and self-corrected.

## Why TCP was unsuitable in 2012

The paper’s objection is not simply that TCP was slow. It combined several mismatches:

1. A TCP connection was bound to the address pair and did not migrate on IP change.
2. Ordered reliable delivery forced new interactive bytes behind lost or obsolete bytes.
3. Kernel retransmission timing was not tuned for isolated keystrokes on lossy mobile paths.
4. A byte stream hid application frame boundaries, so the transport could not know that old terminal output had lost its value.
5. SSH performed echo remotely and could not safely speculate at the terminal-state layer.

## Concepts ASP should reuse

- Separate durable application session identity from an individual path or connection.
- Synchronize typed state, not historical bytes, when only the newest state matters.
- Number snapshots and make every delta name its base and result.
- Generate a fresh current-state delta after loss instead of retransmitting obsolete screen frames.
- Keep reliable, append-only input distinct from replaceable output state.
- Make replay, duplicate, and acknowledgement semantics explicit.
- Coalesce rapid state changes and make output production respect interactive latency.
- Treat prediction as a presentation optimization that is always checked against authoritative state.

The most important extension for ASP is to apply this distinction per object. Terminal screen state and presence can be replaceable. Process exit, file writes, command input, and artifact bytes are not.

## What QUIC makes obsolete

QUIC replaces SSP’s custom encrypted datagram substrate, packet-number security machinery, RTT estimator, loss detector, congestion controller, path validation, and most address-roaming logic. ASP must not recreate those pieces. QUIC also supplies independent reliable streams, so reliable EXEC/FILES traffic need not share head-of-line blocking with a terminal stream.

QUIC does **not** replace SSP’s semantic insight. QUIC STREAM retransmits lost bytes because it cannot know they are obsolete. QUIC DATAGRAM supplies unreliable messages but does not define state numbers, base selection, acknowledgement, compaction, or snapshots. Those remain application responsibilities.

## Limits that matter for ASP

Mosh’s screen-state approach intentionally loses scrollback history during bulk output unless another program such as `tmux`/`less` preserves it. AI agents often require complete logs, exact exit status, and byte-accurate artifacts, so ASP cannot use “latest wins” for all output. It needs both a durable journal/log channel and a compact current-state channel.

Mosh is also terminal-only, relies on SSH bootstrap plus an application secret, has no general structured execution or file semantics, and ties persistence to a running `mosh-server`. These are product boundaries, not failures of SSP.

