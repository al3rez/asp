# Production-readiness gates

Status: **single-user pilot ready; not yet a zero-operator or multi-tenant production service** (2026-08-29).

The supported deployment targets are Linux and macOS Unix hosts. A Windows
compile/protocol-test job runs in CI (and a Zig-backed local compile check is
available for macOS developers), but Windows service-manager, PTY, and
network-failure behavior are not yet release-qualified; do not advertise the
server as a Windows production target until those tests exist.

Latest local release validation passed formatting, Clippy, 290 release tests and
documentation tests on the current macOS run (Linux CI adds platform-specific
checks), the optimized build, and `cargo audit --deny warnings`. After the
latest hardening, the non-container release smoke suite (including the
packaged-runtime and upgrade/rollback gates) and the Quinn stream/DATAGRAM/
rebind smoke passed; the SSH bootstrap smoke had also
passed in the preceding qualification. That suite covers
persistence, idempotent connect, short-lived
client failure cleanup, PTY/agent reconnect, concurrent agents and consumer
cursors, event-subscription reconnect, external file-event journaling, legacy
framing, artifacts, TLS/mTLS rotation, backup/restore,
EXEC-timeout/restart, security and exact loopback PORT_OPEN policy,
fail-closed `--production` startup, storage headroom, process-launcher/PTy,
capacity rejection/lease cleanup, three-abrupt-restart reconnect chaos, and
session-admin lifecycle, plus the bounded sustained capacity soak. The release
archive also rebuilt and verified successfully, and the current client/server
cross-compile cleanly to `x86_64-unknown-linux-gnu` with the checked-in Zig
wrapper; the release packager now selects those wrappers automatically when
`ASP_TARGET=x86_64-unknown-linux-gnu` is used on a host with Zig. This is a
compile/link check, not Linux runtime qualification.
A Zig-backed `tools/check-windows-gnu.sh` also compiles the full workspace and
the strict Windows-target Clippy pass is clean after platform-only adapter
code was gated. This remains a compile/lint check; Windows runtime, PTY,
service-manager, and network-failure qualification still require a Windows
host.
A clean release-binary sweep of these Unix smokes completed 32 scripts with
zero failures on 2026-08-29; it is still single-host evidence and does not
replace the independent-host, supervisor, or platform gates below.
After the response-codec classifier regression, a fresh focused release run
also passed the agent and direct-retry reconnect smokes, event subscription
reconnect, PTY reconnect, three-restart chaos, a 64 MiB resumable transfer, and
the ten-second eight-worker capacity soak with zero failures and zero residual
resources. This is current single-host regression evidence, not a
multi-tenant capacity or WAN qualification.
The current PTY ordering hardening also generation-guards reliable lag-recovery
`PTY_READY` snapshots, so neither delayed reliable state nor delayed DATAGRAM
state can repaint over newer output; the affected client tests and reconnect
smoke pass.
The daemon/client `ASP_*` environment-default path was also exercised
end-to-end on the release binaries: a daemon started with only environment
deployment settings, an authenticated `asp exec --summary` used only client
environment settings, and the daemon shut down cleanly. This validates the
supervisor `EnvironmentFile` ergonomics locally; it does not replace a
site-owned secret-distribution policy.
The current source was repackaged and independently verified on 2026-08-29.
The packaged-runtime and readiness-gated rollback/upgrade smokes passed against
the native archive; the Linux artifact is cross-compiled and verified here,
but still needs runtime qualification on an independent Linux host. The
generated `.sha256` sidecars, rather than this bundled document, are the
authoritative release digests so rebuilding the archive cannot make its own
provenance stale.
The release now also ships `deploy/sign-release.sh` and
`deploy/verify-release-signature.sh`; `benchmarks/smoke-release-signature.sh`
uses those packaged helpers with a local ephemeral key, verifies the rebuilt
native archive, and rejects an unexpected signer fingerprint. GnuPG key
distribution, rotation, revocation, and promotion attestation remain
operator-owned controls; the helper deliberately disables automatic key
retrieval. The atomic installer and readiness-gated upgrader can enforce the
same signature plus exact-fingerprint check before mutating a release pointer;
`--require-signature` (or `ASP_REQUIRE_RELEASE_SIGNATURE=1`) makes the check
mandatory; the local smoke covers both paths, including rejection when the
sidecar is absent. When signature enforcement is enabled, the installer
snapshots the archive and sidecars into a private bounded directory, verifies
the exact fingerprint on that snapshot, and extracts the same verified bytes;
the readiness-gated upgrader passes the enforcement options through to that
check.
Two consecutive native package builds were byte-identical after the installer
and verifier portability hardening, so the archive checksum remains a stable
release identity rather than a timestamp artifact.
The persistent-agent smoke also asserts that the process-log durability
telemetry reports nonzero synchronized bytes and zero sync failures, keeping
the new crash-ordering cost observable in the daily path.
Durable journal replay is also bounded: a resume that would materialize more
than 100,000 events or 64 MiB of event data falls back to the current snapshot
with `compacted=true` instead of allocating the entire on-disk WAL. The live
replay validator checks framing, checksums, and event ordering without rebuilding
the startup process/request/artifact maps; only daemon startup performs that
full materialization. The WAL is left untouched, and regressions cover both
the limit and the no-materialization path. This keeps reconnect memory bounded
even when retention has not yet compacted a busy session; it does not replace
the external event-retention and backup policy.
The latest filesystem-boundary pass now validates every daemon-owned session
subtree (`processes`, `transactions`, `uploads`, and `artifacts`) before use,
fails closed when transaction discovery encounters an unreadable or symlinked
directory/entry, and refuses to descend through workspace directories that
are symlinks. Regression tests cover process-artifact redirection, pending
transaction recovery, and workspace traversal; this closes a restart/query
race without changing the durable session contract.
Production startup now also rejects an untrusted symlink in any existing
component of the supplied workspace-root path before canonicalization.
Root-owned compatibility aliases below non-writable parents (for example
macOS `/var`) remain allowed; development mode retains general symlink
convenience, while production treats user-controlled path components as part
of the trust boundary.
The 2026-08-29 follow-up run also passed the remaining Unix operator smokes on
the current release (agent socket, bootstrap, artifacts, capacity admission
and concurrency, idempotent connect, consumer cursors, timeout/restart,
external file events, v16 framing, mTLS rotation, persistence, three-restart
chaos, session admin, storage headroom, TLS reload, and UDP-proxy reconnect).
The agent smoke now also exercises `asp batch --summary --tail-bytes`, keeping
the warm scripted-command path bounded when tests or builds emit large logs.
The CLI also has an explicit `asp batch --parallel N --summary --tail-bytes 0`
path for independent status/check commands: bounded concurrent requests share
one authenticated QUIC connection while preserving idempotent request IDs and
input-ordered exit markers. Output-dependent or order-dependent scripts keep
the sequential mode. The warm batch retry path now retains the authenticated
endpoint across daemon/path recovery, so a transient reconnect can reuse the
same UDP socket and TLS session cache instead of rebuilding endpoint state for
each command. Parallel tasks drain only replacement connections they own;
cloned handles for the shared original transport remain open until the parent
batch closes, preventing both leaked leases and accidental sibling teardown.
Filtered command/result streams never advance the durable event-consumer
cursor across unrelated journal entries, so a later `asp events` or explicit
`asp resume` cannot silently skip file/process events that an EXEC or SPAWN
response did not deliver. Transport retries now reconnect after HELLO and
repeat the original operation directly: stable request IDs and durable
idempotency records protect side effects, while range/digest/offset parameters
protect reads. Only explicit event consumers replay the journal. The release
regression `benchmarks/smoke-event-cursor-safety.sh` still exercises the
filtered-vs-durable cursor boundary against the release binaries by
interleaving a detached SPAWN and an EXEC, then requiring a full RESUME to
recover both outputs; CI runs it alongside the independent-consumer cursor
smoke.
The bounded 8-worker/15-second capacity soak completed 2,854 responses with
zero request failures and zero residual resources. A same-archive,
mechanics-only mixed-release drill also resumed 8 MiB file/artifact transfers
and timeout-bound EXEC in both restart directions; it is not evidence for an
independently built historical binary.
The authentication path now commits a connection's principal identity and
per-principal lease atomically across concurrent HELLO streams, including the
credential-rotation window; a second identity cannot take over an existing
QUIC attachment.
Startup recovery now fails closed if a durable running-process record cannot
be mapped to a safe process-artifact root. Previously that edge case was
silently skipped, which could leave a child running without a monitor or
recoverable output; the supervisor now gets an explicit startup failure to
alert on and retry after state repair.
Recovered process monitors now begin with an unverified identity when they
were reconstructed from a legacy WAL without Linux start-time metadata. They
must match ASP's private wrapper before a timeout or monitor-failure path can
signal a process group; an ambiguous PID is recorded as an unknown exit and
left untouched rather than risking a recycled-PID kill. A regression test
covers the unverified-PID case.
Readiness also validates the live token/principal/certificate source and
exports `asp_auth_config_healthy`, so a missing or malformed rotated secret
causes a prompt 503 instead of a false healthy signal.
The `/ready` JSON also includes a bounded `ready_reasons` array with stable
codes for failed gates, making supervisor actions explicit instead of
requiring a parser to infer causes from unrelated counters.
The loopback health handler also bounds its response write, preventing a local
peer that never reads from exhausting all health-probe permits.
The new userspace UDP proxy end-to-end smoke also passed locally; CI runs that
same authenticated-through-shaper check on Linux and macOS. In addition to
`HEALTH`, the smoke keeps a JSONL agent alive across a 17-second proxy outage
and same-address restart, then proves a follow-up EXEC reconnects after the
15-second QUIC idle bound. This is a deterministic local path-recovery check,
not evidence of physical Wi-Fi/cellular roaming.
The production-policy smoke also runs `asp doctor --strict` against its
authenticated daemon and its loopback `/ready` endpoint, validates the
healthy `ready_reasons: []` contract, removes and restores the auth file, and
asserts a 503 with `auth_config_unhealthy` while it is absent. The Linux
container smoke uses the same strict gate plus `/ready` before exercising the image. This
keeps the client-side protocol/auth/tmux/readiness preflight in the release
qualification path instead of leaving it as a manual operator check.
After the PTY/process-boundary harness was made tolerant of macOS hosts without
the optional `timeout` utility and given a bounded tmux startup grace, a fresh
ordered run of all core Unix release smokes passed end to end. The
process-launcher/PTy smoke also passed five consecutive isolated runs on
separate ports, and the launcher-drift readiness unit test passed ten exact
repetitions; these checks guard the release harness itself against the
order-sensitive failures found during this audit.
The standalone rich-PTY codec smoke also passes: a deterministic ANSI redraw
that is 7,777 bytes in the plain `PR` form becomes a 348-byte `PZ`/`AF`
datagram under a 1,200-byte MTU budget and decodes byte-for-byte. This closes
the oversized replaceable-state path without changing the reliable PTY output
contract; it is not a substitute for the two-host loss/roaming qualification.
The PTY reconnect smoke now opts into `--prefer-pty-delta` and requires a
nonzero `asp_pty_state_delta_datagrams_sent_total` metric before restarting the
daemon, so the negotiated row-delta path is exercised in addition to the
reliable reconnect contract.
The 2026-08-28 local run built the production-shaped container from the
corrected Dockerfile and passed its authenticated EXEC, detached
SPAWN/log-retrieval, read-only-runtime, and loopback-readiness smoke before
the final digest/fixed-identity hardening. The current image uses a
deterministic non-root UID/GID (`10001:10001`), ships an immutable exec-only
worker wrapper, and enables the fail-closed `--production` profile by default;
the deployment notes use a named volume by default, avoiding image-dependent
bind-mount ownership failures. The wrapper is an execution-policy anchor, not
a sandbox; the container cgroup and read-only/no-new-privileges settings remain
the aggregate boundary. Linux CI must re-run the smoke against that final image
because the local OrbStack engine became unavailable during the rebuild.
The idempotent-connect smoke also proves repeated `asp connect` invocations
reuse one saved session, while `asp connect --new` creates exactly one explicit
replacement without silently multiplying durable sessions; its health counter
asserts that ordinary connect does not replay the journal.
The client error-cleanup smoke deliberately fails a short-lived CLI after it
has authenticated and verifies that the server releases the connection lease
promptly; this prevents bursts of malformed/local-error calls from occupying
the per-principal quota until the QUIC idle timeout.
The local `agent-listen` pool applies the same cleanup rule to stale, full, or
aborted adapter connections: it closes and removes discarded endpoint handles
instead of waiting for transport idle expiry, while reusable connections remain
bounded by the four-entry pool.
The warm-agent reconnect smoke keeps a JSONL adapter alive across three daemon
replacements and verifies that workspace and process-log reads reconnect
without a redundant journal replay. It also kills the daemon after an
`EXEC_SUMMARY` process is admitted but before its final response, then checks
that the stable request ID replays exactly one durable result after a direct
HELLO retry; all read/log/side-effect resume deltas remain zero. The raw
machine-readable row is
`benchmarks/raw/agent-reconnect-direct-retry-2026-08-29.jsonl`.
The timeout-restart smoke also kills the daemon during an attached EXEC and
verifies the persisted deadline after recovery.
The release installer is also exercised with a restrictive caller umask, an
idempotent reinstall, a second versioned archive, and a pointer rollback; it
never overwrites an existing release with a different archive digest and
rejects untrusted symlinks or group/world-writable existing directories in any
`--prefix` component before creating directories. Sticky shared ancestors and
root-owned compatibility aliases below non-writable parents (for example macOS
`/var`) remain allowed.
The release also carries a narrowly scoped SSH bootstrap helper for the
trusted single-workspace bearer-token pilot; it preserves normal SSH host-key
checking, validates paths, and atomically installs a private credential pair.
It is not the PKI/secret-distribution solution for shared deployments.
The PTY reconnect smoke drives a real tmux-backed `asp shell` through a daemon
restart and verifies command input/output on both sides of the reconnect plus
a pre-restart scrollback marker after reattachment (or records an explicit
skip when tmux is unavailable). The PTY integration test also drops and
reattaches the tmux view directly, guarding the detached-owner contract. The
harness accepts `ASP_PTY_RECONNECT_DAEMON_SIGNAL=KILL`; a local hard-kill run
also passed, including the bounded capture retry while tmux recovered. Because
a hard kill cannot emit `CONNECTION_CLOSE`, that assertion waits the full
15-second QUIC keepalive/idle-loss bound plus scheduler margin; graceful
shutdowns reconnect immediately. CI runs both the graceful and hard-kill
variants on Unix runners.
The PORT_OPEN policy smoke also replaces only `aspd`, verifies that the local
forwarding listener reconnects its durable session, and carries a new TCP flow
after recovery; an already-established TCP flow remains stream-scoped and may
need to reconnect at the application layer.
The event-subscription reconnect smoke keeps `asp events` alive while the
daemon is replaced and verifies that one pre-restart and one post-restart
process event are each delivered exactly once from the saved cursor.
The external file-events smoke edits and deletes a regular workspace file
outside ASP and verifies that the watcher emits one durable `FILE_CHANGED`
event for each observed state, despite backend callback bursts; private ASP
state remains excluded and queue drops are exposed for resync handling.
Linux CI additionally builds and runs the hardened container smoke, covering
the image's read-only runtime, generated credentials, EXEC, detached SPAWN,
and durable log retrieval; the image healthcheck uses loopback `/ready`, so
audit/storage/process-boundary readiness failures are visible to the
orchestrator rather than being masked by an authenticated HEALTH response.
The backup/restore/tamper smoke also passes. The legacy compatibility smoke
additionally carries a durable session/process from the v16 framing mode
through a v17 daemon restart, then back through the v16 ceiling, and recovers
its status/log in both directions. These checks prove
the current invariants; they do not replace an independently built
mixed-release upgrade drill, the two-host qualification gates below, or
off-host backup drills.

## What remains before a broad production release

The current build is suitable for a single trusted workspace behind a private
overlay and a supervisor. It is not yet a generally deployable, zero-operator,
multi-tenant service. The remaining release work is deliberately finite:

1. **Run the external qualification matrix.** Use two independently managed
   hosts and at least 30 paired trials for each RTT/loss/jitter/bandwidth cell,
   including address migration, abrupt disconnect, and laptop sleep/wake. Run
   the structured agent fixture against warm SSH+agent, Mosh/RoSE where the
   step applies, and ASP; retain raw JSONL, packet captures, p50/p90/p99,
   interface bytes, CPU/RSS, integrity failures, and reconnect times. The
   checked-in qualifier rejects incomplete or unpaired data. This machine now
   has a Linux `tc netem` container harness and a qualified single-container
   matrix, but no second independently managed ASP host, so the two-host results
   cannot be honestly generated here. For macOS/local regression work,
   `asp-bench udp-proxy` now supplies a deterministic, bounded userspace
   delay/jitter/loss/rate shaper; it improves repeatability but does not replace
   independent hosts, packet captures, or physical network migration tests.
   The checked-in `benchmarks/two-host-agent-matrix.sh` now provides the split
   client/server runner: it executes the fixture from a client host, reads
   daemon CPU/RSS from the server's loopback metrics endpoint, optionally
   shapes both egress interfaces, fetches client-side UDP pcaps, and publishes
   only after paired/resource-aware qualification. Its dry-run/contract smoke
   is not evidence; an operator must still run it on genuinely independent
   hosts and add the migration/sleep-wake cases. The runner now accepts an
   operator-owned `--network-event-hook` plus `--network-event-kind
   migration|sleep-wake|custom`; it invokes the executable on the client host
   in both paired legs, waits for a successful completion, and records the
   event boundary in every row. This makes the physical event auditable while
   keeping the actual route/interface or laptop sleep operation outside ASP;
   runs without a hook explicitly record that migration/sleep evidence is
   absent.
   For the full Cartesian sweep, `benchmarks/two-host-agent-grid.sh` invokes
   that runner for every configured RTT/loss/jitter/rate cell (180 cells by
   default), annotates the nominal and configured delays, and re-qualifies the
   combined capture before publishing it. Inspect its `--dry-run` manifest and
   set `--max-cells` before starting a long run. For an interruption-safe run,
   provide an empty operator-owned `--checkpoint-dir`; each qualified cell is
   atomically marked with SHA-256 digests for both its JSONL and provenance
   sidecar, and `--resume` revalidates those digests plus the configuration,
   host-version provenance, exact shaping scenario, and cell capture before
   skipping it. A requested pcap is mandatory for a pcap-enabled run; a null
   path invalidates the checkpoint. The wrapper still requires
   operator-provisioned hosts, credentials, `tc`, pcaps, and supervisors.
2. **Perform a real rolling-upgrade drill.** Exercise an independently built
   v16 daemon and the current v17 client in both directions, mixed binaries
   behind the intended service endpoint, daemon restart during EXEC/PUT/artifact
   continuation, and persisted in-progress/result recovery. Keep the old
   version acceptance window and rollback procedure documented and tested.
3. **Install deployment controls outside ASP.** Supply an operator-owned PKI
   and secret distribution path, central audit export/retention and alerting,
   encrypted off-host backups with restore drills, supervisor-enforced
   cgroup/filesystem/network boundaries, and a tested rollback/incident runbook.
   `--production` verifies that these hooks are configured; it cannot attest
   that a site wrapper is a sandbox or that an operator is watching its alerts.
4. **Qualify capacity and abuse behavior.** Establish per-workspace and
   per-principal SLOs with long-running soak tests, concurrent-agent/load
   tests beyond the local 32-client boundary smoke, file/log size envelopes, disk
   exhaustion, audit/WAL growth, credential revocation, and supervisor crash
   recovery. Review the resulting limits before exposing more than a trusted
   workspace.
5. **Close platform and release-process gaps.** Linux is the primary server
   target; macOS launchd is a starting point. PTY behavior on other platforms,
   signed/provenanced multi-platform artifacts, SBOM publication/promotion,
   and release ownership remain. The archive now includes a deterministic SPDX
   inventory generated from the locked dependency graph and a
   checksum-verified installer that atomically switches versioned binaries and
   retains a tested rollback pointer; each deployment still needs to integrate
   that workflow with its supervisor and signing/promotion system.

6. **Complete independent security assurance.** The checked-in tests cover
   malformed frames, path confinement, credential rotation, quotas, and
   crash/replay invariants, but a broad production release still needs an
   independent review of the wire/state machine and launcher boundary,
   protocol and codec fuzzing under resource limits, dependency/license review,
   and a time-bounded remediation record. A passing local suite is not a
   substitute for a second set of eyes on authentication, resume/idempotency,
   file writes, process signalling, and denial-of-service behavior.

The fastest path to daily use is therefore a supervised, single-workspace
pilot on Tailscale. The protocol and local failure invariants are exercised;
the claims that still require evidence are cross-host performance, roaming
under loss, broad isolation, and operational recovery.

The current release is appropriate for a trusted workspace behind Tailscale or another private overlay, supervised by systemd/launchd, with `.asp/` on durable storage. The supplied systemd unit uses `ProtectSystem=strict` with an explicit workspace write allowlist, intentionally leaves `PrivateTmp` disabled because its `KillMode=process`/tmux contract requires a restarted daemon to see a surviving tmux socket, and gives the 10-second request drain a 30-second stop timeout for WAL/audit flush margin. Deployments that enable private temporary storage must move tmux to a persistent workspace socket and test reattachment. The release now includes separate fail-closed production templates: [`aspd-production.service`](../deploy/systemd/aspd-production.service) and [`com.asp.aspd-production.plist`](../deploy/launchd/com.asp.aspd-production.plist). Both pass `--production`, require an operator-installed `/usr/local/libexec/asp-worker-wrapper`, and keep the original units as pilot baselines because a generic template cannot choose a site's sandbox policy. The supplied systemd, launchd, and container templates disable `PORT_OPEN` by default; add only reviewed exact loopback targets when forwarding is required. Templates are provided for [systemd](../deploy/systemd/aspd.service) and [macOS launchd](../deploy/launchd/com.asp.aspd.plist); the day-two procedures are in the [operations runbook](OPERATIONS.md). Shared deployments can use the new mTLS mode (`--client-ca` plus `--auth-certificates-file`) to bind TLS client certificates to owners/scopes, but the service still needs host-level isolation and operational controls before broad Internet exposure. State backups contain credentials and command metadata; encrypt them with the deployment's KMS/backup system before copying off-host because ASP verifies integrity but does not provide backup encryption.

The container deployment now supplies its own immutable exec-only
`asp-worker-wrapper` and enables `--production` by default. The wrapper is only
an execution-policy anchor; the container's cgroup, read-only root, and
no-new-privileges settings remain the aggregate boundary. Host systemd/launchd
deployments still need a site-owned wrapper or sandbox.

The explicit `--production` profile now fails closed before state/listener
initialization unless authentication, loopback readiness/metrics,
an operator-supplied process boundary, `PR_SET_NO_NEW_PRIVS`-compatible
privilege policy, a non-group/world-writable workspace path and non-sticky
ancestors,
nonzero CPU/wall-clock command limits, filesystem headroom
(`--min-free-bytes`), and an explicit PORT_OPEN policy (`--port-target ...` or
`--disable-port-forwarding`) are configured.
It is a deployment guardrail, not a sandbox: the launcher must still be a
reviewed bwrap/systemd/site wrapper or a per-workspace container/VM, and the
profile should be used in both the supervisor preflight and the live command.
Local development intentionally keeps the historical defaults without this
profile. `benchmarks/smoke-production-policy.sh` proves both fail-closed and
complete-profile startup behavior, including the storage-headroom gate.

The server-side `--client-ca` input accepts one DER CA or a bounded directory
of up to eight regular `.der` CAs (16 MiB aggregate, symlinks rejected).
SIGHUP reloads the bundle with the server certificate/key, enabling an
old/new CA overlap without dropping sessions; external PKI issuance,
distribution, and revocation ownership remain deployment responsibilities.

The release dependency graph passes `cargo audit --deny warnings`; the CI
workflow runs that locked RustSec audit on every push/PR and on a weekly
schedule. An unused Postcard embedded/`heapless` default was disabled so the
unmaintained
`atomic-polyfill` crate is not pulled into ASP binaries.
The workflow's third-party actions are pinned to immutable commits, and
Dependabot is configured to propose weekly Cargo and Actions updates.

Release packaging is now checked in CI: the workspace package manifests and
source lists are validated, and the publishable `asp-protocol` crate is packed,
its bundled schema registry is tested from the generated tarball, and compiled
from that package. Release archives and checksums are emitted with explicit
public-read permissions, so a restrictive caller umask cannot make a generated
release unusable by the deployment user. The executable crates are explicitly
`publish = false`; production binaries are distributed as versioned,
checksummed platform artifacts rather than pretending that a local path
dependency is already available on crates.io. Each archive also carries the
declared Apache-2.0 and MIT license texts so an offline deployment has the
complete redistribution terms. The archive builder normalizes
file order, ownership, permissions, timestamps, and the gzip header, and CI
rebuilds it twice to prove byte-for-byte reproducibility. The archive builder
also disables implicit directory recursion so duplicate archive members cannot
slip into a release; the verifier and CI reject duplicates. The standalone
verifier also rejects archive basenames outside the bounded `asp-*.tar.gz`
contract before invoking platform `tar`, preventing option-shaped input from
reaching the archive parser, and parses the checksum sidecar as exactly one
record for that archive before hashing it directly, preventing arbitrary-path
probes through `*sum -c`. Before checksum and extraction, it also caps the
compressed archive at 512 MiB and the member list at 4,096 entries; the current
archives are only a few megabytes, so these are decompression/resource-safety
guards rather than normal release limits. The checksum sidecar is capped at
16 KiB before its single-record parser runs. Signing and
promotion remain release-infrastructure responsibilities. `deploy/package-release.sh`
produces a versioned client/server archive with the documented `docs/`
(including research) and `deploy/` paths, deployment templates, schema,
lockfile, license texts, deterministic `SBOM.spdx.json`, and SHA-256 checksum
without including `.asp/`
credentials or durable state. It invokes `deploy/verify-release.sh`, which additionally
rejects unsafe archive paths, links/special files, missing required files, and credential/
state names; deployment hosts can rerun that verifier before installation.
The archive also carries `deploy/install-release.sh`, which verifies the
archive, extracts an immutable versioned directory, atomically switches a
supervisor-facing `current` pointer, and retains a validated `previous` pointer
for rollback without overwriting a running binary. It intentionally does not
restart the supervisor; operators still own preflight, drain, signing, and
promotion. Signing and promotion remain release-infrastructure
responsibilities.
The archive also carries `deploy/upgrade-release.sh`, which adds an explicit
supervisor restart and loopback readiness gate with automatic rollback; its
packaged failure/recovery smoke passes locally, while independent-host and
historical-binary rollout qualification remain P0 gates.

The current macOS development host also produced a checksum-verified,
versioned Linux archive (`dist/asp-0.1.0-x86_64-unknown-linux-gnu.tar.gz`) with
valid stripped ELF `x86_64-unknown-linux-gnu` client and server binaries using
the optional Zig wrappers in `tools/zig-cc.sh` and `tools/zig-ar.sh`. Linux CI
also executes the packaged Linux client/server `--version`/`--help` binaries
after archive verification, so the release artifact itself is exercised rather
than only the source build. This closes the packaging and compile/link checks
without claiming that the binaries have full PTY or service-manager
qualification; those still belong to Linux host-level release testing.

## Already hardened

Large-frame compression now performs a conservative high-entropy sample before
dispatching zlib, avoiding unnecessary CPU for already-compressed artifacts;
the wire format still uses compression only after a strict byte-win check. The
health/metrics endpoint exports compressed/plain frame counts plus logical and
encoded byte totals, so an operator can verify the codec's CPU-versus-bandwidth
tradeoff instead of tuning it blind.
The zlib decoder also avoids eagerly reserving the attacker-controlled decoded
length: it starts with a bounded body-size estimate and grows only as bytes are
actually produced. A deterministic regression covers a tiny malformed body
advertising the 128 MiB frame limit, and the malformed-input corpus exercises
frame and PTY datagram decoders under `catch_unwind`; this reduces avoidable
memory pressure. The release check `benchmarks/smoke-protocol-fuzz.sh` now runs
the bounded deterministic corpus through ten public decoder paths (10,000
inputs by default) and records the seed/limits for reproduction. These checks
catch regressions and panics but do not replace an independent fuzzing campaign
or security review.

Operational preflight and readiness are explicit: `aspd --validate-config` checks
existing credentials, policy, and configured filesystem headroom without
creating files or taking the daemon lock; `/live` remains liveness-only, while
`/ready` returns HTTP 503 if the durable audit sink is unavailable, has dropped
entries, the configured storage reserve is exhausted, scheduled storage
maintenance has failed or gone stale, the reviewed process launcher has
drifted from its startup filesystem identity, or the daemon is draining after
SIGTERM/SIGINT. Its bounded `ready_reasons` array uses stable codes
(`audit_disabled`, `audit_dropped`, `audit_failed`, `auth_config_unhealthy`,
`storage_headroom`, `storage_maintenance_unhealthy`,
`process_launcher_unhealthy`, `git_helper_unhealthy`, and `draining`) so
operators can route each failure to
the right remediation. The additive `draining` field in `/ready` and
`asp_draining` metric let a supervisor distinguish intentional shutdown from a
crash or dependency failure. In `--production` mode, preflight also requires
an executable, non-group/world-writable `tmux` (or an absolute
`ASP_TMUX_PATH`) before the QUIC listener opens, making a missing or
replaceable durable PTY supervisor a startup failure rather than a late
`PTY_OPEN` error.
- Short-lived tmux metadata/history probes run in private Unix process groups; timeout cleanup kills the group before joining bounded capture readers, so a launcher descendant cannot keep a pipe open and pin a Tokio blocking worker past the 500-ms helper deadline.
- Quinn/rustls QUIC streams, DATAGRAMs, explicitly enabled server-side connection migration, migration smoke test, pinned certificate, default bearer authentication, and a shared bounded transport profile (windows, keepalive, datagram buffers, fair same-priority scheduling, and native stream priorities that keep PTY plus bounded `EXEC_SUMMARY`/control replies ahead of bulk payloads; large legacy PUT/PATCH bodies are classified as bulk).
- Bind policy fails closed: `--insecure-no-auth` is accepted only on a loopback QUIC listener, and the optional health/metrics endpoint can bind only to loopback even when the control listener is exposed through a private overlay.
- EXEC/SPAWN, semantic Git helpers, and tmux-backed PTYs can be routed through the same operator-supplied absolute process launcher (for example a reviewed supervisor or `bwrap` wrapper), with bounded arguments, a regular executable requirement, group/world-writable rejection, and a `--require-process-launcher` fail-closed mode. The launcher must `exec` its arguments so durable PID identity and signal semantics remain observable; ASP canonicalizes it once at startup, binds that canonical executable to its filesystem identity, and refuses same-path or ancestor-redirection replacement until the daemon is restarted. The launcher must be tested with `/bin/sh` command arguments, the canonical Git executable, and the absolute `tmux` command; this remains an integration hook, not a built-in or attested sandbox.
- Request validation errors for signals, file reads/ranges, and hash-guarded patches now return stable machine codes on a finished stream (`invalid_signal`, `unknown_process`, `file_not_found`, `invalid_range`, `invalid_patch`, and related codes) instead of leaving agents to infer semantics from a transport reset.
- Durable idempotency tables are explicitly bounded per session (65,536 request IDs across process, file, signal, and artifact mutations). Existing IDs remain replayable; new side effects fail closed with `idempotency_capacity` once the limit is reached, and occupancy/rejection/limit metrics are exported for alerting.
- PTY screen snapshots are latest-wins QUIC DATAGRAM state published at a bounded roughly 60 Hz; reliable output remains lossless and the server avoids rebuilding a full screen for every output chunk. The client advances the same monotonic generation guard from reliable `PTY_OUTPUT` frames and lag-recovery `PTY_READY` snapshots before accepting a replaceable state, so a delayed DATAGRAM or reliable snapshot cannot repaint an older screen over newer live bytes. Generation-keyed plain/rich render caches are shared by concurrent attachments, and `/metrics` exposes cache hits versus renders so operators can verify that reconnects and observers are not repeatedly walking the terminal parser. Synchronous portable-pty input writes run on Tokio's blocking pool with a per-PTY serialization gate and bounded timeout, so a non-reading terminal cannot pin a reactor worker or multiply blocked tasks during reconnect storms; `asp_pty_input_write_timeouts_total` exposes that failure mode for alerting. Durable tmux attachment resolves standard Linux/macOS/Homebrew executable paths (or `ASP_TMUX_PATH`) before `PATH`, so service-manager environments do not lose PTY support merely because they omit an interactive shell path. Long-lived PTY, event-subscription, and port streams revalidate credentials at a bounded one-second cadence and close on revocation.
- Startup PTY recovery queries each existing tmux pane's bounded `pane_height`/`pane_width` before attaching, preserving the user's last terminal geometry instead of forcing a 24×80 resize; missing or unresponsive tmux metadata falls back safely and is corrected by the next client resize.
- Connection-independent UUID sessions, durable process logs, tmux PTYs, event replay, bounded-memory WAL recovery, and daemon restart reconciliation.
- EXEC request IDs, duplicate-result replay, client reconnect retry, per-stream output offsets, and duplicate-output suppression.
- PTY attachments detach without killing the tmux shell; `asp shell` reattaches and resumes indefinitely across transport loss, address changes, and laptop sleep (Ctrl-] cancels while disconnected) and forwards Unix terminal resize events.
- Independent event subscribers can pass distinct `--consumer-id` values; the lock-protected client cursor file keeps their replay points separate while preserving legacy per-server bootstrap semantics. The client cursor metadata is capped at 8 MiB and refuses an oversized update before publication, so a growing server/consumer map cannot leave a cursor file that future clients are unable to load.
- Atomic client cursor writes, hash-guarded file patches, precondition-aware whole-file/streamed PUTs (create-only by default; blind replacement requires an explicit flag), serialized/rollback-safe file mutations, a daemon-wide workspace commit gate for agents sharing one checkout, cancellation cleanup for temporary upload files, workspace/symlink confinement (including final-component no-follow reads), bounded frames/commands/files/PTY input, and graceful journal flush. The gate orders commits but does not merge concurrent edits; collaborative callers must use the base-hash precondition/PATCH contract or an external ownership policy.
- File replacements preserve existing ordinary Unix mode bits (including executable/shared-workspace bits); newly created files remain private at `0600` because v0 does not carry a mode field.
- Persisted request IDs/hashes for session opens, file mutations, and signals; reused IDs return the original result and conflicting bodies fail.
- Authenticated `asp doctor` (including active connections, WAL bytes, and uptime), a `--strict` mode that fails on an unsupported protocol, disabled server authentication, or an unavailable durable tmux backend, a mandatory singleton state-directory lock, an explicit non-loopback bind acknowledgement, a private rotating JSONL audit sink, and a systemd deployment baseline.
- Optional identity-bound mTLS: rustls validates a configured client CA during the QUIC handshake, and a reloaded SHA-256 fingerprint map binds each certificate to an owner and scopes; the client supports DER certificate/key options across reconnects and bounded multi-pin directories for certificate rollover. Token-file clients also reload the bearer token for each new connection, so rotation can recover long-lived adapters. Operators can stop the daemon and run `aspd --rotate-auth-token` for an atomic 0600 token replacement; the systemd and launchd runbooks include the maintenance sequence. A running daemon accepts `SIGHUP` to reload a complete server certificate/key pair for new handshakes, retaining the last known-good config if staged files are invalid or briefly mismatched. Existing server private keys are tightened through a no-follow descriptor before loading, and auth-map caches include Unix file identity so equal-sized atomic replacements cannot leave revoked credentials cached indefinitely.
- The loopback `/ready` probe validates the live token/principal/certificate source and exports `asp_auth_config_healthy`; a missing or malformed rotated secret therefore returns 503 before a supervisor routes new work to a daemon that cannot authenticate clients. Connection principal identity and its quota lease are committed atomically across concurrent HELLO streams, preventing a second identity from taking over one QUIC attachment.
- First-run TLS setup fails closed when only one half of a certificate/key pair exists; ASP will not overwrite an orphaned private key or certificate while attempting to self-initialize.
- Token-file agent adapters reload credentials on reconnect and retry once when a live connection receives `authentication_required` after rotation; literal `--auth-token` clients remain intentionally static. The supervised local adapter caps bridge tasks at 32, bounds the pooled QUIC leases at four, and drains active local clients for ten seconds on SIGTERM.
- The release concurrency smoke launches independent JSONL adapters against one workspace, overlaps semantic inspection, bounded EXEC summaries, and file commits, and verifies zero request failures plus zero residual frame/response memory. It passes at the daemon-advertised 32-connection per-principal ceiling and rejects larger all-success inputs explicitly. This is a bounded contention regression check; capacity and multi-tenant SLOs still require host-level qualification.
- Authentication maps are metadata-keyed in memory after their first load, so every request still checks for atomic rotation/revocation without reparsing a large principals or certificate JSON file on the Tokio request path. Files are reopened through the symlink-safe reader whenever metadata changes; filesystems without a usable modification time fall back to parsing each request.
- The common private-credential path now avoids a repeated descriptor-level chmod on every warm request; permission repair remains no-follow and is performed when metadata reports a permissive mode.
- Daemon-owned audit, event-WAL, lock, and persistent process-log opens use
  descriptor-level no-follow flags on Unix after the path checks, closing the
  final-component check/use race for append/create paths.
- Audit rotation replaces the retained `.1` segment atomically on Unix instead
  of deleting it first, and restores the active path if reopening fails after
  a rename; a regression test covers retained-segment replacement.
- Bounded/parallel `WORKSPACE_STATE` scans, a watcher-backed per-workspace tree index with epoch/generation validators and safe fresh-scan fallback, a bounded native-watcher hand-off that disables the cache on queue overflow instead of retaining an unbounded event storm, and a coalescing regular-file observer that journals external editor/build changes as deduplicated `FILE_CHANGED(path,version)` events while ASP-owned atomic writes seed the same observation to avoid duplicate notifications. `asp_workspace_file_event_drops_total` makes the resync condition visible. This is alongside the 32 MiB selected-file and 64 MiB response caps, two-slot aggregate memory accounting for selected-file reads, advertised `retained_from_event_id`, release smoke coverage (including live TLS reload, persisted EXEC timeout after daemon loss, and immutable artifact upload/range/restart retrieval), reconnect retries for repeatable requests (including idempotent first-session creation with a stable `OPEN_SESSION` request ID), async lock-protected client session creation/checkpoints that serialize concurrent first launches, and resumable uploads across client processes.
- External file-event fan-out is retry-safe across sessions: if one session WAL commits before a later session fails, the next worker pass reuses that workspace path version and skips the already durable event instead of allocating a duplicate version. The invariant is covered by a two-session failure-shape regression test; durable WAL failure injection and multi-host storage qualification remain deployment tests.
- Bounded semantic Git helpers run in dedicated process groups; Unix regression coverage forces an oversized helper result (including an inherited pipe held by a descendant) and verifies that both the direct helper and its descendant are terminated before the request returns, so output-limit cleanup cannot leak work outside the agent operation.
- Guarded file replacement hashes the existing target through bounded asynchronous I/O before the commit boundary; legacy PATCH construction, selected-file/semantic-result hashing, and upload/download body digesting run outside the workspace gate/blocking pool and recheck the base hash at final commit. Blind creates/overwrites avoid an unnecessary full-file read, and target-presence changes are rejected before the atomic install. This keeps large-file precondition checks and digest work from blocking the Tokio worker or stalling unrelated agents while preserving the final mutation transaction.
- Workspace Git queries resolve a canonical executable from standard service-manager paths before `PATH` (or the absolute `ASP_GIT_PATH` override), bind its file identity at daemon startup, revalidate that identity before each helper invocation, and run through the same configured process launcher and inherited command limits as EXEC/SPAWN when configured. A replacement therefore fails closed instead of silently changing the code run for an agent; the supplied container image installs Git so semantic inspections retain Git status/diff/log behavior under a minimal environment. A non-Git workspace remains valid and omits Git fields rather than failing the whole query.
- Loopback-only `PORT_OPEN` forwarding with an explicit `ports:open` scope; each accepted TCP flow is isolated to one reliable QUIC stream, payload bytes count against the principal's rolling request/response budgets, credentials are revalidated at a bounded one-second cadence while data flows, and operators can install an exact `--port-target HOST:PORT` allowlist (with deny metrics) without changing the development default. It does not add NAT traversal or VPN behavior; reverse forwarding/leases remain future work.
- Configurable filesystem headroom (`--min-free-bytes`) fails readiness and rejects new durable mutations before the workspace volume fills, while read-only inspection and recovery remain available. The threshold and rejection counter are exported through the loopback health endpoint; zero keeps the development default, and `--production` requires an explicit nonzero value. This is a circuit breaker, not a replacement for off-host backups or supervisor disk alerts.
- CRC32 WAL frames with 64 MiB segments, a 4 GiB per-session safety quota, atomic snapshots, strict nonzero/monotonic event-ID and snapshot-boundary gap validation (invalid logs are quarantined), snapshot-boundary recovery tests, background size/age compaction, bounded streaming resume frames, durable resumable-upload staging with retention cleanup, request-header deadlines, size-aware body minimum-rate deadlines, active-connection/global in-flight-request caps (and a 64-connection cap on the loopback probe endpoint), token-file rotation that invalidates old credentials on existing connections, lock-protected checksummed state backup/verify/restore commands, durable process-start intents, and an optional loopback `/live`/`/ready`/`/metrics` endpoint. PTY screen-generation markers are coalesced to a 100 ms durable cadence, so a high-rate terminal stream does not turn every output chunk into a WAL transaction. On the multi-thread Tokio runtime, synchronous WAL append/fsync work yields the reactor worker through `block_in_place`; synchronous startup and current-thread embedders retain the direct fallback.
- Active `events.log` tails are truncated only when they are incomplete at the current append point; a torn frame in an already-rotated immutable segment is quarantined and blocks startup instead of silently discarding acknowledged history. This distinction is covered by a dedicated recovery regression test.
- Event-segment discovery also rejects a symlink masquerading as a rotated WAL file rather than silently ignoring it; the no-follow behavior is covered by a Unix regression test.
- Session recovery rejects symlinked or UUID-named non-directory entries under `.asp/sessions` instead of silently hiding a durable session from inventory and resume; both substitutions are covered by regression tests.
- Process monitors enforce a 512 MiB aggregate stdout/stderr budget. Log/read failures and output-limit termination publish a terminal `PROCESS_EXITED(code=null)` event before releasing the process slot, so live subscriptions and resumed clients do not wait forever for an unbounded or unreadable producer.
- `asp_process_output_limit_terminations_total` counts process groups stopped at that output safety boundary, so an operator can distinguish ordinary command exits from output-volume incidents.
- Recovered process monitors fail closed when an append-only stdout/stderr log is shorter than its durable cursor instead of rewinding and duplicating already-acknowledged output; the truncation regression is covered by a release test.
- On Linux/Android, command trees can inherit address-space limits; on Unix, they can inherit CPU-time limits via `--process-memory-bytes` and `--process-cpu-seconds`. Linux EXEC/SPAWN children also set `PR_SET_NO_NEW_PRIVS` by default, preventing setuid/file-capability privilege gains in the command tree; `--allow-process-privilege-gain` is an explicit escape hatch for a reviewed trusted launcher. The systemd template enables 2 GiB/24 hours. These are per-command guardrails, while cgroups remain necessary for aggregate daemon/child limits and non-Linux memory isolation.
- Process-output delivery has a 128 MiB aggregate queue-memory budget and bounded per-process channels; output permits remain held until the response item has been consumed by the QUIC writer, so slow readers cannot move an uncharged copy into a second queue. A persistently exhausted budget causes the live attachment to detach after a bounded 250 ms wait; durable logs/events remain authoritative and the client resumes from its cursor instead of stalling a disk-backed monitor indefinitely. Slow readers backpressure child pipes instead of multiplying retained output with the number of active EXEC streams. Live readers use 64 KiB chunks, while persistent monitors batch up to four 64 KiB source reads before one ordered log sync and journal event, reducing sync/framing overhead without weakening crash recovery. Event fan-out retains at most 256 events per session before a subscriber must resume.
- `asp_process_output_attachment_detaches_total` records process-level live-output attachment detachments caused by a closed reader or an exhausted aggregate output budget, making backpressure visible to operators even though the process and durable logs continue.
- The loopback health endpoint exposes daemon `asp_process_cpu_time_us_total` and peak `asp_process_max_rss_bytes` gauges from the host `getrusage` API, plus best-effort cgroup-v2 current/limit memory, CPU, and process-count gauges when the service manager mounts them. Supervisor telemetry remains authoritative for child/cgroup policy and alert delivery.
- `/metrics` also exports fixed-cardinality `asp_request_duration_us_bucket`, `_count`, and `_sum` histograms for every request-response operation (plus `unknown` for malformed or undecodable streams). Long-lived PTY, subscription, and port streams are intentionally excluded from request latency and remain covered by connection/transport gauges. This gives operators p50/p90/p99 inputs without retaining request IDs, paths, or other unbounded labels; the histogram is observational and never gates scheduling.
- The daemon records Quinn connection statistics at attachment close (UDP
  datagrams/bytes, lost packets, congestion events, last path RTT, and MTU)
  and exports them through `/ready` and `/metrics`; these are transport
  observations for SLOs and benchmark analysis, not a second congestion or
  retransmission layer.
- The loopback health endpoint also exposes workspace-index hit/miss/invalidation counters, external file-event queue drops, complete-result digest-hit counters, daemon-side digest-cache fast-path hits, repeated-search and Git-metadata cache hit/miss counters plus bounded cache occupancy/limits, and a watcher-health gauge, so operators can detect when semantic queries have fallen back to fresh scans or are under memory pressure and when event consumers need a resync.
- Selected workspace-file reads share a daemon-wide 32 MiB memory budget; permits are retained through response serialization, while the encoded workspace response borrows from a separate daemon-wide 256 MiB response budget. Potentially large response shapes are serialized before their exact permit is acquired, closing the transient uncharged-`Vec` window for concurrent large RESUME/workspace responses. Bounded control/interactive responses bypass that gate so large workspace/log encodes do not delay PTY or session control; their retained payloads remain charged to the response semaphore. New readers give up after a bounded 250 ms wait, and the endpoint exposes `asp_workspace_file_memory_bytes`/`asp_workspace_file_memory_limit`, `asp_frame_memory_bytes`/`asp_frame_memory_limit`, `asp_response_memory_bytes`/`asp_response_memory_limit`, and `asp_response_memory_rejections`. Low-cardinality `asp_response_encode_gate_wait_us_total`, `asp_response_encode_gate_acquisitions_total`, and `asp_response_encode_duration_us_total` counters make the remaining large-response queueing and codec costs measurable before changing this invariant.
- Workspace Git queries disable interactive credential prompts and process-wide Git configuration, run through the same validated process launcher and inherited limits as EXEC/SPAWN when configured, and have a 60-second kill/reap deadline, so a corrupt repository or unusual Git helper cannot hold a request stream indefinitely or bypass the worker boundary. Repository-local configuration remains available for repository semantics and is covered by that boundary. The deadline is a guardrail, not a substitute for worker/cgroup policy.
- The canonical Git helper is identity-bound at daemon startup and checked before each invocation; a replacement fails closed and makes `/ready` return 503 until the reviewed executable is restored and the daemon is restarted. `asp_workspace_git_helper_configured` and `asp_workspace_git_helper_healthy` expose this boundary to supervisors.
- `EXEC_SUMMARY` provides a bounded-tail/byte-count result for test and build commands while the full output remains durable for later subscription or resume; agents do not have to transfer every log byte synchronously when they only need a verdict and final diagnostics.
- Concurrent event consumers can pass a stable, distinct `--consumer-id`; the client keeps each cursor in a separate lock-protected entry while bootstrapping a newly named consumer from the legacy per-server session entry. When both peers advertise `event_consumer_leases`, the client coalesces ACKs on a background QUIC stream and the server persists named cursors/lease heartbeats, deferring compaction behind unexpired consumers and expiring abandoned leases after seven days. `asp_event_consumer_lag_max` exposes the largest durable replay gap so operators can alert before retention is blocked, while `asp_resume_replay_limited_total` distinguishes replay tails that intentionally fell back to a current snapshot because of the 100,000-event/64 MiB live replay bound. Older peers retain advisory ACK semantics.
- `PROCESS_OUTPUT_STREAM` plus `asp logs` provides a bounded offset/length read from durable stdout/stderr logs after journal compaction, with chunk-offset validation and a 64 MiB per-request cap.
- The persistent JSONL `asp agent` adapter and `asp batch --stdin` keep one authenticated QUIC connection/session open across repeated commands; `asp batch --summary` adds the same bounded-tail/byte-count contract to scripted command loops, so a test/build batch can stay warm without forwarding every log byte. The adapter now covers EXEC/summary, detached SPAWN, resumable durable log ranges, semantic workspace inspection, durable process signals, and small hash-aware file get/put/patch calls. When an inspected file's exact base is still in the bounded adapter cache, a guarded `file_put` automatically derives and sends a smaller prefix/suffix patch or negotiated multi-range patch without a second `FILE_GET`; a byte-identical replacement emits `file_unchanged` locally after a zero-byte metadata/hash check, preserving the valid workspace cache and avoiding a mutation request/event entirely; uncached or non-beneficial edits retain the normal full PUT path. The multi-range path is covered by the real agent smoke (including an explicit `file_patch_ranges` request) and exact v17 wire-size fixtures; older peers never receive it. Agent output/file bytes are lossless base64 with absolute offsets or content hashes, and transport retries reuse request IDs. New clients keep their cursor outside the workspace by default so local state writes do not invalidate the remote watcher/index; an explicit `--session-file` remains available. The shared cursor lock now merges same-session updates monotonically, preventing concurrent adapters from moving the durable resume point backwards. `agent-listen`/`agent-connect` add a supervisor-managed private Unix-socket endpoint for warm local adapters; its asynchronous 64 MiB output queue keeps slow local consumers from blocking QUIC request handling or growing memory without bound. Bounded 8 MiB/32 MiB QUIC flow-control windows cover the expected high-BDP development links without unbounded buffering.
- `docs/schema.json` is the machine-readable v17 operation/feature registry and is checked against the Rust wire constants in the protocol test suite. The server accepts the tested v16 plain framing during a rolling deployment and pins one mode per connection; v17 stream frames use the bounded `AF` envelope and fast zlib only when it produces a strict byte win. The current client caches a successful v16 endpoint negotiation for five minutes, avoiding a failed probe on every reconnect while still re-probing for v17 after expiry. Client-side large request-frame codec work (compression or high-entropy plain serialization) runs on Tokio's blocking pool so file/artifact uploads do not stall interactive control traffic; responses use the same 64 KiB off-reactor threshold, and their Postcard serialization yields the normal multi-thread worker before frame compression. The server charges both wire and decoded buffers.
- Resource guardrails now include 256 total sessions, 64 sessions per principal, 64 running processes per session, 256 running processes per principal across sessions, 64 active subscriptions per session, 256 active QUIC connections, 32 active QUIC connections per principal, 4,096 global in-flight request streams, 512 in-flight request streams per principal, a 256 MiB aggregate decoded-frame memory budget, and a separate 256 MiB aggregate encoded-response memory budget; `/metrics` exposes active subscriptions, active request streams, authentication failures, admitted request bytes, encoded response-frame bytes, port-forward payload bytes, resume counts/replay volume/maximum observed lag, process-quota, connection-quota, request-stream-quota, and byte-budget rejections, decoded-frame admission rejections, request/response memory occupancy/limits, and process-output queue occupancy/limit alongside the existing counters.
- Authenticated request payloads are metered per principal and operation, and encoded response payloads are metered per principal, in bounded rolling one-minute windows (4 GiB/minute by default; configurable with `--principal-request-bytes-per-minute` and `--principal-response-bytes-per-minute`); streamed file chunks and PTY continuation frames are included, and rejections are audited and exposed through `/ready` and `/metrics`. Response-frame and loopback port payload counters remain process-level telemetry; QUIC/IP overhead still requires host/Quinn counters.
- Failed HELLO attempts are rate-limited per source address (32 failures per 60 seconds followed by a 30-second cooldown), with bounded source-state eviction and an `asp_auth_rate_limited_total` metric. This is a brute-force/CPU guardrail, not a replacement for a firewall, private overlay, or certificate-based identity.
- Connections that never complete HELLO are closed after a bounded 10-second authentication deadline; `asp_auth_handshake_timeouts_total` makes pre-auth slot exhaustion visible. Authenticated idle PTY/event streams remain exempt so persistence is not traded for abuse resistance.
- Quinn stateless address validation is now enabled automatically by the
  fail-closed `--production` profile (and is available explicitly as
  `--stateless-retry`/`ASP_STATELESS_RETRY=1` for shared-interface development
daemons). Retry-token generation and validation remain Quinn's responsibility;
  `asp_quic_stateless_retry_enabled`,
  `asp_quic_stateless_retries_total`, and
  `asp_quic_stateless_retry_failures_total` expose the effective initial-
  handshake amplification guard and any path/firewall failures. The first connection
  pays one extra handshake flight; durable sessions and reconnect/migration
  semantics are unchanged.
- Client QUIC bidirectional-stream admission has a separate bounded 10-second open deadline; an exhausted peer stream/flow-control budget becomes a retryable transport error instead of an unbounded agent wait. Individual request-frame writes also have a size-aware 64 KiB/s deadline (10-second floor, five-minute cap), so a peer that stops consuming an admitted stream cannot pin an upload indefinitely. Server response-frame writes use the matching size-aware deadline, releasing encoded-response memory and request tasks when a slow reader stops consuming a stream.
- `rust-toolchain.toml` pins Rust 1.88 with rustfmt/clippy, `Cargo.lock` is enforced by the CI build/test/lint commands, and `.github/workflows/ci.yml` runs locked metadata/docs/script checks, formatting, Clippy, release tests/builds, the Quinn stream/datagram/rebind smoke, the bounded protocol-decoder mutation smoke, persistence/agent/concurrent-consumer-cursor/TLS/mTLS/backup smokes, fail-closed production-policy, abrupt-reconnect-chaos, and session-admin lifecycle smokes, the Linux container build/runtime smoke, and `cargo audit --deny warnings` on every push and pull request.
- Scheduled compaction, process-log pruning, resumable-upload cleanup, and artifact retention/GC run immediately at startup and then on a 30-second cadence on Tokio's blocking pool; large maintenance passes no longer execute synchronous filesystem work on the QUIC reactor. Each pass exports run/failure/in-progress/last-success telemetry, and `/ready` reports `storage_maintenance_unhealthy` if cleanup fails or the scheduler becomes stale, so retention drift is actionable instead of log-only.
- Process and PTY launch preparation (durable intent/log files, metadata checks, fsyncs, tmux setup, and fork/exec) is also dispatched through Tokio's blocking path on the normal multi-thread runtime. A slow disk, tmux socket, or saturated host process table therefore cannot pin a QUIC reactor worker while the per-session commit boundary remains held for correctness.
- `/metrics` now separates that process-start transaction from end-to-end request latency with a bounded `asp_process_launch_duration_us` histogram and `asp_process_launch_failures_total`; the timer covers durable preparation, policy recheck, and spawn/bookkeeping, and excludes response draining and child lifetime. Capture these series in the next shaped multi-host run before setting launch SLOs.
- The storage-maintenance interval skips missed ticks after a slow pass rather than issuing catch-up sweeps back-to-back, so retention cleanup remains low priority under disk pressure while live QUIC requests and process monitors retain capacity.
- Resumable file/artifact staging metadata, symlink/permission checks, storage-headroom probes, and cleanup are dispatched through the same blocking path. Streaming body I/O stays asynchronous, and headroom is checked before the body plus at durable 1 MiB boundaries rather than once per 64 KiB chunk; this keeps large transfers from turning filesystem stats into reactor stalls or Quinn receive-buffer gaps.
- Artifact GC appends a durable `ARTIFACT_DELETED` tombstone before unlinking, leases active readers, retains unknown-age pre-v15 records, retries safe orphan digest files, and exposes object/byte/failure counters for alerting.
- SIGTERM/SIGINT now enter a bounded drain: new handshakes are refused, in-flight request streams get the configured grace window, and only then are QUIC connections closed and journals/audit flushed. PTY/event streams that exceed the window reconnect through the normal session resume path.
- Recovered-process monitors use asynchronous status/log reads on their polling path; a detached child cannot repeatedly run synchronous filesystem or process probes on the QUIC reactor.
- Process liveness, group liveness, and cancellation use direct POSIX `kill(2)` probes/signals instead of spawning the external `kill` utility, removing PATH dependence and helper-process latency from recovery polling and SIGNAL requests.
- Bounded semantic Git helpers now run in their own process group; timeout,
  output-limit, and read-error cleanup terminates the verified group and then
  reaps the direct child, so a credential/helper descendant cannot linger
  after its workspace query is refused.
- Recovered PIDs are treated as locators, not identities: ASP validates the persisted wrapper command before monitoring and rechecks it periodically while no child handle is available; Linux reads `/proc/<pid>/cmdline` directly (exact argument matching, no PATH-dependent `ps` helper) and compares `/proc/<pid>/stat` start-time ticks when a record carries them. Legacy WAL records without that identity begin unverified and are never signaled until the wrapper check succeeds, so an ambiguous recovery is recorded as an unknown exit instead of risking a recycled-PID kill. BSD/macOS retain an absolute-path `ps` fallback.
- Age/size compaction now drops exited process records from materialized snapshots while retaining compact command-hash tombstones, so long-lived sessions do not retain every historical command body or accidentally rerun an expired request.
- Local operators can inventory durable sessions as JSON with `aspd
  --list-sessions` and remove a quiescent session with the lock-protected
  `aspd --delete-session UUID`; the destructive path refuses running processes,
  persisted PTYs, and active subscriptions and is covered by a release smoke.

## P0 gates before calling it production

### Identity and authorization

The daemon supports either a JSON principals file that maps bearer tokens to named owners/scopes or an identity-bound mTLS mode that maps CA-validated certificate fingerprints to those owners/scopes. Sessions persist the authenticated owner and every resource operation checks scope plus owner. State directories/files reject symlink substitution and are tightened to private permissions. The extended HELLO handshake negotiates the feature set, and PTY input has sequence/ACK duplicate protection per attachment. A private rotating JSONL audit sink records non-sensitive operation/principal/remote/outcome labels and authentication failures asynchronously, with queue-drop and write-failure counters exposed for alerting; the lock-protected `--migrate-legacy-owner` command now provides a one-shot migration for legacy single-token sessions. Token/certificate rotation and SIGHUP server-certificate reload are documented in the operations runbook, but external PKI/secret distribution and central audit export/retention still need deployment controls. Whole-file PUT and streamed upload now enforce a create-only or caller-supplied base-hash contract at the final commit, preventing a concurrent agent from silently replacing an edited target; `allow_blind`/`--force` is an explicit escape hatch. Keep shell execution under a dedicated least-privilege service account; ASP is not a sandbox. Unauthenticated mode is additionally rejected on any non-loopback listener, and health/metrics exposure remains loopback-only.

The release mTLS rotation smoke now runs this identity path through the
fail-closed production profile, loopback readiness, launcher/resource limits,
and stateless-retry configuration. It remains local evidence rather than an
operator-owned PKI qualification.

### Crash consistency and retention

The log now uses CRC32-protected 64 MiB segments, atomic snapshots, background size/age compaction, a 4 GiB per-session safety quota, an advertised retained cursor, quarantine-on-corruption startup behavior, a checksummed lock-protected state backup/verify/restore flow, and durable process-start intents that block ambiguous child adoption. Backup verification rejects symlinked roots/entries, unexpected or missing files, payload/hash changes, and permission-mode changes before restore. Configure the event, process-log, and artifact retention-hour flags for the deployment, and run under a dedicated supervisor (systemd, launchd, or another process supervisor) for child adoption rather than relying on PID liveness alone. The supplied systemd unit sets `KillMode=process`; without an equivalent setting, supervisor stop/restart may kill session children before ASP can recover them.

Atomic credential, process-metadata, snapshot, and client-cursor writers now set
permissions on the opened descriptor before syncing and publishing the path, and
remove failed temporary names. Shutdown WAL flushing also serializes with the
per-session commit lock, so a stop cannot sync a half-written append or
compaction result. These are local crash/race invariants; off-host recovery and
supervisor drills remain release gates.

Persistent process-log monitors now read at most a 256 KiB batch (four 64 KiB
source reads), `sync_data` that batch, and only then append its `PROCESS_OUTPUT`
event. This keeps the durable journal cursor from outrunning the source file
across a power loss while reducing sync/event overhead; the corresponding
`asp_process_log_sync_*` counters make the tradeoff measurable.
Those counters expose count, bytes, duration, and failures so high-output
qualification can measure the durability cost instead of inferring it from
end-to-end latency.

When `event_consumer_leases` is negotiated, named cumulative ACKs are persisted
in each session's checksummed `event-consumers.bin` sidecar. Compaction defers
behind an unexpired consumer and a seven-day inactivity lease bounds abandoned
state. Backups include the sidecar because it lives below `.asp/`; older peers
continue using the advisory ACK path.

### Protocol completion

The machine-readable schema registry is now checked in as [`docs/schema.json`](schema.json), bundled with the publishable `asp-protocol` crate, compared against the workspace copy by unit tests, and tested again from the generated package tarball. Mixed-release compatibility tests remain the gate described in [`docs/SCHEMA.md`](SCHEMA.md). Protocol v17 uses the bounded `AF` stream envelope and fast zlib compression when it wins, while the server also accepts the tested v16 plain framing and rejects mode/version mismatches. This is in addition to hash-guarded/explicit-blind file writes, the workspace-index and complete-result digest validators, immutable artifact streams, durable artifact-deletion tombstones, principal-scoped cross-session artifact hard-link reuse, the exact loopback `PORT_OPEN` target allowlist, and point-in-time `PROCESS_STATE` reads, while unknown/adjacent versions fail closed. The current client prefers v17 and retries v16 after a failed v17 handshake. `benchmarks/smoke-mixed-release.sh` accepts two release archives plus explicit SHA-256 sidecars, validates both archives, and runs both independently supplied daemon/client directions through in-flight FILE_PUT/artifact continuation; a same-archive local run validates the harness mechanics, while an independently built historical v16 archive is still required for the production compatibility gate. Reverse port leases and persisted in-progress/result states across a real rolling upgrade remain release work. Reliable cursor-based `SUBSCRIBE_EVENTS` now streams retained and live process/file events with bounded-queue lag recovery. Large `asp get`/`asp put` bodies and artifact objects now use bounded streamed frames (up to 1 GiB) with SHA-256 validation and crash-safe checkpoints; artifact ranges are exact and restart-safe. The legacy single-frame request remains for compatibility and is capped at 16 MiB. A 128 MiB logical message ceiling is still a prototype guardrail for other objects.

### Operations and abuse resistance

The mixed-release harness also covers timeout-bound EXEC recovery when supplied
with two release archives; same-digest archives are rejected unless the caller
explicitly opts into a mechanics-only run (which is labeled in the JSON result),
while a separately built historical v16 artifact is still required for the
compatibility gate.

Daemon-level output-attachment, CPU-time, peak-RSS, and (on Linux cgroup-v2
hosts) current/limit cgroup memory/CPU/process gauges are now available. The
remaining accounting gate is supervisor-enforced child/cgroup policy plus
exported alerting, not another application-level counter.

`EXEC`/`EXEC_SUMMARY` now support an explicit `--exec-timeout-seconds` policy
that persists a deadline and terminates the process group with conventional
exit code 124; `SPAWN` remains intentionally unbounded for durable development
servers. The default is disabled for compatibility, and a deployment still
needs a supervisor/worker wall-time policy as the stronger boundary for
untrusted workloads. On Linux, command children set `PR_SET_NO_NEW_PRIVS` by
default; use `--allow-process-privilege-gain` only when a reviewed trusted
launcher requires setuid/file-capability transitions. Do not treat a
client-side request timeout as proof that the remote process stopped.

Wire the exported metrics into authenticated or tightly scoped service-manager probes, scheduled backup checks, dashboards/alerts, and a locked dependency/SBOM/vulnerability scan in CI. v0 now exposes request/failure counters, fixed-cardinality per-operation request-duration histograms, active request streams, active named event consumers, resume counts/replay volume/maximum lag/replay-limit fallbacks, admitted request bytes, encoded response-frame bytes, response-frame write timeouts, PTY input-write timeouts, port-forward payload bytes, principal request/response-budget rejections and limits, active connections, WAL bytes, bounded process-output queue occupancy, request-header/body deadlines (including a size-aware minimum receive rate), bounded response-frame write deadlines, mandatory singleton state-directory, global in-flight-request, aggregate decoded-request and encoded-response memory occupancy/limits, and audit queue/write-failure caps, plus the optional loopback metrics endpoint. Per-principal/session count and request/response-byte quotas and a local durable audit sink are now enforced; central audit export/retention and alert wiring remain deployment gates.

The daemon now exports process CPU time and peak RSS from `getrusage` and
best-effort cgroup-v2 usage/limits when available; these are useful pressure
signals but do not replace supervisor policy, central audit export, or alert
wiring, which remain deployment gates.

Release packaging now includes a GnuPG detached-signature helper over the
checksum sidecar, with optional exact-fingerprint verification and automatic
key retrieval disabled. Key distribution, trust-root rotation, and promotion
policy remain operator-owned supply-chain gates; a local signature smoke
cannot establish that an organization's release key is protected.

## P1 performance work

Dual-stack reconnects now race the first four resolved addresses with a
50&nbsp;ms stagger on one Quinn endpoint, while retaining the 16-address and
single-deadline bounds. This avoids serially waiting on a black-holed address;
the two-host roaming matrix still needs to measure the resulting tail latency.

Same-principal artifact reuse now has a bounded principal+digest index rebuilt
on startup and maintained across commit, retention, and session deletion. The
index is an optimization only: stale or unindexed entries fall back to the
verified session scan. `asp_artifact_index_entries`,
`asp_artifact_dedup_hits_total`, and `asp_artifact_dedup_bytes_total` expose the
index occupancy, actual reuse rate, and logical bytes
avoided; a two-host run still needs to measure hash-validation cost and
cross-filesystem fallback behavior.

- Keep a warm client/agent connection or local connection pool; the CLI now has `asp batch` (including `--stdin`) and a persistent JSONL `asp agent` adapter for EXEC, workspace inspection, and small file mutations over one connection, skips a redundant preflight resume, retries idempotent requests, retries transient initial handshakes, and auto-reconnects interactive PTYs. All retryable requests now reconnect directly after `HELLO` without replaying the event journal: side effects are protected by stable request IDs and durable idempotency records, while reads carry range/digest/offset guards. The reconnect smoke asserts that `asp_resume_requests_total` does not increase for read or side-effect retries; explicit `asp resume` and event subscriptions remain the only implicit journal replay. The supervisor-managed `asp agent-listen` endpoint now reuses a bounded four-connection idle pool for sequential `agent-connect` invocations while isolating concurrent clients, and validates the durable cursor/session before reuse. A warm agent, long-lived event subscriber, interactive shell, and forwarding listener retain their configured Quinn endpoint across reconnects, reusing the UDP socket and rustls session cache instead of rebuilding them after every daemon restart or network flap; this is a transport setup optimization, not a replacement for QUIC migration or session resume. When mTLS client certificate/key options are configured, the endpoint is deliberately rebuilt on reconnect so rotated client credentials are reloaded. Quinn attachments use five-second keepalives and a 15-second idle bound so dead paths are detected sooner than the 30-second default; session state remains durable after that transport timeout. Client QUIC/TLS connection attempts default to ten seconds and can be bounded per invocation with `--connect-timeout-ms` (1–120000 ms), so automation can fail fast without hard-coding a transport timeout into the protocol. Hostname resolution uses Tokio's resolver under the same deadline, so a blocked DNS lookup cannot stall the client's reactor during reconnects; up to 16 unique A/AAAA results are tried within that one connection budget when a dual-stack endpoint has a stale or unreachable first address. A five-command loopback smoke measured 295 ms through the persistent adapter versus 748 ms for five one-shot processes; a separate five-inspect trial was roughly 80 ms versus 330 ms, while a later 20-command loopback trial was only 1,727 ms versus 1,838 ms because local process spawning dominated. These are single-trial process-start measurements; the shaped two-host comparison remains required.
- Client connection settings can now be supplied through explicit `ASP_*` environment defaults (`ASP_SERVER`, `ASP_CERT`, `ASP_AUTH_TOKEN_FILE`, session/certificate identity, PTY preference, and timeout variables), while command-line values take precedence when both parse. Malformed environment values fail fast; token-file use remains preferred over exposing a bearer token in process environments. `ASP_SERVER` fills the endpoint for every server-facing command; positional-ID/path commands additionally expose an explicit `--server` option, while `exec`/`spawn` accept `--command` so a supervised daily profile can reuse one endpoint without repeating it. Legacy `asp COMMAND SERVER ...` syntax remains compatible.
- Daemon deployment settings now accept explicit `ASP_*` environment defaults as well, including listener/root/TLS/authentication paths, readiness, process-boundary and resource-policy settings, retention/quota values, and shutdown policy. This lets a supervisor use a reviewed non-secret `EnvironmentFile` without copying site paths into the binary command line; CLI values still override valid environment values, malformed values fail closed, and credential contents should remain in private files rather than environment variables. The production profile, launcher requirement, and port policy should remain visible in the service unit/preflight so inherited environment state cannot weaken the intended gate.
- If a client host loses its local cursor file, `asp resume SERVER --session-id UUID --after-event-id N` now bootstraps the durable session identity from another trusted host, lets the server re-check owner authorization, and persists the recovered cursor for ordinary commands. The UUID remains an address rather than a credential; omitting the explicit ID retains the saved-cursor-only behavior. This closes a practical laptop-replacement recovery gap without adding a new wire operation.
- Removing the attachment cursor from transport retries keeps the short-command path free of an atomic sidecar write and an implicit journal replay. A fresh three-round loopback check measured 1.28–1.35 s for twenty warm summary commands versus 3.03–3.22 s for twenty cold invocations; raw rows are in `benchmarks/raw/agent-adapter-local-2026-08-29-attachment-memory.jsonl`. These are local regression signals, not a remote-network SLO; the shaped two-host comparison remains required.
- For genuinely independent status/check commands, `asp batch --parallel N --summary --tail-bytes 0` now overlaps up to 32 idempotent EXEC requests over one warm QUIC connection and emits input-ordered exit markers. It intentionally suppresses command output and does not change the safe sequential default; measure it in the two-host matrix before turning the path into a workload-wide latency claim.
- Semantic inspections can now opt out of work they do not need: `include_tree:false` omits the tree payload and, when no search is requested, skips the repository-wide tree walk; `include_git_status:false` skips the Git-status subprocess. The equivalent CLI flags are `asp inspect --no-tree` and `--no-git-status`. The adapter cache key includes both switches so a compact query can never reuse a complete-query result. This is a latency optimization for agents that already have a tree/digest or only need selected files/searches; a search still requires a bounded file walk, and the full defaults remain unchanged.
- Short-lived CLI connections retain their Quinn endpoint until `CONNECTION_CLOSE` drains, so normal `doctor`/`exec`/file-command exits release the daemon's per-principal connection lease promptly instead of waiting for the 15-second idle timeout. The concurrent-agent smoke exercises this close-drain path before filling the advertised 32-connection boundary.
- The bounded `smoke-capacity-soak.sh` keeps independent warm adapters active while repeating summaries, semantic reads, and guarded writes, then requires all connection, request-stream, decoded-frame, and encoded-response gauges to return to zero. The default local run (eight workers for 15 seconds) and the shorter CI profile pass. This closes a local leak/contention gap but is not a multi-tenant capacity SLO; longer independent-workspace soaks and supervisor/cgroup/disk exhaustion tests remain required.
- The capacity-soak harness now bounds the post-work drain with `ASP_CAPACITY_SOAK_DRAIN_GRACE_SECONDS` (default 60 seconds, maximum 600). A watchdog kills blocked writers/adapters and fails the run after the duration-plus-grace deadline, so overloaded local contention cannot create an unbounded test process tree. This improves qualification safety; it does not turn the local smoke into a capacity SLO.
- An extended local run on 2026-08-28 kept eight workers active for 60 seconds and completed 5,116 responses with the same zero-leak postcondition; its raw JSONL row is retained in `benchmarks/raw/capacity-soak-2026-08-28-final.jsonl`. It remains single-host evidence, not a multi-tenant capacity qualification.
- A 16-worker local run on 2026-08-28 kept warm adapters active for 30 seconds at a 200-ms command interval, completed 5,432 responses with zero request failures and zero residual connection/request/frame/response memory, and recorded 9.18 s of daemon CPU across the 127 s drain. The raw row is retained in `benchmarks/raw/capacity-soak-2026-08-28-16x30.jsonl`; the extended drain is process-launch contention evidence, not a production latency SLO or multi-tenant qualification.
- The `EXEC_SUMMARY` attachment path now accumulates its bounded tail inside the process monitor and emits no live output chunks, while retaining the complete durable log and journal events. A rebuilt 16-worker/30-second local soak completed 5,474 responses with zero request failures and zero residual memory; its 120-second drain is retained in `benchmarks/raw/capacity-soak-2026-08-28-summary-fastpath.jsonl`. The shorter drain is an improvement, but per-command process launch and lifecycle durability still dominate high-concurrency throughput and require a real multi-workspace SLO qualification.
- A follow-up rebuilt run with the relaxed command-metadata/committed-intent cleanup barriers completed the same 5,474 responses in a coarse 116 seconds and used 8.86 seconds of daemon CPU; the raw row is `benchmarks/raw/capacity-soak-2026-08-28-summary-fastpath-relaxed.jsonl`. This is directionally better than the earlier 127-second baseline, but the runs are not controlled performance trials and process launch/lifecycle durability remain the bottleneck.
- A rebuilt 8-worker/15-second telemetry run recorded 2,740 adapter responses and 454 successful process launches with zero launch failures; cumulative launch time was 39.27 seconds (86.5 ms per launch) and all resource gauges returned to zero. The raw row is `benchmarks/raw/capacity-soak-2026-08-29-launch-metrics.jsonl`; this is a local regression signal, not a cross-host SLO.
- A launch-path optimization now syncs one exact, private process-wrapper template per session and hard-links it into each process record. Two rebuilt 8-worker/15-second runs recorded 456 launches in 32.72 seconds (71.8 ms/launch) and 453 launches in 30.97 seconds (68.4 ms/launch), with zero launch failures and zero residual resources. Against the earlier same-profile 454-launch/39.27-second capture (86.5 ms/launch), this is directionally about 17--21% lower wrapper/launch-transaction time. The paired raw rows are in `benchmarks/raw/capacity-soak-2026-08-29-wrapper-reuse.jsonl`; filesystem variance and the absence of a controlled host mean this is an optimization signal, not a production SLO or a universal percentage claim.
- The pending process/file/artifact intent paths now avoid a redundant parent-directory `fsync`: `write_atomic_file` already syncs the renamed file and parent directory before returning. A paired 8-worker/15-second run retained 2,734 responses and 453 successful launches with zero failures and zero residual resources; cumulative launch time fell from 32.37 s (71.5 ms/launch) to 29.45 s (65.0 ms/launch), about 9% in this local pair. Raw rows are in `benchmarks/raw/capacity-soak-2026-08-29-atomic-dir-sync.jsonl`; this is a durability-preserving local optimization signal, not a production SLO.
- Fresh persisted stdout/stderr logs are now opened with exclusive creation and descriptor-level 0600 mode, avoiding a follow-up metadata open/`fchmod` while retaining no-follow and stale-path rejection. The persisted-output test asserts both modes. A follow-up 8-worker/15-second soak remained clean at 442 launches and 65.4 ms/launch; the difference from the preceding pair is within local filesystem variance, so it is not treated as another percentage improvement.
- The exact packaged release path now has an end-to-end runtime smoke in `benchmarks/smoke-packaged-runtime.sh`: it verifies and extracts the archive, runs the fail-closed production profile, checks strict readiness, exercises the packaged bounded parallel-summary batch path (including nonzero exit propagation), performs a packaged file round trip, then kills/restarts the packaged daemon, bootstraps the session from an explicit UUID into a second cursor location, and verifies detached-process/log recovery through that recovered cursor. A rebuilt native archive passed this smoke on 2026-08-29. This closes the gap between source-tree smoke coverage and the binaries an operator actually installs; it remains loopback evidence, not a supervisor, WAN, or multi-host qualification.
- The packaged `deploy/upgrade-release.sh` now provides an explicit, readiness-gated supervisor rollout with automatic restoration of `previous` on failure and a fail-closed prefix lock that serializes concurrent install/restart/rollback transactions; `benchmarks/smoke-upgrade-release.sh` forces lock contention, a failed restart, and then a successful upgrade against the real packaged binaries. The local rollout gate passed on 2026-08-29. It does not prove independent-host, historical-binary, or service-manager behavior, which remain P0 release gates.
- The release-upgrade path now runs a non-mutating prefix trust preflight before creating its transaction lock, rejects untrusted symlinked or group/world-writable existing ancestors, and repeats the check after locking; the installer performs the same check before publishing. The packaged upgrade smoke covers both unsafe-prefix classes and invokes the installer preflight on a missing leaf to assert that no directory or lock is created. This closes a local deployment-boundary gap but does not replace independent-host, supervisor, or privilege-isolation qualification.
- `deploy/verify-release.sh`, `deploy/verify-release-signature.sh`, and `deploy/install-release.sh` now take bounded private snapshots of release material before listing, signature verification, or extraction. They verify and extract only those snapshots, closing archive/checksum/signature pathname-replacement windows; the packaged release-signature smoke injects replacement attempts into both the standalone verifier and installer. This protects the local release boundary, but release-key custody and promotion provenance remain operator responsibilities.
- The benchmark evidence gate now rejects non-finite or negative required resource counters (including interface bytes and daemon/client CPU/RSS), requires byte/RSS/count counters to be integers rather than merely JSON numbers, and the strict `agent-workload` profile also requires finite timing/payload fields, successful persistence observation, and ASP Quinn transport counters. CI and the two-host contract smoke include negative and fractional-counter regressions, so a hand-edited capture cannot turn an impossible resource value into a plausible percentile.
- `smoke-transfer-restart.sh` can now pin the initial daemon to the v16 plain-framing ceiling and restart it with v17 (`ASP_TRANSFER_RESTART_INITIAL_MAX_PROTOCOL_VERSION=16`, `ASP_TRANSFER_RESTART_RESTARTED_MAX_PROTOCOL_VERSION=17`). An 8 MiB macOS run resumed FILE_PUT from a 196,608-byte durable prefix and artifact upload from a 262,144-byte prefix, both byte-for-byte with status 0. The smoke still uses one current binary with an explicit ceiling, so independently built historical-peer compatibility remains a release gate.
- `smoke-transfer-restart.sh` pauses an in-flight streamed FILE_PUT and artifact upload after a nonzero durable prefix, kills/restarts the daemon, and verifies that the original client resumes byte-for-byte. Resumable clients yield once after the readiness handshake and pace each four-frame (256 KiB) continuation burst with a 10 ms pause; this prevents Quinn's bounded receive assembler from treating a fast restart burst as a transport failure without imposing a per-64 KiB sleep on the whole transfer. The smoke is a local crash/restart invariant, not a substitute for the two-host impairment matrix.
- Extend the indexed/versioned workspace state to cache search results and git metadata with field-level deltas, and add file/process subscriptions so unchanged state is not recomputed or sent. The tree index, epoch/generation validator, native watcher, invalidation race retry, fresh-scan fallback, bounded repeated-search cache, bounded Git metadata cache, v13 complete-result digest validator, and a watcher-invalidated daemon-side digest-only fast path are now present; the persistent agent adapter retains a bounded local semantic cache and reconstructs unchanged results without transferring their payloads. Selected-file reads and Git output larger than the cache threshold still run per request when the digest cache misses or expires. File preconditions remove one extra read round trip for agents that already carry a base hash, and `FileStored.version` is a workspace-shared monotonic clock across sessions.
- Extend log modes from the bounded `EXEC_SUMMARY`, snapshot-relative `tail_bytes`, and durable range result to filtered tails; immutable artifact fetch/range plus retention leases/GC are implemented. Large stream frames now use transparent bounded zlib when it wins. The CLI and cached agent path choose a prefix/suffix or multi-range delta versus a full PUT using conservative size thresholds; equal-length byte runs and bounded line-aware length-changing source edits can stay in separate `FILE_PATCH_RANGES`, while oversized/ambiguous matches fall back safely. The deterministic v17 `asp-bench file-sync` capture shows 516 B saved for a localized edit, 2,458 B for three scattered length-changing source edits, 409 B saved for three equal-length scattered edits with `FILE_PATCH_RANGES`, and a deliberate PUT fallback for a broad rewrite. Generalized content-defined delta selection and codec calibration remain. Full exact 10 MiB output cannot become fewer bytes without changing the contract.
- The optional `pty_rich_state` path now preserves ANSI cell attributes in a bounded full redraw while older peers continue to receive the plain snapshot. The separately negotiated `pty_rich_compression` path zlib-compresses oversized rich-state datagrams when the result fits the QUIC DATAGRAM budget; older peers never receive the `PZ` marker. Plain peers may also negotiate the bounded `pty_state_delta` path, which sends complete changed rows relative to an exact generation and emits a full checkpoint at least every 16 updates so datagram loss self-heals. The separate `pty_scrollback` capability now supplies a bounded plain-text history page after `PTY_READY`, so a recreated client retains recent context. Add RoSE/wezterm-quality terminal state parity, speculative echo, and explicit cross-network delta-loss tests before claiming Mosh-class PTY behavior. Leave QUIC congestion, loss recovery, migration, and NAT traversal to Quinn/Tailscale.
- PTY screen materialization is generation-keyed and cached inside the durable PTY owner. Plain and rich attachments therefore share one parser render per output generation instead of independently walking the terminal at the 60 Hz datagram cadence; output and resize invalidate both caches. This reduces CPU contention for an interactive shell plus one or more agent observers without changing the lossless stream or latest-wins state contract. The cache is an implementation optimization, not a replacement for the RoSE-quality diff/echo work still listed above.
- The client keeps compressed and genuinely large response frames on the blocking codec pool, but decodes ordinary uncompressed 64 KiB transfer chunks inline. This avoids one scheduler task per chunk, keeps Quinn's bounded receive assembler from accumulating avoidable ranges during fast artifact/file downloads, and preserves off-reactor handling for expensive compression or large semantic responses. Five consecutive 64 MiB daemon-restart transfer smokes passed after this change; the result is a local stability signal, not WAN throughput evidence.
- A client regression test now locks the response-codec classifier at that boundary: ordinary 64 KiB chunks stay inline on both protocol versions, genuinely large plain responses use the blocking pool, and compressed v17 frames remain off-reactor. This protects the transfer fix from threshold drift while leaving the wire contract unchanged.

The optional `pty_scrollback` capability now sends a bounded plain-text
history page on a fresh PTY attachment, so restarting the client process does
not discard all recent terminal context. The page is limited to 256 rows and
256 KiB, is emitted only after explicit feature negotiation, and leaves the
existing v16/v17 response sequence unchanged for older peers. tmux-backed
sessions obtain the page with a bounded `capture-pane` call through the same
validated launcher used for PTY ownership; the call runs off the Tokio reactor
and falls back safely if tmux is unavailable or slow. This closes the basic
reconnect-history gap; full wezterm/RoSE terminal parity and speculative local
echo still require separate work and cross-network measurement.

## Evidence required for the production gate

Run two-host, multi-trial (at least 30 trials per cell) tests covering 0/20/100/200/300 ms RTT, 0/1/5/10% loss, 0/20/100 ms jitter, 1/10/100 Mbps, abrupt disconnect, address migration, and laptop sleep/wake. Compare warm SSH+agent, Mosh/RoSE for terminal steps, and ASP. Record p50/p90/p99 latency, output integrity, reconnect time, application/interface bytes, CPU, RSS, and failures with raw rows and packet captures. For the structured agent fixture, `benchmarks/docker-agent-matrix.sh` now provides fresh-container trial IDs and atomic aggregation; run it separately for exact-output and `EXEC_SUMMARY` contracts before applying the qualification gate. `benchmarks/compare-agent-contracts.sh` rejects exact/summary pairs with mismatched migration/sleep metadata or an incomplete operator hook.
Use `bash benchmarks/qualify-results.sh RESULTS.jsonl` to reject incomplete,
duplicate, malformed, or failed cells before publishing a comparison; use the
explicit `command-latency` matrix profile for the complete 13-cell
RTT/loss/jitter/bandwidth sweep:

```sh
bash benchmarks/qualify-results.sh RESULTS.jsonl 30 command-latency
```

The profile rejects omitted scenarios or systems; use `agent-workload` for a
two-system agent capture. Run `benchmarks/summarize-results.sh` only after the
qualification gate and retain the raw rows.
Agent-workload captures must also include finite non-negative timings,
payload/count fields, persistence observation, interface bytes and client
CPU/RSS fields on both systems, plus ASP daemon CPU/RSS and Quinn transport
counters on ASP rows; the qualifier rejects older or hand-edited captures
that omit those evidence costs.

The current evidence is deliberately bounded. A fresh Linux-container cell now has
30 paired successful trials at approximately 100 ms RTT (50 ms one-way `tc
netem`, 100 Mbps, zero loss/jitter, no deliberate outage). ASP p50 wall time was
3,236 ms versus 9,484 ms for warm SSH ControlMaster, network-blocked time was
2,212 ms versus 8,359 ms, recovery was 341 ms versus 1,569 ms, and application
gates were 12 versus 18. Because the fixture's exact 10 MiB log is all `x`, ASP
used 104,996 interface bytes per direction at p50 versus SSH's 10,566,544; this
is a compression-sensitive result, not a universal bandwidth claim. The older
single 30-second-outage trial remains the evidence for durable sleep/reconnect.
The 30-trial cell strengthens the semantic-session direction, but it is still
one container and one no-loss condition; the two-host full matrix, migration,
loss, and sleep/wake measurements remain release gates. A corrected
30-trial incompressible exact-output cell
(`benchmarks/raw/docker-agent-workload-2026-08-27-rtt100-incompressible.jsonl`)
measured 4,475 ms versus 9,261 ms p50 wall time and 10,853,696 versus 10,584,889
interface bytes per direction, confirming that the latency gain survives
without compression while exact output is not a bandwidth win. Its paired
`EXEC_SUMMARY` capture reduced p50 interface bytes to 47,694 per direction and
application payload to 8,403 B while retaining the full log. A separate 1,170-row cold command-start matrix
(`benchmarks/raw/docker-command-latency-2026-08-27-30trials.jsonl`; 39 cells,
30 trials per cell) completed with zero failures across RTT, loss, jitter,
bandwidth, and a harsh corner condition; it shows ASP ahead of SSH/Mosh
startup in this single-container harness, but it is not a warm-session,
keystroke, or independent-host SLO. A paired 30-trial `EXEC_SUMMARY` capture
also reduced ASP's p50 application payload 99.92% (10,485,971 B to 8,403 B)
and interface bytes 62.2% (104,996 B to 39,981 B per direction) while keeping
the full log durable; this is a contract-level optimization, not an exact-output
or universal bandwidth guarantee. The incompressible contract comparison is
documented separately in `docs/BENCHMARKS.md` and still needs the two-host
impairment matrix before it becomes a production SLO.
