# The actual problem

## Thesis under test

Remote development is already technically possible with SSH + tmux, Mosh, RoSE, Tailscale, remote IDEs, and repository-aware agent servers. ASP is only warranted if a structured, durable session layer measurably removes semantic round trips, repeat computation, and redundant bytes for coding agents. “SSH, but QUIC” is not sufficient.

## Workload properties

Coding agents differ from a human terminal user:

- They issue bursts of discrete operations and can label/idempotently retry them.
- They read/search many files and repeatedly ask for git/test/process state.
- They need exact output, exit codes, and artifacts—not merely a correct screen.
- They can consume structured results and avoid ANSI/human-format parsing.
- They may emit or receive 10–100 MB logs where the newest screen is irrelevant but the final summary is valuable.
- Several agents may share one workspace and require causal/version conflict information.
- The client process may restart; persistence cannot depend on one terminal emulator.

## Where existing systems spend work

### SSH

SSH gives secure authentication, exec, PTY, forwarding, and a mature deployment story. Multiple channels can share one TCP connection. With connection multiplexing and a remote agent daemon, it can already avoid most handshake cost.

Its inefficiency arises when the *application* is shell commands: each command/request waits for remote parsing/execution, scans state again, serializes human-formatted bytes, and makes the client parse them. TCP also binds the active connection to a path and preserves every byte in order. `tmux` adds process persistence but not semantic resume, file versions, or structured event cursors.

### Mosh

Mosh solves perceived terminal latency, roaming, and obsolete screen output. It deliberately does not provide exact scrollback, structured exec, file transfer, port forwarding, agent events, multi-object sessions, or server restart persistence. An agent using Mosh still sends terminal bytes and scrapes terminal state. Mosh input/state security and congestion machinery are now duplicated by QUIC.

### RoSE

RoSE already delivers the modern Mosh design in Rust/QUIC with a mature terminal emulator, PTY, state datagrams, scrollback streams, TLS, reconnection, and bootstrap. It is the right baseline for interactive terminal work. It remains terminal-first and lacks a durable cross-resource event journal and semantic workspace operations. Its GPL license is also a deliberate project choice, not a technical defect.

### Remote IDE/agent servers

VS Code Remote, JetBrains Gateway, language servers, and agent-specific servers already expose richer semantics and often run a remote daemon over SSH. They may solve most real workloads without a new standardized wire protocol. Their weaknesses are product coupling, uneven reconnect semantics, and the absence of a common agent-oriented resource/event contract.

## Round-trip model

The following is an estimate for a naive shell-driven inspection. It excludes initial SSH/QUIC handshake and assumes each command is sequential because later steps depend on prior output.

| Action | Shell requests | Minimum application RTT gates | Avoidable repeated work |
|---|---:|---:|---|
| `git status`, `git diff`, 10 commits, tree | 4 | 4 | repository/index walks and text formatting |
| 3 searches then 8 file reads | 11 | 11 | path lookup and separate framing |
| edit 3 files then inspect diff | 4+ | 4+ | whole-file writes or patch parsing |
| run tests, inspect result/artifacts | 2+ | 2+ | log transfer and summary parsing |
| poll dev server 10 times | 10 | 10 | unchanged status responses |
| reconnect and rediscover processes/git | 4–8 | 4–8 | state that server already knew |

At 100 ms RTT, 25 serialized gates impose 2.5 seconds of network blocking even if commands are instantaneous. At 300 ms, the floor is 7.5 seconds. Connection transport tuning cannot remove this floor. A `WORKSPACE_STATE` query returning tree/status/diff/recent commits in one response reduces four gates to one, saving approximately `3 × RTT` and allowing one coordinated repository scan. A subscription replaces repeated polls with zero request RTTs when nothing changes.

The implemented agent fixture subsequently validated the direction, though not the simple magnitude: `WORKSPACE_STATE` replaced tree + git status + three searches + three reads (eight serial operations) with one gate. Across the complete fixture ASP used 13 application gates versus 18 SSH ControlMaster channels. At approximately 100 ms RTT, one trial measured 3.08 s versus 8.00 s blocked in network operations. The larger-than-`5 × RTT` difference also includes per-channel/process overhead, initial/recovery handshakes, and implementation differences, so it is not attributed solely to propagation delay. See `BENCHMARKS.md` and the raw rows.

## Byte waste model

- ANSI escapes, prompts, command echoes, and human column formatting are redundant for agents.
- Re-running `git status` or scans resends unchanged results instead of a version token/empty delta.
- Reconnect often replays terminal scrollback or forces full rediscovery.
- Huge logs may cross the network even when the agent needs only exit code, failing test names, and an artifact reference.
- Whole-file PUT is wasteful for a localized edit when both ends share an exact base.

Conversely, semantic protocols add schema/framing bytes and server memory/index work. For a one-off `cat small-file`, SSH may be smaller and simpler.

The measured 10 MiB full-log workload confirms the converse: after switching ASP from JSON to compact binary framing, ASP still used 10.87 MB of loopback interface traffic versus SSH's 10.61 MB. If the caller requires every log byte, semantic aggregation cannot remove the payload; `EXEC_SUMMARY` is deliberately a different workload contract, returning counts and a bounded tail while retaining exact output for later retrieval.

## Correct problem statement

ASP should provide a standard remote-agent session service that:

1. names sessions/resources independently of connections;
2. preserves processes and ordered events while clients disappear;
3. resumes by cursor or compact snapshot;
4. offers structured EXEC, PTY, and FILES without removing ordinary shell escape hatches;
5. distinguishes durable history from replaceable current state;
6. enables aggregate workspace queries/subscriptions that eliminate semantic round trips;
7. delegates transport security, reliability, congestion, and migration to QUIC and connectivity to Tailscale-like systems.

## Falsification criteria

ASP is unnecessary if SSH multiplexing + a small remote agent daemon (or RoSE + such a daemon) matches its agent workload time/bytes/recovery with lower complexity. v0 must therefore compare the *semantics*, not just SSH keystroke latency. If `WORKSPACE_STATE` and subscriptions do not measurably reduce gates/bytes on representative repositories, keep ASP as a library/API experiment rather than a new protocol.
