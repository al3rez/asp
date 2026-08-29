# Plan 9 / 9P: architectural ideas

Sources: [Plan 9 from Bell Labs overview](https://9p.io/sys/doc/9.html), [Plan 9 papers index](https://9p.io/sys/doc/), and Pike et al., [“Plan 9 from Bell Labs”](https://www.usenix.org/publications/compsystems/1995/sum_pike.pdf).

## Core philosophy

Plan 9 applies three ideas consistently: resources are named like files in hierarchies, 9P is the uniform protocol for accessing them, and processes assemble private namespaces by binding/mounting service trees. Devices, processes, graphics, networks, and ordinary files participate through the same small interface. Location and implementation are separated from naming.

The deeper lesson is not “turn everything into bytes.” It is that a small composable resource interface, stable naming, and per-client views can be more powerful than many bespoke connection mechanisms.

## Translation to ASP

A development session can present a conceptual resource tree:

```text
/workspace/{id}/files/...
/workspace/{id}/git/{status,diff,commits}
/sessions/{id}/processes/{pid}/{status,stdin,stdout,stderr,signal}
/sessions/{id}/terminal/{input,state,scrollback}
/sessions/{id}/tests/{run_id}/{status,events,artifacts}
/sessions/{id}/ports/{port}
/sessions/{id}/agents/{agent_id}/{presence,events}
/sessions/{id}/artifacts/{artifact_id}
```

Names should be stable and capabilities discoverable. A subscription can watch a resource’s version/event cursor. Namespace scoping can restrict an agent to one workspace or a subset of operations.

## What not to copy

ASP should not implement 9P or force every operation into open/read/write/stat. `EXEC` has idempotency, environment, exit, cancellation, and output-channel semantics that deserve typed messages. Port forwarding is a stream. A file patch is an optimistic mutation. Tests and git queries benefit from structured schemas.

The resource hierarchy should therefore be a conceptual naming/discovery layer over typed operations, not a fake filesystem. Byte-oriented “everything is a file” can hide semantics just as an SSH terminal does.

## Useful design constraints

- Every resource has a stable ID/path independent of the transport connection.
- Resource type and supported verbs are discoverable.
- Views can be scoped per session/agent without changing server internals.
- Watches report versions/events, not repeated directory polling.
- Common operations share uniform error, version, cancellation, and authorization rules.
- Aggregated resources such as `/workspace/state` may replace several commands while remaining composable.

The Plan 9 influence is strongest in ASP’s resource model and namespace discipline, not its wire encoding.

