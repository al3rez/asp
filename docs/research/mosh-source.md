# Mosh source architecture

## Revision inspected

Repository: [mobile-shell/mosh](https://github.com/mobile-shell/mosh), revision `decd9b705eb81626f694335b8d5940538beb06da` (inspected 2026-08-26). This note describes behavior; no GPL source was copied.

## Component map

| Area | Files | Role |
|---|---|---|
| State objects | `src/statesync/completeterminal.*`, `user.*` | Terminal and append-only user-input state; logical diff/apply interface |
| Generic SSP transport | `src/network/networktransport*.h`, `transportsender*.h`, `transportstate.h` | State queues, instructions, ACK/throwaway processing, send timing |
| UDP/crypto/path | `src/network/network.*`, `crypto/*` | Datagram framing, packet sequence, AES-OCB, timestamps, remote address |
| Terminal model | `src/terminal/*` | Parser, framebuffer, cells/rows, display modes and rendering |
| Prediction | `src/frontend/terminaloverlay.*`, `stmclient.*` | Epochs, speculative overlays, notification state |
| Process integration | `src/frontend/mosh-server.cc`, `mosh-client.cc` | PTY and terminal loops, bootstrap inputs, resize and shutdown |

## SSP implementation

`TransportSender<MyState>` retains `TimestampedState` entries. Its first entry is the receiver state known to be acknowledged; later entries are sent candidates. The current object produces `diff_from(assumed_receiver_state)`. The sender packages `old_num`, `new_num`, `ack_num`, `throwaway_num`, and the serialized logical diff into a protobuf transport instruction. Fragmentation is below this layer.

The sender does not blindly choose the last ACK as a base. `calculate_timers()` and `update_assumed_receiver_state()` consider acknowledgements, send time, smoothed timing, and outstanding states. New local states receive monotonically increasing numbers. Empty ACKs, delayed ACKs, collection delay, shutdown retries, and heartbeat timing share the same scheduler.

On receive, `Transport<MyState, RemoteState>` reassembles a complete instruction, processes the peer acknowledgement, locates the named base in `received_states`, clones it, applies the diff, and installs the target only if it advances state. A wrong/missing base or stale target is ignored. `throwaway_num` prunes receiver reference states. The sender also prunes acknowledged sent states.

## State representations and diffing

`Complete` combines parser state, `Terminal`, display, pending terminal actions, input history, and the server’s echo-ack number. Its serialized diff uses host-input protobuf instructions. Terminal actions are generated at the semantic terminal level rather than as a row-string comparison. `init_diff()` is just a diff from a fresh empty terminal of the same dimensions. `apply_string()` parses the instruction list and advances the local model.

`UserStream` is an ordered deque of `UserEvent` values (bytes and resizes). Its difference is the suffix missing from an earlier input history. `subtract()` permits the acknowledged prefix to be discarded. This is an important warning for ASP: “state synchronization” does not mean all states are replaceable. User input is modelled as append-only state specifically so no keystroke is skipped.

The terminal framebuffer contains structured rows and cells, cursor/display modes, renditions, wrapping, fallback Unicode state, title/clipboard-related behavior, and other terminal flags. Equality/diff correctness depends on much more than visible UTF-8 rows.

## Acknowledgement and sequence handling

The protobuf `Instruction` contains:

- `old_num`: diff base;
- `new_num`: resulting state;
- `ack_num`: newest peer state applied;
- `throwaway_num`: earliest reference state the peer still needs.

Packet crypto also has a unique sequence number and compact timestamps. These are different namespaces: packet ordering/sec­urity, object-state numbering, and prediction input-frame numbering must not be conflated. ASP should likewise keep QUIC packet numbers opaque and define application event/snapshot IDs separately.

## Prediction/local echo

`STMClient` adds each input byte to the SSP user state and separately tells the prediction engine. The overlay engine predicts visible insertion/backspace effects against a framebuffer, groups predictions by epoch, tracks sent/acked/late-acked local frames, and resets around hard-to-predict input or bulk paste. Authoritative terminal updates validate and erase overlays. `Complete::set_echo_ack()` advances a server-provided input acknowledgement after the 50 ms eligibility period.

## Roaming

`Connection` stores a `remote_addr`. Authenticated newer packets update it; `mosh-server.cc` reports connection-address changes but the application does not recreate its session. Heartbeats ensure the server hears from an idle client after a path change. The cryptographic sequence check prevents an old captured packet from steering the server back to a previous address.

## Frame-rate control

Constants and scheduling are in `transportsender*`: an 8 ms minimum collection delay, delayed ACK policy, heartbeat, RTT-derived send interval, and a 50 Hz ceiling. Each transmission recomputes the current diff from a useful base. This makes queue length bounded by state history rather than application output volume.

## Lessons and cautions

- Mosh’s reusable core is the algebra: snapshot, diff(base,current), apply(base,diff), identity, and compaction.
- State numbers only work if their scope and reset rules are explicit.
- A state queue must preserve an acknowledged base even while pruning newer intermediates.
- Terminal correctness requires a mature emulator; a “last bytes” buffer is not a terminal state.
- Append-only input and replaceable display need different policies.
- The current implementation mixes transport scheduling with object synchronization in C++ templates. ASP should expose explicit Rust traits/data types and delegate delivery/congestion to QUIC.

