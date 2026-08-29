# QUIC transport findings

Primary sources: [RFC 9000](https://www.rfc-editor.org/rfc/rfc9000.html), especially §2, §4–§10 and §13–§19; [RFC 9001](https://www.rfc-editor.org/rfc/rfc9001.html) for TLS integration; [RFC 9002](https://www.rfc-editor.org/rfc/rfc9002.html) for loss detection/congestion control.

## Establishment and security levels

QUIC combines transport and TLS 1.3 handshakes. A first connection normally reaches protected application data at 1-RTT; a server may send after 0.5-RTT, subject to client-authentication caveats. A resumed client can attempt 0-RTT. Zero-RTT data is replayable and may be rejected even when the connection succeeds, so ASP must restrict it to explicitly idempotent operations (for example, a read-only snapshot request with a nonce) and must detect/retry rejection. `EXEC`, `FILE_PUT`, `SIGNAL`, and session creation must not run from unguarded 0-RTT.

## Connection IDs and migration

QUIC connections are identified by endpoint-selected connection IDs rather than only the UDP four-tuple. After handshake confirmation, a client can send from a new address. The peer detects the new path, performs `PATH_CHALLENGE`/`PATH_RESPONSE` validation, limits amplification to an unvalidated address, and resets congestion/RTT state for the new path. NAT rebinding is handled similarly. Servers do not spontaneously migrate to an arbitrary new address in QUIC v1, apart from the handshake-time preferred-address mechanism.

Connection migration solves a short Wi-Fi→cellular path change while connection state remains alive. It does not make a session durable across laptop sleep longer than idle timeout, process restart, server restart, a lost connection ID context, or a new relay path that cannot preserve the same UDP endpoint. ASP still needs `RESUME_SESSION` on a new QUIC connection.

## Streams and multiplexing

Streams are reliable ordered byte sequences with independent stream IDs and per-stream state. Loss on one stream does not block delivery on another, though all streams share connection congestion control and connection-level flow control. Bidirectional streams fit request/response operations; unidirectional streams fit logs, artifacts, and one-way subscriptions. Stream reset/stop is explicit and application errors are typed with integers.

ASP must frame messages within streams because QUIC does not preserve application write boundaries. Large artifacts should have dedicated streams so their flow-control window and cancellation do not interfere with control traffic. Many tiny EXEC operations can use one bi stream each; a long-lived control stream is useful for session-scoped coordination.

## Flow control and congestion interaction

Receivers advertise connection and stream credit. Flow control protects memory; congestion control protects the network. DATAGRAM frames are congestion-controlled even though they are not retransmitted. Therefore datagrams cannot bypass a congested link; they avoid retransmission and stream head-of-line behavior. ASP should reserve receiving capacity for control, cap journal replay, and avoid allowing a 10 MB log stream to consume all connection credit.

## Loss recovery

QUIC packet numbers, ACK ranges, RTT estimation, loss detection, PTO, pacing, and congestion response belong to the QUIC stack. An application may observe statistics and select supported controllers but must not implement a second retransmission or congestion layer for reliable ASP messages. Application acknowledgement is still needed where it means “state applied/durable,” not merely “packet arrived.”

## ASP consequences

1. A QUIC connection is an attachment to an ASP session, never the session itself.
2. Use streams for commands, input, files, logs, event replay, and port bytes.
3. Use datagrams only where a newer state semantically supersedes an older one.
4. Keep application event IDs separate from QUIC packet/stream identifiers.
5. Reconnect after QUIC connection loss and resume by durable session/event identity.
6. Expose Quinn statistics for measurement, but do not tune congestion algorithms without evidence.
7. Authentication identity must authorize session UUIDs; UUID secrecy is not authentication.

## Limits and open experiments

QUIC over UDP may be blocked; ASP v0 assumes direct reachability or Tailscale. Connection migration must be tested on real path changes, not inferred from localhost rebinding. Reverse proxies may terminate QUIC and therefore change client identity/migration properties. ASP configures a 15-second maximum idle timeout and five-second keepalives: healthy idle PTY/event attachments stay alive, while a dead path is detected quickly enough for the client to reconnect and resume. Deployments should remeasure this trade-off on their mobile/NAT paths. Certificate rotation, 0-RTT replay policy, and server restart persistence remain application/deployment work.
