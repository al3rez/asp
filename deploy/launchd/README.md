# launchd deployment

`com.asp.aspd.plist` is a macOS LaunchAgent starting point for a single-user
workspace. Install a verified release archive with `deploy/install-release.sh`
so `/usr/local/lib/asp/current/bin/aspd` is available; the plist already points
at that atomic release pointer. The workspace defaults to
`/Users/asp/asp-workspace`; replace that path in the plist before loading it.
The daemon's default loopback listener is intentional. For a remote client,
run ASP through Tailscale or another private overlay and explicitly configure
`--allow-non-loopback` plus a firewall policy.

For binary upgrades, use the archive's `deploy/install-release.sh` to populate
a versioned `/usr/local/lib/asp/releases` directory and atomically switch its
`current` pointer. Run `--validate-config` against that pointer, then use
`launchctl kickstart -k` deliberately. Keep
the `previous` pointer until the new daemon is ready; `--rollback` swaps back
without overwriting either release directory. The installer verifies the
archive (including its bundled SPDX SBOM) and never restarts LaunchAgents
itself.

For a readiness-gated rollout with automatic rollback, use the packaged
`deploy/upgrade-release.sh` and pass the exact `launchctl` command for the
loaded job:

```sh
deploy/upgrade-release.sh \
  --prefix /usr/local/lib/asp \
  --ready-url http://127.0.0.1:9443/ready \
  --restart-command 'launchctl kickstart -k gui/$(id -u)/com.asp.aspd' \
  /srv/asp/releases/asp-VERSION-TARGET.tar.gz \
  /srv/asp/releases/asp-VERSION-TARGET.sha256
```

The helper returns nonzero after a failed rollout even when rollback succeeds;
retain the previous release directory until the observation window has closed.

Create the workspace and load the job as the owning user:

```sh
install -d -m 700 /Users/asp/asp-workspace
install -d -m 700 /Users/asp/asp-workspace/.asp
cp deploy/launchd/com.asp.aspd.plist ~/Library/LaunchAgents/com.asp.aspd.plist
# Edit the copied plist paths first.
launchctl bootstrap gui/"$(id -u)" ~/Library/LaunchAgents/com.asp.aspd.plist
launchctl kickstart -k gui/"$(id -u)"/com.asp.aspd
curl http://127.0.0.1:9443/live
```

For a fail-closed production deployment, the release also includes
`com.asp.aspd-production.plist`. It passes `--production` and
`--require-process-launcher` to the daemon and uses the same reviewed
`/usr/local/libexec/asp-worker-wrapper` convention as the production systemd
unit. Install and qualify that wrapper before loading the plist; it must
enforce the site's process/filesystem/network policy and `exec` the arguments
it receives. The plist also passes `--stateless-retry`, enabling Quinn's
address-validation token exchange before application resources are allocated.
A passthrough wrapper is not a sandbox. Replace the example
workspace paths in the plist before loading it:

```sh
install -d -m 700 /Users/asp/asp-workspace
install -d -m 700 /Users/asp/asp-workspace/.asp
cp deploy/launchd/com.asp.aspd-production.plist \
  ~/Library/LaunchAgents/com.asp.aspd-production.plist
launchctl bootstrap gui/"$(id -u)" \
  ~/Library/LaunchAgents/com.asp.aspd-production.plist
launchctl kickstart -k gui/"$(id -u)"/com.asp.aspd.production
curl --fail http://127.0.0.1:9443/ready
```

The production plist binds `0.0.0.0:4433` with
`--allow-non-loopback`; restrict UDP/4433 to the Tailscale/private-overlay
interface or firewall before loading it. `PORT_OPEN` is disabled by default.
Run `--production --validate-config` against the final paths before a binary
upgrade or plist reload.

`AbandonProcessGroup=true` is required for ASP's durable-session contract:
unloading/restarting the daemon must not terminate its supervised EXEC
children. The plist also bounds open files and processes with launchd resource
limits; tune them for the host before enabling a large agent workload. Stop
the job with `launchctl bootout` only when those children have
been intentionally stopped or their state has been backed up. The plist's
stdout/stderr files are launchd diagnostics; monitor or rotate them separately
from ASP's bounded event/process logs.

The supplied plist sets `--exec-timeout-seconds 3600` and
`--min-free-bytes 1073741824` (1 GiB headroom), plus
`--artifact-retention-hours 720` (30 days), so attached
`EXEC`/`EXEC_SUMMARY` commands that wait forever are terminated after one hour
with exit code 124; detached `SPAWN` processes remain long-lived. Port
forwarding is disabled by default; add reviewed exact `--port-target HOST:PORT`
values to the copied plist only when required. Adjust the argument for the
workspace and use a stronger supervisor/worker boundary for untrusted commands.

If a reviewed worker wrapper is required, add its absolute path and repeated
`--process-launcher-arg` values to the copied plist and add
`--require-process-launcher`. The wrapper must `exec` both the final shell
command and the absolute `tmux` command that ASP passes for PTY creation, so
PID identity, process groups, and durable restart recovery remain valid. This
is an integration hook rather than a built-in sandbox; the operator must
qualify the wrapper's PTY behavior and parent-death policy, which must not
terminate durable `SPAWN` jobs or tmux sessions during a daemon-only restart.

For a fail-closed production profile, add `--production` to the copied plist's
preflight/live arguments together with the reviewed launcher,
`--process-cpu-seconds`, `--exec-timeout-seconds`, `--min-free-bytes`,
`--health-listen`, and an explicit port policy (`--disable-port-forwarding` or repeated
`--port-target HOST:PORT` values).
The profile intentionally is not enabled in this template because the correct
sandbox wrapper is site-specific; without one, startup should remain an
explicitly trusted single-user deployment.

The PTY backend resolves `tmux` from `/usr/bin`, `/bin`, `/usr/local/bin`, and
Apple-Silicon Homebrew's `/opt/homebrew/bin` even when launchd supplies a
minimal `PATH`. For a custom installation, add an absolute `ASP_TMUX_PATH`
environment entry to the copied plist before loading it.

Workspace Git metadata uses the same deterministic lookup (`/usr/bin/git`,
`/bin/git`, common package-manager prefixes, then `PATH`). If Git is installed
elsewhere, add an absolute `ASP_GIT_PATH` environment entry to the copied
plist; a non-Git workspace remains valid and simply omits Git fields.

On macOS, a running daemon accepts `SIGHUP` for a TLS certificate/key reload
that affects new QUIC handshakes only. Clients can point `--cert` at a
directory containing both old and replacement `.der` pins before the reload;
remove the retired pin after every client has reconnected. After atomically
installing a complete replacement pair, send the signal to the job's daemon
PID (for example, `kill -HUP $(pgrep -x aspd)`). An invalid or temporarily
incomplete pair is rejected and the last known-good configuration remains
active; retry after both files are present. A full `launchctl kickstart -k`
restart is still appropriate for binary upgrades.

To rotate the default bearer token, stop the LaunchAgent before replacing the
credential, then start it again:

```sh
launchctl bootout gui/"$(id -u)"/com.asp.aspd
/usr/local/lib/asp/current/bin/aspd --root /Users/asp/asp-workspace --rotate-auth-token
launchctl bootstrap gui/"$(id -u)" ~/Library/LaunchAgents/com.asp.aspd.plist
launchctl kickstart -k gui/"$(id -u)"/com.asp.aspd
```

Token-file clients reload the new value on their next connection; clients
configured with a literal `--auth-token` need an explicit secret update.

For a multi-user or Internet-facing deployment, use a dedicated account and
stronger isolation (a VM/container or per-workspace service) rather than
sharing this LaunchAgent. Configure mTLS identity mapping and backup/restore
policy as described in the main production-readiness document.
