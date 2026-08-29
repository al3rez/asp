FROM rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
       git iproute2 jq mosh openssh-client openssh-server ripgrep time tmux util-linux \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work
COPY Cargo.toml Cargo.lock mise.toml README.md ./
COPY crates ./crates
RUN cargo build --locked --release --workspace

COPY benchmarks/docker-benchmark.sh /usr/local/bin/asp-docker-benchmark
COPY benchmarks/docker-agent-workload.sh /usr/local/bin/asp-agent-workload
ENTRYPOINT ["/usr/local/bin/asp-docker-benchmark"]
