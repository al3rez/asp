# ASP — Agent Session Protocol

## Project status

ASP is ready for daily use as a supervised, single-user pilot behind Tailscale
or another private network. It is not ready for broad production, public
Internet exposure, or multi-tenant use.

Most of the remaining work is outside the core protocol: two-host network
qualification, real mixed-version upgrades, site-owned operational controls,
capacity testing, release qualification, and an independent security review.
See [TODO.md](TODO.md) for the open work and
[docs/PRODUCTION_READINESS.md](docs/PRODUCTION_READINESS.md) for the full gates
and supporting evidence.

ASP is not a sandbox. A production-shaped pilot still needs a supervisor, a
reviewed process boundary, durable storage, backups, monitoring, and private
network access. Keep Quinn and Tailscale responsible for transport security,
congestion control, migration, NAT traversal, and routing. Performance work
should focus on cutting semantic round trips rather than replacing those
layers.

For a reproducible container deployment, see [deploy/container/README.md](deploy/container/README.md); it is a deployment boundary, not a hostile-command sandbox. EXEC/SPAWN and tmux-backed PTYs can run through an operator-supplied absolute `--process-launcher` (for example a reviewed supervisor wrapper); use `--require-process-launcher` when startup must fail closed without that boundary. The launcher must be a private, regular executable: group/world-writable launchers are rejected, ASP canonicalizes the path and binds its startup filesystem identity, and same-path or ancestor-redirection replacement is refused until restart. The launcher must `exec` its arguments (including the absolute tmux command) so durable process identity remains observable. For a deployment that must refuse unsafe defaults, add `--production`: it requires authenticated clients, loopback readiness/metrics, a configured process boundary, a non-group/world-writable workspace path and non-sticky ancestors, nonzero CPU/wall-clock limits, an explicit port policy (`--port-target HOST:PORT` or `--disable-port-forwarding`), and automatically enables Quinn stateless address validation before the daemon initializes.

The production profile also requires `--min-free-bytes BYTES`; readiness drops
and new durable mutations are rejected before the workspace filesystem fills.
Production preflight additionally requires a regular, non-group/world-writable
`tmux` executable (or an absolute `ASP_TMUX_PATH`) so durable PTY support is
validated before the QUIC listener opens.
The production workspace argument must not traverse an untrusted symlink in
any existing path component; ASP rejects user-controlled links before
canonicalizing the root while permitting root-owned system aliases such as
macOS `/var`/`/tmp`. Development mode keeps the usual convenience of
following a user-selected workspace symlink.

Unauthenticated QUIC connections must complete `HELLO` within ten seconds;
the loopback metrics endpoint reports `asp_auth_handshake_timeouts_total`.

Before a service restart or deployment, run `aspd --validate-config` with the
same path/authentication flags as the unit. It checks existing TLS material,
private credential permissions, authentication maps, retention/budget values,
and loopback health binding without taking the daemon lock or creating files.
The health endpoint's /live probe reports process liveness; /ready returns
HTTP 503 after audit entries are dropped or the audit writer fails, so a
supervisor can restart a daemon that can no longer guarantee its operational
record. Scheduled storage compaction/pruning is also tracked: a failed pass or
stalled maintenance scheduler sets `storage_maintenance_unhealthy` and makes
`/ready` fail closed, with bounded run/failure/in-progress/last-success metrics
for alerting. One pass runs immediately at startup, then on the normal cadence.
During SIGTERM/SIGINT drain, `/live` stays available while `/ready` reports
`draining: true` and `asp_draining 1`, allowing routers to stop new work before
the shutdown grace period expires. A failed `/ready` JSON response also
includes a bounded `ready_reasons` array with stable codes such as
`auth_config_unhealthy`, `storage_headroom`,
`storage_maintenance_unhealthy`, `process_launcher_unhealthy`, and `draining`,
so a supervisor can choose the right remediation without scraping every health
field.

ASP is a hardened research prototype for durable, semantic remote-development sessions over QUIC. The v0 production target is a Linux or macOS Unix host; CI checks Windows compilation and protocol tests, but Windows PTY/service-manager behavior is not release-qualified. It is not a replacement QUIC implementation, VPN, or cryptographic design; see the production-readiness gates before deploying beyond a trusted private overlay. ASP is dual-licensed under [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT). The executable crates are distributed as versioned release binaries with checksums (they are intentionally not published as standalone crates); signing and promotion belong to release infrastructure. The reusable wire-format library is package-verified in CI.

For a local install from a checked-out revision, use `cargo install --locked
--path crates/asp-server` and `cargo install --locked --path crates/asp-client`.
The executable crates carry versioned path dependencies and are intentionally
`publish = false`; release automation should build and sign the two binaries
together with the matching protocol schema.

On a macOS development host without a Linux GCC/sysroot, a repository
checkout's `tools/zig-cc.sh` and `tools/zig-ar.sh` wrappers provide an optional
`x86_64-unknown-linux-gnu` release compile check when Zig is installed. (The
binary archive intentionally omits these developer-only helpers.)

```sh
CC_x86_64_unknown_linux_gnu="$PWD/tools/zig-cc.sh" \
CXX_x86_64_unknown_linux_gnu="$PWD/tools/zig-cc.sh" \
AR_x86_64_unknown_linux_gnu="$PWD/tools/zig-ar.sh" \
RANLIB_x86_64_unknown_linux_gnu=true \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$PWD/tools/zig-cc.sh" \
mise exec -- cargo build --locked --target x86_64-unknown-linux-gnu \
  --workspace --all-features --release
```

This is a compile/link check only; run the resulting ELF binaries and the
container/network qualification on Linux before publishing a release.

To build a versioned Linux archive from this macOS checkout when Zig is
installed, set `ASP_TARGET`; `deploy/package-release.sh` automatically selects
the checked-in wrappers when no target-specific compiler overrides are set.
Explicit overrides remain supported for CI or a host toolchain:

```sh
CC_x86_64_unknown_linux_gnu="$PWD/tools/zig-cc.sh" \
CXX_x86_64_unknown_linux_gnu="$PWD/tools/zig-cc.sh" \
AR_x86_64_unknown_linux_gnu="$PWD/tools/zig-ar.sh" \
RANLIB_x86_64_unknown_linux_gnu=true \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$PWD/tools/zig-cc.sh" \
ASP_TARGET=x86_64-unknown-linux-gnu \
mise exec -- deploy/package-release.sh dist
```

The resulting ELF archive still needs Linux runtime, PTY, service-manager, and
network qualification before promotion.

The same kind of compile-only check is available for the portable Windows
workspace when Zig is installed. The Windows wrapper uses the GNU ABI target;
it does not qualify Windows PTY, service-manager, filesystem, or network
failure behavior, so the supported production target remains Unix until those
tests run on a Windows host.

```sh
CC_x86_64_pc_windows_gnu="$PWD/tools/zig-windows-cc.sh" \
CXX_x86_64_pc_windows_gnu="$PWD/tools/zig-windows-cc.sh" \
AR_x86_64_pc_windows_gnu="$PWD/tools/zig-windows-ar.sh" \
RANLIB_x86_64_pc_windows_gnu=true \
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$PWD/tools/zig-windows-cc.sh" \
mise exec -- cargo check --locked --target x86_64-pc-windows-gnu \
  --workspace --all-features
```

The same check is available as `tools/check-windows-gnu.sh`; it detects Zig,
installs the target-specific compiler and archive variables, and forwards any
extra Cargo arguments.

To create a versioned binary archive and checksum, run
`mise exec -- deploy/package-release.sh [output-directory]`. The archive excludes
`.asp/` state and credentials; verify its `.sha256` file before installing
`bin/asp` and `bin/aspd`.
The generated artifact can be checked on the deployment host with
`deploy/verify-release.sh RELEASE.tar.gz`; verification requires an exact
single-record checksum sidecar for that archive and covers the archive
name/path, traversal/special-file/link safety, required binaries/templates/schema,
and accidental credential/state inclusion, plus a 512 MiB compressed-size and
4,096-member safety bound before extraction; the checksum sidecar is capped at
16 KiB before parsing. The packager normalizes archive
metadata and gzip headers, and CI rebuilds the archive twice to prove
byte-for-byte reproducibility before signing/promotion. Each archive also
carries a deterministic `SBOM.spdx.json` generated from the locked Cargo
resolve graph; it is an inventory for review, while signatures and
attestations remain release-infrastructure responsibilities.

The release also ships a GnuPG detached-signature helper for that promotion
boundary. Sign the checksum with an operator-controlled key, then verify the
exact fingerprint on the deployment host before installation:

```sh
deploy/sign-release.sh --key-id "$ASP_RELEASE_SIGNING_KEY" \
  asp-0.1.0-aarch64-apple-darwin.tar.gz \
  asp-0.1.0-aarch64-apple-darwin.sha256
deploy/verify-release-signature.sh \
  --fingerprint "$ASP_RELEASE_SIGNING_FINGERPRINT" \
  asp-0.1.0-aarch64-apple-darwin.tar.gz \
  asp-0.1.0-aarch64-apple-darwin.sha256 \
  asp-0.1.0-aarch64-apple-darwin.sha256.asc
```

The verifier disables automatic key retrieval; distribute and trust the
signing key through the site's existing key-management channel. This is a
supply-chain gate, not a replacement for TLS authentication or the private
network policy.

Promotion automation can enforce the same check inside the atomic installer or
readiness-gated upgrader by passing `--require-signature`, plus `--signature`
and `--fingerprint` for an explicit sidecar and signer (or setting
`ASP_REQUIRE_RELEASE_SIGNATURE=1`, `ASP_RELEASE_SIGNATURE`, and
`ASP_RELEASE_SIGNING_FINGERPRINT`). Without those options, the helpers retain
their development-compatible checksum-only behavior.

Before promotion, execute the installed artifact itself with
`bash benchmarks/smoke-packaged-runtime.sh RELEASE.tar.gz RELEASE.sha256`.
This verifies strict production readiness, packaged parallel-batch and
explicit-session recovery, a packaged file round trip, and durable
process/log recovery after an abrupt packaged-daemon restart. It is a loopback
release gate; it does not replace Linux supervisor, two-host WAN, or
multi-tenant qualification.

For a versioned, atomic install, run the archive's
`deploy/install-release.sh` with an absolute prefix (the default is
`/usr/local/lib/asp`). It verifies the archive first, extracts a new immutable
release directory, and atomically switches `<prefix>/current`; the previous
pointer is retained for rollback:

```sh
deploy/install-release.sh --prefix /usr/local/lib/asp \
  asp-0.1.0-aarch64-apple-darwin.tar.gz
deploy/install-release.sh --prefix /usr/local/lib/asp --rollback
```

The installer never restarts a supervisor. Run the daemon's matching
`--validate-config` preflight, then drain/restart systemd or launchd explicitly.
It refuses to overwrite a release directory with a different archive digest or
to follow a symlinked release/pointer path. It also rejects untrusted symlinks
and group/world-writable existing directories in any component of `--prefix`
before creating directories; sticky shared ancestors and root-owned
compatibility aliases below non-writable parents (such as macOS `/var`) remain
allowed.

For a readiness-gated upgrade with automatic rollback, use the packaged
`deploy/upgrade-release.sh` and provide the exact supervisor restart command:

```sh
deploy/upgrade-release.sh \
  --prefix /usr/local/lib/asp \
  --ready-url http://127.0.0.1:9443/ready \
  --restart-command 'systemctl restart aspd-production.service' \
  asp-0.1.0-aarch64-apple-darwin.tar.gz \
  asp-0.1.0-aarch64-apple-darwin.sha256
```

It waits for the existing daemon to be ready, atomically switches the release,
restarts only through that explicit command, and restores the previous release
if readiness fails. The helper returns nonzero after a failed rollout even when
rollback succeeds; promotion tooling must observe that result and investigate.
Before acquiring its transaction lock, it performs the installer's
non-mutating `--validate-prefix` trust check (and repeats it after locking), so
an unsafe symlinked or group/world-writable existing prefix component fails
closed before release state is touched. It accepts only loopback HTTP `/ready`
endpoints and never infers a service-manager command.

For a trusted single-workspace pilot, bootstrap the client-side certificate and
token over an already verified SSH connection:

```sh
deploy/bootstrap-client.sh --output-dir /absolute/credential-dir \
  user@host /absolute/remote/workspace
```

It preserves normal SSH host-key checking and installs a dedicated `0600`
credential pair atomically. Shared or Internet-facing deployments should use
operator-managed mTLS/PKI instead of copying a bearer token.

The prototype provides:

- QUIC/TLS connectivity using Quinn and rustls
- connection-independent session UUIDs
- structured remote execution with resumable output events and bounded reconnect retries
- in-daemon EXEC/SPAWN request deduplication and result replay
- persistent PTYs that survive client detach, daemon restart, and client auto-reconnect
- a monotonic event journal with replay-or-snapshot resume
- bounded streamed file transfer with crash-safe resumable downloads/uploads and optimistic prefix/suffix patches (adaptive full-file fallback for broad edits)
- immutable SHA-256-addressed artifact transfer with resumable uploads and bounded range downloads
- same-principal cross-session artifact reuse via verified hard links (with streamed fallback)
- parsed, latest-wins PTY screen state over QUIC DATAGRAMs, with an optional
  negotiated ANSI-formatted snapshot that preserves cell attributes
- an optional negotiated base-relative PTY row-delta path for localized plain
  screen changes, with bounded validation and periodic full checkpoints after
  datagram loss
- one-request workspace tree/git/search/read aggregation with watcher-invalidated semantic digest caching and durable external `FILE_CHANGED` hints
- crash-durable event logs and process output logs with daemon restart recovery (group-committed high-rate output)
- transparent bounded zlib stream-frame compression for large messages when it produces a strict byte win (incompressible data stays plain); large request/response codec work runs off the Tokio reactor
- high-entropy sampling avoids a zlib pass for large already-compressed payloads while retaining compression for source and log data
- bearer-token client authentication, atomic state writes, workspace/symlink confinement
- tmux-backed PTYs that can be reattached after an `aspd` restart

For a metered or high-latency terminal link, pass the global
`--prefer-pty-delta` option to `asp shell`. It omits ANSI rich-state negotiation
and opts into compact plain row deltas with periodic full checkpoints; reliable
PTY output is unchanged, while cell-color preservation is intentionally traded
for lower replaceable-state bytes.

Both endpoints use one shared Quinn transport profile: bounded flow-control
windows, one-megabyte replaceable-datagram buffers, fair scheduling among
same-priority streams, five-second keepalives, and a 15-second maximum idle
timeout. Healthy idle attachments remain alive; a dead path is detected
promptly so the client can reconnect and resume the durable session. This is a
transport failure-detection bound, not a process/session lifetime limit.
Client request/attachment stream creation is also bounded to ten seconds, so a
peer that has exhausted its QUIC stream or flow-control budget cannot leave an
agent call waiting indefinitely; individual request-frame writes use the same
64 KiB/s minimum-rate policy as the server (10-second floor, five-minute cap),
so a peer that stops consuming an admitted stream cannot pin an upload forever.
Server response-frame writes use the matching per-frame deadline, releasing
encoded-response memory and the request task when a reader stops consuming.
The normal retry policy handles these transport timeouts.

Stream framing is versioned and self-describing: protocol v17 wraps each
Postcard message in an `AF` envelope with a decoded-length bound, while the
server still accepts tested v16 plain frames during a rolling deployment. The
current client prefers v17 and retries a v16 handshake when it detects an old
daemon that cannot parse the envelope. A bounded five-minute endpoint hint
avoids repeating that failed probe on every reconnect and is refreshed after
expiry. DNS lookup is bounded and up to 16 unique addresses are tried within
the same connection timeout when a dual-stack endpoint returns an unreachable
first address; the first four are raced with a 50 ms stagger so a black-holed
family does not serialize reconnects. `--max-protocol-version
16` provides a controlled rollback drill without changing durable session state.
Measure the codec independently with `mise exec -- cargo run --release -p
asp-bench -- frame-compression`; the benchmark reports repetitive and
pseudo-random wire ratios without pretending they are network results.
The `asp-bench -- pty-state-delta` fixture reports the full-screen versus
localized-row wire cost and verifies broad rewrites fall back safely.
The release `asp-bench -- protocol-fuzz` command runs a bounded deterministic
mutation corpus through every public frame/message/PTY decoder; use
`benchmarks/smoke-protocol-fuzz.sh` as a fast panic/limit regression before
promotion. It is not a substitute for an independent coverage-guided fuzzing
campaign or security review.
For local macOS impairment experiments, `asp-bench udp-proxy --target HOST:PORT`
adds deterministic delay/jitter/loss/rate shaping with a bounded queue; it is
benchmark tooling only and does not replace the two-host qualification matrix.

The full two-host agent qualification grid is intentionally long (180 cells by
default). Pass an empty operator-owned `--checkpoint-dir` so each qualified
cell survives a control-host interruption, then rerun with `--resume`; the
manifest, host provenance, exact shaping contract, per-cell qualifier, and
SHA-256-bound capture marker are checked before reuse.
For genuine Wi-Fi/cellular migration or sleep/wake evidence, add an
operator-owned `--network-event-hook` and choose `--network-event-kind
migration` or `sleep-wake`; the hook runs on the client in both paired legs and
rows record whether it completed. Runs without a hook explicitly do not claim
physical roaming.

See [docs/PROTOCOL.md](docs/PROTOCOL.md), [docs/SCHEMA.md](docs/SCHEMA.md), [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), the evidence-backed [docs/CONCLUSIONS.md](docs/CONCLUSIONS.md), the [operations runbook](docs/OPERATIONS.md), the short [production TODO](TODO.md), and the explicit [production-readiness gates](docs/PRODUCTION_READINESS.md). The prototype uses a pinned self-signed server certificate and a per-workspace bearer token; use a private network (for example Tailscale) or add a production identity/bootstrap layer before exposing it to an untrusted network.

## Quick start

```sh
mise exec -- cargo run -p asp-server -- --listen 127.0.0.1:4433
mise exec -- cargo run -p asp-client -- connect 127.0.0.1:4433
# Recover a session from another client host when its local cursor is gone.
mise exec -- cargo run -p asp-client -- resume 127.0.0.1:4433 \
  --session-id SESSION_UUID --after-event-id 0
mise exec -- cargo run -p asp-client -- doctor 127.0.0.1:4433
mise exec -- cargo run -p asp-client -- doctor --strict 127.0.0.1:4433
# When running on the daemon host, include its loopback readiness probe.
mise exec -- cargo run -p asp-client -- doctor --strict 127.0.0.1:4433 \
  --ready-url http://127.0.0.1:9443/ready
mise exec -- cargo run -p asp-client -- exec 127.0.0.1:4433 "git status"
mise exec -- cargo run -p asp-client -- exec 127.0.0.1:4433 --summary "cargo test"
mise exec -- cargo run -p asp-client -- signal 127.0.0.1:4433 PROCESS_UUID --signal TERM
mise exec -- cargo run -p asp-client -- batch 127.0.0.1:4433 -c "git status" -c "cargo test"
# Keep the connection warm while returning only bounded diagnostic tails.
mise exec -- cargo run -p asp-client -- batch 127.0.0.1:4433 --summary --tail-bytes 8192 \
  -c "cargo test" -c "cargo clippy"
# For independent checks where only exit status matters, overlap up to four
# commands on the same QUIC connection (explicit zero-tail summary contract).
mise exec -- cargo run -p asp-client -- batch 127.0.0.1:4433 \
  --summary --tail-bytes 0 --parallel 4 \
  -c "git status --short" -c "cargo metadata --no-deps" -c "rg -n TODO ."
printf '%s\n' '{"id":"status","op":"exec_summary","command":"git status"}' | \
  mise exec -- cargo run -p asp-client -- agent 127.0.0.1:4433
mise exec -- cargo run -p asp-client -- events 127.0.0.1:4433 --no-output
mise exec -- cargo run -p asp-client -- logs 127.0.0.1:4433 PROCESS_UUID --stream stderr --offset 0 --length 65536
mise exec -- cargo run -p asp-client -- status 127.0.0.1:4433 PROCESS_UUID
mise exec -- cargo run -p asp-client -- artifact-put 127.0.0.1:4433 ./target/test-output.tar --name test-output
mise exec -- cargo run -p asp-client -- artifact-get 127.0.0.1:4433 ARTIFACT_SHA256 ./retrieved.tar
mise exec -- cargo run -p asp-client -- artifact-get 127.0.0.1:4433 ARTIFACT_SHA256 ./chunk.bin --offset 1048576 --length 65536
mise exec -- cargo run -p asp-client -- inspect 127.0.0.1:4433 --search TODO --read src/lib.rs
# Skip the tree payload (and its scan when no search is requested).
mise exec -- cargo run -p asp-client -- inspect 127.0.0.1:4433 --no-tree --search TODO
# Skip both the tree walk and Git status when only selected files are needed.
mise exec -- cargo run -p asp-client -- inspect 127.0.0.1:4433 --no-tree --no-git-status --read src/lib.rs
mise exec -- cargo run -p asp-client -- forward 127.0.0.1:4433 --listen 127.0.0.1:8080 --target 127.0.0.1:3000
mise exec -- cargo run -p asp-client -- shell 127.0.0.1:4433
```

For a daily profile, set the endpoint and credentials once. `--command` keeps
the command text separate from the endpoint, while IDs and paths can follow
directly when `ASP_SERVER` (or `--server`) supplies the endpoint:

```sh
export ASP_SERVER=127.0.0.1:4433 ASP_CERT=/path/to/server-cert.der
export ASP_AUTH_TOKEN_FILE=/path/to/auth-token
asp exec --summary --command 'git status --short'
asp put ./README.md workspace/README.md
asp status PROCESS_UUID
```

`asp connect SERVER` is idempotent for the local cursor: repeating it performs
the connection/authentication handshake and reuses the saved durable session
without replaying its journal. Use
`asp connect SERVER --new` only when an intentionally fresh session is needed;
the old session and its detached processes remain available until explicitly
cleaned up by an operator.

If a client host loses its local cursor file, recover a known durable session
with `asp resume SERVER --session-id UUID --after-event-id N`. The UUID is an
address, not a credential: the server still checks that the authenticated
principal owns the session. The resumed snapshot and retained events are
written into the normal local cursor file, after which ordinary `asp exec`,
`asp shell`, and agent commands can reuse the session. Omit both options for
the usual saved-cursor resume; `--after-event-id` without `--session-id` is
rejected to avoid accidentally replaying a different session.

`asp batch --parallel N` is an explicit throughput path for independent
commands. It requires command arguments plus `--summary --tail-bytes 0`, runs
up to 32 requests at once over the same authenticated QUIC connection, and
prints `ASP_BATCH_RESULT <index> <status>` markers in input order. It does not
forward command output, and commands must not depend on one another; use the
default sequential batch mode when output or ordering matters. Each request
keeps its own idempotent request ID and can reconnect independently. Batch
reconnects retain the original bearer-token endpoint when available, preserving
the UDP socket and TLS session cache across a daemon restart or path flap.

For semantic inspections, the JSONL adapter accepts `include_tree` and
`include_git_status` (both default to `true`). Set `include_tree:false` when a
repository-wide tree is not needed; if no search is requested, the server also
skips the tree scan. A search still needs a bounded file walk even when its
tree payload is omitted. Set `include_git_status:false` when Git status is not
needed; the Git-status subprocess is then skipped. These switches can be
materially faster on large workspaces. The CLI equivalents are
`asp inspect SERVER --no-tree` and `asp inspect SERVER --no-git-status`.

The server writes its certificate to `<workspace>/.asp/server-cert.der`; relative server paths (certificate, key, token, principals, and lock) are resolved below `--root`, while absolute paths are honored. The client pins that certificate by default when run from the workspace. `--cert` may also point to a directory containing up to eight regular `.der` pins, which allows an old/new certificate overlap during renewal. Use `--cert` on both programs to choose another path. The client uses `localhost` for TLS SNI by default (matching generated certificates); for an operator-issued certificate, pass `--server-name asp.example.test` (or the IP SAN) so hostname validation matches the certificate. New clients store their session cursor in the per-user state directory (`$XDG_STATE_HOME/asp/sessions.json`, macOS Application Support, or Windows LocalAppData) so cursor writes do not invalidate a remote workspace watcher; an existing `.asp-session` file remains supported, and `--session-file` overrides the location. When multiple agents consume the same durable session, pass a stable, distinct `--consumer-id` to each process (for example `--consumer-id reviewer-a`); each ID gets an independent cursor while a newly named consumer can bootstrap from the legacy per-server session entry. The local cursor file is bounded at 8 MiB; ASP refuses to publish a larger metadata file instead of writing a cursor that future clients could no longer load. Remove stale server/consumer entries or use a fresh `--session-file` before retrying.

For daily remote use, the global client connection options also accept
environment defaults: `ASP_SERVER`, `ASP_CERT`, `ASP_SERVER_NAME`, `ASP_SESSION_FILE`,
`ASP_CONSUMER_ID`, `ASP_AUTH_TOKEN_FILE`, `ASP_AUTH_TOKEN`,
`ASP_PREFER_PTY_DELTA`, `ASP_CONNECT_TIMEOUT_MS`, `ASP_RECONNECT_TIMEOUT_MS`,
`ASP_CLIENT_CERT`, and `ASP_CLIENT_KEY`. A command-line flag always overrides
its valid environment value; malformed environment values fail fast rather
than being silently ignored. Prefer `ASP_AUTH_TOKEN_FILE` pointing to a
private `0600` file; `ASP_AUTH_TOKEN` is supported for controlled automation
but can be visible to process-environment inspection.

`ASP_SERVER` supplies the endpoint for every server-facing subcommand, so a
daily profile can use `asp exec --summary --command 'git status'`,
`asp status PROCESS_UUID`, or `asp put local.txt remote.txt` after exporting
the endpoint once. Commands with positional IDs or paths also accept an
explicit `--server SERVER`; the endpoint option and environment form keep
those operands from being mistaken for the server. The explicit
`--command` form avoids ambiguity with the legacy positional syntax
(`asp exec SERVER COMMAND...` and `asp spawn SERVER COMMAND...`). The legacy
`asp COMMAND SERVER ...` forms remain supported, and `asp agent-connect`
remains local-only.

The daemon accepts the same deployment settings from explicit `ASP_*`
environment defaults, including `ASP_LISTEN`, `ASP_ROOT`, `ASP_CERT`,
`ASP_KEY`, `ASP_AUTH_TOKEN_FILE`, `ASP_HEALTH_LISTEN`,
`ASP_AUTH_PRINCIPALS_FILE`, `ASP_CLIENT_CA`, `ASP_AUTH_CERTIFICATES_FILE`,
`ASP_PROCESS_LAUNCHER`, `ASP_REQUIRE_PROCESS_LAUNCHER`,
`ASP_PROCESS_CPU_SECONDS`, `ASP_EXEC_TIMEOUT_SECONDS`,
`ASP_MIN_FREE_BYTES`, `ASP_DISABLE_PORT_FORWARDING`, and the retention,
quota, and shutdown settings. This is useful with a supervisor
`EnvironmentFile`: keep secrets in a private file path rather than placing
bearer tokens directly in the environment, and keep security-sensitive
production flags explicit in the reviewed unit or preflight command. A CLI
value overrides a valid environment value; malformed values fail closed.

On Unix, send `SIGHUP` to a running `aspd` after atomically installing a
complete replacement certificate/key pair to reload TLS for new connections
without dropping existing sessions. Reload is fail-closed: missing, invalid,
or mismatched material leaves the last known-good configuration active. For a
no-downtime pin rollover, point clients at a directory containing both the old
and replacement DER certificates, reload the server, then remove the retired
pin after all clients have reconnected. See the [operations runbook](docs/OPERATIONS.md)
for a safe rollout.

`aspd` refuses non-loopback listeners unless `--allow-non-loopback` is explicit. If remote clients connect over Tailscale, bind to the tailnet interface or use the flag with an appropriate firewall/private overlay; do not publish UDP/4433 directly to the Internet.

The client bounds each QUIC/TLS handshake to ten seconds by default. Use the
global `--connect-timeout-ms` option (1–120000 ms) for scripts that should fail
faster or links whose handshake needs a larger budget. Request-level recovery
retries for 90 seconds by default; tune the bounded global
`--reconnect-timeout-ms` option (1–600000 ms) for a longer or fail-fast policy.

The server creates `.asp/auth-token` with mode `0600` and writes a private rotating JSONL audit log to `.asp/audit.log` by default. Audit entries contain only operation, principal, remote, and outcome labels; commands, paths, tokens, and file contents are never recorded. The client reads the token file by default when run from the same workspace; otherwise pass `--auth-token-file /path/to/auth-token` or `--auth-token TOKEN`. Token-file clients refuse group/world-readable credentials, and mTLS client keys must likewise be private; the pinned server certificate may remain public. Long-lived clients/agent adapters re-read the token file when establishing a reconnect, so an atomic server-side rotation does not require restarting the adapter; an explicitly supplied `--auth-token` remains static by design. Stop the daemon, then run `aspd --root /srv/asp/workspace --rotate-auth-token` to replace the token atomically; the command prints only the protected file path and is refused while another daemon owns the state lock. For multiple owners, pass `aspd --auth-principals-file /path/to/principals.json`, containing for example `{"alice":{"token":"<32+ chars>","scopes":["*"]},"reader":{"token":"<32+ chars>","scopes":["session:read","process:read","file:read"]}}`; sessions are then owner-bound. For shared deployments, prefer identity-bound mTLS: provide a DER CA with `--client-ca`, map SHA-256 fingerprints in `--auth-certificates-file`, and pass the matching DER client certificate/key to `asp`. The mapping file looks like `{"alice":{"certificate_sha256":"<64 hex chars>","scopes":["*"]}}`; the server requires a certificate signed by that CA and never treats a session UUID as a credential. Authentication can be disabled only explicitly with `aspd --insecure-no-auth` for localhost development.
Semantic Git status/diff/log queries resolve `/usr/bin/git`, `/bin/git`, common package-manager prefixes, and then `PATH`; set an absolute `ASP_GIT_PATH` when Git lives elsewhere. A non-Git workspace remains valid and simply omits Git metadata.

When a process launcher is configured, semantic Git queries run through that
same validated launcher and inherit its command limits; process-wide Git
credential/system configuration is disabled while repository-local semantics
remain inside the operator-owned boundary.

For mTLS client-CA rotation, `--client-ca` accepts either one DER CA file or a
directory of up to eight regular `.der` CAs (16 MiB aggregate). Stage old and
new CAs together, reload the server with `SIGHUP`, verify a replacement client
and fingerprint-map entry, then retire the old CA after reconnects complete.

Token-file adapters reload the credential on reconnect and automatically
reconnect once when a live server reports `authentication_required` after
rotation; an explicit `--auth-token` remains static and must be replaced by
the caller. The supervised local pool returns `agent_connection_pool_busy`
after its bounded four-lease wait, and the listener caps local bridge tasks at
32 with a ten-second SIGTERM drain.

The JSONL `logs` operation also accepts `tail_bytes` to fetch only the final
bounded suffix from a process-state snapshot. The CLI equivalent is
`asp logs SERVER PROCESS_UUID --tail 65536`; it cannot be combined with an
explicit offset or length.

Guarded agent `file_put` requests can also use the negotiated
`file_patch_ranges` capability: when an inspected base contains several
scattered edits, the adapter sends sorted byte ranges instead of the whole
file. Equal-length byte runs and bounded line-aware length-changing source
edits are supported; ambiguous or over-budget matches fall back to the
existing contiguous patch or full PUT path. Explicit `file_patch_ranges` JSONL
requests are available when callers need deterministic control.

For an AI coding-agent adapter, keep one process running with `asp agent SERVER` and send one JSON object per input line. The adapter emits a `ready` line (with adapter API version 1), then `started`, `spawned`, offset-addressed `output` (base64), `summary`, `workspace_state`, `process_state`, `file_data`, `file_stored`, `file_unchanged`, `log`, `log_end`, `signal_applied`, and `exit` lines. In addition to `exec`/`exec_summary`, the warm adapter accepts detached `spawn` (returning a durable `process_id` for later `status`/`logs`/`signal`/event resume), point-in-time `status` (or `process_status`) reads, offset/range-bounded `logs` (or `process_logs`) for durable stdout/stderr retrieval, `inspect` (or `workspace_state`) with `workspace`, `include_tree` (default `true`), `include_git_status` (default `true`), `searches`, `read_paths`, `diff`, and `recent_commits`, `signal` with `process_id` plus `signal` (`HUP`, `INT`, `KILL`, or `TERM`), plus `file_get`, `file_put` (`data_base64`, optional `expected_sha256`, and explicit `force` for blind replacement), hash-aware `file_patch` (`expected_sha256`, `prefix_len`, `suffix_len`, `replacement_base64`), and explicit `file_patch_ranges` (`expected_sha256` plus sorted `ranges` with `replacement_base64`). Set `include_tree:false` to omit the tree payload (and skip its scan when no search is requested) or `include_git_status:false` to skip Git status. After an inspection, a guarded `file_put` automatically uses the cached base for a smaller contiguous or multi-range patch when that avoids transferring the whole file; if the replacement is byte-identical to that cached base, it emits `file_unchanged` and sends no mutation request. Callers can still use explicit `file_patch` or `file_patch_ranges` for deterministic control. Workspace responses include a `tree_version` epoch/generation token and `tree_unchanged`; repeated inspections automatically send the latest token for that workspace (or callers may provide `known_tree_version`). All retryable requests reconnect after `HELLO` without replaying the event journal: stable request IDs and durable idempotency records protect mutations, and read ranges/digests/offsets make reads repeatable. Use a caller-generated `request_id` for mutating operations when a request must remain deduplicable across an adapter restart; transport retries inside the adapter reuse it automatically. `ping` reports the current session cursor and `close` detaches cleanly. Input lines are capped at 128 KiB, and malformed or unknown operations return a structured error without tearing down the session. This mode avoids a process and QUIC handshake for every agent tool call; it also retains the configured Quinn endpoint so a reconnect can reuse its UDP socket and TLS session cache. `asp batch --stdin` is the simpler raw-output alternative. One-shot commands retry transient initial handshakes, and first-session creation retries `OPEN_SESSION` with one stable request ID so a lost reply cannot create an orphan session.

For a supervisor-managed local endpoint, run `asp agent-listen SERVER /run/user/$UID/asp-agent.sock` and keep that process warm. Agents can use `asp agent-connect /run/user/$UID/asp-agent.sock` with the same JSONL protocol; the listener accepts multiple local clients and keeps a bounded four-connection idle pool, so sequential short-lived clients reuse an authenticated QUIC transport while concurrent clients remain on separate connections. This removes a new adapter process and handshake from repeated tool calls, discards a pooled connection when its session identity no longer matches, and performs one resume replay if another adapter advanced the shared durable cursor while it was idle. The listener removes its private socket on a clean SIGTERM. The socket parent must be an absolute, non-group/world-writable directory (the listener creates a missing final directory with mode `0700`); this endpoint is a local adapter convenience, not a replacement for QUIC authentication or the remote session owner checks.

For a separate durable event feed, run `asp --consumer-id agent-events events SERVER --no-output`. It prints structured JSON events, persists that consumer's cursor, and reconnects after daemon restarts or network loss; use a distinct consumer ID for each independent subscriber.

Filtered EXEC/SPAWN/file-result attachments never advance the durable event cursor. Transport retries reconnect after HELLO and repeat the original operation directly, using stable request IDs for side effects and ranges/digests/offsets for reads; only explicit `resume` and event consumers replay the journal. The release regression is `bash benchmarks/smoke-event-cursor-safety.sh`.

Large downloads write locked `<local>.asp-download`, `.meta`, and `.lock` sidecars so a later invocation can resume a verified prefix after a client crash. Large uploads write `<local>.asp-upload` plus a lock sidecar, preserving the request ID across client crashes; the server keeps a private staging prefix, so retries resume at the last durable chunk instead of retransmitting the whole file. File uploads are create-only by default: pass `--expected-sha256 <64-hex-digest>` for a hash-guarded replacement, or pass `--force` only when an intentional blind replacement is acceptable. The upload checkpoint persists this policy across client crashes; the agent also accepts `expected_sha256`/`file_patch` hash guards for concurrent edits. `asp patch` and the agent's cached-base path choose a contiguous prefix/suffix PATCH or negotiated multi-range PATCH only when it is materially smaller than a full FILE_PUT. Source files with several length-changing line edits use a bounded matcher; ambiguous or over-budget matches safely fall back to one contiguous patch or PUT. A byte-identical CLI patch is a no-op that does not create a new workspace version. `asp exec --summary` returns only bounded output tails and byte counts while retaining the complete process log for later resume, which avoids moving huge test logs through the agent request path. `asp logs SERVER PROCESS_UUID --stream stderr --offset 0 --length 65536` fetches a bounded durable range even after journal output events compact. `asp forward` exposes a local TCP listener through one QUIC stream per connection to a server-host loopback service; v0 rejects non-loopback targets, payloads count against the principal's rolling byte budgets, and credentials are revalidated while a flow is active. Servers can repeat `--port-target HOST:PORT` to allow only exact loopback targets; an explicit policy rejects unlisted ports before dialing, while omitting it preserves the development default. Tailscale/firewall policy remains responsible for wider connectivity. Remove transfer sidecars only when abandoning an operation.

Each authenticated principal may hold at most 512 concurrent request streams by
default, in addition to the process-wide 4,096-stream cap. Long-lived PTY,
event, and forwarding streams release their lease automatically on completion
or disconnect; `principal_request_stream_limit` and the matching health metrics
make admission failures observable.

Artifact transfers use `asp artifact-put` and `asp artifact-get`: objects are
immutable SHA-256-addressed bytes, full downloads are verified and resumable,
and bounded range downloads avoid replaying an already received prefix. The
CLI keeps a locked `<local>.asp-artifact-upload` sidecar for crash-safe upload
resume. Artifact objects are private to their session and bounded by a 1 GiB
object/8 GiB session quota. The daemon garbage-collects committed objects
older than `--artifact-retention-hours` (30 days by default), writing a durable
tombstone before unlinking; active downloads are leased and skipped. Set the
retention window to match the project's artifact/backup policy.

Artifact metadata remains private to its session. A same-principal session may
reuse a verified object from another session through a hard link; missing or
cross-filesystem links fall back to a normal upload.

Sessions, event journals, process logs, and idempotency metadata live below `.asp/sessions/`. ASP tightens the state tree to private permissions and rejects symlinked state files/directories at startup. The client serializes concurrent updates and first-session creation in its saved session map with a lock sidecar, and merges same-session cursors monotonically, so multiple agent processes do not silently move the durable resume point backwards or create orphan remote sessions. Filtered EXEC/SPAWN/file-result streams never advance the durable event cursor; retry identity comes from stable request IDs and operation offsets/digests. Use distinct `--consumer-id` values when independent agents follow one session; those entries keep separate local event cursors while preserving the legacy per-server entry for bootstrap. EXEC/SPAWN children continue through client disconnects and `aspd` restarts. Durable PTY reattachment uses a named `tmux` session; the PTY resolver checks standard Linux/macOS/Homebrew executable paths (or an explicit absolute `ASP_TMUX_PATH`) before consulting `PATH`, which keeps launchd/systemd environments deterministic. The PTY remains connection-independent, while the terminal screen is synchronized as replaceable state; peers that negotiate optional `pty_rich_state` preserve ANSI cell attributes in reconnect redraws, with the plain snapshot retained for mixed-release compatibility. A peer that also negotiates `pty_rich_compression` can receive MTU-fitting zlib rich-state datagrams instead of losing an oversized replaceable update; reliable output remains the source of truth. PTY master writes are serialized per backend and run on the blocking pool with a bounded timeout; a stalled writer cannot multiply blocked tasks during reconnect storms, and `asp_pty_input_write_timeouts_total` exposes the condition.

The named tmux session is created detached before the first attachment. Each
daemon owns only an `attach-session` view and explicitly detaches it before
shutdown, so a PTY hangup cannot terminate the durable shell or inject EOF into
it during a restart.

The container image ships an immutable exec-only worker wrapper and enables
the fail-closed `--production` profile by default. Its cgroup, read-only root,
and no-new-privileges settings provide the aggregate worker boundary; the
wrapper is an execution-policy anchor, not a sandbox.

For a long-running deployment, run `aspd` under a service manager (launchd, systemd, or a container supervisor), keep `.asp/` on durable storage, back it up according to the event-retention policy, and restrict workspace permissions to the service identity. The release includes fail-closed production templates for both `deploy/systemd/aspd-production.service` and `deploy/launchd/com.asp.aspd-production.plist`; each requires the operator's reviewed `/usr/local/libexec/asp-worker-wrapper` before the service can start. The original systemd/launchd units remain pilot baselines. If a workspace is being converted from the legacy single-token mode to named principals, stop the daemon and run `aspd --auth-principals-file /etc/asp/principals.json --migrate-legacy-owner alice` once; this explicit, lock-protected operation binds only sessions whose owner is still `legacy`. Configure retention explicitly; for example `--event-retention-hours 168 --process-log-retention-hours 168 --artifact-retention-hours 720` keeps one week of replay/log data and 30 days of immutable artifacts before snapshot compaction and pruning. The daemon enforces rolling per-principal request and response-byte budgets (4 GiB/minute by default); tune them with `--principal-request-bytes-per-minute` and `--principal-response-bytes-per-minute` when a deployment has a different transfer profile. For command-tree isolation, set `--process-memory-bytes` on Linux/Android and `--process-cpu-seconds` on Unix; zero disables the corresponding RLIMIT, while a cgroup/container should remain the aggregate boundary. On Linux, EXEC/SPAWN children set `PR_SET_NO_NEW_PRIVS` by default, so setuid/file-capability privilege gains are blocked and the bit propagates to descendants; only a reviewed trusted launcher should use the explicit `--allow-process-privilege-gain` escape hatch. `--insecure-no-auth` is refused on non-loopback listeners, and the optional health endpoint is always loopback-only. For a fail-closed service profile, pass `--production` to both `ExecStartPre --validate-config` and `ExecStart`, together with an absolute reviewed launcher, `--process-cpu-seconds`, `--exec-timeout-seconds`, and `--health-listen`; the profile rejects missing controls before it creates state or opens a socket. The launcher is still an external sandbox/supervisor integration, not a capability ASP implements itself. The metrics include `asp_request_bytes_total`, `asp_response_bytes_total`, `asp_response_frame_bytes_total`, `asp_port_forward_bytes_total`, `asp_artifact_gc_objects_total`, `asp_artifact_gc_bytes_total`, `asp_artifact_gc_failures`, `asp_principal_budget_rejections`, `asp_principal_response_budget_rejections`, `asp_principal_request_stream_rejections`, `asp_principal_request_stream_limit`, `asp_active_request_streams`, `asp_resume_requests_total`, `asp_resume_events_replayed_total`, `asp_resume_compacted_total`, `asp_resume_lag_events_max`, `asp_event_consumer_lag_max`, the corresponding limits, bounded process-output queue gauges `asp_process_output_queue_bytes`/`asp_process_output_queue_limit`, and `asp_workspace_git_helper_configured`/`asp_workspace_git_helper_healthy` for the canonical Git helper identity check:
In that profile, also pass `--disable-port-forwarding` or one or more exact
loopback `--port-target HOST:PORT` flags; omitting an explicit port policy is
rejected before startup.

The health endpoint also exports `asp_auth_config_healthy`, which drops to zero
when a live token/principal/certificate source is missing or malformed, so a
supervisor can stop routing work during credential rotation failures.

The health endpoint also exports `asp_response_frame_write_timeouts_total`,
which counts response streams detached after a peer stops consuming data.

It also exports `asp_pty_input_write_timeouts_total`, which counts bounded
timeouts in the synchronous PTY master writer; the attachment can reconnect
while the durable tmux session remains alive.

It also exports `asp_response_encode_gate_acquisitions_total`,
`asp_response_encode_gate_wait_us_total`, and
`asp_response_encode_duration_us_total`. The gate covers potentially large
response shapes; bounded control/interactive frames bypass it so a large
workspace/log response cannot delay PTY or session control. Compare gate wait
with request latency: a high wait share identifies large-response
head-of-line blocking, while a high encode share identifies
codec/serialization cost.

It also exports `asp_response_frame_compressed_total`,
`asp_response_frame_plain_total`, `asp_response_frame_logical_bytes_total`,
and `asp_response_frame_encoded_bytes_total`; compare the logical and encoded
totals to verify that zlib CPU is buying a wire-size reduction in production.

Artifact reuse is observable through `asp_artifact_index_entries`,
`asp_artifact_dedup_hits_total`, and `asp_artifact_dedup_bytes_total`.

The local maintenance commands `aspd --list-sessions` and
`aspd --delete-session UUID` provide a lock-protected inventory and explicit
cleanup path for abandoned sessions. Stop the daemon first; deletion refuses
running processes or persisted PTYs and should follow a verified backup.

```sh
mise exec -- cargo run -p asp-server -- --health-listen 127.0.0.1:9443
curl http://127.0.0.1:9443/live
curl http://127.0.0.1:9443/ready
```

The health metrics also expose `asp_auth_rate_limited_total` for per-source failed-HELLO throttling, `asp_principal_connection_rejections` and `asp_principal_active_connections_limit` for the per-principal connection quota, `asp_principal_process_rejections` and `asp_principal_running_processes_limit` for the cross-session process quota, `asp_idempotency_capacity_rejections`, `asp_idempotency_records`, and `asp_idempotency_records_limit` for the per-session durable request-record budget, daemon `asp_process_cpu_time_us_total`/`asp_process_max_rss_bytes` resource gauges, best-effort Linux cgroup-v2 `asp_cgroup_memory_current_bytes`/`asp_cgroup_memory_limit_bytes`, `asp_cgroup_cpu_usage_us`, and `asp_cgroup_pids_current`/`asp_cgroup_pids_limit` gauges, `asp_request_duration_us_bucket{operation=...,le=...}` plus matching `_count`/`_sum` series for fixed-cardinality per-operation latency SLOs (long-lived PTY, subscription, and port streams are excluded), `asp_workspace_file_memory_bytes`/`asp_workspace_file_memory_limit` selected-file memory gauges, `asp_frame_memory_bytes`/`asp_frame_memory_limit` decoded-request memory gauges, `asp_frame_memory_rejections` for requests refused while aggregate decode capacity is busy, `asp_response_memory_bytes`/`asp_response_memory_limit` encoded-response memory gauges, `asp_response_memory_rejections` for temporary response-capacity refusals, `asp_workspace_index_hits_total`, `asp_workspace_index_misses_total`, `asp_workspace_index_invalidations_total`, and `asp_workspace_index_watcher_healthy` for the semantic tree cache, `asp_workspace_state_digest_hits_total` for compact responses, `asp_workspace_digest_cache_hits_total` for requests that also skipped server-side semantic work, `asp_workspace_search_cache_hits_total`/`asp_workspace_search_cache_misses_total` for repeated content searches, `asp_workspace_git_cache_hits_total`/`asp_workspace_git_cache_misses_total` for repeated Git metadata queries, `asp_workspace_search_cache_bytes`/`asp_workspace_search_cache_limit` and `asp_workspace_git_cache_bytes`/`asp_workspace_git_cache_limit` for bounded cache occupancy, `asp_process_timeouts_total` for server-enforced EXEC deadlines, `asp_process_output_limit_terminations_total` for process groups terminated at the 512 MiB aggregate stdout/stderr cap, the process-level `asp_process_output_attachment_detaches_total` counter for live output attachments that close because a reader disappears or the shared output budget stays exhausted, `asp_pty_state_datagrams_sent_total`/`asp_pty_state_datagram_bytes_total` for replaceable PTY state traffic, `asp_pty_state_delta_datagrams_sent_total`/`asp_pty_state_delta_datagram_bytes_total` for base-relative plain-row traffic, `asp_pty_state_delta_datagrams_skipped_total` for deltas that do not fit or cannot be encoded, `asp_pty_state_datagrams_compressed_total` for rich-state zlib use, `asp_pty_state_datagrams_skipped_total` for snapshots that exceed the path budget, and `asp_port_target_policy_entries`/`asp_port_target_rejections_total` for exact PORT_OPEN target policy occupancy and denied attempts. A cgroup limit of zero means the host reports `max` or does not expose that controller; supervisor policy remains authoritative.
The process-log durability counters `asp_process_log_sync_total`, `asp_process_log_sync_bytes_total`, `asp_process_log_sync_duration_us_total`, and `asp_process_log_sync_failures_total` expose the bounded `sync_data` work performed before persistent output events are published. Use them to quantify the durability cost before changing the chunk/checkpoint policy.

`asp_resume_replay_limited_total` separately counts resume/subscription tails
that exceeded the bounded 100,000-event/64 MiB live replay budget and therefore
returned the current snapshot. A rising value is an operational signal to tune
retention or investigate a lagging consumer; it is not a data-loss counter.

Process-start contention is exposed separately through the fixed-cardinality
`asp_process_launch_duration_us` histogram and
`asp_process_launch_failures_total`; its timer covers durable preparation and
spawn/bookkeeping, excluding response draining and child lifetime.
The daemon syncs the immutable process wrapper once per session and hard-links
it into each process record, avoiding a repeated wrapper write/fsync on
short-lived commands while keeping per-process recovery paths and drift checks.

The configured child limits are reported as `asp_process_memory_limit_bytes`, `asp_process_cpu_seconds_limit`, `asp_process_no_new_privs`, and `asp_exec_timeout_seconds_limit`; zero means the corresponding numeric guardrail is disabled, while `asp_process_no_new_privs` is 1 when Linux privilege-gain blocking is enabled. Set `--exec-timeout-seconds` for attached `EXEC`/`EXEC_SUMMARY` commands; timed-out process groups report exit code 124, while detached `SPAWN` processes remain long-lived. On platforms without Linux address-space RLIMIT support, a nonzero `--process-memory-bytes` is rejected so a deployment cannot assume an unenforced limit. The process-boundary gauges `asp_process_launcher_configured` and `asp_process_launcher_required` show whether the external EXEC/SPAWN launcher hook is installed and whether startup was configured to fail closed without it; `asp_process_launcher_healthy` is 0 when its reviewed executable has drifted, and `/ready` fails closed until the daemon is restarted with the restored launcher.
The QUIC address-validation gauge/counters `asp_quic_stateless_retry_enabled`, `asp_quic_stateless_retries_total`, and `asp_quic_stateless_retry_failures_total` report whether Quinn stateless retry is active plus its attempts and failures. `--production` enables this amplification guard automatically; development daemons can opt in with `--stateless-retry` or `ASP_STATELESS_RETRY=1`. The first handshake may take one additional flight, while session persistence and path migration remain QUIC-owned.

The PTY cache counters `asp_pty_snapshot_cache_hits_total` and
`asp_pty_snapshot_cache_renders_total` (plus the corresponding
`asp_pty_rich_snapshot_*` counters) show whether concurrent shell/agent
attachments are reusing one generation render instead of repeating terminal
parser work.

Storage safety is visible through `asp_storage_free_bytes`,
`asp_storage_free_bytes_limit`, `asp_storage_headroom_ok`, and
`asp_storage_headroom_rejections_total`; configure a nonzero
`--min-free-bytes` in production so new durable mutations stop before the
workspace filesystem fills.

Selected-file responses share a daemon-wide 32 MiB memory budget and retain
their permits until serialization completes; when another request cannot
acquire capacity within 250 ms it fails fast so a slow reader cannot stall all
workspace queries. Workspace response encoding also borrows from the
daemon-wide 256 MiB response budget until the QUIC write completes; response
potentially large response shapes are serialized before their exact permit is
acquired, so concurrent large responses cannot create uncharged 128 MiB
buffers. Bounded control/interactive responses bypass that gate while
retaining the response-memory charge before their payload is held for QUIC.
`/metrics` exposes
`asp_frame_memory_bytes`/`asp_frame_memory_limit` for decoded request pressure
and `asp_response_memory_bytes`/`asp_response_memory_limit` for encoded response
pressure. Each encoded response frame also has a size-aware 64 KiB/s write
deadline (10-second floor, five-minute cap), so a stopped reader cannot retain
the response permit indefinitely; `asp_response_frame_write_timeouts_total`
counts these detachments for monitoring.
The client bounds one-shot response reads at five minutes as well, returning a
retryable timeout instead of hanging a control command forever; long-lived
PTY/event/port streams keep their QUIC liveness and reconnect behavior.
Git-backed workspace queries disable terminal prompts and kill/reap their
helper after 60 seconds, so a broken repository or credential helper cannot
pin the request stream indefinitely.

State backups are lock-protected and self-verifying. They include credentials
and command metadata, so encrypt them with your KMS/backup system before
copying off host. Stop any running daemon, then create and verify a backup
directory:

```sh
mise exec -- cargo run -p asp-server -- --root /srv/asp/workspace --backup-state /srv/backup/asp-state
mise exec -- cargo run -p asp-server -- --root /srv/asp/workspace --verify-state /srv/backup/asp-state
```

Restores require an explicit recovery-preserving flag; the current `.asp/` is moved to a unique sibling rather than deleted:

```sh
mise exec -- cargo run -p asp-server -- --root /srv/asp/workspace \
  --restore-state /srv/backup/asp-state --force-restore
```

A hardened single-user systemd starting point is in [deploy/systemd/aspd.service](deploy/systemd/aspd.service), with installation notes in [deploy/systemd/README.md](deploy/systemd/README.md). The unit keeps `PrivateTmp=false` intentionally so a restarted daemon can reattach a surviving tmux PTY; move tmux to a persistent workspace socket before enabling private temporary storage. macOS users can use the [launchd template](deploy/launchd/com.asp.aspd.plist) and [launchd notes](deploy/launchd/README.md).

When the client advertises the optional `pty_scrollback` capability, a fresh
PTY attachment also receives a bounded page of recent plain-text history. The
client prints that page before the authoritative current-screen redraw, so a
new `asp shell` process retains useful context after the previous client was
restarted. For tmux-backed sessions the page is collected with a bounded
`capture-pane` query through the configured process launcher and falls back
without blocking QUIC control traffic if tmux is unavailable. The history page
is capped at 256 rows/256 KiB and is omitted for older peers; it is not a
replacement for a full terminal emulator's scrollback or speculative local
echo.
