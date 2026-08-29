# ASP operations runbook

This runbook is for the current production-shaped deployment: one trusted
workspace, a dedicated service identity, durable `.asp/` storage, and a
private network such as Tailscale. ASP is not yet a multi-tenant Internet
service or a sandbox for hostile commands.

## Install and preflight

Build a pinned release and install `aspd` and `asp` as immutable binaries. From
the repository, `mise exec -- deploy/package-release.sh /srv/asp/releases`
creates a versioned client/server archive and SHA-256 checksum; verify that
artifact with `deploy/verify-release.sh /srv/asp/releases/asp-VERSION-TARGET.tar.gz` before
installing the two binaries together. Verification takes a bounded private
snapshot before listing or extracting, then checks the checksum, archive paths,
traversal/special-file/link safety, required templates/schema, and absence of
credentials/state, and validates the bundled SPDX SBOM when `jq` is available.
On a Linux host, use the
supplied systemd unit; on macOS, use the launchd template. For a fail-closed
Linux deployment, install `deploy/systemd/aspd-production.service` instead of
the pilot baseline; it refuses to start until the reviewed
`/usr/local/libexec/asp-worker-wrapper` exists and passes `--production` to
both the preflight and live daemon.
Create a dedicated account and make the workspace writable only by that
account. Keep `.asp/` on durable storage, not a temporary directory.

For a promoted release, sign the checksum with an operator-owned GnuPG key and
verify the exact fingerprint on the deployment host before installing. The
bundled helpers use no automatic key retrieval; distribute the trusted public
key through the site's existing key-management channel:

```sh
deploy/sign-release.sh --key-id "$ASP_RELEASE_SIGNING_KEY" \
  /srv/asp/releases/asp-VERSION-TARGET.tar.gz \
  /srv/asp/releases/asp-VERSION-TARGET.sha256
deploy/verify-release-signature.sh \
  --fingerprint "$ASP_RELEASE_SIGNING_FINGERPRINT" \
  /srv/asp/releases/asp-VERSION-TARGET.tar.gz \
  /srv/asp/releases/asp-VERSION-TARGET.sha256 \
  /srv/asp/releases/asp-VERSION-TARGET.sha256.asc
```

Keep signature verification in the promotion pipeline; checksum verification
alone proves integrity but not release identity.
The atomic installer and readiness-gated upgrader can enforce this boundary
themselves with `--require-signature`,
`--signature /srv/asp/releases/asp-VERSION-TARGET.sha256.asc`, and
`--fingerprint "$ASP_RELEASE_SIGNING_FINGERPRINT"` (or the matching
`ASP_REQUIRE_RELEASE_SIGNATURE=1`, `ASP_RELEASE_SIGNATURE`, and
`ASP_RELEASE_SIGNING_FINGERPRINT` environment variables). Leave the required
flag and exact fingerprint in the production promotion job; checksum-only
invocation remains available for local development.

The archive also contains `deploy/install-release.sh` for a safe binary switch.
After verifying the checksum, install into a versioned prefix rather than
copying over a running executable:

```sh
deploy/install-release.sh --prefix /usr/local/lib/asp \
  /srv/asp/releases/asp-VERSION-TARGET.tar.gz
```

The script verifies the archive again, refuses symlinked or conflicting release
directories, rejects an untrusted symlink or group/world-writable existing
directory in any prefix component, extracts both binaries before publishing
them, and atomically updates `/usr/local/lib/asp/current`. Sticky shared
ancestors and root-owned compatibility aliases below non-writable parents are
accepted; mutable-prefix links/directories fail closed. It keeps
`/usr/local/lib/asp/previous` for a tested rollback:

```sh
deploy/install-release.sh --prefix /usr/local/lib/asp --rollback
```

It does not restart the supervisor. Run the exact `--validate-config` command
below against `current/bin/aspd`, then perform the planned drain/restart. Keep
old release directories until the new daemon has passed readiness and the
rollback window has closed.

For a supervised rollout with an automatic readiness-gated rollback, use the
release's `deploy/upgrade-release.sh`. It waits for the current `/ready` probe,
installs and atomically activates the archive, runs the explicit supervisor
restart command, and restores `previous` if the new daemon does not become
ready. The helper never guesses how a service is managed:

```sh
deploy/upgrade-release.sh \
  --prefix /usr/local/lib/asp \
  --ready-url http://127.0.0.1:9443/ready \
  --restart-command 'systemctl restart aspd-production.service' \
  --ready-timeout-seconds 120 \
  /srv/asp/releases/asp-VERSION-TARGET.tar.gz \
  /srv/asp/releases/asp-VERSION-TARGET.sha256
```

The restart command is operator-controlled and must leave the daemon using the
`current` pointer. The helper accepts only loopback HTTP `/ready` URLs and
returns failure even when rollback succeeds, so promotion tooling cannot treat
a failed rollout as successful. Use `--skip-current-ready` only for deliberate
recovery when the current daemon is already down. Before it creates the
transaction lock, the helper performs the same non-mutating prefix trust
preflight as the installer and fails closed on an untrusted symlink or
group/world-writable existing ancestor; the installer repeats that check before
publishing the new release. Keep the previous release directory until the
post-upgrade observation window has passed.

Before every start or binary/configuration change, run the non-mutating
preflight with the exact paths used by the service:

```sh
aspd --validate-config \
  --root /srv/asp/workspace \
  --cert /srv/asp/workspace/.asp/server-cert.der \
  --key /srv/asp/workspace/.asp/server-key.der \
  --auth-token-file /srv/asp/workspace/.asp/auth-token \
  --health-listen 127.0.0.1:9443
```

The check must pass before the supervisor is restarted. It verifies existing
credentials, permissions, paths, retention, budgets, and the configured
filesystem headroom without creating state or taking the daemon lock.
In `--production` mode, no untrusted existing component of the configured
workspace-root path may be a symlink; root-owned compatibility aliases below
non-writable parents (such as macOS `/var`) are permitted. This prevents a
service restart from silently moving the durable state boundary to a different
tree without rejecting normal system layouts.

For a fail-closed production deployment, add `--production` to both the
preflight and the matching `ExecStart` command, and provide an absolute,
reviewed process boundary plus nonzero `--process-cpu-seconds` and
`--exec-timeout-seconds` values:

```sh
aspd --production --validate-config \
  --root /srv/asp/workspace \
  --cert /srv/asp/workspace/.asp/server-cert.der \
  --key /srv/asp/workspace/.asp/server-key.der \
  --auth-token-file /srv/asp/workspace/.asp/auth-token \
  --process-launcher /usr/local/libexec/asp-worker-wrapper \
  --process-cpu-seconds 86400 \
  --exec-timeout-seconds 3600 \
  --min-free-bytes 1073741824 \
  --disable-port-forwarding \
  --health-listen 127.0.0.1:9443
```

The profile refuses to initialize if authentication, readiness/metrics,
launcher, command limits, filesystem headroom, a group/world-writable
workspace path/non-sticky ancestor, or an explicit port policy are missing. Use
`--disable-port-forwarding` when the workspace does not need dev-server
forwards, or repeat `--port-target 127.0.0.1:3000` (and other exact loopback
targets) to allow only those services. It does not provide a sandbox itself;
the wrapper must enforce the site's filesystem/network/cgroup policy and must
preserve ASP's `exec`/PID contract. Keep the non-production command for local
development only.

Production preflight also requires a regular executable `tmux` (or an absolute
`ASP_TMUX_PATH`) with no group/world-write bits before the listener is opened,
so a missing or replaceable durable PTY supervisor fails at startup instead of
on the first `PTY_OPEN` request.

On a service restart, `aspd` refuses new QUIC handshakes and drains active
request streams for 10 seconds by default before flushing journals and audit
records. Set `--shutdown-grace-seconds` (0–600) below the supervisor's stop
timeout, leaving margin for synchronous WAL/audit flushes (the supplied
systemd unit uses 10s/30s). Long-lived PTY/event streams that exceed the
window resume from their durable session cursor after reconnect.

## Network and identity

Bind to loopback for local use. For a remote client, bind to the Tailscale
interface or explicitly acknowledge a private/firewalled address with
`--allow-non-loopback`; never publish UDP/4433 directly to the Internet.
`--insecure-no-auth` is rejected whenever the QUIC listener is non-loopback,
even if `--allow-non-loopback` is supplied. The optional health endpoint is
always required to be loopback-only, so do not put it directly on a remote
interface; expose it through an authenticated monitoring proxy if needed.
Clients pin the server certificate (or a bounded directory of DER pins) and
must receive the certificate and credential through a separate trusted
channel. The client uses `localhost` for TLS SNI by default to match the
generated certificate; with an operator-issued certificate, pass
`--server-name` containing the certificate's DNS name or IP SAN.

For the trusted single-workspace pilot, the release includes
`deploy/bootstrap-client.sh` to fetch the generated certificate and bearer
token over an already verified OpenSSH connection:

```sh
deploy/bootstrap-client.sh \
  --output-dir "$HOME/.config/asp/servers/dev" \
  dev-user@dev-host /srv/asp/workspace
```

The helper keeps normal SSH host-key and authentication policy, rejects
ambiguous targets and remote path traversal, installs only regular `0600`
credential files, and publishes the pair as one local directory. It is a
bootstrap convenience for a trusted owner, not a substitute for operator-owned
PKI, mTLS identity, or secret distribution in shared/Internet-facing
deployments. Its two `scp` transfers use a 20-second connection bound and
five-second server-alive probes so an asleep/unreachable host cannot leave a
partial staging directory hanging indefinitely.

Client DNS resolution and QUIC/TLS connection attempts are bounded to ten
seconds by default; up to 16 unique addresses returned by a dual-stack lookup
are tried within that one budget, with the first four raced at a 50 ms
stagger so a black-holed address does not delay a usable family. Tune the
global client option `--connect-timeout-ms` (1–120000) for a deliberately
short fail-fast probe or a higher-latency link; reconnect loops retain their
own bounded retry/backoff policy and ordinary request recovery remains
available for up to 90 seconds, covering the documented 30-second sleep
window. Tune the global `--reconnect-timeout-ms` option (1–600000) when a
deployment needs a longer or fail-fast recovery policy. Opening a new
bidirectional request stream
has a separate ten-second bound, so an exhausted peer stream limit fails as a
retryable transport error instead of hanging the caller; long-lived PTY/event/
port attachments use the same bound when they are created. Each request frame
write also has a size-aware 64 KiB/s minimum-rate deadline (10-second floor,
five-minute cap), so a peer that stops consuming an admitted stream cannot
hold an upload indefinitely.

The daemon's `--stateless-retry` option enables Quinn's address-validation
token exchange for unvalidated QUIC Initial packets. The fail-closed
`--production` profile enables this automatically; development daemons may
opt in explicitly when they listen on a shared interface. Quinn owns the
retry-token cryptography and validation. Expect one additional flight on the
first connection, while durable sessions and subsequent migration/reconnect
behavior remain unchanged. Monitor
`asp_quic_stateless_retry_enabled`,
`asp_quic_stateless_retries_total`, and
`asp_quic_stateless_retry_failures_total` in `/metrics` for the effective
guard configuration, path validation, or firewall problems.

For clients that run outside the workspace, the connection defaults can be
kept in the environment instead of repeated on every invocation:
`ASP_SERVER`, `ASP_CERT`, `ASP_SERVER_NAME`, `ASP_SESSION_FILE`, `ASP_CONSUMER_ID`,
`ASP_AUTH_TOKEN_FILE`, `ASP_PREFER_PTY_DELTA`, `ASP_CONNECT_TIMEOUT_MS`,
`ASP_RECONNECT_TIMEOUT_MS`, `ASP_CLIENT_CERT`, and `ASP_CLIENT_KEY` (or
`ASP_AUTH_TOKEN` for controlled automation). Explicit command-line flags take
precedence when the environment value parses; malformed environment values are
reported instead of ignored. Keep bearer tokens in a private `0600` file when
possible; an environment token may be visible to local process inspection.
`ASP_SERVER` supplies the endpoint for every server-facing subcommand,
including positional-ID/path commands. For example,
`ASP_SERVER=... asp status PROCESS_UUID` and
`ASP_SERVER=... asp put local.txt remote.txt` omit the endpoint safely; those
commands also accept an explicit `--server SERVER`. The explicit
`--command` form avoids ambiguity with the legacy
`asp exec SERVER COMMAND...` and `asp spawn SERVER COMMAND...` forms. All
legacy `asp COMMAND SERVER ...` forms remain supported; `agent-connect` is
local-only and ignores this variable.

The daemon's deployment arguments also accept explicit environment defaults,
including `ASP_LISTEN`, `ASP_ROOT`, `ASP_CERT`, `ASP_KEY`,
`ASP_AUTH_TOKEN_FILE`, `ASP_HEALTH_LISTEN`, `ASP_AUTH_PRINCIPALS_FILE`,
`ASP_CLIENT_CA`, `ASP_AUTH_CERTIFICATES_FILE`, `ASP_PROCESS_LAUNCHER`,
`ASP_REQUIRE_PROCESS_LAUNCHER`, `ASP_PROCESS_CPU_SECONDS`,
`ASP_EXEC_TIMEOUT_SECONDS`, `ASP_MIN_FREE_BYTES`, and
`ASP_DISABLE_PORT_FORWARDING`, together with retention, quota, and shutdown
settings. A supervisor may load these from a root-owned, mode-0644
configuration file when the values are non-secret. Keep the bearer token and
private keys in separate mode-0600 files and reference their paths; do not put
credential contents in an environment file. Command-line values override valid
environment values, while malformed values fail closed. Keep the production
profile, process boundary, and port policy visible in the reviewed service
unit or preflight invocation so an inherited environment cannot silently
weaken the deployment contract.
One-shot response reads have a matching five-minute bound; a stalled control
request therefore returns a retryable timeout instead of hanging a CLI or
adapter forever. Long-lived PTY/event/port streams intentionally bypass that
bound and rely on QUIC liveness plus their reconnect loops.

Use the default bearer token for one owner. For shared workspaces, prefer
CA-validated mTLS (`--client-ca` plus `--auth-certificates-file`) or a
principals file with narrow scopes. Replace credentials atomically. The daemon
accepts `SIGHUP` to reload a complete server certificate/key pair for new QUIC
handshakes without dropping existing sessions; a failed or mismatched reload
keeps the last known-good configuration. For a no-downtime pin rollover,
distribute a directory containing both the old and replacement DER certificates
to clients and point `--cert` at it before reloading. Then verify `/ready` and a
fresh client connection. Authentication-map changes are re-read on requests;
client-CA bundle changes take effect on a successful `SIGHUP` reload. Map a
replacement identity and test it before revoking the old one.

For no-downtime client-CA rotation, provision `--client-ca` as a directory
containing the current and replacement regular `.der` CA files (the daemon
accepts at most eight files and 16 MiB total, and rejects symlinks). Install
the new client certificates and fingerprint-map entries, send `SIGHUP`, and
verify `/ready` plus a fresh client connection before removing the retired CA
and mapping in a later change. The SIGHUP reload is fail-closed, so an invalid
bundle leaves the last known-good TLS configuration active.

For a legacy single-token workspace, stop the daemon and bind only its legacy
sessions to an explicitly selected principal:

```sh
aspd --root /srv/asp/workspace \
  --auth-principals-file /etc/asp/principals.json \
  --migrate-legacy-owner alice
```

Do not use this command to combine sessions belonging to different owners.

## Daily operation

Run `asp doctor --strict SERVER` after provisioning or a rollout when a
client-side gate is useful: it confirms that the negotiated protocol is
supported, authentication is enabled, and the server can currently resolve
the tmux PTY backend. When the command runs on the daemon host, add
`--ready-url http://127.0.0.1:PORT/ready` to include the loopback readiness
probe in the same bounded preflight; the URL must be a literal loopback HTTP
address. The command still prints the authenticated health JSON on success.
The readiness check covers the audit sink, storage headroom,
process-launcher identity, live authentication-source validity, and drain
state; `asp_auth_config_healthy=0` identifies a missing or malformed rotated
secret. A failed `/ready` JSON response also carries stable `ready_reasons`
codes (for example `auth_config_unhealthy`, `storage_headroom`,
`process_launcher_unhealthy`, or `draining`) so remediation does not depend on
interpreting the full health snapshot. The release production-policy smoke
exercises this contract by removing and restoring the token file without
restarting the daemon.

Keep one `asp agent SERVER` process alive for an AI coding agent. It avoids a
new process and QUIC handshake per request and provides idempotent request IDs,
resume cursors, semantic workspace inspection, and hash-guarded file writes.
The client keeps its cursor in the per-user state directory by default rather
than inside the checked-out workspace; pass `--session-file` when a deployment
needs an explicitly managed location. If two agents follow the same session,
give each a stable, distinct `--consumer-id`; this creates independent local
event cursors in the same lock-protected state file. A new consumer can attach
to the existing per-server session entry and then advances only its own cursor.
The cursor file is capped at 8 MiB. Once that bound is reached, a new cursor
update fails closed rather than publishing metadata that subsequent clients
would reject; remove stale server/consumer entries or choose a fresh
`--session-file` before retrying.

If the client host loses its cursor file, recover a known session from another
host with `asp resume SERVER --session-id UUID --after-event-id N` (use `0` to
request the current snapshot plus all retained events). The server authorizes
the UUID against the authenticated owner before returning state; the UUID is
never treated as a credential. A successful explicit resume writes the selected
session into the local cursor file so subsequent daily commands can use the
ordinary saved-cursor path. Without an explicit ID, `asp resume SERVER` keeps
the safer saved-cursor-only behavior.

For a durable process/file event feed, keep `asp events SERVER` running with a
stable `--consumer-id`. It reconnects and resubscribes from the saved cursor
after a daemon restart or network loss; bearer-token clients retain the Quinn
endpoint's UDP socket and TLS session cache across that reconnect, while mTLS
clients rebuild the endpoint so rotated client credentials are reloaded. The
release smoke `bash benchmarks/smoke-events-reconnect.sh` verifies one
pre-restart and one post-restart event arrive exactly once. The companion
`bash benchmarks/smoke-event-cursor-safety.sh` verifies that filtered EXEC and
SPAWN result streams cannot advance this durable cursor past unrelated journal
entries; run both after changing client retry or resume code.

If the agent launcher itself is supervised, keep a local adapter endpoint warm
instead of starting one adapter process per agent: `asp agent-listen SERVER
/run/user/$UID/asp-agent.sock`, then connect with `asp agent-connect
/run/user/$UID/asp-agent.sock`. The endpoint uses the same JSONL adapter
contract, accepts multiple local clients, reuses a bounded four-connection
idle pool for sequential clients, bounds queued output to 64 MiB, and
requires an absolute socket path below a non-group/world-writable directory.
Run it under the same user that owns the client cursor and terminate it with
SIGTERM so the socket is removed cleanly. Local socket access is not remote
authorization; the listener still authenticates every QUIC connection and
enforces the remote session owner/scopes.
Use `asp exec --summary` for builds/tests whose complete output is not needed
immediately; use `asp logs` for a durable range later.

Set `--exec-timeout-seconds` in the service command line to bound attached
`EXEC`/`EXEC_SUMMARY` commands that sleep or wait forever. A timed-out process
group reports exit code 124 and the deadline survives a daemon restart;
detached `SPAWN` processes remain long-lived by design. Keep a supervisor or
per-workspace worker boundary for untrusted commands.

For that worker boundary, set an absolute executable with
`--process-launcher /absolute/path` and optional repeated
`--process-launcher-arg VALUE` flags. ASP appends `/bin/sh` and the command (or
durable wrapper path) for EXEC/SPAWN, passes the absolute `tmux` command and
its arguments for PTY creation, and passes the canonical Git executable for
semantic workspace queries. The launcher must `exec` its arguments rather
than forking a hidden intermediary; this keeps PID identity, process groups,
timeout, and signal handling correct. Add `--require-process-launcher` to make
startup fail closed when the policy is missing. The launcher is an integration
point, not a claim that ASP has a sandbox: choose a reviewed `bwrap`, system
supervisor, or site wrapper that supports both command shapes and preserves
children across daemon restarts. Do not use a `--die-with-parent` policy for
durable `SPAWN` or tmux sessions unless the intended behavior is to terminate
them with the daemon. On Linux, ASP also sets
`PR_SET_NO_NEW_PRIVS` for EXEC/SPAWN children by default; this blocks setuid and
file-capability privilege gains in the command tree. A reviewed trusted
launcher that genuinely needs privilege transitions must opt out explicitly
with `--allow-process-privilege-gain`.

The health metrics `asp_process_launcher_configured` and
`asp_process_launcher_required` expose whether that external EXEC/SPAWN
boundary is installed and whether startup was configured to fail closed without
it. `asp_process_launcher_healthy` is continuously checked against the
launcher's startup filesystem identity; if it becomes `0`, `/ready` returns
503 and new process requests fail closed until the reviewed executable is
restored and `aspd` is restarted.

For PTY state tuning, compare `asp_pty_state_delta_datagrams_sent_total` and
`asp_pty_state_delta_datagram_bytes_total` with the ordinary
`asp_pty_state_datagrams_sent_total`/`asp_pty_state_datagram_bytes_total`
counters. `asp_pty_state_delta_datagrams_skipped_total` records deltas that
did not fit the negotiated DATAGRAM budget or could not be encoded; periodic
full checkpoints remain visible in the ordinary counters.

Check local readiness and liveness:

```sh
curl -fsS http://127.0.0.1:9443/live
curl -fsS http://127.0.0.1:9443/ready
curl -fsS http://127.0.0.1:9443/metrics
```

During a planned restart, `/live` intentionally remains `200` while `/ready`
returns `503` with `draining: true` and `asp_draining 1`. Treat that state as
an intentional handoff: stop assigning new agent work, let the configured
shutdown grace drain active requests, then wait for the supervisor to report
the replacement daemon ready.

Session inventory and cleanup are local maintenance operations. Stop `aspd`
first (the state lock makes this explicit), then inspect durable sessions as
JSON or delete one only after its processes and PTY have been drained:

```sh
aspd --root /srv/asp/workspace --list-sessions
aspd --root /srv/asp/workspace --delete-session SESSION_UUID
```

Deletion is fail-closed for running processes, persisted PTYs, and active
subscriptions; it never recursively removes an arbitrary path. Keep a
verified backup before deleting a session whose logs or artifacts may still
be needed for incident review.

Alert on `/ready` returning 503, `asp_auth_config_healthy=0`, audit drops or writer failures, authentication
failures or `asp_auth_handshake_timeouts_total`, storage headroom failures or
`asp_storage_headroom_rejections_total`, principal connection-limit or
request-stream-limit rejections, process timeouts,
`asp_process_output_limit_terminations_total`,
`asp_process_log_sync_failures_total`, and a rising ratio of
`asp_process_log_sync_duration_us_total` to `asp_process_log_sync_total`,
`asp_process_launch_failures_total`, and a rising p95/p99 of the
`asp_process_launch_duration_us` histogram. The launch histogram covers the
admitted durable preparation and spawn/bookkeeping transaction; compare it
with `asp_request_duration_us` to distinguish host launch contention from
transport or response-drain latency.
Also alert on a rising `asp_resume_replay_limited_total`: it counts resume or
subscription tails that exceeded the bounded 100,000-event/64 MiB live replay
budget and therefore returned the current snapshot. This is a bounded-memory
fallback, not a data-loss counter; investigate lagging consumers and tune
retention/compaction before it becomes routine.
process/frame/output queue occupancy near its limit, cgroup memory or
PID pressure, WAL growth, an unhealthy workspace watcher, and increases in
`asp_workspace_file_event_drops_total`. A watcher failure or file-event queue
drop is safe but makes workspace queries fall back to fresh scans; event
consumers must resync their semantic view rather than treating the feed as a
complete filesystem journal. Also alert when
`asp_workspace_git_helper_configured` is `1` but
`asp_workspace_git_helper_healthy` is `0`; restore the reviewed Git executable
before restarting so semantic queries do not run an unexpected helper. Alert
on `asp_storage_maintenance_healthy=0`, a nonzero
`asp_storage_maintenance_last_failures`, or a stalled
`asp_storage_maintenance_last_run_unix_ms` (or a nonzero
`asp_storage_maintenance_started_unix_ms` that remains unchanged); `/ready`
uses the same signal and returns `storage_maintenance_unhealthy` so the
supervisor can stop routing new work while compaction/pruning is repaired.
When named event-consumer leases are enabled, alert on
`asp_event_consumer_lag_max` and investigate a sustained nonzero value before
retention/compaction is blocked by an abandoned subscriber; leases expire after
seven days without an ACK.
Track `asp_workspace_state_digest_hits_total` as the direct signal that an
agent is reusing a complete semantic result instead of receiving its payload
again. Track `asp_workspace_digest_cache_hits_total` alongside it to confirm
the daemon also skipped the scan/Git/file work; a sudden drop usually means
cache invalidation, query-shape churn, or agent restarts.
For loopback development-server forwards, track
`asp_port_forward_bytes_total`, `asp_port_target_policy_entries`,
`asp_port_target_rejections_total`, and the principal request/response-budget
rejection counters. The daemon accepts repeated `--port-target HOST:PORT`
flags to install an exact loopback allowlist; without the flags, the historical
development behavior allows any loopback port, while an explicit empty policy
can deny all `PORT_OPEN` attempts. Targets are checked before dialing, so a
rejected port cannot trigger a local connection attempt. Payloads are charged
while the bridge is active and a credential is revalidated at most once per
second, including while the flow is idle; a revoked identity closes the
stream, so a reconnect must authenticate again. Non-loopback and reverse
forwards remain outside the v0 policy surface.
Track `asp_artifact_gc_failures` and the object/byte totals
(`asp_artifact_gc_objects_total`, `asp_artifact_gc_bytes_total`). A rising
failure counter indicates an unsafe or tampered object, a storage permission
problem, or a directory-sync failure; do not manually delete referenced
objects while investigating.
Track `asp_artifact_index_entries`, `asp_artifact_dedup_hits_total`, and
`asp_artifact_dedup_bytes_total` to
confirm repeated same-principal build/test artifacts are being linked instead
of retransmitted. A low hit rate is expected when sessions do not share
artifacts; a sudden drop for a stable workload can indicate index pressure,
retention churn, or a filesystem that does not support hard links.
Track `asp_frame_memory_bytes`/`asp_frame_memory_limit` for decoded request
pressure and `asp_response_memory_bytes`/`asp_response_memory_limit` for
encoded response pressure. `asp_response_memory_rejections` counts bounded
temporary refusals. The response encoder serializes allocation before charging
its exact permit, so a sustained high response gauge indicates slow readers or
genuinely large semantic results rather than an unbounded burst of transient
buffers.
Each encoded frame is written under a size-aware 64 KiB/s minimum-rate deadline
(10-second floor, five-minute cap); a reader that stops consuming is detached
and its response permit is released instead of pinning the request indefinitely.
Monitor `asp_response_frame_write_timeouts_total` to detect clients or network
paths that repeatedly stop consuming response streams.
Monitor `asp_pty_input_write_timeouts_total` for terminals whose synchronous
master writer stopped accepting input; the attachment is bounded and can be
reattached, while the durable tmux session remains alive.
For codec tuning, compare `asp_response_frame_logical_bytes_total` with
`asp_response_frame_encoded_bytes_total` and watch the compressed/plain frame
counts. A low encoded/logical ratio indicates useful savings; a ratio near one
with high compressed counts suggests the workload or threshold deserves
re-evaluation. The plain count also includes deliberately uncompressed v16
responses during a rolling compatibility window.
Also compare `asp_response_encode_gate_wait_us_total` divided by
`asp_response_encode_gate_acquisitions_total` with the per-operation latency
histograms. The gate covers only potentially large response shapes; bounded
control/interactive frames bypass it. Sustained wait on the large-response
gate is evidence for a future weighted or sharded reservation redesign, while
high control latency with low gate wait points to transport, process, or
workspace work instead.
Track `asp_request_duration_us_bucket{operation=...,le=...}` together with
the matching `_count` and `_sum` series for request p50/p90/p99 SLOs. Buckets
are fixed at 0.1/0.5/1/5/10/50/100/500 ms, 1/5/30 s, plus `+Inf`; `unknown`
covers malformed streams that never yielded an operation. Long-lived PTY,
subscription, and port-forward streams are excluded because their lifetime is
not a request latency; use connection/transport gauges for those attachments.
These counters are process-local and reset on restart, so scrape them
regularly and aggregate in the monitoring system.
For network SLOs, record the Quinn counters
`asp_quic_udp_{tx,rx}_{datagrams,bytes}_total`,
`asp_quic_lost_packets_total`, `asp_quic_congestion_events_total`, and
`asp_quic_path_{rtt_us,mtu}`; they are cumulative transport observations
captured when attachments close.

## Backups and recovery

Stop the service before backing up or restoring state. Verify every backup and
keep at least one encrypted copy off the host. The backup includes the bearer
token/private key and command metadata; ASP checks integrity but does not
encrypt it, so use the site's KMS/backup encryption and access controls:

```sh
aspd --root /srv/asp/workspace --backup-state /srv/backup/asp-state
aspd --root /srv/asp/workspace --verify-state /srv/backup/asp-state
```

Verification is strict: it rejects symlinked backup roots or entries, missing
or unexpected files, payload/hash changes, and permission-mode changes. This
keeps a restore from silently widening access to credentials or other private
state.

Restore only during an incident and pass `--force-restore`. ASP preserves the
previous `.asp/` tree under a unique sibling for rollback. After a restore,
run `--validate-config`, start the service, check `/ready`, and reconnect an
agent before allowing new writes. Perform a restore drill at the retention
interval; a backup that has never been restored is not evidence of recovery.

Set event, process-log, and artifact retention explicitly (for example, 168
hours for a week of replay/logs and 720 hours for 30 days of immutable
artifacts). Event retention determines how far an offline client can resume
before it must accept a compacted snapshot; artifact retention appends a
durable tombstone before deleting an object. Retention does not replace an
external backup.

## Rotation and upgrades

For bearer-token rotation, stop the service, run
`aspd --rotate-auth-token`, distribute the new protected file, run preflight,
and restart. Token-file clients reload it on their next connection; literal
`--auth-token` clients require an explicit update.

For a server certificate renewal, stage a CA-issued DER certificate and
private key with modes `0644` and `0600`, respectively, atomically rename both
into the configured paths, and reload the supervisor (`systemctl reload aspd`
for the supplied systemd unit, or send `SIGHUP` to the daemon). If the two
files are briefly out of sync, ASP logs the rejected reload and continues
using the old pair; repeat the signal after both renames complete. Keep both
client pins in the directory until every client has received the replacement
and completed a reconnect, then remove the retired pin in a later change.

For a client-CA renewal, stage a directory containing both the old and
replacement DER CA certificates and atomically replace the configured
`--client-ca` path (or its contents), then reload with the same supervisor
command. The bundle is bounded to eight regular `.der` files and 16 MiB
total. Verify a new leaf certificate and its fingerprint-map entry before
removing the old CA; existing connections are not dropped by the reload.

The current wire version is v17. Servers accept the tested v16 plain framing
and current clients prefer v17, retrying v16 when an older daemon cannot parse
the v17 envelope; every connection is still pinned to one framing mode and
mixed modes fail closed. Until a full mixed-binary compatibility window is
published, upgrade by draining/stopping the daemon, installing the new
binaries, running preflight, and restarting. Durable sessions and child
processes resume after the new daemon starts, but the deployment should still
schedule a maintenance window and retain the old binary for rollback.
For a deterministic rollback drill, start the new daemon with
`--max-protocol-version 16`; this only limits accepted wire framing and does
not alter the persisted session/event format. Return to the default (17) only
after the v17 client/server pair has been qualified.

## Incident limits

ASP's process limits and quotas are guardrails, not a security boundary. If
commands are untrusted, isolate each workspace with a container, VM, or
per-workspace service account and enforce aggregate cgroup policy there. If the
audit sink fails, `/ready` intentionally fails closed; preserve the local log
and fix storage before accepting more work. If a journal is quarantined for
corruption, stop writes, preserve the original state tree, verify the latest
backup, and restore only after recording the incident and desired recovery
point.
