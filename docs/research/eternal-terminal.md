# Eternal Terminal

## Sources and revision

Repository: [MisterTea/EternalTerminal](https://github.com/MisterTea/EternalTerminal), revision `b74a12efc567dbc1360ac0846f889c945a2eba60`. Read the repository [protocol documentation](https://github.com/MisterTea/EternalTerminal/blob/master/docs/protocol.md), protobufs, `BackedReader`, `BackedWriter`, `Connection`, client/server reconnection, terminal router, SSH setup, and forwarding code.

## Architecture

ET uses three roles: `et` on the client, a user-owned `etterminal` holding the PTY, and `etserver` routing clients to terminals. The client uses SSH to launch `etterminal`; the helper registers a generated client ID/passkey with `etserver` through a local FIFO and returns credentials through SSH. The direct ET data connection is TCP (default port 2022).

The client ID names the persistent terminal. On a new TCP connection, the server returns `NEW_CLIENT` or `RETURNING_CLIENT`. Encryption is application-managed using the passkey established through SSH.

## Reconnect and recovery

`BackedWriter` assigns a monotonically increasing sequence to encrypted packets and retains a bounded backup buffer. `BackedReader` tracks the last complete received sequence. After reconnect, peers exchange `SequenceHeader` values and then `CatchupBuffer` messages containing missing encrypted packets. Partial framed messages are reconstructed, duplicates are avoided by sequence, and the PTY process continues while the network socket is absent.

This is reliable byte/event replay rather than Mosh-style current-state fast-forward. It preserves exact terminal output while retained, but large disconnected output is bounded (`MAX_BACKUP_BYTES` and disconnected-byte limits) and old bytes can crowd interactive data. TCP also lacks native path migration and independent streams.

ET supports forward/reverse TCP and Unix-socket forwarding through typed packets on the same reconnectable channel. Jumphost mode launches helpers at both hops.

## Lessons for ASP

- A session/terminal identity must outlive the connection.
- Reconnect needs explicit receive cursors in both directions.
- Buffer retention and the “cursor older than retained history” error must be protocol-visible.
- SSH bootstrap can reuse installed identity and host configuration.
- A server-side user process/daemon boundary can preserve least privilege.
- Exact replay and latest-state recovery are complementary policies, not competitors.

## What ASP changes

QUIC supplies secure transport, loss recovery, multiplexed streams, and migration. ASP’s journal generalizes ET packet sequence into typed durable events with snapshots/compaction. Terminal presentation can fast-forward while process logs remain exact. Structured EXEC/FILES avoid routing every action through PTY bytes. ASP must also authenticate reattach with a durable identity; a session UUID alone is insufficient.

