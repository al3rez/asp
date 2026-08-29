# systemd deployment

The unit is a hardened starting point for a single-user daemon. Create a dedicated `asp` account, install a verified release archive with `deploy/install-release.sh` so `/usr/local/lib/asp/current/bin/aspd` is available, place the workspace at `/srv/asp/workspace`, and install the unit. Verify a downloaded archive with its bundled `deploy/verify-release.sh` (or the repository copy) before installing; it checks the digest and rejects unsafe archive entries or credentials. The installer also snapshots the archive and sidecars into a private bounded directory before verification/extraction, so a pathname replacement during installation cannot swap the bytes being installed. The unit already points at the atomic `current` release pointer, so upgrades do not require editing the service file. Keep the previous pointer and release directory until readiness is confirmed so `--rollback` remains available:

```sh
sudo install -d -o asp -g asp /srv/asp/workspace
# Install the verified archive before loading the unit. The current pointer is
# the path used by both ExecStartPre and ExecStart below.
sudo deploy/install-release.sh --prefix /usr/local/lib/asp \
  /srv/asp/releases/asp-VERSION-TARGET.tar.gz
sudo install -m 0644 deploy/systemd/aspd.service /etc/systemd/system/aspd.service
# Start aspd once in the foreground as the service account so it creates
# the pinned certificate, private key, token, and state directory; press
# Ctrl-C after the "ASP server ready" message.
sudo -u asp /usr/local/lib/asp/current/bin/aspd --root /srv/asp/workspace --listen 127.0.0.1:4433
sudo -u asp /usr/local/lib/asp/current/bin/aspd --validate-config --listen 0.0.0.0:4433 --allow-non-loopback --root /srv/asp/workspace --cert /srv/asp/workspace/.asp/server-cert.der --key /srv/asp/workspace/.asp/server-key.der --auth-token-file /srv/asp/workspace/.asp/auth-token
sudo systemctl daemon-reload
sudo systemctl enable --now aspd
```

For a readiness-gated binary rollout, use the archive's
`deploy/upgrade-release.sh` with the exact unit restart command. It waits for
the current `/ready` endpoint, atomically activates the new release, restarts
the unit, and restores `previous` if readiness fails:

```sh
sudo deploy/upgrade-release.sh \
  --prefix /usr/local/lib/asp \
  --ready-url http://127.0.0.1:9443/ready \
  --restart-command 'systemctl restart aspd.service' \
  /srv/asp/releases/asp-VERSION-TARGET.tar.gz \
  /srv/asp/releases/asp-VERSION-TARGET.sha256
```

The helper returns nonzero after a failed rollout even when rollback succeeds;
keep the old release directory until the observation window has closed. A
fail-closed prefix trust preflight rejects untrusted symlinked or
group/world-writable existing ancestors before the lock is created, and the
installer repeats the check before publishing. The prefix lock then serializes
concurrent upgrade attempts across the entire install/restart/rollback
transaction; if a host loses the upgrader to a hard kill, remove the stale lock
only after verifying that no rollout remains active.

For the fail-closed deployment, the release also includes
`aspd-production.service`. It is deliberately a separate unit so the pilot
baseline above remains easy to run locally while production installs cannot
silently omit the process boundary. Before enabling it, install and review an
operator-owned executable at `/usr/local/libexec/asp-worker-wrapper`. The
wrapper must enforce the site's filesystem/network/cgroup policy and `exec`
the arguments it receives (the shell command, canonical Git helper, or
absolute tmux command); a passthrough script is not a sandbox. The unit has a
`ConditionPathIsExecutable` guard and passes `--production` and
`--require-process-launcher` to both preflight and live commands. It also
passes `--stateless-retry`, so Quinn validates untrusted UDP Initial packets
before the daemon allocates TLS/application state:

```sh
sudo install -m 0644 deploy/systemd/aspd-production.service \
  /etc/systemd/system/aspd-production.service
sudo systemctl daemon-reload
sudo systemctl enable --now aspd-production
sudo systemctl is-active --quiet aspd-production
curl --fail http://127.0.0.1:9443/ready
```

The production unit allows up to five minutes for startup so incremental WAL
replay, snapshot recovery, and PTY/process reconciliation on a large durable
workspace do not trip systemd's shorter default start timeout.

The production unit intentionally binds `0.0.0.0:4433` with
`--allow-non-loopback`; restrict that UDP port to the Tailscale/private-overlay
interface or firewall before enabling it. It disables `PORT_OPEN` by default;
add reviewed exact targets to both command lines only when required. Adjust
the fixed `/srv/asp/workspace`, credential paths, resource limits, and
launcher path in a reviewed deployment change when the host layout differs,
then run `--production --validate-config` before restarting the service.

The daemon accepts `SIGHUP` as a TLS reload. Provision a replacement
certificate and private key with restrictive permissions, atomically replace
both files, then ask systemd to reload the configuration:

```sh
# Replace these paths with files issued by the deployment's CA/PKI.
sudo install -m 0644 /secure/staged/server-cert.der /srv/asp/workspace/.asp/server-cert.der.new
sudo install -m 0600 -o asp -g asp /secure/staged/server-key.der /srv/asp/workspace/.asp/server-key.der.new
sudo -u asp mv /srv/asp/workspace/.asp/server-cert.der.new /srv/asp/workspace/.asp/server-cert.der
sudo -u asp mv /srv/asp/workspace/.asp/server-key.der.new /srv/asp/workspace/.asp/server-key.der
sudo systemctl reload aspd
sudo systemctl is-active --quiet aspd
curl --fail http://127.0.0.1:9443/ready
```

`SIGHUP` affects new QUIC handshakes only; existing connections and durable
sessions are left untouched. If a staged pair is invalid or briefly
mismatched, ASP keeps the last known-good TLS configuration and logs the
failure, so retry the reload after both files are in place. Clients can point
`--cert` at a directory containing both the old and replacement `.der` pins;
copy that directory to each client before reloading, then remove the retired
pin after all clients have completed a reconnect. A client that still pins only
the retired certificate cannot reconnect once the server presents the
replacement.

Keep `.asp/` on durable storage and copy the pinned certificate and token to clients out of band. The daemon intentionally executes shell commands as the `asp` service account; this unit is not a container or a multi-tenant sandbox. The unit explicitly acknowledges the non-loopback bind with `--allow-non-loopback`; restrict UDP/4433 to the Tailscale interface/firewall and never expose it directly to the Internet. Add an explicit `ReadWritePaths` entry if the workspace is elsewhere.

Rotate the default bearer token during a maintenance window. Stop the unit first (the state lock refuses rotation while `aspd` is serving), rotate the file atomically, then start the unit and distribute the new value through the existing out-of-band secret path:

```sh
sudo systemctl stop aspd
sudo -u asp /usr/local/lib/asp/current/bin/aspd --root /srv/asp/workspace --rotate-auth-token
sudo systemctl start aspd
```

Clients that read `--auth-token-file` pick up the replacement on their next connection; an explicitly supplied `--auth-token` must be updated by the operator. Existing authenticated connections are revalidated and lose access after rotation.

For shared deployments, replace bearer-token authentication with a CA-signed client certificate. Install a DER CA (or a directory of up to eight regular `.der` CAs for overlap) and a mode-0600 JSON fingerprint map outside the workspace (or use absolute paths), then add `--client-ca /etc/asp/client-ca.der --auth-certificates-file /etc/asp/certificates.json` to `ExecStart`. Generate the map from each leaf certificate's SHA-256 DER fingerprint (for example, `openssl x509 -in client.pem -outform der | shasum -a 256`) and provision the matching client DER certificate/key out of band. Rotate by staging old/new CAs in the bundle, atomically replacing the map, sending `systemctl reload aspd`, and verifying a fresh client before removing the retired CA; existing connections are rechecked on their next request.

When converting an existing single-token workspace to a principals file, stop
the daemon and explicitly bind its legacy sessions once. The command validates
the selected principal and takes the same state lock as `aspd`; it never runs
while the daemon is serving:

```sh
sudo systemctl stop aspd
sudo -u asp /usr/local/lib/asp/current/bin/aspd \
  --root /srv/asp/workspace \
  --auth-principals-file /etc/asp/principals.json \
  --migrate-legacy-owner alice
sudo systemctl start aspd
```

Do not use this migration to combine unrelated owners; create a separate
workspace/service when the legacy token represented more than one person.

Back up the state directory during a maintenance window while the unit is stopped. The release binary verifies every file before restore:

```sh
sudo systemctl stop aspd
sudo -u asp /usr/local/lib/asp/current/bin/aspd --root /srv/asp/workspace --backup-state /srv/backup/asp-state
sudo -u asp /usr/local/lib/asp/current/bin/aspd --root /srv/asp/workspace --verify-state /srv/backup/asp-state
sudo systemctl start aspd
```

To restore, stop the unit and pass `--restore-state ... --force-restore`; the existing `.asp` directory is retained under a unique `.asp.pre-restore-*` name for rollback.

The supplied unit also sets `--exec-timeout-seconds 3600` and
`--min-free-bytes 1073741824` (1 GiB headroom), plus
`--artifact-retention-hours 720` (30 days): attached
`EXEC`/`EXEC_SUMMARY` commands that sleep or wait forever are terminated after
one hour and report exit code 124, while detached `SPAWN` processes remain
long-lived. Port forwarding is disabled by default; add reviewed exact
`--port-target HOST:PORT` values to both command lines only when required.
Tune this value for the workspace, and keep a stronger
supervisor/worker boundary for untrusted commands.

If the workspace policy requires a dedicated worker boundary, add the same
absolute launcher and arguments to both `ExecStartPre` and `ExecStart`, for
example `--process-launcher /usr/local/libexec/asp-worker-wrapper
--require-process-launcher`. The wrapper must `exec` the final shell command,
the canonical Git executable used by semantic workspace queries, and the
absolute `tmux` command that ASP passes for PTY creation. ASP validates
that it is a regular executable and exposes the configured/required state in
health metrics. Keep the wrapper's supervisor policy separate from this unit's
daemon cgroup, and do not use a parent-death option that would kill durable
`SPAWN` jobs or tmux sessions during an `aspd` restart.

For a fail-closed production profile, also add `--production` to both command
lines. The profile requires the launcher, `--process-cpu-seconds`,
`--exec-timeout-seconds`, `--min-free-bytes`, `--health-listen`, and an
explicit port policy (`--disable-port-forwarding` or one or more
`--port-target HOST:PORT` flags) and rejects startup before creating state when
any control is missing. The checked-in unit intentionally
remains a single-user pilot baseline because it cannot guess a site's sandbox
wrapper path; enable the profile only after installing and reviewing that
wrapper.

The unit includes conservative cgroup, filesystem, namespace, syscall-architecture, capability, and address-family limits (`TasksMax`, `MemoryMax`, `CPUQuota`, `NOFILE`, `NoNewPrivileges`, an empty capability bounding/ambient set, `ProtectSystem=strict`, `RestrictNamespaces`, `RestrictAddressFamilies`, and kernel-protection settings). `MemoryAccounting`, `CPUAccounting`, and `TasksAccounting` are enabled so the cgroup-v2 gauges in `/metrics` report useful usage on systemd hosts. `ProtectSystem=strict` makes the host filesystem read-only to the daemon; `ReadWritePaths` grants only the workspace. `PrivateTmp` is intentionally disabled: `KillMode=process` leaves session children alive across a daemon restart, and tmux's default control socket lives under `/tmp`; a private temporary namespace would prevent PTY reattachment. If a deployment needs private temporary storage, configure tmux to use a persistent socket directory in the workspace and validate that migration before enabling it. `PrivateDevices` is deliberately disabled in the supplied PTY-capable unit because tmux needs `/dev/ptmx` and devpts; deployments that disable PTY support may turn it on after testing their command workload. Each command also inherits a 2 GiB virtual-address-space and 24-hour CPU-time RLIMIT from `--process-memory-bytes`/`--process-cpu-seconds`; these stop one command tree from consuming the whole workspace account, while the cgroup remains the authority for aggregate daemon-plus-child usage. `KillMode=process` is intentional: systemd must not kill session children when only the daemon is restarted; ASP reconciles them from durable process logs. Startup is allowed up to five minutes for WAL recovery and systemd suppresses a rapid restart storm after five failures per minute; inspect the journal and restore a verified backup when startup fails closed on corrupt state. Tune the limits for the host and workload; they are guardrails, not a substitute for per-identity quotas or a stronger sandbox such as a container, VM, or systemd service per workspace. The unit's loopback health endpoint can be queried by a local monitor at `http://127.0.0.1:9443/live`, `/ready`, or `/metrics`; keep it loopback-only unless an authenticated reverse proxy is in front.
