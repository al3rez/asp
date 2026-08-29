# Container deployment

`Dockerfile` builds a minimal, non-root `aspd`/`asp` image for one trusted
workspace. It keeps the image filesystem read-only at runtime, includes Git
for semantic workspace queries, gives ASP a private persistent `/workspace`
volume, and starts the daemon in the fail-closed `--production` profile. The
container cgroup, read-only root, and no-new-privileges settings provide the
aggregate worker boundary; the image's immutable `asp-worker-wrapper` is an
exec-only shim that lets ASP apply and recheck one process-launch policy to
EXEC, SPAWN, Git helpers, and tmux PTYs. Cgroup, network, and backup policy
still belong to the container runtime or orchestrator. The Rust and Debian
base images are pinned by digest; update them deliberately as a reviewed
release change rather than inheriting a moving tag.

Build and run it with a persistent volume:

```sh
docker build -f deploy/container/Dockerfile -t asp:dev .
docker volume create asp-workspace
docker run -d --name aspd \
  --read-only --tmpfs /tmp:mode=1777 \
  --cap-drop=ALL --security-opt=no-new-privileges \
  --pids-limit=512 --memory=2g --cpus=2 \
  --stop-timeout=30 \
  -p 4433:4433/udp \
  -v asp-workspace:/workspace \
  asp:dev
```

The image runs as the fixed non-root UID/GID `10001:10001`. A named volume is
the simplest option because Docker initializes it with the image's private
workspace ownership. For a host bind mount, create the directory with the
same ownership before starting the container (or use an orchestrator-managed
volume):

```sh
sudo install -d -m 700 -o 10001 -g 10001 /srv/asp/workspace
# Then replace `-v asp-workspace:/workspace` above with
# `-v /srv/asp/workspace:/workspace`.
```

Do not make the workspace world-writable to work around a permission error;
the daemon deliberately refuses unsafe production workspace paths.

Restrict UDP/4433 to a Tailscale/private-overlay interface or firewall. The
image defaults to the generated bearer token; for shared workspaces override
the command with `--client-ca` and `--auth-certificates-file` (or a principals
file) and provision those files through a secret volume. Do not publish the
unauthenticated health endpoint; it is bound to container loopback. The image
has a Docker `HEALTHCHECK` against loopback `/ready`, so an orchestrator sees
audit/storage/launcher readiness rather than merely a successful authenticated
HEALTH request, without publishing `/live`, `/ready`, or `/metrics`. A monitor
inside the container can query those endpoints or expose them through an
authenticated sidecar.

Keep the runtime stop timeout above ASP's 10-second request drain (the example
uses 30 seconds) so the daemon can synchronously flush journals and audit
records before the container runtime sends its final kill.

The persistent volume must include `.asp/`; back it up with `aspd
--backup-state` while the container is stopped and verify it before restore.
Container replacement is a host-level outage for supervised child processes,
so stop or migrate long-running jobs before replacing the container. A daemon
restart inside the same container preserves the tmux-backed PTY contract; a
container restart requires the orchestrator to provide an equivalent child
lifecycle policy. The image is a deployment boundary, not a complete hostile
command sandbox; use per-tenant containers or VMs and separate service
identities when commands are not trusted.

The default image command enables `--production` and sets
`--exec-timeout-seconds 3600`,
`--min-free-bytes 1073741824` (1 GiB headroom), and
`--artifact-retention-hours 720` (30 days), so attached
`EXEC`/`EXEC_SUMMARY` commands are terminated after one hour with exit code 124;
detached `SPAWN` processes remain long-lived. Tune or override this policy for
your build workload. Port forwarding is disabled by default; add reviewed
`--port-target HOST:PORT` values only when the workspace needs them. Keep the
container/cgroup boundary for aggregate resource enforcement. The image also
passes `--stateless-retry`, so Quinn validates the source address of untrusted
UDP Initial packets before the daemon allocates application state; the first
connection pays one additional handshake flight.

The default command already supplies `--production`, `--health-listen`,
`--process-cpu-seconds`, `--exec-timeout-seconds`, `--min-free-bytes`, an
explicit port policy, and the immutable wrapper. If you override the command,
retain those controls and either retain the image wrapper or replace it with a
reviewed absolute launcher that `exec`s `/bin/sh`, the canonical Git
executable, and the absolute `tmux` command ASP passes for PTY creation. The
launcher must preserve children across an `aspd` restart. The container itself
remains the recommended boundary for a trusted workspace; hostile
multi-tenant workloads need separate containers/VMs or another reviewed
sandbox policy. Do not use a parent-death option that would terminate durable
`SPAWN` processes or tmux sessions when only the daemon is restarted.
