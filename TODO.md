# Production TODO

ASP is a hardened single-user pilot, not a broad production service. This file
tracks the work that is still open. The detailed requirements and current
evidence live in [docs/PRODUCTION_READINESS.md](docs/PRODUCTION_READINESS.md).
A task is done only when its results and provenance have been saved and
reviewed. A passing local smoke test is not enough.

## Release gates

### Two-host qualification

- [ ] Provision two independently managed hosts with production-shaped ASP,
  SSH, Mosh or RoSE, packet capture, resource telemetry, and network shaping.
- [ ] Run at least 30 paired trials for every required RTT, loss, jitter, and
  bandwidth cell.
- [ ] Test abrupt disconnects, physical Wi-Fi-to-cellular migration, and laptop
  sleep/wake. A userspace proxy or scripted route change does not count as the
  physical migration result.
- [ ] Compare warm SSH plus its agent path, the relevant Mosh or RoSE terminal
  path, and ASP using the same workload and integrity checks.
- [ ] Retain raw JSONL, pcaps, p50/p90/p99 results, interface bytes, CPU/RSS,
  reconnect times, failures, and host/build provenance. Run the checked-in
  qualifier before publishing results.

### Rolling upgrades

- [ ] Build v16 and v17 independently and record the source and artifact
  provenance for each binary.
- [ ] Test old client/new daemon and new client/old daemon combinations through
  the intended service endpoint.
- [ ] Restart the daemon during EXEC, FILE_PUT, and artifact transfers, then
  verify continuation, durable results, and byte-for-byte integrity.
- [ ] Roll back to the previous release and verify that persisted sessions,
  processes, logs, files, and artifacts remain usable.
- [ ] Document and test the supported version window and rollback decision.

### Operator controls

- [ ] Replace development credentials with site-owned PKI and secret
  distribution, rotation, and revocation procedures.
- [ ] Export the audit log to central storage with retention, dashboards, and
  alerts for drops, write failures, authentication failures, and readiness
  changes.
- [ ] Store encrypted backups off-host and complete a timed restore drill.
- [ ] Enforce cgroup, filesystem, network, and least-privilege process limits
  through the supervisor, container, or VM boundary.
- [ ] Test supervisor restart, drain, readiness, resource-limit, and failed
  rollout policies on the deployment hosts.
- [ ] Finish and rehearse the incident-response and rollback runbook.

### Capacity and abuse testing

- [ ] Set measurable SLOs and limits for each workspace and principal.
- [ ] Run long-duration, multi-workspace, and multi-principal soaks beyond the
  current local 32-client smoke.
- [ ] Measure concurrent agent and process-launch behavior under the intended
  cgroup and storage limits.
- [ ] Test disk headroom, WAL, audit, process-log, and artifact exhaustion.
- [ ] Test quotas, oversized files and logs, credential revocation, crash
  recovery, and cleanup after rejected or abandoned work.
- [ ] Confirm that overload fails in a bounded way and that alerts reach the
  operator.

### Platform and release qualification

- [ ] Run the packaged Linux binaries on an independent Linux host, including
  PTY, systemd, filesystem, network, restart, and rollback tests.
- [ ] Test the production launchd service on a real macOS host.
- [ ] Decide whether Windows is supported. If it is, qualify PTY, filesystem,
  networking, and service-manager behavior on Windows; otherwise document it
  as unsupported.
- [ ] Promote signed, reproducible artifacts with recorded build provenance and
  the checked-in SPDX SBOM.
- [ ] Test signing-key rotation, overlapping trust, revocation, and recovery
  from a bad release.
- [ ] Assign release ownership and document the promotion and rollback process.

### Independent security assurance

- [ ] Commission an external review of authentication, authorization, protocol
  state, resume/idempotency, file writes, process signaling, launcher
  boundaries, and denial-of-service behavior.
- [ ] Run coverage-guided fuzzing against the protocol, codecs, and PTY parsers
  with memory, CPU, and time limits.
- [ ] Complete dependency, vulnerability, and license reviews for the promoted
  build.
- [ ] Record findings, owners, deadlines, fixes, and retest results before the
  production sign-off.

Broad production remains blocked until every release gate above has evidence
and an owner has signed off on the result.

## Performance work after the safety gates

These tasks matter for the "super fast" goal, but they do not replace the
release gates.

- [ ] Collect real WAN traces before changing timeouts, retry thresholds, or
  stream priorities.
- [ ] Compare ASP PTY behavior with Mosh and RoSE for keystroke latency, loss,
  migration, terminal-state fidelity, and speculative echo.
- [ ] Profile warm and cold Claude Code, Codex, and OpenCode workloads.
- [ ] Measure whether prewarmed agents or worker processes save enough time to
  justify their memory and isolation cost.
- [ ] Tune compression, workspace caches, file patch selection, and stream
  priorities from the captured traces.

Do not build custom cryptography, congestion control, QUIC, NAT traversal, or
relays. Keep using Quinn and Tailscale. The working speed hypothesis is fewer
semantic round trips, not faster packets.
