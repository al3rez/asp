# Production-shaped single-workspace image. The orchestrator should still
# supply a persistent /workspace volume and enforce its own memory/CPU/pid
# limits; the image deliberately does not contain a shell toolchain beyond
# the POSIX shell and tmux needed by ASP's command/PTY contract.
FROM rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock mise.toml README.md ./
COPY crates ./crates
RUN cargo build --locked --release -p asp-server -p asp-client

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
       ca-certificates curl git tmux \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 asp \
    && useradd --system --uid 10001 --gid 10001 --create-home --home-dir /workspace --shell /usr/sbin/nologin asp \
    && install -d -m 0755 /usr/local/libexec \
    && install -d -o asp -g asp -m 0700 /workspace

COPY --from=build /src/target/release/aspd /usr/local/bin/aspd
COPY --from=build /src/target/release/asp /usr/local/bin/asp
COPY deploy/container/asp-worker-wrapper /usr/local/libexec/asp-worker-wrapper
RUN chmod 0555 /usr/local/libexec/asp-worker-wrapper

WORKDIR /workspace
USER asp

EXPOSE 4433/udp

# Keep the control endpoint on the container's private network and expose the
# health probe only on loopback. The default image is fail-closed: the
# container's cgroup/read-only-root/no-new-privileges settings provide the
# aggregate worker boundary, while the immutable exec shim lets aspd enforce
# its production policy for every command, Git helper, and tmux PTY. Override
# CMD for mTLS/principal files or a different private-overlay bind policy.
ENTRYPOINT ["/usr/local/bin/aspd"]
CMD ["--production", "--listen", "0.0.0.0:4433", "--allow-non-loopback", "--stateless-retry", "--root", "/workspace", "--cert", "/workspace/.asp/server-cert.der", "--key", "/workspace/.asp/server-key.der", "--auth-token-file", "/workspace/.asp/auth-token", "--process-launcher", "/usr/local/libexec/asp-worker-wrapper", "--require-process-launcher", "--process-memory-bytes", "2147483648", "--process-cpu-seconds", "86400", "--exec-timeout-seconds", "3600", "--min-free-bytes", "1073741824", "--artifact-retention-hours", "720", "--shutdown-grace-seconds", "10", "--health-listen", "127.0.0.1:9443", "--disable-port-forwarding"]

# Use the loopback readiness endpoint for container health. Unlike a plain
# authenticated HEALTH request, `/ready` also fails when the audit sink,
# storage reserve, or reviewed process-launcher identity is unhealthy. The
# endpoint is not published outside the container.
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD ["/usr/bin/curl", "--fail", "--silent", "--show-error", "http://127.0.0.1:9443/ready"]

STOPSIGNAL SIGTERM
VOLUME ["/workspace"]
