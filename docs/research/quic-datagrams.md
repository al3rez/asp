# QUIC DATAGRAM versus STREAM

Primary source: [RFC 9221](https://www.rfc-editor.org/rfc/rfc9221.html), with QUIC transport semantics from RFC 9000 and current Quinn API documentation.

## What DATAGRAM provides—and does not

The extension negotiates a maximum DATAGRAM frame size and carries application messages that are unreliable and unordered. DATAGRAMs share QUIC encryption, authentication, path, congestion control, and connection context. They are not flow-controlled and are never retransmitted. A message must fit one QUIC packet/path-MTU budget; the application must choose reliable fallback or its own fragmentation if larger.

DATAGRAM does not define object identity, sequence numbers, duplicate policy, acknowledgement, expiration, or delta bases. Those are ASP state-sync fields.

Quinn’s non-waiting `send_datagram` is useful for latest-wins state because older queued datagrams may be discarded to make space for the newest. `send_datagram_wait` has the opposite priority—it waits and therefore favors old datagrams—and is usually wrong for a replaceable screen snapshot.

## Decision rule

Use a STREAM if any of the following is true:

- loss changes a side effect or final result;
- every byte/event must eventually arrive;
- the payload may exceed path MTU;
- order matters and a later message does not subsume an earlier one;
- the receiver needs backpressure;
- the action will be replayed after reconnect.

Use a DATAGRAM only if the message has an object/epoch/state number, is independently parseable, fits negotiated size, and the receiver can safely discard it because a newer state or reliable snapshot repairs the view.

## Assessment of the initial hypothesis

| Operation | Decision | Reason |
|---|---|---|
| Authentication/control | STREAM | ordered, security-sensitive, must not disappear |
| EXEC/SPAWN/SIGNAL | STREAM | side effects require idempotency and response |
| File patches/artifacts | STREAM | lossless, often large, flow-controlled |
| Port forwarding | STREAM per forwarded connection | byte accuracy and half-close/reset semantics |
| Durable structured events | STREAM | replayable journal, no gaps |
| Terminal screen state | DATAGRAM + reliable snapshot fallback | latest state wins; old render frames expire |
| PTY input/keystrokes | **STREAM**, not DATAGRAM | lost input is unacceptable, especially for agents |
| Presence/typing/status | DATAGRAM | ephemeral and self-repairing |
| Process CPU/progress gauge | DATAGRAM or coalesced subscription | intermediate samples expire |
| Log/output bytes | STREAM/event journal | exact output and offsets matter |
| “tail -f” UI preview | DATAGRAM possible | only if separately backed by durable log offsets |

The hypothesis is therefore mostly correct, but “terminal” must be split into reliable input, durable history when required, and replaceable screen presentation. A DATAGRAM-only terminal channel repeats Mosh’s scrollback limitation.

## State-datagram envelope

ASP’s eventual envelope should include:

```text
object_id, object_epoch, base_state, result_state,
encoding, expires_after_ms, payload
```

An initialization snapshot may use base zero. A receiver accepts a delta only when its current state matches the base; otherwise it ignores it and waits for a later compatible/full state or requests a reliable snapshot. ACKs can be small datagrams when advisory, but a durable “snapshot installed” acknowledgement belongs on control STREAM.

## Congestion and fairness

Unreliable does not mean uncongested or free. Terminal state still competes with file/log streams at the connection congestion controller. ASP needs application scheduling: cap state frame rate, coalesce dirty objects, bound preview size, and consider separate QUIC connections only if measurements show one connection’s congestion/flow-control coupling harms interactivity.

