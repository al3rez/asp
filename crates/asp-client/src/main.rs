use anyhow::{Context, Result, anyhow, bail};
use asp_protocol::{
    EventKind, FRAME_CODEC_OFFLOAD_MIN_BYTES, FRAME_HEADER_BYTES, FilePatchRange,
    LEGACY_PROTOCOL_VERSION, OPTIONAL_FEATURES, OutputStream, PROTOCOL_VERSION,
    PTY_SCROLLBACK_MAX_BYTES, PTY_SCROLLBACK_MAX_LINE_BYTES, PTY_SCROLLBACK_MAX_LINES,
    PtyRichSnapshot, PtyScrollbackSnapshot, PtySnapshot, PtyStateDeltaDatagram,
    QUIC_STREAM_PRIORITY_BULK, Request, Response, SUPPORTED_FEATURES, WorkspaceFile,
    WorkspaceSearchHit, WorkspaceTreeEntry, WorkspaceVersion, configure_quic_transport,
    decode_frame_payload_for_version, decode_message, decode_pty_rich_datagram,
    decode_pty_state_datagram, decode_pty_state_delta_datagram, encode_frame_payload_for_version,
    encode_message, protocol_version_supported, quic_stream_priority, read_frame_for_version,
    read_frame_payload_for_version, should_attempt_frame_compression,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::{Parser, Subcommand};
use fs2::FileExt as _;
use quinn::{Connection, Endpoint, RecvStream, SendStream, TransportConfig};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as UnixOpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt,
};
use tokio::net::{TcpListener, TcpStream};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(
    name = "asp",
    about = "ASP agent-native remote development client",
    version
)]
struct Args {
    /// Pinned server certificate DER file, or a directory containing a
    /// bounded set of `.der` pins for certificate rollover.
    #[arg(
        long,
        env = "ASP_CERT",
        default_value = ".asp/server-cert.der",
        global = true
    )]
    cert: PathBuf,
    /// TLS server name/SNI. The default `localhost` keeps generated ASP
    /// certificates compatible; set this to the DNS name (or IP SAN) in an
    /// operator-issued certificate.
    #[arg(
        long,
        env = "ASP_SERVER_NAME",
        default_value = "localhost",
        global = true,
        value_name = "NAME"
    )]
    server_name: String,
    #[arg(
        long,
        env = "ASP_SESSION_FILE",
        default_value = ".asp-session",
        hide_default_value = true,
        global = true,
        help = "Session cursor path (defaults to per-user state; legacy .asp-session is reused when present)"
    )]
    session_file: PathBuf,
    /// Stable local identity for an independent event consumer. Use a
    /// different value for concurrent agents that follow the same session;
    /// the default cursor remains backward-compatible for single consumers.
    #[arg(long, env = "ASP_CONSUMER_ID", global = true, value_name = "ID")]
    consumer_id: Option<String>,
    /// Local file containing the server's bearer token.
    #[arg(
        long,
        env = "ASP_AUTH_TOKEN_FILE",
        default_value = ".asp/auth-token",
        global = true
    )]
    auth_token_file: PathBuf,
    /// Explicit server bearer token. Prefer --auth-token-file for daily use.
    #[arg(long, env = "ASP_AUTH_TOKEN", global = true)]
    auth_token: Option<String>,
    /// Prefer the compact plain PTY state-delta path over ANSI rich snapshots.
    /// This trades terminal cell attributes for lower replaceable-state bytes;
    /// reliable PTY output remains unchanged. Useful for high-latency or
    /// metered links where localized screen changes dominate.
    #[arg(
        long,
        env = "ASP_PREFER_PTY_DELTA",
        default_value_t = false,
        global = true
    )]
    prefer_pty_delta: bool,
    /// Maximum time to wait for one QUIC/TLS connection attempt across all
    /// resolved addresses. Retry loops apply their own bounded backoff on top
    /// of this value; keeping the timeout configurable lets scripts fail
    /// quickly while high-latency links can retain a generous handshake budget.
    #[arg(
        long,
        env = "ASP_CONNECT_TIMEOUT_MS",
        default_value_t = DEFAULT_CONNECT_TIMEOUT_MS,
        global = true,
        value_name = "MILLISECONDS"
    )]
    connect_timeout_ms: u64,
    /// Maximum time to keep retrying an established session after transport
    /// loss before returning an error. Interactive shells and event
    /// subscriptions remain cancellable and retry indefinitely; this bound
    /// applies to ordinary request/response recovery.
    #[arg(
        long,
        env = "ASP_RECONNECT_TIMEOUT_MS",
        default_value_t = DEFAULT_RECONNECT_TIMEOUT_MS,
        global = true,
        value_name = "MILLISECONDS"
    )]
    reconnect_timeout_ms: u64,
    /// DER-encoded client certificate for mTLS deployments.
    #[arg(long, env = "ASP_CLIENT_CERT", global = true, requires = "client_key")]
    client_cert: Option<PathBuf>,
    /// DER-encoded client private key (PKCS#8, PKCS#1, or SEC1).
    #[arg(long, env = "ASP_CLIENT_KEY", global = true, requires = "client_cert")]
    client_key: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Verify TLS pinning, authentication, protocol, session store, and PTY readiness.
    Doctor {
        #[arg(env = "ASP_SERVER")]
        server: String,
        /// Fail if the authenticated endpoint does not advertise a supported
        /// protocol, requires no client authentication, or cannot provide
        /// the durable tmux PTY backend. The default remains a diagnostic
        /// health query so local development can inspect an intentionally
        /// insecure daemon.
        #[arg(long, default_value_t = false)]
        strict: bool,
        /// Also check the daemon's loopback HTTP readiness endpoint. This is
        /// useful on the server host because `/ready` covers audit, storage
        /// headroom, process-launcher identity, and drain state that the
        /// portable authenticated HEALTH response intentionally omits.
        #[arg(long, value_name = "HTTP_URL")]
        ready_url: Option<String>,
    },
    /// Establish a connection and reuse the saved durable session. Pass
    /// `--new` when a separate session identity is intentional.
    Connect {
        #[arg(env = "ASP_SERVER")]
        server: String,
        /// Create a fresh durable session even when this client already has
        /// a saved session for the server. The default reuses the saved
        /// session so repeating `asp connect` cannot orphan processes.
        #[arg(long, default_value_t = false)]
        new: bool,
    },
    Resume {
        #[arg(env = "ASP_SERVER")]
        server: String,
        /// Resume an explicitly supplied durable session when the local
        /// cursor file is unavailable (for example, after moving to another
        /// laptop). The authenticated owner still has to match the server's
        /// session owner; a UUID is not a credential.
        #[arg(long, value_name = "UUID")]
        session_id: Option<Uuid>,
        /// Event cursor to use for an explicitly supplied session. A zero
        /// cursor requests a current snapshot plus all retained events.
        #[arg(long, default_value_t = 0, value_name = "EVENT_ID")]
        after_event_id: u64,
    },
    /// Follow durable process/file events without polling. Ctrl-C detaches;
    /// the saved cursor is updated so a later invocation can resume.
    Events {
        #[arg(env = "ASP_SERVER")]
        server: String,
        #[arg(long)]
        after_event_id: Option<u64>,
        #[arg(long)]
        process_id: Option<Uuid>,
        #[arg(long)]
        no_output: bool,
    },
    /// Read a bounded range from a durable process stdout/stderr log. Log
    /// ranges remain available after journal output events are compacted.
    Logs {
        /// Legacy endpoint positional argument. Use `--server` or
        /// `ASP_SERVER` when the process ID is the only positional operand.
        server: Option<String>,
        process_id: Option<Uuid>,
        #[arg(long = "server", env = "ASP_SERVER", value_name = "SERVER")]
        server_option: Option<String>,
        #[arg(long, default_value = "stdout")]
        stream: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        length: Option<u64>,
        /// Fetch only the final N bytes as of the process-state snapshot.
        /// This avoids transferring an entire compiler/test log when an
        /// agent needs only the current diagnostic tail.
        #[arg(long)]
        tail: Option<u64>,
    },
    /// Read one detached process's current durable state without replaying
    /// the entire session snapshot. This is useful for status checks before
    /// fetching a growing log range.
    Status {
        /// Legacy endpoint positional argument. Use `--server` or
        /// `ASP_SERVER` when the process ID is the only positional operand.
        server: Option<String>,
        process_id: Option<Uuid>,
        #[arg(long = "server", env = "ASP_SERVER", value_name = "SERVER")]
        server_option: Option<String>,
    },
    /// Retrieve a content-addressed artifact range. Without --offset/--length
    /// this downloads the complete immutable object and resumes through a
    /// locked local checkpoint if the connection or client is interrupted.
    ArtifactGet {
        /// Legacy endpoint positional argument. With `--server` or
        /// `ASP_SERVER`, pass only `ARTIFACT_ID LOCAL`.
        server: Option<String>,
        artifact_id: Option<String>,
        local: Option<PathBuf>,
        #[arg(long = "server", env = "ASP_SERVER", value_name = "SERVER")]
        server_option: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        length: Option<u64>,
    },
    /// Publish a local file as an immutable SHA-256-addressed artifact.
    ArtifactPut {
        /// Legacy endpoint positional argument. With `--server` or
        /// `ASP_SERVER`, pass only `LOCAL`.
        server: Option<String>,
        local: Option<PathBuf>,
        #[arg(long = "server", env = "ASP_SERVER", value_name = "SERVER")]
        server_option: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    Exec {
        #[arg(env = "ASP_SERVER")]
        server: String,
        /// Return exit status, byte counts, and bounded output tails instead
        /// of forwarding the complete transcript.
        #[arg(long, default_value_t = false)]
        summary: bool,
        /// Maximum stdout/stderr tail returned by --summary.
        #[arg(long, default_value_t = 8 * 1024, requires = "summary")]
        tail_bytes: u32,
        /// Execute one command string. This option is useful with
        /// `ASP_SERVER`, because the endpoint can then be omitted without
        /// colliding with the command's positional arguments.
        #[arg(long = "command", short = 'c', conflicts_with = "command")]
        command_text: Option<String>,
        command: Vec<String>,
    },
    /// Execute repeated commands over one authenticated QUIC connection.
    /// This is the low-overhead mode for coding-agent adapters.
    Batch {
        #[arg(env = "ASP_SERVER")]
        server: String,
        #[arg(long = "command", short = 'c', conflicts_with = "stdin")]
        commands: Vec<String>,
        /// Return bounded output tails and byte counts for each command
        /// instead of forwarding the complete transcript. This keeps a
        /// warm batch connection useful for test/build loops that produce
        /// large logs without making every command pay the full transfer.
        #[arg(long, default_value_t = false)]
        summary: bool,
        /// Maximum stdout/stderr tail returned by --summary.
        #[arg(long, default_value_t = 8 * 1024, requires = "summary")]
        tail_bytes: u32,
        /// Run independent summary commands concurrently over the same QUIC
        /// connection. This is intentionally explicit and only supports
        /// zero-tail summaries: command output is not ordered when requests
        /// overlap, while exit markers remain in input order. Keep the
        /// default of one for commands with dependencies or output needs.
        #[arg(long, default_value_t = 1, value_name = "N")]
        parallel: usize,
        /// Read one shell command per line from stdin and keep the QUIC
        /// connection open until EOF. Command output remains on its normal
        /// stdout/stderr streams; a `ASP_BATCH_RESULT <index> <code>` marker
        /// is written to stderr after each command.
        #[arg(long, conflicts_with = "commands")]
        stdin: bool,
    },
    /// Keep one authenticated QUIC connection open for an agent adapter.
    /// Input and output are newline-delimited JSON; EXEC output is streamed
    /// as offset-addressed base64 events and reconnects reuse request IDs.
    Agent {
        #[arg(env = "ASP_SERVER")]
        server: String,
    },
    /// Serve the persistent JSONL adapter over a private Unix-domain socket.
    /// A supervisor can keep this endpoint warm so local agents connect
    /// without starting a new remote adapter process for every tool call.
    AgentListen {
        /// Legacy endpoint positional argument. With `--server` or
        /// `ASP_SERVER`, pass only `SOCKET`.
        server: Option<String>,
        socket: Option<PathBuf>,
        #[arg(long = "server", env = "ASP_SERVER", value_name = "SERVER")]
        server_option: Option<String>,
    },
    /// Connect stdin/stdout to an `agent-listen` Unix-domain socket.
    AgentConnect { socket: PathBuf },
    Spawn {
        #[arg(env = "ASP_SERVER")]
        server: String,
        /// Execute one command string when the endpoint comes from
        /// `ASP_SERVER`; the positional form remains available for
        /// compatibility with `asp spawn SERVER COMMAND...`.
        #[arg(long = "command", short = 'c', conflicts_with = "command")]
        command_text: Option<String>,
        command: Vec<String>,
    },
    /// Send a durable, idempotent signal to a detached process.
    Signal {
        /// Legacy endpoint positional argument. Use `--server` or
        /// `ASP_SERVER` when the process ID is the only positional operand.
        server: Option<String>,
        process_id: Option<Uuid>,
        #[arg(long = "server", env = "ASP_SERVER", value_name = "SERVER")]
        server_option: Option<String>,
        /// Signal name (HUP, INT, KILL, TERM) or number.
        #[arg(long, default_value = "TERM")]
        signal: String,
    },
    Shell {
        #[arg(env = "ASP_SERVER")]
        server: String,
    },
    /// Forward a local TCP listener to a loopback service on the remote host.
    /// Each accepted TCP flow uses one bidirectional QUIC stream.
    Forward {
        #[arg(env = "ASP_SERVER")]
        server: String,
        #[arg(long)]
        listen: SocketAddr,
        #[arg(long)]
        target: SocketAddr,
        /// Explicitly expose the local listener beyond loopback.
        #[arg(long, default_value_t = false)]
        allow_non_loopback: bool,
    },
    Get {
        /// Legacy endpoint positional argument. With `--server` or
        /// `ASP_SERVER`, pass `REMOTE LOCAL`.
        server: Option<String>,
        remote: Option<String>,
        local: Option<PathBuf>,
        #[arg(long = "server", env = "ASP_SERVER", value_name = "SERVER")]
        server_option: Option<String>,
    },
    Put {
        /// Legacy endpoint positional argument. With `--server` or
        /// `ASP_SERVER`, pass `LOCAL REMOTE`.
        server: Option<String>,
        local: Option<PathBuf>,
        remote: Option<String>,
        #[arg(long = "server", env = "ASP_SERVER", value_name = "SERVER")]
        server_option: Option<String>,
        /// Allow replacing an existing remote file without a base hash.
        /// Without this flag, uploads are create-only unless the caller uses
        /// the hash-guarded PATCH operation.
        #[arg(long, default_value_t = false, conflicts_with = "expected_sha256")]
        force: bool,
        /// SHA-256 of the remote file being replaced. This avoids a blind
        /// overwrite and remains safe if another agent edits the target
        /// while the upload is in flight.
        #[arg(long, value_name = "SHA256", conflicts_with = "force")]
        expected_sha256: Option<String>,
    },
    Patch {
        /// Legacy endpoint positional argument. With `--server` or
        /// `ASP_SERVER`, pass `LOCAL REMOTE`.
        server: Option<String>,
        local: Option<PathBuf>,
        remote: Option<String>,
        #[arg(long = "server", env = "ASP_SERVER", value_name = "SERVER")]
        server_option: Option<String>,
    },
    /// Fetch tree, git state, searches, and selected files in one semantic request.
    Inspect {
        #[arg(env = "ASP_SERVER")]
        server: String,
        #[arg(long, default_value = ".")]
        workspace: String,
        /// Skip the repository-wide tree walk when only Git/search/file data
        /// is needed. The default keeps the historical complete inspection.
        #[arg(long, default_value_t = false)]
        no_tree: bool,
        /// Skip the Git status subprocess when only tree/search/file data is
        /// needed. The default preserves the complete inspection contract.
        #[arg(long, default_value_t = false)]
        no_git_status: bool,
        #[arg(long = "search")]
        searches: Vec<String>,
        #[arg(long = "read")]
        read_paths: Vec<String>,
        #[arg(long)]
        diff: bool,
        #[arg(long, default_value_t = 0)]
        recent_commits: u16,
    },
    /// Run the reproducible coding-agent benchmark over one connection.
    AgentWorkload {
        #[arg(env = "ASP_SERVER")]
        server: String,
        #[arg(long, default_value = "agent-fixture-asp")]
        workspace: String,
        #[arg(long, default_value_t = 30)]
        disconnect_seconds: u64,
        /// Use EXEC_SUMMARY for command output so the benchmark can measure
        /// the semantic contract that keeps large logs durable without
        /// retransmitting every byte to the agent.
        #[arg(long, default_value_t = false)]
        summary_output: bool,
        /// Maximum stdout/stderr tail returned by summary-mode commands.
        #[arg(long, default_value_t = 8 * 1024, requires = "summary_output")]
        tail_bytes: u32,
        /// Log payload used by the workload benchmark. `compressible` keeps
        /// the historical all-zero fixture; the other modes expose realistic
        /// compression and semantic-output behavior.
        #[arg(long, default_value = "compressible", value_parser = ["compressible", "incompressible", "mixed"])]
        log_mode: String,
    },
}

#[derive(Debug, Serialize)]
struct WorkloadMetrics {
    experiment: &'static str,
    system: &'static str,
    application_round_trips: u64,
    transport_connections: u64,
    quic_tx_datagrams: u64,
    quic_tx_bytes: u64,
    quic_rx_datagrams: u64,
    quic_rx_bytes: u64,
    quic_lost_packets: u64,
    quic_congestion_events: u64,
    quic_last_path_rtt_us: u64,
    application_payload_bytes: u64,
    wall_ms: f64,
    network_blocked_ms: f64,
    recovery_ms: f64,
    disconnect_seconds: u64,
    summary_output: bool,
    summary_tail_bytes: u32,
    log_mode: String,
    resumed_events: usize,
    persistent_process_observed: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct WorkloadTransportStats {
    tx_datagrams: u64,
    tx_bytes: u64,
    rx_datagrams: u64,
    rx_bytes: u64,
    lost_packets: u64,
    congestion_events: u64,
    last_path_rtt_us: u64,
}

impl WorkloadTransportStats {
    fn observe(&mut self, connection: &Connection) {
        let stats = connection.stats();
        self.tx_datagrams = self.tx_datagrams.saturating_add(stats.udp_tx.datagrams);
        self.tx_bytes = self.tx_bytes.saturating_add(stats.udp_tx.bytes);
        self.rx_datagrams = self.rx_datagrams.saturating_add(stats.udp_rx.datagrams);
        self.rx_bytes = self.rx_bytes.saturating_add(stats.udp_rx.bytes);
        self.lost_packets = self.lost_packets.saturating_add(stats.path.lost_packets);
        self.congestion_events = self
            .congestion_events
            .saturating_add(stats.path.congestion_events);
        self.last_path_rtt_us = stats.path.rtt.as_micros().min(u64::MAX as u128) as u64;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SavedSession {
    session_id: Uuid,
    /// The cursor for the local durable event consumer.  This advances only
    /// after a full RESUME or an event subscription has delivered a complete
    /// backlog boundary; filtered EXEC/FILE result streams must not move it
    /// past unrelated journal entries.
    last_event_id: u64,
}

impl SavedSession {
    fn advance_event_cursor(&mut self, event_id: u64) {
        self.last_event_id = self.last_event_id.max(event_id);
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SavedSessions {
    servers: HashMap<String, SavedSession>,
    /// Per-consumer cursors are kept separate from the legacy per-server
    /// entry. The field is optional so older cursor files remain readable.
    #[serde(default)]
    consumers: HashMap<String, HashMap<String, SavedSession>>,
}

#[derive(Clone, Copy)]
struct RetryContext<'a> {
    server: &'a str,
    cert: &'a Path,
    auth_token: Option<&'a str>,
    session_file: &'a Path,
    /// Keep the QUIC endpoint alive for long-lived bearer-token attachments
    /// (agent, event, shell, and forwarding) so a reconnect can reuse its UDP
    /// socket and TLS session cache. Client certificate adapters intentionally
    /// leave this unset so rotated key material is reloaded on reconnect.
    /// One-shot callers also leave it unset; their existing bounded
    /// endpoint-drain path is unchanged.
    endpoint: Option<&'a Endpoint>,
}

#[derive(Clone)]
struct ClientIdentity {
    cert: PathBuf,
    key: PathBuf,
}

static CLIENT_IDENTITY: OnceLock<Option<ClientIdentity>> = OnceLock::new();
static TLS_SERVER_NAME: OnceLock<String> = OnceLock::new();
// Keep the token-file path so reconnects can observe an operator's atomic
// rotation. Explicit `--auth-token` remains intentionally static.
static AUTH_TOKEN_FILE: OnceLock<Option<PathBuf>> = OnceLock::new();
// A process-level cursor namespace avoids threading another argument through
// every one-shot and adapter call. The value only affects the local cursor
// store; the wire-level session identity remains the durable UUID.
static CLIENT_CONSUMER_ID: OnceLock<Option<String>> = OnceLock::new();
// A process-wide preference keeps every reconnect on the same negotiated PTY
// state shape. Rich snapshots remain the default; callers that prioritize
// replaceable-state bytes can opt into plain row deltas explicitly.
static CLIENT_PREFER_PTY_DELTA: OnceLock<bool> = OnceLock::new();

/// Quinn connections do not carry application protocol state, so retain the
/// negotiated framing mode in a small bounded side table keyed by Quinn's
/// stable connection ID. This lets existing call sites keep using `&Connection`
/// while a v17 client falls back to the tested v16 plain framing during a
/// rolling deployment. Entries are bounded because stable IDs are unique for
/// the lifetime of an endpoint, not for the lifetime of the process.
const CLIENT_FRAME_VERSION_CACHE_LIMIT: usize = 1024;
static CLIENT_FRAME_VERSIONS: OnceLock<Mutex<HashMap<usize, u16>>> = OnceLock::new();
const CLIENT_FEATURE_CACHE_LIMIT: usize = 1024;
static CLIENT_CONNECTION_FEATURES: OnceLock<Mutex<HashMap<usize, HashSet<String>>>> =
    OnceLock::new();

// Cache only the endpoint-level result of a successful protocol handshake.
// This avoids paying a failed v17 probe on every reconnect to a known v16
// daemon, while the short TTL lets a rolling upgrade converge back to v17
// without requiring a client restart.
const CLIENT_SERVER_VERSION_CACHE_LIMIT: usize = 256;
const CLIENT_SERVER_VERSION_CACHE_TTL: Duration = Duration::from_secs(300);
static CLIENT_SERVER_VERSIONS: OnceLock<Mutex<HashMap<String, (u16, Instant)>>> = OnceLock::new();

// Quinn's `Connection::close` queues a CONNECTION_CLOSE frame, but a short
// lived CLI process can terminate its Tokio runtime before the endpoint has
// flushed that packet. Retain the endpoint by connection stable ID so explicit
// one-shot shutdowns can wait for drain; long-lived callers are cleaned up by
// the monitor spawned at connection establishment. This is bounded like the
// protocol-version/feature side tables and holds only endpoint handles, not
// application payloads.
const CLIENT_ENDPOINT_CACHE_LIMIT: usize = 1024;
static CLIENT_ENDPOINTS: OnceLock<Mutex<HashMap<usize, Endpoint>>> = OnceLock::new();

fn client_frame_versions() -> &'static Mutex<HashMap<usize, u16>> {
    CLIENT_FRAME_VERSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn client_connection_features() -> &'static Mutex<HashMap<usize, HashSet<String>>> {
    CLIENT_CONNECTION_FEATURES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn prefer_pty_state_delta() -> bool {
    CLIENT_PREFER_PTY_DELTA.get().copied().unwrap_or(false)
}

fn requested_features(prefer_delta: bool) -> Vec<String> {
    SUPPORTED_FEATURES
        .iter()
        .chain(OPTIONAL_FEATURES.iter())
        .filter(|feature| {
            !prefer_delta || !matches!(**feature, "pty_rich_state" | "pty_rich_compression")
        })
        .map(|value| (*value).to_owned())
        .collect()
}

fn remember_connection_features(connection: &Connection, features: &[String]) {
    let mut cache = client_connection_features()
        .lock()
        .expect("client feature cache poisoned");
    if !cache.contains_key(&connection.stable_id())
        && cache.len() >= CLIENT_FEATURE_CACHE_LIMIT
        && let Some(oldest) = cache.keys().next().copied()
    {
        cache.remove(&oldest);
    }
    cache.insert(
        connection.stable_id(),
        features.iter().cloned().collect::<HashSet<_>>(),
    );
}

fn connection_supports_feature(connection: &Connection, feature: &str) -> bool {
    client_connection_features()
        .lock()
        .expect("client feature cache poisoned")
        .get(&connection.stable_id())
        .is_some_and(|features| features.contains(feature))
}

fn negotiated_connection_features(connection: &Connection) -> Vec<String> {
    let mut features = client_connection_features()
        .lock()
        .expect("client feature cache poisoned")
        .get(&connection.stable_id())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect::<Vec<_>>();
    features.sort();
    features
}

fn frame_version_for_connection(connection: &Connection) -> u16 {
    client_frame_versions()
        .lock()
        .expect("client frame-version cache poisoned")
        .get(&connection.stable_id())
        .copied()
        .unwrap_or(PROTOCOL_VERSION)
}

fn remember_frame_version(connection: &Connection, version: u16) {
    let mut versions = client_frame_versions()
        .lock()
        .expect("client frame-version cache poisoned");
    if !versions.contains_key(&connection.stable_id())
        && versions.len() >= CLIENT_FRAME_VERSION_CACHE_LIMIT
        && let Some(oldest) = versions.keys().next().copied()
    {
        versions.remove(&oldest);
    }
    versions.insert(connection.stable_id(), version);
}

fn client_server_versions() -> &'static Mutex<HashMap<String, (u16, Instant)>> {
    CLIENT_SERVER_VERSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn client_endpoints() -> &'static Mutex<HashMap<usize, Endpoint>> {
    CLIENT_ENDPOINTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_client_endpoint(connection: &Connection, endpoint: &Endpoint) {
    let mut endpoints = client_endpoints()
        .lock()
        .expect("client endpoint cache poisoned");
    if !endpoints.contains_key(&connection.stable_id())
        && endpoints.len() >= CLIENT_ENDPOINT_CACHE_LIMIT
        && let Some(oldest) = endpoints.keys().next().copied()
    {
        endpoints.remove(&oldest);
    }
    endpoints.insert(connection.stable_id(), endpoint.clone());
}

fn take_client_endpoint(connection: &Connection) -> Option<Endpoint> {
    client_endpoints()
        .lock()
        .expect("client endpoint cache poisoned")
        .remove(&connection.stable_id())
}

fn clone_client_endpoint(connection: &Connection) -> Option<Endpoint> {
    // A client-certificate endpoint captures the certificate and private key
    // in its rustls config.  Do not reuse it across reconnects: rebuilding the
    // endpoint lets a deployment rotate those files without keeping a revoked
    // identity alive in a long-lived adapter. Bearer-token adapters have no
    // endpoint-bound credential and can safely use the socket/session cache.
    if CLIENT_IDENTITY
        .get()
        .and_then(|identity| identity.as_ref())
        .is_some()
    {
        return None;
    }
    client_endpoints()
        .lock()
        .expect("client endpoint cache poisoned")
        .get(&connection.stable_id())
        .cloned()
}

fn forget_client_endpoint(connection: &Connection) {
    let _ = client_endpoints()
        .lock()
        .expect("client endpoint cache poisoned")
        .remove(&connection.stable_id());
}

/// Close a connection and wait for Quinn to drain its endpoint when the
/// caller is about to exit a short-lived process. Without this wait the peer
/// may not observe CONNECTION_CLOSE until the full QUIC idle timeout.
async fn close_connection_and_wait(connection: &Connection, reason: &[u8]) {
    connection.close(0_u32.into(), reason);
    if let Some(endpoint) = take_client_endpoint(connection) {
        wait_for_endpoint_idle(endpoint).await;
    }
}

async fn wait_for_endpoint_idle(endpoint: Endpoint) {
    let _ = tokio::time::timeout(CLIENT_CLOSE_DRAIN_TIMEOUT, endpoint.wait_idle()).await;
}

/// Drain every endpoint that is still registered when a short-lived client
/// exits through an error path.  Most one-shot commands close explicitly on
/// success, but a failed response, malformed peer, or local checkpoint error
/// can otherwise drop the Tokio runtime before Quinn has sent
/// `CONNECTION_CLOSE`.  That leaves the server's connection/principal leases
/// occupied until its idle timeout and makes a burst of failed agent calls
/// look like capacity exhaustion.  Long-lived commands (`agent`, `shell`,
/// `events`, and `forward`) normally return only after their own reconnect or
/// detach logic; this is a final bounded safety net for anything they leave
/// behind.
async fn close_registered_client_endpoints() {
    let endpoints = {
        let mut registered = client_endpoints()
            .lock()
            .expect("client endpoint cache poisoned");
        registered
            .drain()
            .map(|(_, endpoint)| endpoint)
            .collect::<Vec<_>>()
    };
    if endpoints.is_empty() {
        return;
    }
    for endpoint in &endpoints {
        endpoint.close(0_u32.into(), b"client error");
    }
    for endpoint in endpoints {
        wait_for_endpoint_idle(endpoint).await;
    }
}

fn cached_server_version(server: &str) -> Option<u16> {
    let mut versions = client_server_versions()
        .lock()
        .expect("client server-version cache poisoned");
    let (version, checked_at) = versions.get(server).copied()?;
    if checked_at.elapsed() <= CLIENT_SERVER_VERSION_CACHE_TTL {
        Some(version)
    } else {
        versions.remove(server);
        None
    }
}

fn remember_server_version(server: &str, version: u16) {
    let mut versions = client_server_versions()
        .lock()
        .expect("client server-version cache poisoned");
    if !versions.contains_key(server)
        && versions.len() >= CLIENT_SERVER_VERSION_CACHE_LIMIT
        && let Some(oldest) = versions
            .iter()
            .min_by_key(|(_, (_, checked_at))| *checked_at)
            .map(|(server, _)| server.clone())
    {
        versions.remove(&oldest);
    }
    versions.insert(server.to_owned(), (version, Instant::now()));
}

fn forget_server_version(server: &str) {
    client_server_versions()
        .lock()
        .expect("client server-version cache poisoned")
        .remove(server);
}

struct UploadTransfer<'a> {
    session_id: Uuid,
    request_id: Uuid,
    remote: &'a str,
    local: &'a Path,
    total_size: u64,
    digest: &'a str,
    expected_sha256: Option<&'a str>,
    allow_blind: bool,
    resume: bool,
}

#[derive(Debug, Default)]
struct ExecOutputCursor {
    stdout_seen: u64,
    stderr_seen: u64,
}

const STREAM_FILE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const FILE_STREAM_CHUNK_BYTES: usize = 64 * 1024;
const ARTIFACT_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const AGENT_ARTIFACT_MAX_BYTES: u64 = 16 * 1024 * 1024;
const PROCESS_OUTPUT_MAX_BYTES: u64 = 512 * 1024 * 1024;
const PROCESS_LOG_RANGE_MAX_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
const MAX_CONNECT_TIMEOUT_MS: u64 = 120_000;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS);
const REQUEST_STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
// A close packet should normally drain immediately on a healthy path, but an
// erroring short-lived client must never hang while trying to be polite to a
// peer that has already disappeared. Once this bound expires, dropping the
// endpoint still releases local resources; the server's idle/liveness policy
// remains the backstop for a close packet that could not be delivered.
const CLIENT_CLOSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
// One-shot operations must not wait forever if a peer accepts a request and
// then stops producing its response. Long-lived PTY/event/port streams do
// not use this deadline; they are governed by QUIC liveness and reconnect
// loops instead. The five-minute cap matches the server's maximum encoded
// response-frame write deadline and keeps request retries idempotent.
const REQUEST_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
// A laptop sleep or Wi-Fi-to-cellular handoff can outlast a handful of
// refused-dial retries. Keep ordinary request recovery bounded, but long
// enough to cover the documented 30-second persistence scenario with margin.
// Interactive shells and event subscriptions use their own cancellable,
// indefinite reconnect loops.
const DEFAULT_RECONNECT_TIMEOUT_MS: u64 = 90_000;
const MAX_RECONNECT_TIMEOUT_MS: u64 = 10 * 60 * 1_000;
const RECONNECT_RETRY_WINDOW: Duration = Duration::from_millis(DEFAULT_RECONNECT_TIMEOUT_MS);
const REQUEST_FRAME_MIN_RATE_BYTES_PER_SECOND: u64 = 64 * 1024;
const REQUEST_FRAME_MIN_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_FRAME_MAX_WRITE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
// Stream transfers are emitted in 64 KiB application chunks.  Plain chunks
// are cheap to decode, and dispatching one blocking task per chunk can let
// Quinn's receive assembler accumulate more out-of-order packet ranges than
// its bounded safety limit under a fast local sender.  Keep compressed frames
// and genuinely large plain frames off the reactor, while decoding ordinary
// plain chunks inline.
const PLAIN_RESPONSE_CODEC_OFFLOAD_MIN_BYTES: usize = 256 * 1024;
// A resumed upload begins after the server has scanned its durable prefix.
// Keep a small pacing interval between continuation bursts so a fast local
// sender cannot fill Quinn's bounded out-of-order assembler before the server
// task returns to its receive loop. This applies only after a resume; fresh
// uploads retain the normal QUIC pacing and flow-control path.
const RESUMED_UPLOAD_PACING: Duration = Duration::from_millis(10);
// A short continuation burst amortizes the scheduler handoff while keeping
// the post-restart sender from outrunning Quinn's receive assembler. This is
// deliberately conservative: four 64 KiB chunks are only 256 KiB, then the
// same ten-millisecond pause used by the proven conservative path applies.
const RESUMED_UPLOAD_PACING_CHUNKS: usize = 4;
// Postcard adds a small variant/length prefix around body-bearing requests.
// Start offloading slightly before the generic codec threshold so a payload
// just under 64 KiB cannot serialize synchronously merely because its framing
// metadata pushes the encoded message over that boundary.
const REQUEST_BODY_OFFLOAD_THRESHOLD: usize = FRAME_CODEC_OFFLOAD_MIN_BYTES - 256;
// Consumer ACKs are retention heartbeats, not delivery acknowledgements. A
// short coalescing window prevents a high-rate process-output subscription
// from creating one reliable ACK stream and journal write per event while
// keeping the durable cursor lag comfortably below the seven-day lease.
const EVENT_ACK_COALESCE: Duration = Duration::from_millis(25);
static CLIENT_CONNECT_TIMEOUT: OnceLock<Duration> = OnceLock::new();
static CLIENT_RECONNECT_TIMEOUT: OnceLock<Duration> = OnceLock::new();
const MAX_CLIENT_METADATA_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CLIENT_CERT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CLIENT_CERT_BUNDLE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CLIENT_CERT_BUNDLE_ENTRIES: usize = 8;
const MAX_RESOLVED_ADDRESSES: usize = 16;
// Race a small number of DNS results on one Quinn endpoint. A stale
// dual-stack route must not consume the whole connection budget before the
// usable family is attempted, while the bound avoids creating a burst of
// handshakes for a hostname with many addresses.
const MAX_PARALLEL_CONNECT_ATTEMPTS: usize = 4;
const CONNECT_ATTEMPT_STAGGER: Duration = Duration::from_millis(50);
const LEGACY_SESSION_FILE: &str = ".asp-session";

fn connect_attempt_delay(index: usize) -> Duration {
    let delay_ms = CONNECT_ATTEMPT_STAGGER
        .as_millis()
        .saturating_mul(index as u128)
        .min(u64::MAX as u128) as u64;
    Duration::from_millis(delay_ms)
}

fn client_connect_timeout() -> Duration {
    *CLIENT_CONNECT_TIMEOUT.get_or_init(|| CONNECT_TIMEOUT)
}

fn client_reconnect_timeout() -> Duration {
    *CLIENT_RECONNECT_TIMEOUT.get_or_init(|| RECONNECT_RETRY_WINDOW)
}

fn validate_connect_timeout_ms(milliseconds: u64) -> Result<Duration> {
    if milliseconds == 0 || milliseconds > MAX_CONNECT_TIMEOUT_MS {
        bail!("--connect-timeout-ms must be between 1 and {MAX_CONNECT_TIMEOUT_MS} milliseconds");
    }
    Ok(Duration::from_millis(milliseconds))
}

fn validate_reconnect_timeout_ms(milliseconds: u64) -> Result<Duration> {
    if milliseconds == 0 || milliseconds > MAX_RECONNECT_TIMEOUT_MS {
        bail!(
            "--reconnect-timeout-ms must be between 1 and {MAX_RECONNECT_TIMEOUT_MS} milliseconds"
        );
    }
    Ok(Duration::from_millis(milliseconds))
}

#[derive(Debug, Clone)]
struct StreamFileInfo {
    total_size: u64,
    offset: u64,
    length: u64,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct DownloadCheckpoint {
    remote: String,
    total_size: u64,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ArtifactDownloadCheckpoint {
    server: String,
    session_id: Uuid,
    artifact_id: String,
    total_size: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct ArtifactUploadCheckpoint {
    server: String,
    session_id: Uuid,
    artifact_id: String,
    total_size: u64,
    request_id: Uuid,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Clone)]
struct ArtifactStreamInfo {
    artifact_id: String,
    total_size: u64,
    offset: u64,
    length: u64,
    name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct UploadCheckpoint {
    server: String,
    session_id: Uuid,
    remote: String,
    total_size: u64,
    sha256: String,
    request_id: Uuid,
    /// Optional remote base hash for an optimistic streamed replacement.
    /// Defaults preserve checkpoints written by pre-v11 clients.
    #[serde(default)]
    expected_sha256: Option<String>,
    /// Explicit blind replacement permission, persisted so a resumed upload
    /// cannot silently change its conflict semantics.
    #[serde(default)]
    allow_blind: bool,
}

struct ResumeResult {
    snapshot: asp_protocol::SessionSnapshot,
    events: Vec<asp_protocol::Event>,
    compacted: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let result = run().await;
    if result.is_err() {
        close_registered_client_endpoints().await;
    }
    result
}

async fn run() -> Result<()> {
    let mut args = Args::parse();
    let connect_timeout = validate_connect_timeout_ms(args.connect_timeout_ms)?;
    let reconnect_timeout = validate_reconnect_timeout_ms(args.reconnect_timeout_ms)?;
    CLIENT_CONNECT_TIMEOUT
        .set(connect_timeout)
        .map_err(|_| anyhow!("client connect timeout was initialized more than once"))?;
    CLIENT_RECONNECT_TIMEOUT
        .set(reconnect_timeout)
        .map_err(|_| anyhow!("client reconnect timeout was initialized more than once"))?;
    // Keep the historical workspace-local cursor when it already exists, but
    // put new session metadata in the per-user state directory. A client
    // cursor is not workspace content; writing it beside a checked-out tree
    // needlessly invalidates the server's watcher/index and can perturb the
    // semantic-cache fast path.
    args.session_file = resolve_session_file(args.session_file);
    validate_consumer_id(args.consumer_id.as_deref())?;
    CLIENT_CONSUMER_ID
        .set(args.consumer_id.clone())
        .map_err(|_| anyhow!("consumer identity was initialized more than once"))?;
    CLIENT_PREFER_PTY_DELTA
        .set(args.prefer_pty_delta)
        .map_err(|_| anyhow!("PTY state preference was initialized more than once"))?;
    validate_server_name(&args.server_name)?;
    TLS_SERVER_NAME
        .set(args.server_name.clone())
        .map_err(|_| anyhow!("TLS server name was initialized more than once"))?;
    let auth_token = load_auth_token(&args.auth_token_file, args.auth_token.as_deref())?;
    let client_identity = match (&args.client_cert, &args.client_key) {
        (Some(cert), Some(key)) => {
            reject_symlink(cert)?;
            reject_symlink(key)?;
            Some(ClientIdentity {
                cert: cert.clone(),
                key: key.clone(),
            })
        }
        (None, None) => None,
        _ => bail!("--client-cert and --client-key must be supplied together"),
    };
    CLIENT_IDENTITY
        .set(client_identity)
        .map_err(|_| anyhow!("client identity was initialized more than once"))?;
    AUTH_TOKEN_FILE
        .set(
            args.auth_token
                .is_none()
                .then_some(args.auth_token_file.clone()),
        )
        .map_err(|_| anyhow!("auth token source was initialized more than once"))?;
    match args.command {
        Command::Doctor {
            server,
            strict,
            ready_url,
        } => {
            let conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let response = one(&conn, Request::Health).await?;
            if let Response::Health {
                protocol_version,
                auth_required,
                pty_backend,
                ..
            } = &response
            {
                if strict {
                    validate_strict_doctor(*protocol_version, *auth_required, pty_backend)?;
                }
                if let Some(ready_url) = ready_url.as_deref() {
                    check_ready_endpoint(ready_url).await?;
                }
                println!("{}", serde_json::to_string_pretty(&response)?)
            } else {
                match response {
                    Response::Error { code, message } => bail!("{code}: {message}"),
                    other => return unexpected(other),
                }
            }
            close_connection_and_wait(&conn, b"doctor complete").await;
        }
        Command::Connect { server, new } => {
            if new {
                let response = open_session_with_retry(
                    &server,
                    &args.cert,
                    auth_token.as_deref(),
                    Uuid::new_v4(),
                )
                .await?;
                let Response::SessionOpened {
                    session_id,
                    event_id,
                } = response
                else {
                    return unexpected(response);
                };
                save(
                    &args.session_file,
                    &server,
                    SavedSession {
                        session_id,
                        last_event_id: event_id,
                    },
                )?;
                println!("{session_id}");
            } else {
                // Reusing a saved UUID is deliberately a local operation:
                // ordinary connect must not replay the session journal just
                // to prove that a daily reconnect is possible. The explicit
                // `resume` command performs journal/snapshot recovery when a
                // caller needs missed events. ensure_session holds the local
                // cursor lock across first-session creation, preventing two
                // invocations from creating orphan sessions concurrently.
                let mut conn =
                    connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
                let state = ensure_session(
                    &mut conn,
                    &server,
                    &args.cert,
                    auth_token.as_deref(),
                    &args.session_file,
                )
                .await?;
                println!("{}", state.session_id);
                close_connection_and_wait(&conn, b"connect complete").await;
            }
        }
        Command::Resume {
            server,
            session_id,
            after_event_id,
        } => {
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = if let Some(session_id) = session_id {
                if session_id.is_nil() {
                    bail!("--session-id must not be the nil UUID");
                }
                // An explicit session ID is the recovery escape hatch for a
                // new client host that has lost its local cursor file. Save
                // the selected identity after a successful resume so all
                // normal commands can continue using it. The server remains
                // authoritative for ownership and existence checks.
                SavedSession {
                    session_id,
                    last_event_id: after_event_id,
                }
            } else {
                if after_event_id != 0 {
                    bail!("--after-event-id requires --session-id");
                }
                require_saved(&args.session_file, &server)?
            };
            resume_with_retry(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
                &mut state,
                true,
            )
            .await?;
            close_connection_and_wait(&conn, b"resume complete").await;
        }
        Command::Events {
            server,
            after_event_id,
            process_id,
            no_output,
        } => {
            subscribe_events(
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
                after_event_id,
                process_id,
                !no_output,
            )
            .await?;
        }
        Command::Logs {
            server,
            process_id,
            server_option,
            stream,
            offset,
            length,
            tail,
        } => {
            let (server, process_id) = resolve_process_target(server, process_id, server_option)?;
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let stream = parse_output_stream(&stream)?;
            if tail.is_some() && (offset != 0 || length.is_some()) {
                bail!("--tail cannot be combined with --offset or --length");
            }
            let (offset, length) = if let Some(tail_bytes) = tail {
                let retry = RetryContext {
                    server: &server,
                    cert: &args.cert,
                    auth_token: auth_token.as_deref(),
                    session_file: &args.session_file,
                    endpoint: None,
                };
                resolve_process_log_tail(
                    &mut conn, retry, &mut state, process_id, &stream, tail_bytes,
                )
                .await?
            } else {
                (offset, length)
            };
            get_process_output(&conn, state.session_id, process_id, stream, offset, length).await?;
            close_connection_and_wait(&conn, b"logs complete").await;
        }
        Command::Status {
            server,
            process_id,
            server_option,
        } => {
            let (server, process_id) = resolve_process_target(server, process_id, server_option)?;
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let session_id = state.session_id;
            let response = retry_request(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &mut state,
                Request::ProcessState {
                    session_id,
                    process_id,
                },
            )
            .await?;
            match response {
                Response::ProcessState { snapshot } => {
                    println!("{}", serde_json::to_string_pretty(&snapshot)?);
                }
                Response::Error { code, message } => bail!("{code}: {message}"),
                other => return unexpected(other),
            }
            close_connection_and_wait(&conn, b"status complete").await;
        }
        Command::ArtifactGet {
            server,
            artifact_id,
            local,
            server_option,
            offset,
            length,
        } => {
            let (server, artifact_id, local) =
                resolve_artifact_get_args(server, artifact_id, local, server_option)?;
            if !valid_sha256(&artifact_id) {
                bail!("artifact_id must be a 64-character hexadecimal SHA-256 digest");
            }
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let retry = RetryContext {
                server: &server,
                cert: &args.cert,
                auth_token: auth_token.as_deref(),
                session_file: &args.session_file,
                endpoint: None,
            };
            let info = download_artifact_with_retry(
                &mut conn,
                &retry,
                &mut state,
                &artifact_id,
                &local,
                offset,
                length,
            )
            .await?;
            println!(
                "{} bytes, sha256 {}, name={}",
                info.total_size,
                info.artifact_id,
                info.name.as_deref().unwrap_or("")
            );
            close_connection_and_wait(&conn, b"artifact get complete").await;
        }
        Command::ArtifactPut {
            server,
            local,
            server_option,
            name,
        } => {
            let (server, local) =
                resolve_single_path_args(server, local, server_option, "artifact local path")?;
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let retry = RetryContext {
                server: &server,
                cert: &args.cert,
                auth_token: auth_token.as_deref(),
                session_file: &args.session_file,
                endpoint: None,
            };
            let response =
                upload_artifact_with_retry(&mut conn, &retry, &mut state, &local, name).await?;
            match response {
                Response::ArtifactStored {
                    artifact_id,
                    total_size,
                    name,
                    event_id,
                } => {
                    println!(
                        "{} bytes, sha256 {}, name={}, event_id={}",
                        total_size,
                        artifact_id,
                        name.as_deref().unwrap_or(""),
                        event_id
                    );
                }
                Response::Error { code, message } => bail!("{code}: {message}"),
                other => return unexpected(other),
            }
            close_connection_and_wait(&conn, b"artifact put complete").await;
        }
        Command::Exec {
            server,
            summary,
            tail_bytes,
            command_text,
            command,
        } => {
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let command = join_command_option(command, command_text)?;
            let request_id = Uuid::new_v4();
            let mut output_cursor = ExecOutputCursor::default();
            let mut summary_printed = false;
            let mut attempt = 0_u8;
            let code = loop {
                match exec_once(
                    &conn,
                    &mut state,
                    request_id,
                    &command,
                    &mut output_cursor,
                    summary,
                    tail_bytes,
                    &mut summary_printed,
                    true,
                )
                .await
                {
                    Ok(code) => break code,
                    Err(error) if attempt < 2 && is_server_busy_error(&error) => {
                        attempt += 1;
                        let delay_ms = 100_u64.saturating_mul(1_u64 << attempt);
                        eprintln!(
                            "ASP EXEC temporarily busy; retrying in {delay_ms}ms (attempt {attempt}/2)"
                        );
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                    Err(error) if attempt < 2 && retryable_connection_error(&error) => {
                        attempt += 1;
                        eprintln!(
                            "EXEC connection interrupted; reconnecting (attempt {attempt}/2)"
                        );
                        let endpoint = clone_client_endpoint(&conn);
                        conn = reconnect_on_endpoint(
                            endpoint,
                            &server,
                            &args.cert,
                            auth_token.as_deref(),
                            &mut state,
                        )
                        .await?;
                    }
                    Err(error) => return Err(error),
                }
            };
            close_connection_and_wait(&conn, b"exec complete").await;
            if code.unwrap_or(1) != 0 {
                std::process::exit(code.unwrap_or(1));
            }
        }
        Command::Batch {
            server,
            commands,
            summary,
            tail_bytes,
            parallel,
            stdin,
        } => {
            if commands.is_empty() && !stdin {
                bail!("batch requires at least one --command or --stdin");
            }
            if !(1..=32).contains(&parallel) {
                bail!("batch --parallel must be between 1 and 32");
            }
            if parallel > 1 && (stdin || !summary || tail_bytes != 0) {
                bail!(
                    "batch --parallel > 1 requires command arguments plus --summary --tail-bytes 0"
                );
            }
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let reusable_endpoint = clone_client_endpoint(&conn);
            let retry = RetryContext {
                server: &server,
                cert: &args.cert,
                auth_token: auth_token.as_deref(),
                session_file: &args.session_file,
                // Batch is a warm attachment. Keep the endpoint alive for a
                // reconnect so a daemon restart or path flap can reuse the
                // same UDP socket and TLS session cache instead of paying a
                // fresh endpoint setup on the next command.
                endpoint: reusable_endpoint.as_ref(),
            };
            if stdin {
                let stdin = tokio::io::stdin();
                let mut lines = tokio::io::BufReader::new(stdin).lines();
                let mut index = 0_u64;
                while let Some(command) = lines.next_line().await? {
                    if command.is_empty() {
                        continue;
                    }
                    let code = run_batch_command(
                        &mut conn, retry, &mut state, &command, summary, tail_bytes, true,
                    )
                    .await?;
                    eprintln!("ASP_BATCH_RESULT {index} {}", code.unwrap_or(1));
                    index = index.saturating_add(1);
                    if code.unwrap_or(1) != 0 {
                        close_connection_and_wait(&conn, b"batch failed").await;
                        std::process::exit(code.unwrap_or(1));
                    }
                }
            } else if parallel == 1 {
                for command in commands {
                    let code = run_batch_command(
                        &mut conn, retry, &mut state, &command, summary, tail_bytes, true,
                    )
                    .await?;
                    if code.unwrap_or(1) != 0 {
                        close_connection_and_wait(&conn, b"batch failed").await;
                        std::process::exit(code.unwrap_or(1));
                    }
                }
            } else {
                let code =
                    run_parallel_batch_commands(&conn, retry, &mut state, &commands, parallel)
                        .await?;
                if code != 0 {
                    close_connection_and_wait(&conn, b"parallel batch failed").await;
                    std::process::exit(code);
                }
            }
            close_connection_and_wait(&conn, b"batch complete").await;
        }
        Command::Agent { server } => {
            agent_loop(
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
        }
        Command::AgentListen {
            server,
            socket,
            server_option,
        } => {
            let (server, socket) =
                resolve_single_path_args(server, socket, server_option, "agent socket path")?;
            #[cfg(unix)]
            run_agent_listener(
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
                &socket,
            )
            .await?;
            #[cfg(not(unix))]
            {
                let _ = (server, socket);
                bail!("agent-listen requires Unix-domain socket support on this platform");
            }
        }
        Command::AgentConnect { socket } => {
            #[cfg(unix)]
            agent_connect(&socket).await?;
            #[cfg(not(unix))]
            {
                let _ = socket;
                bail!("agent-connect requires Unix-domain socket support on this platform");
            }
        }
        Command::Spawn {
            server,
            command_text,
            command,
        } => {
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let session_id = state.session_id;
            let response = retry_request(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &mut state,
                Request::Spawn {
                    session_id,
                    request_id: Uuid::new_v4(),
                    command: join_command_option(command, command_text)?,
                },
            )
            .await?;
            let Response::ProcessAccepted {
                process_id,
                event_id: _,
            } = response
            else {
                return unexpected(response);
            };
            println!("{process_id}");
            close_connection_and_wait(&conn, b"spawn complete").await;
        }
        Command::Signal {
            server,
            process_id,
            server_option,
            signal,
        } => {
            let (server, process_id) = resolve_process_target(server, process_id, server_option)?;
            let signal = parse_signal(&signal)?;
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let session_id = state.session_id;
            let response = retry_request(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &mut state,
                Request::Signal {
                    session_id,
                    request_id: Uuid::new_v4(),
                    process_id,
                    signal,
                },
            )
            .await?;
            match response {
                Response::Acked {
                    through_event_id: _,
                } => {
                    println!("signal {signal} delivered to {process_id}");
                }
                Response::Error { code, message } => bail!("{code}: {message}"),
                other => return unexpected(other),
            }
            close_connection_and_wait(&conn, b"signal complete").await;
        }
        Command::Shell { server } => {
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            // A shell is a long-lived attachment just like an event
            // subscriber. Reuse the endpoint that owns the initial QUIC
            // connection when bearer-token auth is in use so a reconnect can
            // keep its UDP socket and TLS session cache. mTLS intentionally
            // opts out so rotated client key material is reloaded.
            let reusable_endpoint = clone_client_endpoint(&conn);
            let retry = RetryContext {
                server: &server,
                cert: &args.cert,
                auth_token: auth_token.as_deref(),
                session_file: &args.session_file,
                endpoint: reusable_endpoint.as_ref(),
            };
            shell(conn, retry, &mut state).await?;
        }
        Command::Forward {
            server,
            listen,
            target,
            allow_non_loopback,
        } => {
            validate_forward_listener(listen, allow_non_loopback)?;
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            // Keep the forwarding listener's endpoint alive across control
            // reconnects. Existing TCP flows remain bound to their original
            // QUIC streams; new flows use the resumed connection and avoid a
            // fresh UDP socket/TLS setup after a network flap.
            let reusable_endpoint = clone_client_endpoint(&conn);
            let retry = RetryContext {
                server: &server,
                cert: &args.cert,
                auth_token: auth_token.as_deref(),
                session_file: &args.session_file,
                endpoint: reusable_endpoint.as_ref(),
            };
            forward(&mut conn, retry, &mut state, listen, target).await?;
        }
        Command::Get {
            server,
            remote,
            local,
            server_option,
        } => {
            let (server, remote, local) = resolve_get_args(server, remote, local, server_option)?;
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let retry = RetryContext {
                server: &server,
                cert: &args.cert,
                auth_token: auth_token.as_deref(),
                session_file: &args.session_file,
                endpoint: None,
            };
            let info =
                download_file_with_retry(&mut conn, &retry, &mut state, &remote, &local).await?;
            println!("{} bytes, sha256 {}", info.total_size, info.sha256);
            close_connection_and_wait(&conn, b"get complete").await;
        }
        Command::Put {
            server,
            local,
            remote,
            server_option,
            force,
            expected_sha256,
        } => {
            let (server, local, remote) = resolve_put_args(server, local, remote, server_option)?;
            if let Some(expected) = expected_sha256.as_deref()
                && !valid_sha256(expected)
            {
                bail!("--expected-sha256 must be a 64-character hexadecimal SHA-256 digest");
            }
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let retry = RetryContext {
                server: &server,
                cert: &args.cert,
                auth_token: auth_token.as_deref(),
                session_file: &args.session_file,
                endpoint: None,
            };
            let response = upload_file_with_retry(
                &mut conn,
                &retry,
                &mut state,
                &remote,
                &local,
                expected_sha256.as_deref(),
                force,
            )
            .await?;
            print_file_response(response)?;
            close_connection_and_wait(&conn, b"put complete").await;
        }
        Command::Patch {
            server,
            local,
            remote,
            server_option,
        } => {
            let (server, local, remote) = resolve_put_args(server, local, remote, server_option)?;
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let session_id = state.session_id;
            let old = retry_request(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &mut state,
                Request::FileGet {
                    session_id,
                    path: remote.clone(),
                },
            )
            .await?;
            let Response::FileData {
                version,
                data: old,
                sha256,
                ..
            } = old
            else {
                return unexpected(old);
            };
            let new = tokio::fs::read(&local).await?;
            if old == new {
                // A patch is a mutation, so sending an empty replacement here
                // would still create a new workspace version and event for a
                // byte-for-byte no-op. The GET already established the
                // current version and digest; return that stable result
                // without opening a second request stream.
                println!("{remote} version={version} sha256={sha256} (unchanged)");
                close_connection_and_wait(&conn, b"patch unchanged").await;
                return Ok(());
            }
            let prefix = common_prefix(&old, &new);
            let suffix = common_suffix(&old[prefix..], &new[prefix..]);
            let replacement = new[prefix..new.len() - suffix].to_vec();
            let ranges = derive_file_patch_ranges(&old, &new);
            let ranges_enabled = connection_supports_feature(&conn, "file_patch_ranges");
            // Prefix/suffix patches are excellent for localized source edits,
            // but a broad rewrite can make the replacement larger than the
            // complete file. Pick the smaller body before framing so the
            // normal v17 codec does not have to recover from a bad choice.
            // The allowance covers the patch's fixed hashes/lengths and
            // leaves a margin for framing overhead; the optimistic hash guard
            // is retained for both choices.
            let request_id = Uuid::new_v4();
            let response = if ranges_enabled && should_use_file_patch_ranges(&ranges, new.len()) {
                retry_request(
                    &mut conn,
                    &server,
                    &args.cert,
                    auth_token.as_deref(),
                    &mut state,
                    Request::FilePatchRanges {
                        session_id,
                        request_id,
                        path: remote,
                        expected_sha256: sha256,
                        ranges,
                    },
                )
                .await?
            } else if should_use_file_patch(replacement.len(), new.len()) {
                retry_request(
                    &mut conn,
                    &server,
                    &args.cert,
                    auth_token.as_deref(),
                    &mut state,
                    Request::FilePatch {
                        session_id,
                        request_id,
                        path: remote,
                        expected_sha256: sha256,
                        prefix_len: prefix as u64,
                        suffix_len: suffix as u64,
                        replacement,
                    },
                )
                .await?
            } else {
                retry_request(
                    &mut conn,
                    &server,
                    &args.cert,
                    auth_token.as_deref(),
                    &mut state,
                    Request::FilePut {
                        session_id,
                        request_id,
                        path: remote,
                        expected_sha256: Some(sha256),
                        allow_blind: false,
                        data: new,
                    },
                )
                .await?
            };
            print_file_response(response)?;
            close_connection_and_wait(&conn, b"patch complete").await;
        }
        Command::Inspect {
            server,
            workspace,
            no_tree,
            no_git_status,
            searches,
            read_paths,
            diff,
            recent_commits,
        } => {
            let mut conn = connect_with_retry(&server, &args.cert, auth_token.as_deref()).await?;
            let mut state = ensure_session(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &args.session_file,
            )
            .await?;
            let session_id = state.session_id;
            let response = retry_request(
                &mut conn,
                &server,
                &args.cert,
                auth_token.as_deref(),
                &mut state,
                Request::WorkspaceState {
                    session_id,
                    workspace,
                    include_tree: !no_tree,
                    include_git_status: !no_git_status,
                    include_diff: diff,
                    recent_commits,
                    searches,
                    read_paths,
                    known_tree_version: None,
                    known_state_digest: None,
                },
            )
            .await?;
            match response {
                Response::WorkspaceState { .. } => {
                    println!("{}", serde_json::to_string_pretty(&response)?)
                }
                Response::Error { code, message } => bail!("{code}: {message}"),
                other => return unexpected(other),
            }
            close_connection_and_wait(&conn, b"inspect complete").await;
        }
        Command::AgentWorkload {
            server,
            workspace,
            disconnect_seconds,
            summary_output,
            tail_bytes,
            log_mode,
        } => {
            let metrics = agent_workload(
                &server,
                &args.cert,
                &args.session_file,
                &workspace,
                disconnect_seconds,
                summary_output,
                tail_bytes,
                &log_mode,
                auth_token.as_deref(),
            )
            .await?;
            println!("{}", serde_json::to_string(&metrics)?);
        }
    }
    Ok(())
}

async fn forward(
    connection: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    listen: SocketAddr,
    target: SocketAddr,
) -> Result<()> {
    let listener = TcpListener::bind(listen).await?;
    eprintln!(
        "ASP forwarding {} -> {} (Ctrl-C to stop)",
        listener.local_addr()?,
        target
    );
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                let connection = connection.clone();
                let session_id = state.session_id;
                tokio::spawn(async move {
                    if let Err(error) = proxy_forward(connection, session_id, target, stream).await {
                        eprintln!("ASP forward {peer} failed: {error}");
                    }
                });
            }
            _ = connection.closed() => {
                eprintln!("ASP forward transport lost; reconnecting");
                *connection = reconnect_forward(retry, state).await?;
                eprintln!("ASP forward transport resumed");
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break;
            }
        }
    }
    connection.close(0_u32.into(), b"forward stopped");
    Ok(())
}

fn validate_forward_listener(listen: SocketAddr, allow_non_loopback: bool) -> Result<()> {
    if !listen.ip().is_loopback() && !allow_non_loopback {
        bail!(
            "refusing non-loopback local forward listener {listen}; pass --allow-non-loopback only with an intentional firewall policy"
        );
    }
    Ok(())
}

async fn reconnect_forward(
    retry: RetryContext<'_>,
    state: &mut SavedSession,
) -> Result<Connection> {
    let mut attempt = 0_u32;
    loop {
        match reconnect_with_retry(retry, state).await {
            Ok(connection) => return Ok(connection),
            Err(error) if retryable_connection_error(&error) => {
                let delay_ms = (100_u64.saturating_mul(1_u64 << attempt.min(6))).min(5_000);
                eprintln!("ASP forward reconnect failed; retrying in {delay_ms}ms: {error}");
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                attempt = attempt.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
    }
}

async fn proxy_forward(
    connection: Connection,
    session_id: Uuid,
    target: SocketAddr,
    stream: TcpStream,
) -> Result<()> {
    let (mut send, mut recv) = open_bi_with_timeout(&connection).await?;
    write_request_frame(
        &connection,
        &mut send,
        Request::PortOpen {
            session_id,
            host: target.ip().to_string(),
            port: target.port(),
        },
    )
    .await?;
    match read_response_frame(&connection, &mut recv).await? {
        Some(Response::PortReady { port, .. }) if port == target.port() => {}
        Some(Response::Error { code, message }) => bail!("{code}: {message}"),
        Some(other) => return unexpected(other),
        None => bail!("server closed port-forward stream before PORT_READY"),
    }

    let (mut tcp_read, mut tcp_write) = stream.into_split();
    let tcp_to_quic = async {
        let copied = tokio::io::copy(&mut tcp_read, &mut send).await?;
        send.shutdown().await?;
        Ok::<u64, std::io::Error>(copied)
    };
    let quic_to_tcp = async {
        let copied = tokio::io::copy(&mut recv, &mut tcp_write).await?;
        tcp_write.shutdown().await?;
        Ok::<u64, std::io::Error>(copied)
    };
    tokio::try_join!(tcp_to_quic, quic_to_tcp)?;
    Ok(())
}

async fn timed_one(
    conn: &Connection,
    request: Request,
    blocked: &mut Duration,
    gates: &mut u64,
) -> Result<Response> {
    let started = Instant::now();
    let response = one(conn, request).await;
    *blocked += started.elapsed();
    *gates += 1;
    response
}

#[allow(clippy::too_many_arguments)]
async fn workload_exec(
    conn: &Connection,
    session_id: Uuid,
    command: String,
    blocked: &mut Duration,
    gates: &mut u64,
    response_bytes: &mut u64,
    summary_output: bool,
    summary_tail_bytes: u32,
) -> Result<()> {
    let started = Instant::now();
    *gates += 1;
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    let request_id = Uuid::new_v4();
    let request = if summary_output {
        Request::ExecSummary {
            session_id,
            request_id,
            command,
            tail_bytes: summary_tail_bytes,
        }
    } else {
        Request::Exec {
            session_id,
            request_id,
            command,
        }
    };
    write_request_frame(conn, &mut send, request).await?;
    send.finish()?;
    let mut summary_seen = false;
    loop {
        let Some(response) = read_response_frame(conn, &mut recv).await? else {
            bail!("EXEC response stream closed before PROCESS_EXITED");
        };
        match response {
            Response::ProcessAccepted { event_id: _, .. } => {}
            Response::ProcessOutput {
                event_id: _, data, ..
            } => {
                *response_bytes += data.len() as u64;
            }
            Response::ProcessSummary {
                event_id: _,
                stdout_bytes: _,
                stderr_bytes: _,
                stdout_tail,
                stderr_tail,
                stdout_truncated: _,
                stderr_truncated: _,
                ..
            } => {
                if !summary_output {
                    bail!("received EXEC_SUMMARY response for a full-output request");
                }
                if !summary_seen {
                    *response_bytes = (*response_bytes)
                        .saturating_add(stdout_tail.len() as u64)
                        .saturating_add(stderr_tail.len() as u64);
                    summary_seen = true;
                }
            }
            Response::ProcessExited {
                event_id: _, code, ..
            } => {
                if code.unwrap_or(1) != 0 {
                    bail!("workload command exited with {code:?}");
                }
                break;
            }
            Response::Error { code, message } => bail!("{code}: {message}"),
            other => return unexpected(other),
        }
    }
    *blocked += started.elapsed();
    Ok(())
}

/// Execute one request stream. The request id is intentionally supplied by the
/// caller so a retry is idempotent: the server maps it to the original
/// process and replays the durable result instead of starting a second shell.
#[allow(clippy::too_many_arguments)]
async fn exec_once(
    conn: &Connection,
    state: &mut SavedSession,
    request_id: Uuid,
    command: &str,
    output_cursor: &mut ExecOutputCursor,
    summary: bool,
    tail_bytes: u32,
    summary_printed: &mut bool,
    emit_summary_output: bool,
) -> Result<Option<i32>> {
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    let request = if summary {
        Request::ExecSummary {
            session_id: state.session_id,
            request_id,
            command: command.to_owned(),
            tail_bytes,
        }
    } else {
        Request::Exec {
            session_id: state.session_id,
            request_id,
            command: command.to_owned(),
        }
    };
    write_request_frame(conn, &mut send, request).await?;
    send.finish()?;

    loop {
        let Some(response) = read_response_frame(conn, &mut recv).await? else {
            bail!("EXEC response stream closed before PROCESS_EXITED");
        };
        match response {
            Response::ProcessAccepted { event_id: _, .. } => {}
            Response::ProcessOutput {
                event_id: _,
                stream,
                offset,
                data,
                ..
            } => {
                let seen = match &stream {
                    OutputStream::Stdout => &mut output_cursor.stdout_seen,
                    OutputStream::Stderr => &mut output_cursor.stderr_seen,
                };
                write_unseen(stream, offset, &data, seen)?;
            }
            Response::ProcessSummary {
                event_id: _,
                stdout_bytes,
                stderr_bytes,
                stdout_tail,
                stderr_tail,
                stdout_truncated,
                stderr_truncated,
                ..
            } => {
                if !*summary_printed {
                    if emit_summary_output {
                        eprintln!(
                            "ASP summary: stdout_bytes={stdout_bytes} stderr_bytes={stderr_bytes} stdout_truncated={stdout_truncated} stderr_truncated={stderr_truncated}"
                        );
                        if !stdout_tail.is_empty() {
                            std::io::stdout().write_all(&stdout_tail)?;
                            std::io::stdout().flush()?;
                        }
                        if !stderr_tail.is_empty() {
                            std::io::stderr().write_all(&stderr_tail)?;
                            std::io::stderr().flush()?;
                        }
                    }
                    *summary_printed = true;
                }
            }
            Response::ProcessExited {
                event_id: _, code, ..
            } => {
                return Ok(code);
            }
            Response::Error { code, message } => bail!("{code}: {message}"),
            other => return unexpected(other),
        }
    }
}

/// Run one ordinary batch command while retaining one authenticated
/// connection. A stable request ID makes a transport retry safe: the server
/// either returns the existing process or starts it exactly once.
async fn run_batch_command(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    command: &str,
    summary: bool,
    tail_bytes: u32,
    emit_summary_output: bool,
) -> Result<Option<i32>> {
    let request_id = Uuid::new_v4();
    let mut output_cursor = ExecOutputCursor::default();
    let mut summary_printed = false;
    let mut attempt = 0_u8;
    loop {
        match exec_once(
            conn,
            state,
            request_id,
            command,
            &mut output_cursor,
            summary,
            tail_bytes,
            &mut summary_printed,
            emit_summary_output,
        )
        .await
        {
            Ok(code) => return Ok(code),
            Err(error) if attempt < 2 && retryable_connection_error(&error) => {
                attempt += 1;
                eprintln!("BATCH command interrupted; reconnecting (attempt {attempt}/2)");
                *conn = reconnect_with_retry(retry, state).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Execute independent, summary-only batch commands concurrently while
/// retaining one authenticated QUIC transport. Each command gets its own
/// request ID and cloned session cursor, so a reconnect/retry remains
/// idempotent without serializing unrelated work behind the slowest command.
/// The caller deliberately opts into zero-tail summaries: retaining or
/// interleaving arbitrary command output would turn this throughput path into
/// an unbounded local buffer and would make ordering ambiguous.
async fn run_parallel_batch_commands(
    conn: &Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    commands: &[String],
    parallel: usize,
) -> Result<i32> {
    let mut first_error: Option<anyhow::Error> = None;
    let mut first_nonzero: Option<(usize, i32)> = None;
    let mut next = 0_usize;
    while next < commands.len() {
        let end = next.saturating_add(parallel).min(commands.len());
        let mut tasks = tokio::task::JoinSet::new();
        for (index, command) in commands[next..end].iter().enumerate() {
            let index = next + index;
            let command = command.clone();
            let mut task_state = state.clone();
            let mut task_conn = conn.clone();
            let shared_connection_id = conn.stable_id();
            let task_endpoint = retry.endpoint.cloned();
            // `JoinSet` owns `'static` tasks. Move the small retry context
            // values into each task and borrow them only for the duration of
            // that task; no connection/session payload is shared mutably.
            let task_server = retry.server.to_owned();
            let task_cert = retry.cert.to_path_buf();
            let task_auth_token = retry.auth_token.map(str::to_owned);
            let task_session_file = retry.session_file.to_path_buf();
            tasks.spawn(async move {
                let task_retry = RetryContext {
                    server: &task_server,
                    cert: &task_cert,
                    auth_token: task_auth_token.as_deref(),
                    session_file: &task_session_file,
                    endpoint: task_endpoint.as_ref(),
                };
                let result = run_batch_command(
                    &mut task_conn,
                    task_retry,
                    &mut task_state,
                    &command,
                    true,
                    0,
                    false,
                )
                .await;
                // A cloned Quinn `Connection` is the same underlying
                // transport, so closing it unconditionally would tear down
                // sibling commands and the parent batch. Only a retry that
                // replaced the task's handle owns an independent connection;
                // drain that handle here so its endpoint cache entry and
                // principal lease do not linger until the idle timeout.
                if task_conn.stable_id() != shared_connection_id {
                    close_connection_and_wait(&task_conn, b"parallel batch task complete").await;
                }
                let code = result?;
                Ok::<(usize, Option<i32>, u64), anyhow::Error>((
                    index,
                    code,
                    task_state.last_event_id,
                ))
            });
        }

        let mut results = Vec::with_capacity(end - next);
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Ok(result)) => results.push(result),
                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(anyhow!("parallel batch task failed: {error}"));
                    }
                }
            }
        }
        results.sort_unstable_by_key(|(index, _, _)| *index);
        for (index, code, _last_event_id) in results {
            eprintln!("ASP_BATCH_RESULT {index} {}", code.unwrap_or(1));
            if code.unwrap_or(1) != 0 && first_nonzero.is_none() {
                first_nonzero = Some((index, code.unwrap_or(1)));
            }
        }
        next = end;
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(first_nonzero.map(|(_, code)| code).unwrap_or(0))
}

const AGENT_INPUT_MAX_BYTES: usize = 128 * 1024;
const AGENT_ADAPTER_VERSION: u16 = 1;

/// Read one JSONL adapter line without allowing a peer on stdin to force an
/// unbounded allocation by withholding a newline. Oversized lines are drained
/// through their newline so the next request remains aligned.
async fn read_bounded_agent_line<R>(
    reader: &mut R,
    line: &mut Vec<u8>,
    maximum: usize,
) -> Result<Option<bool>>
where
    R: AsyncBufRead + Unpin,
{
    line.clear();
    let mut oversized = false;
    loop {
        let (consumed, newline) = {
            let buffer = reader.fill_buf().await?;
            if buffer.is_empty() {
                if line.is_empty() && !oversized {
                    return Ok(None);
                }
                return Ok(Some(oversized));
            }
            let newline = buffer.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(buffer.len(), |position| position + 1);
            if !oversized {
                let available = maximum.saturating_sub(line.len());
                if consumed > available {
                    oversized = true;
                } else {
                    line.extend_from_slice(&buffer[..consumed]);
                }
            }
            (consumed, newline.is_some())
        };
        reader.consume(consumed);
        if newline {
            return Ok(Some(oversized));
        }
    }
}

fn default_include_tree() -> bool {
    true
}

fn default_include_git_status() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentRequest {
    #[serde(default)]
    id: Option<String>,
    op: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    request_id: Option<Uuid>,
    #[serde(default)]
    summary: bool,
    /// Maximum output tail for `exec_summary`, or final log bytes for
    /// `logs`/`process_logs`. Log tails are resolved at a process-state
    /// snapshot boundary and never exceed the bounded range limit.
    #[serde(default)]
    tail_bytes: Option<u32>,
    /// Workspace-relative semantic inspection options. These fields are
    /// intentionally optional so the adapter remains pleasant for small
    /// EXEC-only callers while allowing an agent to reuse one warm QUIC
    /// session for tree/git/search/read requests.
    #[serde(default)]
    workspace: Option<String>,
    /// Include the bounded repository tree in an inspection. Omitted fields
    /// preserve the complete-inspection behavior for existing adapters.
    #[serde(default = "default_include_tree")]
    include_tree: bool,
    /// Include the bounded Git status result in an inspection. Omitted fields
    /// preserve the complete-inspection behavior for existing adapters.
    #[serde(default = "default_include_git_status")]
    include_git_status: bool,
    #[serde(default)]
    searches: Vec<String>,
    #[serde(default)]
    read_paths: Vec<String>,
    #[serde(default)]
    diff: bool,
    #[serde(default)]
    recent_commits: u16,
    /// Optional validator returned by a prior inspect. If it matches the
    /// server's stable workspace index, the tree is omitted from the next
    /// response while git/files are still evaluated.
    #[serde(default)]
    known_tree_version: Option<WorkspaceVersion>,
    /// Optional digest validator from a prior identical workspace query.
    /// The adapter only sends it when it also has the corresponding cached
    /// semantic result, so a compact server response can always be expanded
    /// back into a complete JSONL response for the agent.
    #[serde(default)]
    known_state_digest: Option<String>,
    /// File operation fields. Binary bodies are base64 because the adapter
    /// itself is newline-delimited JSON; the binary ASP stream remains the
    /// authoritative wire format.
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    expected_sha256: Option<String>,
    /// Explicitly permit replacing an existing file without a base hash.
    /// Without this flag, FILE_PUT is create-only when no hash is supplied.
    #[serde(default)]
    force: bool,
    #[serde(default)]
    prefix_len: Option<u64>,
    #[serde(default)]
    suffix_len: Option<u64>,
    #[serde(default)]
    replacement_base64: Option<String>,
    #[serde(default)]
    ranges: Vec<AgentFilePatchRange>,
    #[serde(default)]
    data_base64: Option<String>,
    /// Immutable artifact fields. Artifact bodies use `data_base64` for the
    /// bounded JSONL adapter; larger objects should use the CLI stream path.
    #[serde(default)]
    artifact_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    /// Detached process signal fields.
    #[serde(default)]
    process_id: Option<Uuid>,
    #[serde(default)]
    signal: Option<String>,
    /// Durable process-log range fields.
    #[serde(default)]
    stream: Option<String>,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    length: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFilePatchRange {
    offset: u64,
    remove_len: u64,
    replacement_base64: String,
}

const AGENT_WORKSPACE_CACHE_MAX_ENTRIES: usize = 16;
const AGENT_WORKSPACE_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WorkspaceQueryKey {
    workspace: String,
    include_tree: bool,
    include_git_status: bool,
    include_diff: bool,
    recent_commits: u16,
    searches: Vec<String>,
    read_paths: Vec<String>,
}

impl WorkspaceQueryKey {
    fn new(
        workspace: &str,
        include_tree: bool,
        include_git_status: bool,
        include_diff: bool,
        recent_commits: u16,
        searches: &[String],
        read_paths: &[String],
    ) -> Self {
        Self {
            workspace: workspace.to_owned(),
            include_tree,
            include_git_status,
            include_diff,
            recent_commits,
            searches: searches.to_vec(),
            read_paths: read_paths.to_vec(),
        }
    }
}

#[derive(Clone)]
struct CachedWorkspaceState {
    digest: String,
    tree_version: Option<WorkspaceVersion>,
    tree: Vec<WorkspaceTreeEntry>,
    git_status: Option<String>,
    diff: Option<String>,
    recent_commits: Vec<String>,
    search_hits: Vec<WorkspaceSearchHit>,
    files: Vec<WorkspaceFile>,
    bytes: usize,
}

#[derive(Default)]
struct AgentWorkspaceCache {
    entries: HashMap<WorkspaceQueryKey, CachedWorkspaceState>,
    bytes: usize,
}

impl AgentWorkspaceCache {
    fn get(&self, key: &WorkspaceQueryKey) -> Option<&CachedWorkspaceState> {
        self.entries.get(key)
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn insert(&mut self, key: WorkspaceQueryKey, mut state: CachedWorkspaceState) {
        state.bytes = workspace_cache_bytes(&state);
        if state.bytes > AGENT_WORKSPACE_CACHE_MAX_BYTES {
            self.entries.remove(&key);
            return;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(previous.bytes);
        }
        while self.entries.len() >= AGENT_WORKSPACE_CACHE_MAX_ENTRIES
            || self.bytes.saturating_add(state.bytes) > AGENT_WORKSPACE_CACHE_MAX_BYTES
        {
            let Some(oldest_key) = self.entries.keys().next().cloned() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest_key) {
                self.bytes = self.bytes.saturating_sub(evicted.bytes);
            }
        }
        self.bytes = self.bytes.saturating_add(state.bytes);
        self.entries.insert(key, state);
    }
}

/// A resume can advance a connection past changes made by another adapter.
/// Workspace validators and complete-result payloads are local hints, not
/// durable session state, so both must be discarded whenever a pooled
/// connection replays a newer cursor.
fn invalidate_workspace_caches(
    workspace_versions: &mut HashMap<String, WorkspaceVersion>,
    workspace_cache: &mut AgentWorkspaceCache,
) {
    workspace_versions.clear();
    workspace_cache.clear();
}

/// The local supervisor endpoint can keep a small number of authenticated
/// QUIC connections warm between short-lived `agent-connect` invocations. A
/// connection is reusable only while its durable cursor still names the same
/// session; an explicit `asp connect` or cursor deletion therefore cannot
/// accidentally inherit an old transport.
#[cfg(unix)]
const AGENT_CONNECTION_POOL_LIMIT: usize = 4;
#[cfg(unix)]
const AGENT_CONNECTION_POOL_WAIT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const AGENT_LISTENER_CLIENT_LIMIT: usize = 32;
#[cfg(unix)]
const AGENT_LISTENER_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

struct AgentConnectionState {
    conn: Connection,
    state: SavedSession,
    workspace_versions: HashMap<String, WorkspaceVersion>,
    workspace_cache: AgentWorkspaceCache,
}

#[cfg(unix)]
struct AgentConnectionPool {
    idle: Mutex<Vec<AgentConnectionState>>,
    slots: Arc<Semaphore>,
}

/// Ensure an adapter connection is closed if request handling aborts before
/// it can return the connection to the warm pool. The adapter body contains
/// many fallible I/O/codec operations; relying on ordinary `Drop` alone would
/// leave the process-global endpoint registry holding a live handle after an
/// error, delaying server lease release until QUIC idle timeout. The guard is
/// disarmed only when ownership is explicitly transferred to the pool/caller.
struct AgentConnectionAbortGuard {
    connection: Option<Connection>,
}

impl AgentConnectionAbortGuard {
    fn new(connection: Connection) -> Self {
        Self {
            connection: Some(connection),
        }
    }

    fn disarm(&mut self) {
        self.connection = None;
    }
}

impl Drop for AgentConnectionAbortGuard {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        connection.close(0_u32.into(), b"agent adapter aborted");
        forget_client_endpoint(&connection);
    }
}

/// Close a connection that will not be returned to the warm adapter pool.
/// Quinn connections are cheap handles around a shared transport, but the
/// endpoint registry is process-global so simply dropping the handle would
/// leave a stale endpoint (and, on the server, a principal lease) until the
/// QUIC idle timeout. Keep this path separate from normal shared-connection
/// shutdown: callers must only pass a connection they own, never a clone of
/// the parent batch/adapter transport.
async fn discard_agent_connection(connection: &Connection, reason: &[u8]) {
    connection.close(0_u32.into(), reason);
    if let Some(endpoint) = take_client_endpoint(connection) {
        wait_for_endpoint_idle(endpoint).await;
    }
}

#[cfg(unix)]
impl AgentConnectionPool {
    fn new() -> Self {
        Self {
            idle: Mutex::new(Vec::with_capacity(AGENT_CONNECTION_POOL_LIMIT)),
            slots: Arc::new(Semaphore::new(AGENT_CONNECTION_POOL_LIMIT)),
        }
    }

    async fn checkout(
        &self,
        server: &str,
        session_file: &Path,
    ) -> Result<(OwnedSemaphorePermit, Option<AgentConnectionState>)> {
        let permit = tokio::time::timeout(
            AGENT_CONNECTION_POOL_WAIT,
            self.slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| anyhow!("agent connection pool is busy"))?
        .map_err(|_| anyhow!("agent connection pool is closed"))?;

        let candidate = self.idle.lock().expect("agent pool mutex poisoned").pop();
        let Some(mut candidate) = candidate else {
            return Ok((permit, None));
        };
        let saved = match saved_session(session_file, server) {
            Ok(saved) => saved,
            Err(error) => {
                discard_agent_connection(&candidate.conn, b"agent pool cursor read failed").await;
                return Err(error);
            }
        };
        let Some(saved) = saved.filter(|saved| {
            candidate.conn.close_reason().is_none()
                && saved.session_id == candidate.state.session_id
        }) else {
            discard_agent_connection(&candidate.conn, b"agent pool cursor changed").await;
            return Ok((permit, None));
        };

        // A second adapter can advance the shared durable cursor while this
        // connection is idle. Merely taking the maximum here would make the
        // pooled client claim it had consumed those events without ever
        // reconstructing the corresponding snapshot. Refresh only when the
        // persisted cursor is newer; the common sequential path remains a
        // zero-RTT pool checkout, while concurrent adapters retain the same
        // event-sourcing semantics as a cold reconnect.
        if saved.last_event_id > candidate.state.last_event_id {
            let mut refreshed = saved.clone();
            if let Err(error) =
                resume(&candidate.conn, server, session_file, &mut refreshed, false).await
            {
                discard_agent_connection(&candidate.conn, b"agent pool cursor refresh failed")
                    .await;
                eprintln!("discarding pooled ASP connection after cursor refresh failed: {error}");
                return Ok((permit, None));
            }
            candidate.state.last_event_id = refreshed.last_event_id;
            // The replay may include FILE_CHANGED/PROCESS_* events produced
            // by another adapter while this connection was idle.  A cursor
            // refresh reconstructs durable session state, but it cannot
            // update this adapter's in-memory workspace result cache.  Drop
            // both cache layers so the next semantic query observes the
            // refreshed workspace instead of returning a stale digest hit.
            invalidate_workspace_caches(
                &mut candidate.workspace_versions,
                &mut candidate.workspace_cache,
            );
        } else {
            candidate.state.last_event_id = candidate.state.last_event_id.max(saved.last_event_id);
        }
        Ok((permit, Some(candidate)))
    }

    async fn checkin(&self, permit: OwnedSemaphorePermit, state: Option<AgentConnectionState>) {
        let mut discard = None;
        if let Some(state) = state {
            if state.conn.close_reason().is_none() {
                let mut idle = self.idle.lock().expect("agent pool mutex poisoned");
                if idle.len() < AGENT_CONNECTION_POOL_LIMIT {
                    idle.push(state);
                } else {
                    discard = Some(state);
                }
            } else {
                discard = Some(state);
            }
        }
        drop(permit);
        if let Some(state) = discard {
            discard_agent_connection(&state.conn, b"agent pool discard").await;
        }
    }
}

fn workspace_cache_bytes(state: &CachedWorkspaceState) -> usize {
    state
        .digest
        .len()
        .saturating_add(
            state
                .tree
                .iter()
                .map(|entry| entry.path.len().saturating_add(std::mem::size_of::<u64>()))
                .sum::<usize>(),
        )
        .saturating_add(
            state
                .git_status
                .as_ref()
                .map_or(0, String::len)
                .saturating_add(state.diff.as_ref().map_or(0, String::len)),
        )
        .saturating_add(state.recent_commits.iter().map(String::len).sum::<usize>())
        .saturating_add(
            state
                .search_hits
                .iter()
                .map(|hit| {
                    hit.query
                        .len()
                        .saturating_add(hit.path.len())
                        .saturating_add(hit.text.len())
                        .saturating_add(std::mem::size_of::<u64>())
                })
                .sum::<usize>(),
        )
        .saturating_add(
            state
                .files
                .iter()
                .map(|file| {
                    file.path
                        .len()
                        .saturating_add(file.sha256.len())
                        .saturating_add(file.data.len())
                })
                .sum::<usize>(),
        )
}

/// A small, intentionally explicit JSONL adapter for coding agents. It keeps
/// the QUIC connection and ASP session alive across many requests, but leaves
/// the wire protocol itself binary and unchanged. Output is base64 because an
/// agent must be able to consume arbitrary bytes without confusing them with
/// JSON framing. Each process-output event carries its absolute stream offset,
/// so a caller can persist a cursor and de-duplicate a replay after loss.
const AGENT_OUTPUT_QUEUE_CAPACITY: usize = 256;
const AGENT_OUTPUT_MEMORY_BYTES: usize = 64 * 1024 * 1024;
const AGENT_OUTPUT_MEMORY_UNIT_BYTES: usize = 16 * 1024;

struct AgentOutputMessage {
    data: Vec<u8>,
    // Holding this permit until the writer has consumed the line keeps the
    // bounded queue's memory accounting honest even when the local consumer
    // is slower than a remote process producing output.
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
struct AgentOutput {
    sender: mpsc::Sender<AgentOutputMessage>,
    memory: Arc<Semaphore>,
}

tokio::task_local! {
    static AGENT_OUTPUT: AgentOutput;
}

impl AgentOutput {
    fn enqueue(&self, data: Vec<u8>) -> Result<()> {
        let units = data.len().max(1).div_ceil(AGENT_OUTPUT_MEMORY_UNIT_BYTES);
        let units = u32::try_from(units)
            .map_err(|_| anyhow!("agent output line is too large to account"))?;
        let permit = self
            .memory
            .clone()
            .try_acquire_many_owned(units)
            .map_err(|_| anyhow!("agent output queue memory is exhausted"))?;
        self.sender
            .try_send(AgentOutputMessage {
                data,
                _permit: permit,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    anyhow!("agent output queue is full; consumer must read responses")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    anyhow!("agent output writer closed")
                }
            })
    }
}

async fn agent_loop(
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    session_file: &Path,
) -> Result<()> {
    let stdin = tokio::io::stdin();
    agent_loop_io(
        server,
        cert,
        auth_token,
        session_file,
        tokio::io::BufReader::new(stdin),
        tokio::io::stdout(),
    )
    .await
}

async fn agent_loop_io<R, W>(
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    session_file: &Path,
    reader: R,
    writer: W,
) -> Result<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    agent_loop_io_with_state(
        server,
        cert,
        auth_token,
        session_file,
        reader,
        writer,
        None,
        false,
    )
    .await
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
async fn agent_loop_io_with_state<R, W>(
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    session_file: &Path,
    reader: R,
    writer: W,
    initial: Option<AgentConnectionState>,
    reuse_connection: bool,
) -> Result<Option<AgentConnectionState>>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (sender, mut receiver) = mpsc::channel(AGENT_OUTPUT_QUEUE_CAPACITY);
    let output = AgentOutput {
        sender: sender.clone(),
        memory: Arc::new(Semaphore::new(
            AGENT_OUTPUT_MEMORY_BYTES.div_ceil(AGENT_OUTPUT_MEMORY_UNIT_BYTES),
        )),
    };
    let writer_task = tokio::spawn(async move {
        let mut writer = tokio::io::BufWriter::new(writer);
        while let Some(message) = receiver.recv().await {
            writer.write_all(&message.data).await?;
            // JSONL callers use each line as a response boundary. Flush after
            // every line to retain the adapter's low-latency contract; the
            // queue still moves blocking pipe/socket writes off the request
            // handling task and keeps memory bounded.
            writer.flush().await?;
            drop(message._permit);
        }
        writer.shutdown().await?;
        Ok::<(), std::io::Error>(())
    });
    let result = AGENT_OUTPUT
        .scope(
            output,
            agent_loop_body(
                server,
                cert,
                auth_token,
                session_file,
                reader,
                initial,
                reuse_connection,
            ),
        )
        .await;
    // The sender owned by `output` is dropped when the task-local scope ends;
    // drop our explicit clone before waiting for the writer to drain.
    drop(sender);
    let writer_result = match writer_task.await {
        Ok(result) => result.map_err(|error| anyhow!("agent output writer failed: {error}")),
        Err(error) => Err(anyhow!("agent output writer task failed: {error}")),
    };
    let state = result?;
    if let Err(error) = writer_result {
        // `agent_loop_body` has already disarmed its abort guard when it
        // returned a reusable state. If the local JSONL writer then fails,
        // reclaim that state here instead of dropping a live QUIC handle and
        // leaving the endpoint registry/server lease behind.
        if let Some(state) = state.as_ref() {
            discard_agent_connection(&state.conn, b"agent output writer failed").await;
        }
        return Err(error);
    }
    Ok(state)
}

async fn agent_loop_body<R>(
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    session_file: &Path,
    mut reader: R,
    initial: Option<AgentConnectionState>,
    reuse_connection: bool,
) -> Result<Option<AgentConnectionState>>
where
    R: AsyncBufRead + Unpin,
{
    let AgentConnectionState {
        mut conn,
        mut state,
        mut workspace_versions,
        mut workspace_cache,
    } = match initial {
        Some(initial) => initial,
        None => {
            let mut conn = connect_with_retry(server, cert, auth_token).await?;
            let state = ensure_session(&mut conn, server, cert, auth_token, session_file).await?;
            AgentConnectionState {
                conn,
                state,
                workspace_versions: HashMap::new(),
                workspace_cache: AgentWorkspaceCache::default(),
            }
        }
    };
    // Keep a separate handle so any `?`/early error in the adapter body
    // closes the transport and removes its endpoint registry entry. On the
    // normal reusable path ownership is transferred to the returned state
    // only after this guard is disarmed below.
    let mut connection_abort_guard = AgentConnectionAbortGuard::new(conn.clone());
    emit_agent(serde_json::json!({
        "type": "ready",
        "adapter_version": AGENT_ADAPTER_VERSION,
        "session_id": state.session_id,
        "protocol_version": frame_version_for_connection(&conn),
        "features": negotiated_connection_features(&conn),
    }))?;
    // Keep the endpoint that owns this attachment alive for the lifetime of
    // the JSONL adapter. Reconnects can then reuse its UDP socket and TLS
    // session cache instead of constructing a fresh endpoint for every
    // daemon restart or network flap. If the connection came from an older
    // caller that did not register an endpoint, the retry path safely falls
    // back to the normal one-shot constructor.
    let reusable_endpoint = clone_client_endpoint(&conn);
    let retry = RetryContext {
        server,
        cert,
        auth_token,
        session_file,
        endpoint: reusable_endpoint.as_ref(),
    };
    let mut line = Vec::new();
    loop {
        let Some(oversized) =
            read_bounded_agent_line(&mut reader, &mut line, AGENT_INPUT_MAX_BYTES).await?
        else {
            break;
        };
        if oversized {
            emit_agent(serde_json::json!({
                "type": "error",
                "code": "input_too_large",
                "message": format!("agent JSONL request exceeds {AGENT_INPUT_MAX_BYTES} bytes"),
            }))?;
            continue;
        }
        let line = match std::str::from_utf8(&line) {
            Ok(line) => line.trim_end_matches(['\r', '\n']),
            Err(error) => {
                emit_agent(serde_json::json!({
                    "type": "error",
                    "code": "invalid_utf8",
                    "message": error.to_string(),
                }))?;
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: AgentRequest = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(error) => {
                emit_agent(serde_json::json!({
                    "type": "error",
                    "code": "invalid_json",
                    "message": error.to_string(),
                }))?;
                continue;
            }
        };
        let id = request.id.as_deref();
        match request.op.as_str() {
            "ping" => {
                emit_agent(serde_json::json!({
                    "type": "pong",
                    "id": id,
                    "session_id": state.session_id,
                    "last_event_id": state.last_event_id,
                }))?;
            }
            "close" => {
                emit_agent(serde_json::json!({"type": "closed", "id": id}))?;
                break;
            }
            "exec" | "exec_summary" => {
                let Some(command) = request.command.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_command",
                        "message": "agent exec requests require a command string",
                    }))?;
                    continue;
                };
                let summary = request.op == "exec_summary" || request.summary;
                let tail_bytes = request.tail_bytes.unwrap_or(8 * 1024);
                let request_id = request.request_id.unwrap_or_else(Uuid::new_v4);
                // Invalidate before issuing the command: even if the
                // connection dies before the final exit frame, the remote
                // process may already have changed files or Git metadata.
                workspace_versions.clear();
                workspace_cache.clear();
                if let Err(error) = agent_exec_with_retry(
                    &mut conn, retry, &mut state, id, request_id, command, summary, tail_bytes,
                )
                .await
                {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "exec_failed",
                        "message": error.to_string(),
                    }))?;
                }
            }
            "spawn" => {
                let Some(command) = request.command.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_command",
                        "message": "agent spawn requests require a command string",
                    }))?;
                    continue;
                };
                let request_id = request.request_id.unwrap_or_else(Uuid::new_v4);
                workspace_versions.clear();
                workspace_cache.clear();
                if let Err(error) =
                    agent_spawn_with_retry(&mut conn, retry, &mut state, id, request_id, command)
                        .await
                {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "spawn_failed",
                        "message": error.to_string(),
                    }))?;
                }
            }
            "logs" | "process_logs" => {
                let Some(process_id) = request.process_id else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_process_id",
                        "message": "agent logs requests require process_id",
                    }))?;
                    continue;
                };
                let stream =
                    match parse_output_stream(request.stream.as_deref().unwrap_or("stdout")) {
                        Ok(stream) => stream,
                        Err(error) => {
                            emit_agent(serde_json::json!({
                                "type": "error",
                                "id": id,
                                "code": "invalid_stream",
                                "message": error.to_string(),
                            }))?;
                            continue;
                        }
                    };
                if let Err(error) = agent_logs_with_retry(
                    &mut conn,
                    retry,
                    &mut state,
                    id,
                    process_id,
                    stream,
                    request.offset,
                    request.length,
                    request.tail_bytes.map(u64::from),
                )
                .await
                {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "logs_failed",
                        "message": error.to_string(),
                    }))?;
                }
            }
            "status" | "process_status" => {
                let Some(process_id) = request.process_id else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_process_id",
                        "message": "agent status requests require process_id",
                    }))?;
                    continue;
                };
                match agent_process_state_with_retry(&mut conn, retry, &mut state, id, process_id)
                    .await
                {
                    Ok(()) => {}
                    Err(error) => {
                        emit_agent(serde_json::json!({
                            "type": "error",
                            "id": id,
                            "code": "status_failed",
                            "message": error.to_string(),
                        }))?;
                    }
                }
            }
            "signal" => {
                let Some(process_id) = request.process_id else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_process_id",
                        "message": "agent signal requests require process_id",
                    }))?;
                    continue;
                };
                let signal = match parse_signal(request.signal.as_deref().unwrap_or("TERM")) {
                    Ok(signal) => signal,
                    Err(error) => {
                        emit_agent(serde_json::json!({
                            "type": "error",
                            "id": id,
                            "code": "invalid_signal",
                            "message": error.to_string(),
                        }))?;
                        continue;
                    }
                };
                let request_id = request.request_id.unwrap_or_else(Uuid::new_v4);
                // A signal can run a trap or terminate a process that was
                // writing the workspace. Treat it like EXEC/SPAWN for local
                // semantic-cache purposes; the next inspect will validate a
                // fresh tree/digest instead of trusting a pre-signal hint.
                invalidate_workspace_caches(&mut workspace_versions, &mut workspace_cache);
                if let Err(error) = agent_signal_with_retry(
                    &mut conn, retry, &mut state, id, request_id, process_id, signal,
                )
                .await
                {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "signal_failed",
                        "message": error.to_string(),
                    }))?;
                }
            }
            "inspect" | "workspace_state" => {
                let workspace = request.workspace.as_deref().unwrap_or(".");
                let query_key = WorkspaceQueryKey::new(
                    workspace,
                    request.include_tree,
                    request.include_git_status,
                    request.diff,
                    request.recent_commits,
                    &request.searches,
                    &request.read_paths,
                );
                // A digest is useful only when this adapter can expand the
                // compact response back into the complete semantic result.
                // Ignore caller-supplied validators for uncached queries;
                // this keeps the JSONL contract complete and avoids a hidden
                // second round trip when a client has no local snapshot.
                let known_state_digest = workspace_cache
                    .get(&query_key)
                    .and_then(|cached| {
                        request
                            .known_state_digest
                            .as_deref()
                            .filter(|digest| *digest == cached.digest)
                            .or(Some(cached.digest.as_str()))
                    })
                    .map(str::to_owned);
                let known_tree_version = request
                    .known_tree_version
                    .clone()
                    .or_else(|| workspace_versions.get(workspace).cloned());
                if let Err(error) = agent_workspace_with_retry(
                    &mut conn,
                    retry,
                    &mut state,
                    id,
                    workspace,
                    request.include_tree,
                    request.include_git_status,
                    request.diff,
                    request.recent_commits,
                    request.searches.clone(),
                    request.read_paths.clone(),
                    known_tree_version,
                    known_state_digest,
                    &mut workspace_versions,
                    &mut workspace_cache,
                )
                .await
                {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "workspace_failed",
                        "message": error.to_string(),
                    }))?;
                }
            }
            "artifact_put" => {
                let Some(data_base64) = request.data_base64.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_data",
                        "message": "agent artifact_put requests require data_base64",
                    }))?;
                    continue;
                };
                let data = match BASE64.decode(data_base64) {
                    Ok(data) => data,
                    Err(error) => {
                        emit_agent(serde_json::json!({
                            "type": "error",
                            "id": id,
                            "code": "invalid_base64",
                            "message": format!("artifact_put data_base64 is invalid: {error}"),
                        }))?;
                        continue;
                    }
                };
                if data.len() as u64 > AGENT_ARTIFACT_MAX_BYTES {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "artifact_too_large",
                        "message": format!("JSONL artifact_put is capped at {AGENT_ARTIFACT_MAX_BYTES} bytes; use the CLI stream path for larger objects"),
                    }))?;
                    continue;
                }
                let request_id = request.request_id.unwrap_or_else(Uuid::new_v4);
                if let Err(error) = agent_artifact_put_with_retry(
                    &mut conn,
                    retry,
                    &mut state,
                    id,
                    request_id,
                    data,
                    request.name.clone(),
                )
                .await
                {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "artifact_put_failed",
                        "message": error.to_string(),
                    }))?;
                }
            }
            "artifact_get" => {
                let Some(artifact_id) = request.artifact_id.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_artifact_id",
                        "message": "agent artifact_get requests require artifact_id",
                    }))?;
                    continue;
                };
                if request
                    .length
                    .is_some_and(|length| length > AGENT_ARTIFACT_MAX_BYTES)
                {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "artifact_too_large",
                        "message": format!("JSONL artifact_get is capped at {AGENT_ARTIFACT_MAX_BYTES} bytes; use the CLI stream path for larger objects"),
                    }))?;
                    continue;
                }
                if let Err(error) = agent_artifact_get_with_retry(
                    &mut conn,
                    retry,
                    &mut state,
                    id,
                    artifact_id,
                    request.offset,
                    request.length,
                )
                .await
                {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "artifact_get_failed",
                        "message": error.to_string(),
                    }))?;
                }
            }
            "file_get" => {
                let Some(path) = request.path.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_path",
                        "message": "agent file_get requests require path",
                    }))?;
                    continue;
                };
                if let Err(error) =
                    agent_file_get_with_retry(&mut conn, retry, &mut state, id, path).await
                {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "file_get_failed",
                        "message": error.to_string(),
                    }))?;
                }
            }
            "file_put" => {
                let Some(path) = request.path.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_path",
                        "message": "agent file_put requests require path",
                    }))?;
                    continue;
                };
                let Some(data_base64) = request.data_base64.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_data",
                        "message": "agent file_put requests require data_base64",
                    }))?;
                    continue;
                };
                let data = match BASE64.decode(data_base64) {
                    Ok(data) => data,
                    Err(error) => {
                        emit_agent(serde_json::json!({
                            "type": "error",
                            "id": id,
                            "code": "invalid_base64",
                            "message": format!("file_put data_base64 is invalid: {error}"),
                        }))?;
                        continue;
                    }
                };
                let request_id = request.request_id.unwrap_or_else(Uuid::new_v4);
                // A prior semantic inspection may already contain the exact
                // base bytes identified by expected_sha256. Reuse them to
                // send only the changed middle when the patch is materially
                // smaller; this keeps the normal FILE_PUT contract and
                // request ID while avoiding a second remote FILE_GET.
                let cached_patch = request
                    .expected_sha256
                    .as_deref()
                    .and_then(|expected_sha256| {
                        cached_file_patch_with_ranges(
                            &workspace_cache,
                            path,
                            expected_sha256,
                            &data,
                            connection_supports_feature(&conn, "file_patch_ranges"),
                        )
                    });
                // An agent often writes a file after an edit pipeline has
                // already established that the bytes are unchanged. If the
                // exact hash-matching base is still cached, avoid a network
                // mutation and, more importantly, avoid creating a durable
                // FILE_CHANGED event for a byte-for-byte no-op. A zero-byte
                // FILE_GET_STREAM metadata check still verifies that a
                // concurrent writer did not change the base before we claim
                // success. Preserve the semantic cache because no workspace
                // mutation occurred.
                let result = match cached_patch {
                    Some(CachedFilePatch::Unchanged { sha256 }) => {
                        match file_hash_with_retry(&mut conn, retry, &mut state, path).await {
                            Ok((version, remote_sha256)) if remote_sha256 == sha256 => {
                                emit_agent(serde_json::json!({
                                    "type": "file_unchanged",
                                    "id": id,
                                    "request_id": request_id,
                                    "transfer": "none",
                                    "path": path,
                                    "version": version,
                                    "sha256": sha256,
                                }))?;
                                continue;
                            }
                            Ok((_version, _remote_sha256)) => {
                                workspace_versions.clear();
                                workspace_cache.clear();
                                agent_file_put_with_retry(
                                    &mut conn,
                                    retry,
                                    &mut state,
                                    id,
                                    request_id,
                                    path,
                                    data,
                                    request.expected_sha256.as_deref(),
                                    request.force,
                                )
                                .await
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Some(CachedFilePatch::Patch {
                        prefix_len,
                        suffix_len,
                        replacement,
                    }) => {
                        workspace_versions.clear();
                        workspace_cache.clear();
                        agent_file_patch_with_retry(
                            &mut conn,
                            retry,
                            &mut state,
                            id,
                            request_id,
                            path,
                            request.expected_sha256.as_deref().unwrap_or_default(),
                            prefix_len,
                            suffix_len,
                            replacement,
                        )
                        .await
                    }
                    Some(CachedFilePatch::Ranges { ranges }) => {
                        workspace_versions.clear();
                        workspace_cache.clear();
                        agent_file_patch_ranges_with_retry(
                            &mut conn,
                            retry,
                            &mut state,
                            id,
                            request_id,
                            path,
                            request.expected_sha256.as_deref().unwrap_or_default(),
                            ranges,
                        )
                        .await
                    }
                    None => {
                        workspace_versions.clear();
                        workspace_cache.clear();
                        agent_file_put_with_retry(
                            &mut conn,
                            retry,
                            &mut state,
                            id,
                            request_id,
                            path,
                            data,
                            request.expected_sha256.as_deref(),
                            request.force,
                        )
                        .await
                    }
                };
                if let Err(error) = result {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "file_put_failed",
                        "message": error.to_string(),
                    }))?;
                }
            }
            "file_patch" => {
                let Some(path) = request.path.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_path",
                        "message": "agent file_patch requests require path",
                    }))?;
                    continue;
                };
                let Some(expected_sha256) = request.expected_sha256.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_expected_sha256",
                        "message": "agent file_patch requests require expected_sha256",
                    }))?;
                    continue;
                };
                let Some(prefix_len) = request.prefix_len else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_prefix_len",
                        "message": "agent file_patch requests require prefix_len",
                    }))?;
                    continue;
                };
                let Some(suffix_len) = request.suffix_len else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_suffix_len",
                        "message": "agent file_patch requests require suffix_len",
                    }))?;
                    continue;
                };
                let Some(replacement_base64) = request.replacement_base64.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_replacement",
                        "message": "agent file_patch requests require replacement_base64",
                    }))?;
                    continue;
                };
                let replacement = match BASE64.decode(replacement_base64) {
                    Ok(data) => data,
                    Err(error) => {
                        emit_agent(serde_json::json!({
                            "type": "error",
                            "id": id,
                            "code": "invalid_base64",
                            "message": format!("file_patch replacement_base64 is invalid: {error}"),
                        }))?;
                        continue;
                    }
                };
                let request_id = request.request_id.unwrap_or_else(Uuid::new_v4);
                workspace_versions.clear();
                workspace_cache.clear();
                if let Err(error) = agent_file_patch_with_retry(
                    &mut conn,
                    retry,
                    &mut state,
                    id,
                    request_id,
                    path,
                    expected_sha256,
                    prefix_len,
                    suffix_len,
                    replacement,
                )
                .await
                {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "file_patch_failed",
                        "message": error.to_string(),
                    }))?;
                }
            }
            "file_patch_ranges" => {
                if !connection_supports_feature(&conn, "file_patch_ranges") {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "unsupported_feature",
                        "message": "file_patch_ranges was not negotiated",
                    }))?;
                    continue;
                }
                let Some(path) = request.path.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_path",
                        "message": "agent file_patch_ranges requests require path",
                    }))?;
                    continue;
                };
                let Some(expected_sha256) = request.expected_sha256.as_deref() else {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_expected_sha256",
                        "message": "agent file_patch_ranges requests require expected_sha256",
                    }))?;
                    continue;
                };
                if request.ranges.is_empty() {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "missing_ranges",
                        "message": "agent file_patch_ranges requests require ranges",
                    }))?;
                    continue;
                }
                let mut ranges = Vec::with_capacity(request.ranges.len());
                let mut invalid = None;
                for range in request.ranges {
                    match BASE64.decode(range.replacement_base64.as_bytes()) {
                        Ok(replacement) => ranges.push(FilePatchRange {
                            offset: range.offset,
                            remove_len: range.remove_len,
                            replacement,
                        }),
                        Err(error) => {
                            invalid = Some(error.to_string());
                            break;
                        }
                    }
                }
                if let Some(message) = invalid {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "invalid_base64",
                        "message": format!("file_patch_ranges replacement_base64 is invalid: {message}"),
                    }))?;
                    continue;
                }
                let request_id = request.request_id.unwrap_or_else(Uuid::new_v4);
                workspace_versions.clear();
                workspace_cache.clear();
                if let Err(error) = agent_file_patch_ranges_with_retry(
                    &mut conn,
                    retry,
                    &mut state,
                    id,
                    request_id,
                    path,
                    expected_sha256,
                    ranges,
                )
                .await
                {
                    emit_agent(serde_json::json!({
                        "type": "error",
                        "id": id,
                        "code": "file_patch_ranges_failed",
                        "message": error.to_string(),
                    }))?;
                }
            }
            _ => {
                emit_agent(serde_json::json!({
                    "type": "error",
                    "id": id,
                    "code": "unsupported_operation",
                    "message": format!("unsupported agent operation {:?}", request.op),
                }))?;
            }
        }
    }
    if reuse_connection {
        connection_abort_guard.disarm();
        Ok(Some(AgentConnectionState {
            conn,
            state,
            workspace_versions,
            workspace_cache,
        }))
    } else {
        conn.close(0_u32.into(), b"agent adapter closed");
        Ok(None)
    }
}

#[cfg(unix)]
async fn run_agent_listener(
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    session_file: &Path,
    socket: &Path,
) -> Result<()> {
    // Set up signal streams before binding the pathname. If the OS refuses a
    // signal subscription, no endpoint exists yet that could be stranded.
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let listener = bind_agent_socket(socket).await?;
    let pool = Arc::new(AgentConnectionPool::new());
    let client_slots = Arc::new(Semaphore::new(AGENT_LISTENER_CLIENT_LIMIT));
    let mut clients = tokio::task::JoinSet::new();
    let mut client_reaper = tokio::time::interval(Duration::from_secs(1));
    eprintln!(
        "ASP agent adapter listening on {} (Ctrl-C to stop)",
        socket.display()
    );
    // Keep the accept loop inside a result-returning scope so an accept,
    // signal, or task setup error still reaches the common endpoint cleanup
    // and client-drain path below. Returning directly with `?` here would
    // otherwise leave the pathname behind as a stale socket.
    let loop_result: Result<()> = async {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, peer) = accepted?;
                    reap_agent_clients(&mut clients);
                    let server = server.to_owned();
                    let cert = cert.to_owned();
                    let auth_token = auth_token.map(str::to_owned);
                    let session_file = session_file.to_owned();
                    let pool = Arc::clone(&pool);
                    let client_permit = match client_slots.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            eprintln!("ASP agent adapter client {peer:?} rejected: client limit");
                            let (_read, mut write) = stream.into_split();
                            let response = b"{\"type\":\"error\",\"code\":\"agent_client_limit\",\"message\":\"local adapter client limit reached\"}\n";
                            let _ = write.write_all(response).await;
                            let _ = write.shutdown().await;
                            continue;
                        }
                    };
                    clients.spawn(async move {
                        let _client_permit = client_permit;
                        let (read, mut write) = stream.into_split();
                        let checkout = pool.checkout(&server, &session_file).await;
                        let (permit, initial) = match checkout {
                            Ok(value) => value,
                            Err(error) => {
                                eprintln!("ASP agent adapter client {peer:?} rejected: {error}");
                                let mut response = serde_json::to_vec(&serde_json::json!({
                                    "type": "error",
                                    "code": "agent_connection_pool_busy",
                                    "message": error.to_string(),
                                }))
                                .unwrap_or_else(|_| b"{\"type\":\"error\",\"code\":\"agent_connection_pool_busy\"}".to_vec());
                                response.push(b'\n');
                                let _ = write.write_all(&response).await;
                                let _ = write.shutdown().await;
                                return;
                            }
                        };
                        let result = agent_loop_io_with_state(
                            &server,
                            &cert,
                            auth_token.as_deref(),
                            &session_file,
                            tokio::io::BufReader::new(read),
                            write,
                            initial,
                            true,
                        )
                        .await;
                        match result {
                            Ok(state) => pool.checkin(permit, state).await,
                            Err(error) => {
                                drop(permit);
                                eprintln!("ASP agent adapter client {peer:?} failed: {error}");
                            }
                        }
                    });
                }
                _ = client_reaper.tick() => {
                    // Join completed adapter tasks as they finish instead of
                    // retaining one JoinHandle per local client until shutdown.
                    reap_agent_clients(&mut clients);
                }
                signal = interrupt.recv() => {
                    signal.ok_or_else(|| anyhow!("agent listener interrupt signal stream closed"))?;
                    break;
                }
                signal = terminate.recv() => {
                    signal.ok_or_else(|| anyhow!("agent listener termination signal stream closed"))?;
                    break;
                }
            }
        }
        Ok(())
    }
    .await;
    drop(listener);
    let cleanup_result = remove_agent_socket(socket);
    // Removing the endpoint prevents new callers from entering while the
    // supervisor gives existing local adapters a bounded opportunity to drain
    // their current request and flush JSONL output. A persistent client that
    // does not close is aborted at the common deadline and can reconnect to a
    // restarted listener using the durable ASP session.
    let deadline = Instant::now() + AGENT_LISTENER_SHUTDOWN_GRACE;
    while !clients.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            clients.abort_all();
            break;
        }
        match tokio::time::timeout(remaining, clients.join_next()).await {
            Ok(Some(Ok(()))) | Ok(None) => {}
            Ok(Some(Err(error))) => {
                eprintln!("ASP agent adapter client task failed during shutdown: {error}");
            }
            Err(_) => {
                clients.abort_all();
                break;
            }
        }
    }
    // `abort_all` only schedules cancellation; wait for every task to observe
    // it before returning so no adapter keeps the process alive after the
    // endpoint has been removed.
    clients.shutdown().await;
    if let Err(error) = cleanup_result {
        if loop_result.is_ok() {
            return Err(error);
        }
        eprintln!("ASP agent adapter endpoint cleanup failed after listener error: {error}");
    }
    loop_result
}

#[cfg(unix)]
fn reap_agent_clients(clients: &mut tokio::task::JoinSet<()>) {
    while let Some(result) = clients.try_join_next() {
        if let Err(error) = result {
            eprintln!("ASP agent adapter client task failed: {error}");
        }
    }
}

#[cfg(unix)]
async fn bind_agent_socket(path: &Path) -> Result<UnixListener> {
    use std::os::unix::fs::FileTypeExt;

    validate_agent_socket_path(path, true)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            bail!(
                "refusing to replace symlinked agent socket {}",
                path.display()
            );
        }
        if !metadata.file_type().is_socket() {
            bail!("agent socket path {} is not a Unix socket", path.display());
        }
        // Do not unlink a live listener. A short connect probe distinguishes a
        // stale socket left by a crashed adapter from an active endpoint; the
        // parent directory is private, so the path cannot be swapped by an
        // unrelated local user between this check and bind.
        match tokio::time::timeout(Duration::from_millis(100), UnixStream::connect(path)).await {
            Ok(Ok(_)) => bail!("agent socket {} is already in use", path.display()),
            Ok(Err(_)) | Err(_) => std::fs::remove_file(path)
                .with_context(|| format!("remove stale agent socket {}", path.display()))?,
        }
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind agent socket {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        return Err(error).with_context(|| format!("protect agent socket {}", path.display()));
    }
    Ok(listener)
}

#[cfg(unix)]
fn validate_agent_socket_path(path: &Path, create_parent: bool) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    if !path.is_absolute() {
        bail!("agent socket path must be absolute: {}", path.display());
    }
    if path.as_os_str().as_bytes().len() > 100 {
        bail!("agent socket path is too long for Unix-domain sockets");
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("agent socket path has no parent directory"))?;
    if create_parent && !parent.exists() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create agent socket directory {}", parent.display()))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let metadata = std::fs::symlink_metadata(parent)
        .with_context(|| format!("stat agent socket directory {}", parent.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "agent socket parent is not a real directory: {}",
            parent.display()
        );
    }
    // The final parent itself must be a real directory. Trusted system
    // aliases such as macOS's `/tmp` -> `/private/tmp` are allowed for an
    // otherwise private child directory; rejecting every canonical-path
    // difference would make the documented short-lived runtime location
    // unusable while the mode check below still prevents an attacker-owned
    // writable parent.
    parent
        .canonicalize()
        .with_context(|| format!("canonicalize agent socket directory {}", parent.display()))?;
    if metadata.permissions().mode() & 0o022 != 0 {
        bail!(
            "agent socket parent must not be group/world writable: {}",
            parent.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn remove_agent_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing to remove replaced symlinked agent socket {}",
                path.display()
            )
        }
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(path)
                .with_context(|| format!("remove agent socket {}", path.display()))?;
            Ok(())
        }
        Ok(_) => bail!(
            "agent socket path changed while shutting down: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("stat agent socket {}", path.display())),
    }
}

#[cfg(unix)]
async fn agent_connect(path: &Path) -> Result<()> {
    validate_agent_socket_path(path, false)?;
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connect agent socket {}", path.display()))?;
    let (mut socket_read, mut socket_write) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let stdin_to_socket = async {
        let copied = tokio::io::copy(&mut stdin, &mut socket_write).await?;
        if let Err(error) = socket_write.shutdown().await
            && !expected_agent_socket_disconnect(&error)
        {
            return Err(error);
        }
        Ok::<u64, std::io::Error>(copied)
    };
    let socket_to_stdout = async {
        let copied = tokio::io::copy(&mut socket_read, &mut stdout).await?;
        stdout.shutdown().await?;
        Ok::<u64, std::io::Error>(copied)
    };
    tokio::try_join!(stdin_to_socket, socket_to_stdout)?;
    Ok(())
}

/// A peer that has already finished the JSONL session may close its Unix
/// socket before the local stdin-copy task performs its final half-close. On
/// macOS this reports `ENOTCONN` (and on some Unix platforms `EPIPE` or
/// `ECONNRESET`) even though every response line was delivered. Those errors
/// are normal shutdown races; unrelated I/O failures must remain visible.
#[cfg_attr(not(unix), allow(dead_code))]
fn expected_agent_socket_disconnect(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
    )
}

#[allow(clippy::too_many_arguments)]
async fn agent_exec_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    request_id: Uuid,
    command: &str,
    summary: bool,
    tail_bytes: u32,
) -> Result<()> {
    let mut output_cursor = ExecOutputCursor::default();
    let mut process_id = None;
    let mut summary_emitted = false;
    let mut attempt = 0_u32;
    let mut auth_refresh_attempted = false;
    loop {
        match agent_exec_once(
            conn,
            state,
            request_label,
            request_id,
            command,
            summary,
            tail_bytes,
            &mut output_cursor,
            &mut process_id,
            &mut summary_emitted,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) if !auth_refresh_attempted && authentication_refresh_error(&error) => {
                auth_refresh_attempted = true;
                eprintln!("agent EXEC credentials changed; reconnecting");
                conn.close(0_u32.into(), b"credentials rotated");
                *conn = reconnect_with_retry(retry, state).await?;
            }
            Err(error) if attempt < 8 && is_server_busy_error(&error) => {
                // SERVER_BUSY means the request was admitted but the daemon
                // could not currently serialize/write its response (for
                // example, aggregate response memory is occupied). Keep the
                // authenticated connection and retry the same request ID;
                // reconnecting here would add a needless HELLO round trip and
                // the request is already idempotent.
                attempt += 1;
                let delay_ms = (100_u64.saturating_mul(1_u64 << attempt.min(6))).min(5_000);
                eprintln!(
                    "agent EXEC temporarily busy; retrying in {delay_ms}ms (attempt {attempt}/8)"
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            Err(error) if attempt < 8 && retryable_connection_error(&error) => {
                attempt += 1;
                eprintln!("agent EXEC interrupted; reconnecting (attempt {attempt}/8)");
                *conn = reconnect_with_retry(retry, state).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn agent_spawn_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    request_id: Uuid,
    command: &str,
) -> Result<()> {
    let session_id = state.session_id;
    let response = retry_request(
        conn,
        retry.server,
        retry.cert,
        retry.auth_token,
        state,
        Request::Spawn {
            session_id,
            request_id,
            command: command.to_owned(),
        },
    )
    .await?;
    match response {
        Response::ProcessAccepted {
            process_id,
            event_id,
        } => {
            emit_agent(serde_json::json!({
                "type": "spawned",
                "id": request_label,
                "request_id": request_id,
                "process_id": process_id,
                "event_id": event_id,
            }))?;
            Ok(())
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => unexpected(other),
    }
}

#[allow(clippy::too_many_arguments)]
async fn agent_logs_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    process_id: Uuid,
    stream: OutputStream,
    offset: u64,
    requested_length: Option<u64>,
    requested_tail: Option<u64>,
) -> Result<()> {
    // PROCESS_OUTPUT_STREAM is a point-in-time read; `seen` and the declared
    // range make retries offset-safe, so reconnect without replaying the
    // unrelated session journal.
    if requested_tail.is_some() && (offset != 0 || requested_length.is_some()) {
        bail!("tail_bytes cannot be combined with offset or length");
    }
    let (offset, requested_length) = if let Some(tail_bytes) = requested_tail {
        resolve_process_log_tail(conn, retry, state, process_id, &stream, tail_bytes).await?
    } else {
        (offset, requested_length)
    };
    if requested_length.is_some_and(|length| length > PROCESS_LOG_RANGE_MAX_BYTES) {
        bail!("requested process log range exceeds {PROCESS_LOG_RANGE_MAX_BYTES} bytes");
    }
    let mut seen = offset;
    let mut declared_length = None;
    let mut attempt = 0_u32;
    let mut auth_refresh_attempted = false;
    loop {
        match agent_logs_once(
            conn,
            request_label,
            state.session_id,
            process_id,
            &stream,
            offset,
            requested_length,
            &mut seen,
            &mut declared_length,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) if !auth_refresh_attempted && authentication_refresh_error(&error) => {
                auth_refresh_attempted = true;
                eprintln!("agent logs credentials changed; reconnecting");
                conn.close(0_u32.into(), b"credentials rotated");
                *conn = reconnect_without_resume_with_retry(retry, state).await?;
            }
            Err(error) if attempt < 8 && retryable_connection_error(&error) => {
                attempt += 1;
                eprintln!("agent logs interrupted; reconnecting (attempt {attempt}/8)");
                conn.close(0_u32.into(), b"agent logs retry");
                *conn = reconnect_without_resume_with_retry(retry, state).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn agent_logs_once(
    conn: &Connection,
    request_label: Option<&str>,
    session_id: Uuid,
    process_id: Uuid,
    stream: &OutputStream,
    offset: u64,
    requested_length: Option<u64>,
    seen: &mut u64,
    declared_length: &mut Option<u64>,
) -> Result<()> {
    let request_length = requested_length.or(*declared_length);
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    write_request_frame(
        conn,
        &mut send,
        Request::ProcessOutputStream {
            session_id,
            process_id,
            stream: stream.clone(),
            offset,
            length: request_length,
        },
    )
    .await?;
    send.finish()?;

    let first = read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed process log stream before BEGIN"))?;
    let (_total_size, begin_offset, length) = match first {
        Response::ProcessOutputStreamBegin {
            process_id: actual_process,
            stream: actual_stream,
            total_size,
            offset: begin_offset,
            length,
        } => {
            if actual_process != process_id || actual_stream != *stream {
                bail!("process log stream metadata mismatch");
            }
            if total_size > PROCESS_OUTPUT_MAX_BYTES
                || begin_offset != offset
                || begin_offset > total_size
                || length > total_size.saturating_sub(begin_offset)
                || length > PROCESS_LOG_RANGE_MAX_BYTES
            {
                bail!("invalid process log stream bounds");
            }
            if request_length.is_some_and(|requested| requested != length) {
                bail!("server changed requested process log range");
            }
            if let Some(previous) = *declared_length {
                if previous != length {
                    bail!("process log range length changed during retry");
                }
            } else {
                *declared_length = Some(length);
            }
            (total_size, begin_offset, length)
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => return unexpected(other),
    };

    let stream_name = output_stream_name(stream);
    let mut received = 0_u64;
    loop {
        let response = read_response_frame(conn, &mut recv)
            .await?
            .ok_or_else(|| anyhow!("server closed process log stream before END"))?;
        match response {
            Response::ProcessOutputStreamChunk {
                offset: chunk_offset,
                data,
            } => {
                if data.is_empty() || data.len() > FILE_STREAM_CHUNK_BYTES {
                    bail!("invalid process log chunk");
                }
                let expected_offset = begin_offset
                    .checked_add(received)
                    .ok_or_else(|| anyhow!("process log offset overflow"))?;
                if chunk_offset != expected_offset {
                    bail!("process log chunk offset mismatch");
                }
                let next_received = received
                    .checked_add(data.len() as u64)
                    .ok_or_else(|| anyhow!("process log byte count overflow"))?;
                if next_received > length {
                    bail!("process log stream exceeded declared range");
                }
                let chunk_end = chunk_offset
                    .checked_add(data.len() as u64)
                    .ok_or_else(|| anyhow!("process log offset overflow"))?;
                if let Some(suffix) = unseen_suffix(chunk_offset, &data, *seen)? {
                    emit_agent(serde_json::json!({
                        "type": "log",
                        "id": request_label,
                        "process_id": process_id,
                        "stream": stream_name,
                        "offset": *seen,
                        "data_base64": BASE64.encode(suffix),
                    }))?;
                    *seen = chunk_end;
                }
                received = next_received;
            }
            Response::ProcessOutputStreamEnd { bytes, complete } => {
                if bytes != received || received != length || !complete {
                    bail!(
                        "process log stream ended incomplete: bytes={bytes} received={received} expected={length}"
                    );
                }
                emit_agent(serde_json::json!({
                    "type": "log_end",
                    "id": request_label,
                    "process_id": process_id,
                    "stream": stream_name,
                    "offset": offset,
                    "bytes": length,
                    "complete": true,
                }))?;
                return Ok(());
            }
            Response::Error { code, message } => bail!("{code}: {message}"),
            other => return unexpected(other),
        }
    }
}

async fn agent_process_state_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    process_id: Uuid,
) -> Result<()> {
    let session_id = state.session_id;
    let response = retry_request(
        conn,
        retry.server,
        retry.cert,
        retry.auth_token,
        state,
        Request::ProcessState {
            session_id,
            process_id,
        },
    )
    .await?;
    match response {
        Response::ProcessState { snapshot } => {
            emit_agent(serde_json::json!({
                "type": "process_state",
                "id": request_label,
                "process_id": snapshot.process_id,
                "command": snapshot.command,
                "running": snapshot.running,
                "exit_code": snapshot.exit_code,
                "stdout_bytes": snapshot.stdout_bytes,
                "stderr_bytes": snapshot.stderr_bytes,
            }))?;
            Ok(())
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => unexpected(other),
    }
}

fn output_stream_name(stream: &OutputStream) -> &'static str {
    match stream {
        OutputStream::Stdout => "stdout",
        OutputStream::Stderr => "stderr",
    }
}

async fn agent_signal_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    request_id: Uuid,
    process_id: Uuid,
    signal: i32,
) -> Result<()> {
    let mut attempt = 0_u32;
    loop {
        let session_id = state.session_id;
        let response = retry_request(
            conn,
            retry.server,
            retry.cert,
            retry.auth_token,
            state,
            Request::Signal {
                session_id,
                request_id,
                process_id,
                signal,
            },
        )
        .await;
        match response {
            Ok(Response::Acked { through_event_id }) => {
                emit_agent(serde_json::json!({
                    "type": "signal_applied",
                    "id": request_label,
                    "request_id": request_id,
                    "process_id": process_id,
                    "signal": signal,
                    "through_event_id": through_event_id,
                }))?;
                return Ok(());
            }
            Ok(Response::Error { code, message }) => bail!("{code}: {message}"),
            Ok(other) => return unexpected(other),
            Err(error) if attempt < 8 && retryable_connection_error(&error) => {
                attempt += 1;
                eprintln!("agent SIGNAL interrupted; reconnecting (attempt {attempt}/8)");
                *conn = reconnect_with_retry(retry, state).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn agent_exec_once(
    conn: &Connection,
    state: &mut SavedSession,
    request_label: Option<&str>,
    request_id: Uuid,
    command: &str,
    summary: bool,
    tail_bytes: u32,
    output_cursor: &mut ExecOutputCursor,
    process_id: &mut Option<Uuid>,
    summary_emitted: &mut bool,
) -> Result<()> {
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    let request = if summary {
        Request::ExecSummary {
            session_id: state.session_id,
            request_id,
            command: command.to_owned(),
            tail_bytes,
        }
    } else {
        Request::Exec {
            session_id: state.session_id,
            request_id,
            command: command.to_owned(),
        }
    };
    write_request_frame(conn, &mut send, request).await?;
    send.finish()?;
    loop {
        let Some(response) = read_response_frame(conn, &mut recv).await? else {
            bail!("EXEC response stream closed before PROCESS_EXITED");
        };
        match response {
            Response::ProcessAccepted {
                process_id: received_process_id,
                event_id,
            } => {
                if process_id.is_none() {
                    *process_id = Some(received_process_id);
                    emit_agent(serde_json::json!({
                        "type": "started",
                        "id": request_label,
                        "request_id": request_id,
                        "process_id": received_process_id,
                        "event_id": event_id,
                    }))?;
                }
            }
            Response::ProcessOutput {
                event_id: _,
                process_id: received_process_id,
                stream,
                offset,
                data,
            } => {
                if process_id.is_none() {
                    *process_id = Some(received_process_id);
                }
                let seen = match stream {
                    OutputStream::Stdout => &mut output_cursor.stdout_seen,
                    OutputStream::Stderr => &mut output_cursor.stderr_seen,
                };
                let Some(suffix) = unseen_suffix(offset, &data, *seen)? else {
                    continue;
                };
                let emitted_offset = *seen;
                *seen = offset
                    .checked_add(data.len() as u64)
                    .ok_or_else(|| anyhow!("process output offset overflow"))?;
                emit_agent(serde_json::json!({
                    "type": "output",
                    "id": request_label,
                    "process_id": received_process_id,
                    "stream": match stream { OutputStream::Stdout => "stdout", OutputStream::Stderr => "stderr" },
                    "offset": emitted_offset,
                    "data_base64": BASE64.encode(suffix),
                }))?;
            }
            Response::ProcessSummary {
                process_id: received_process_id,
                event_id,
                stdout_bytes,
                stderr_bytes,
                stdout_tail,
                stderr_tail,
                stdout_truncated,
                stderr_truncated,
            } => {
                if process_id.is_none() {
                    *process_id = Some(received_process_id);
                }
                if !*summary_emitted {
                    emit_agent(serde_json::json!({
                        "type": "summary",
                        "id": request_label,
                        "process_id": received_process_id,
                        "event_id": event_id,
                        "stdout_bytes": stdout_bytes,
                        "stderr_bytes": stderr_bytes,
                        "stdout_tail_base64": BASE64.encode(stdout_tail),
                        "stderr_tail_base64": BASE64.encode(stderr_tail),
                        "stdout_truncated": stdout_truncated,
                        "stderr_truncated": stderr_truncated,
                    }))?;
                    *summary_emitted = true;
                }
            }
            Response::ProcessExited {
                process_id: received_process_id,
                event_id,
                code,
            } => {
                if process_id.is_none() {
                    *process_id = Some(received_process_id);
                }
                emit_agent(serde_json::json!({
                    "type": "exit",
                    "id": request_label,
                    "process_id": received_process_id,
                    "event_id": event_id,
                    "code": code,
                }))?;
                return Ok(());
            }
            Response::Error { code, message } => bail!("{code}: {message}"),
            other => return unexpected(other),
        }
    }
}

/// Execute one semantic workspace query on the adapter's already-authenticated
/// connection. The response is converted to JSON only at the local adapter
/// boundary; the server still sends compact Postcard bytes over QUIC.
#[allow(clippy::too_many_arguments)]
async fn agent_workspace_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    workspace: &str,
    include_tree: bool,
    include_git_status: bool,
    include_diff: bool,
    recent_commits: u16,
    searches: Vec<String>,
    read_paths: Vec<String>,
    known_tree_version: Option<WorkspaceVersion>,
    known_state_digest: Option<String>,
    workspace_versions: &mut HashMap<String, WorkspaceVersion>,
    workspace_cache: &mut AgentWorkspaceCache,
) -> Result<()> {
    let query_key = WorkspaceQueryKey::new(
        workspace,
        include_tree,
        include_git_status,
        include_diff,
        recent_commits,
        &searches,
        &read_paths,
    );
    let known_state_digest = known_state_digest.filter(|digest| {
        workspace_cache
            .get(&query_key)
            .is_some_and(|cached| cached.digest == *digest)
    });
    let known_tree_version = known_tree_version.filter(|version| {
        workspace_cache
            .get(&query_key)
            .is_some_and(|cached| cached.tree_version.as_ref() == Some(version))
    });
    let response = retry_request(
        conn,
        retry.server,
        retry.cert,
        retry.auth_token,
        state,
        Request::WorkspaceState {
            session_id: state.session_id,
            workspace: workspace.to_owned(),
            include_tree,
            include_git_status,
            include_diff,
            recent_commits,
            searches,
            read_paths,
            known_tree_version,
            known_state_digest,
        },
    )
    .await?;
    let Response::WorkspaceState {
        workspace,
        tree_version,
        tree_unchanged,
        tree,
        git_status,
        diff,
        recent_commits,
        search_hits,
        files,
        state_digest,
        state_unchanged,
    } = response
    else {
        return match response {
            Response::Error { code, message } => bail!("{code}: {message}"),
            other => unexpected(other),
        };
    };
    if !valid_sha256(&state_digest) {
        bail!("invalid workspace state digest from server");
    }
    let (tree, git_status, diff, recent_commits, search_hits, files) = if state_unchanged {
        let Some(cached) = workspace_cache
            .get(&query_key)
            .filter(|cached| cached.digest == state_digest)
            .cloned()
        else {
            bail!("workspace digest acknowledged without a matching local cache");
        };
        (
            cached.tree,
            cached.git_status,
            cached.diff,
            cached.recent_commits,
            cached.search_hits,
            cached.files,
        )
    } else {
        let cached_tree = workspace_cache
            .get(&query_key)
            .filter(|_| tree_unchanged)
            .map(|cached| cached.tree.clone())
            .unwrap_or_default();
        let complete_tree = if tree_unchanged && tree.is_empty() {
            cached_tree
        } else {
            tree.clone()
        };
        workspace_cache.insert(
            query_key,
            CachedWorkspaceState {
                digest: state_digest.clone(),
                tree_version: tree_version.clone(),
                tree: complete_tree.clone(),
                git_status: git_status.clone(),
                diff: diff.clone(),
                recent_commits: recent_commits.clone(),
                search_hits: search_hits.clone(),
                files: files.clone(),
                bytes: 0,
            },
        );
        (
            complete_tree,
            git_status,
            diff,
            recent_commits,
            search_hits,
            files,
        )
    };
    let files = files
        .into_iter()
        .map(|file| {
            serde_json::json!({
                "path": file.path,
                "sha256": file.sha256,
                "data_base64": BASE64.encode(&file.data),
                "bytes": file.data.len(),
            })
        })
        .collect::<Vec<_>>();
    if let Some(version) = tree_version.clone() {
        workspace_versions.insert(workspace.clone(), version);
    }
    emit_agent(serde_json::json!({
        "type": "workspace_state",
        "id": request_label,
        "workspace": workspace,
        "tree_version": tree_version,
        "tree_unchanged": tree_unchanged,
        "state_digest": state_digest,
        "state_unchanged": state_unchanged,
        "tree": tree,
        "git_status": git_status,
        "diff": diff,
        "recent_commits": recent_commits,
        "search_hits": search_hits,
        "files": files,
        "last_event_id": state.last_event_id,
    }))?;
    Ok(())
}

async fn agent_file_get_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    path: &str,
) -> Result<()> {
    let response = retry_request(
        conn,
        retry.server,
        retry.cert,
        retry.auth_token,
        state,
        Request::FileGet {
            session_id: state.session_id,
            path: path.to_owned(),
        },
    )
    .await?;
    match response {
        Response::FileData {
            path,
            version,
            sha256,
            data,
        } => {
            emit_agent(serde_json::json!({
                "type": "file_data",
                "id": request_label,
                "path": path,
                "version": version,
                "sha256": sha256,
                "bytes": data.len(),
                "data_base64": BASE64.encode(data),
            }))?;
            Ok(())
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => unexpected(other),
    }
}

/// Read only the current file metadata for a cached-base no-op check. A
/// zero-length FILE_GET_STREAM still returns the authoritative SHA-256 and
/// version, but never transfers file content or creates a mutation event.
async fn file_hash_once(conn: &Connection, session_id: Uuid, path: &str) -> Result<(u64, String)> {
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    write_request_frame(
        conn,
        &mut send,
        Request::FileGetStream {
            session_id,
            path: path.to_owned(),
            offset: 0,
            length: Some(0),
        },
    )
    .await?;
    send.finish()?;
    let first = read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed file metadata stream before BEGIN"))?;
    let (version, sha256) = match first {
        Response::FileStreamBegin {
            path: response_path,
            version,
            total_size,
            offset,
            length,
            sha256,
        } => {
            if response_path != path || offset != 0 || length != 0 {
                bail!("file metadata response does not describe a zero-length range");
            }
            if total_size > STREAM_FILE_MAX_BYTES
                || sha256.len() != 64
                || !sha256.as_bytes().iter().all(u8::is_ascii_hexdigit)
            {
                bail!("invalid file metadata response");
            }
            (version, sha256)
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected file metadata response: {other:?}"),
    };
    let end = read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed file metadata stream before END"))?;
    match end {
        Response::FileStreamEnd {
            bytes,
            sha256: end_sha256,
        } if bytes == 0 && end_sha256 == sha256 => Ok((version, sha256)),
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("invalid file metadata stream end: {other:?}"),
    }
}

async fn file_hash_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    path: &str,
) -> Result<(u64, String)> {
    let mut attempt = 0_u8;
    let mut auth_refresh_attempted = false;
    loop {
        match file_hash_once(conn, state.session_id, path).await {
            Ok(result) => return Ok(result),
            Err(error) if !auth_refresh_attempted && authentication_refresh_error(&error) => {
                auth_refresh_attempted = true;
                conn.close(0_u32.into(), b"credentials rotated");
                *conn = reconnect_without_resume_with_retry(retry, state).await?;
            }
            Err(error) if attempt < 2 && retryable_connection_error(&error) => {
                attempt += 1;
                conn.close(0_u32.into(), b"file metadata retry");
                *conn = reconnect_without_resume_with_retry(retry, state).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn agent_file_put_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    request_id: Uuid,
    path: &str,
    data: Vec<u8>,
    expected_sha256: Option<&str>,
    allow_blind: bool,
) -> Result<()> {
    let response = retry_request(
        conn,
        retry.server,
        retry.cert,
        retry.auth_token,
        state,
        Request::FilePut {
            session_id: state.session_id,
            request_id,
            path: path.to_owned(),
            expected_sha256: expected_sha256.map(str::to_owned),
            allow_blind,
            data,
        },
    )
    .await?;
    match response {
        Response::FileStored {
            path,
            version,
            sha256,
        } => {
            emit_agent(serde_json::json!({
                "type": "file_stored",
                "id": request_label,
                "request_id": request_id,
                "transfer": "full",
                "path": path,
                "version": version,
                "sha256": sha256,
            }))?;
            Ok(())
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => unexpected(other),
    }
}

#[allow(clippy::too_many_arguments)]
async fn agent_file_patch_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    request_id: Uuid,
    path: &str,
    expected_sha256: &str,
    prefix_len: u64,
    suffix_len: u64,
    replacement: Vec<u8>,
) -> Result<()> {
    let response = retry_request(
        conn,
        retry.server,
        retry.cert,
        retry.auth_token,
        state,
        Request::FilePatch {
            session_id: state.session_id,
            request_id,
            path: path.to_owned(),
            expected_sha256: expected_sha256.to_owned(),
            prefix_len,
            suffix_len,
            replacement,
        },
    )
    .await?;
    match response {
        Response::FileStored {
            path,
            version,
            sha256,
        } => {
            emit_agent(serde_json::json!({
                "type": "file_stored",
                "id": request_label,
                "request_id": request_id,
                "transfer": "patch",
                "path": path,
                "version": version,
                "sha256": sha256,
            }))?;
            Ok(())
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => unexpected(other),
    }
}

#[allow(clippy::too_many_arguments)]
async fn agent_file_patch_ranges_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    request_id: Uuid,
    path: &str,
    expected_sha256: &str,
    ranges: Vec<FilePatchRange>,
) -> Result<()> {
    let response = retry_request(
        conn,
        retry.server,
        retry.cert,
        retry.auth_token,
        state,
        Request::FilePatchRanges {
            session_id: state.session_id,
            request_id,
            path: path.to_owned(),
            expected_sha256: expected_sha256.to_owned(),
            ranges,
        },
    )
    .await?;
    match response {
        Response::FileStored {
            path,
            version,
            sha256,
        } => {
            emit_agent(serde_json::json!({
                "type": "file_stored",
                "id": request_label,
                "request_id": request_id,
                "transfer": "patch_ranges",
                "path": path,
                "version": version,
                "sha256": sha256,
            }))?;
            Ok(())
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => unexpected(other),
    }
}

fn emit_agent(value: serde_json::Value) -> Result<()> {
    let mut line = serde_json::to_vec(&value)?;
    line.push(b'\n');
    if let Ok(output) = AGENT_OUTPUT.try_with(Clone::clone) {
        return output.enqueue(line);
    }
    // Keep a synchronous fallback for unit-level helpers and any future
    // caller that emits an adapter event outside `agent_loop_io`.
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(&line)?;
    stdout.flush()?;
    Ok(())
}

/// Emit only the suffix not already observed before a connection dropped.
/// Process output offsets are monotonically increasing per stream, so replay
/// can safely resend from the beginning without duplicating terminal output.
fn write_unseen(stream: OutputStream, offset: u64, data: &[u8], seen: &mut u64) -> Result<()> {
    let Some(suffix) = unseen_suffix(offset, data, *seen)? else {
        return Ok(());
    };
    let end = offset + data.len() as u64;
    match stream {
        OutputStream::Stdout => {
            std::io::stdout().write_all(suffix)?;
            std::io::stdout().flush()?;
        }
        OutputStream::Stderr => {
            std::io::stderr().write_all(suffix)?;
            std::io::stderr().flush()?;
        }
    }
    *seen = end;
    Ok(())
}

fn unseen_suffix(offset: u64, data: &[u8], seen: u64) -> Result<Option<&[u8]>> {
    let end = offset
        .checked_add(data.len() as u64)
        .ok_or_else(|| anyhow!("process output offset overflow"))?;
    if offset > seen {
        bail!("process output gap: expected offset {seen}, got {offset}");
    }
    if end <= seen {
        return Ok(None);
    }
    let start = (seen - offset) as usize;
    Ok(Some(&data[start..]))
}

/// Only transport/stream failures are safe to retry. Application errors are
/// returned immediately so a bad command or authorization failure is not
/// accidentally replayed; credential-rotation refresh is handled separately
/// and only for the server's explicit `authentication_required` response.
fn retryable_connection_error(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    if [
        "authentication",
        "unauthorized",
        "forbidden",
        "certificate",
        "invalid peer",
        "server name",
        "invalid dns",
        "unknown session",
        "session_not_found",
        "request_id",
        "command must",
        "process_limit",
        "process_spawn_failed",
        // `exec_timeout` is a terminal application result, not a transport
        // timeout. Keep it out of the broad `timeout` retry marker below so a
        // timed-out command cannot be submitted a second time by an adapter.
        "exec_timeout",
        "result_compacted",
        "invalid_cursor",
        "invalid_ack",
        "version_mismatch",
        "principal_byte_budget",
        "principal_response_budget",
        "idempotency_capacity",
    ]
    .iter()
    .any(|marker| text.contains(marker))
    {
        return false;
    }
    [
        "connect to",
        "connection",
        "connection lost",
        "connection reset",
        "connection closed",
        // Quinn reports a peer's graceful application close as
        // `closed by peer: ...` without the word "connection".  A daemon
        // restart intentionally uses this path, so PTY/stream callers must
        // treat it like any other recoverable transport interruption.
        "closed by peer",
        "connection refused",
        "server refused",
        "aborted by peer",
        "closed stream",
        "broken pipe",
        "network is unreachable",
        "no route to host",
        "timed out",
        "timeout",
        "transport error",
        "stream stopped",
        // Opening a request stream after a daemon restart can surface
        // Quinn's transport failure only through this context string; it is
        // still safe to retry the idempotent request on a fresh connection.
        "open quic bidirectional request stream",
        "reset by peer",
        "unexpected eof",
        "server closed response stream",
        "server_busy",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

/// A long-lived connection can be invalidated by bearer-token or certificate
/// rotation while it is idle. The server reports this as an application error
/// on the first subsequent request; reconnect once so token-file clients pick
/// up the replacement credential without making callers restart the adapter.
fn authentication_refresh_error(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .to_ascii_lowercase()
        .contains("authentication_required")
}

fn is_server_busy_error(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .split_once(':')
        .is_some_and(|(code, _)| code.trim() == "server_busy")
}

#[allow(clippy::too_many_arguments)]
async fn agent_workload(
    server: &str,
    cert: &Path,
    session_file: &Path,
    workspace: &str,
    disconnect_seconds: u64,
    summary_output: bool,
    summary_tail_bytes: u32,
    log_mode: &str,
    auth_token: Option<&str>,
) -> Result<WorkloadMetrics> {
    if workspace.contains('\0') || workspace.contains('\'') {
        bail!("workspace must not contain NUL or single quotes");
    }
    let log_command = agent_log_command(log_mode)?;
    let wall_started = Instant::now();
    let mut blocked = Duration::ZERO;
    let mut gates = 0_u64;
    let mut response_bytes = 0_u64;
    let mut transport_stats = WorkloadTransportStats::default();

    let conn_started = Instant::now();
    let mut conn = connect_with_retry(server, cert, auth_token).await?;
    blocked += conn_started.elapsed();
    gates += 1;
    let mut state = ensure_session(&mut conn, server, cert, auth_token, session_file).await?;
    let session_id = state.session_id;

    let quoted = format!("'{workspace}'");
    let inspection = timed_one(
        &conn,
        Request::WorkspaceState {
            session_id,
            workspace: workspace.to_string(),
            include_tree: true,
            include_git_status: true,
            include_diff: false,
            recent_commits: 3,
            searches: vec!["TODO".into(), "alpha".into(), "function".into()],
            read_paths: vec![
                "src/alpha.txt".into(),
                "src/beta.txt".into(),
                "src/gamma.txt".into(),
            ],
            known_tree_version: None,
            known_state_digest: None,
        },
        &mut blocked,
        &mut gates,
    )
    .await?;
    let Response::WorkspaceState { files, .. } = inspection else {
        return unexpected(inspection);
    };
    response_bytes += files.iter().map(|file| file.data.len() as u64).sum::<u64>();

    for file in files {
        let path = file.path;
        let remote = format!("{workspace}/{path}");
        let replacement = format!("\nagent edit {path}\n").into_bytes();
        let response = timed_one(
            &conn,
            Request::FilePatch {
                session_id,
                request_id: Uuid::new_v4(),
                path: remote,
                expected_sha256: file.sha256,
                prefix_len: file.data.len() as u64,
                suffix_len: 0,
                replacement,
            },
            &mut blocked,
            &mut gates,
        )
        .await?;
        if !matches!(response, Response::FileStored { .. }) {
            return unexpected(response);
        }
    }

    for command in [format!("cd {quoted} && ./test.sh"), log_command.to_string()] {
        workload_exec(
            &conn,
            session_id,
            command,
            &mut blocked,
            &mut gates,
            &mut response_bytes,
            summary_output,
            summary_tail_bytes,
        )
        .await?;
    }

    let diff = timed_one(
        &conn,
        Request::WorkspaceState {
            session_id,
            workspace: workspace.to_string(),
            include_tree: false,
            include_git_status: true,
            include_diff: true,
            recent_commits: 0,
            searches: Vec::new(),
            read_paths: Vec::new(),
            known_tree_version: None,
            known_state_digest: None,
        },
        &mut blocked,
        &mut gates,
    )
    .await?;
    if !matches!(diff, Response::WorkspaceState { .. }) {
        return unexpected(diff);
    }
    workload_exec(
        &conn,
        session_id,
        format!("wc -l {quoted}/src/*.txt"),
        &mut blocked,
        &mut gates,
        &mut response_bytes,
        summary_output,
        summary_tail_bytes,
    )
    .await?;

    let response = timed_one(
        &conn,
        Request::Spawn {
            session_id,
            request_id: Uuid::new_v4(),
            command: format!("sleep {disconnect_seconds}; printf persistent-agent-work-complete"),
        },
        &mut blocked,
        &mut gates,
    )
    .await?;
    if !matches!(response, Response::ProcessAccepted { .. }) {
        return unexpected(response);
    }
    transport_stats.observe(&conn);
    conn.close(0_u32.into(), b"intentional workload disconnect");
    drop(conn);

    tokio::time::sleep(Duration::from_secs(disconnect_seconds + 1)).await;
    let recovery_started = Instant::now();
    let conn = connect_with_retry(server, cert, auth_token).await?;
    let response = resume_stream(&conn, session_id, state.last_event_id).await?;
    blocked += recovery_started.elapsed();
    gates += 1;
    let recovery = recovery_started.elapsed();
    let ResumeResult {
        snapshot,
        events,
        compacted: _,
    } = response;
    let persistent_process_observed = events.iter().any(|event| {
        matches!(
            &event.kind,
            EventKind::ProcessOutput { data, .. }
                if data.windows(b"persistent-agent-work-complete".len())
                    .any(|window| window == b"persistent-agent-work-complete")
        )
    });
    let resumed_events = events.len();
    state.advance_event_cursor(snapshot.latest_event_id);
    save(session_file, server, state.clone())?;
    let final_state = timed_one(
        &conn,
        Request::WorkspaceState {
            session_id,
            workspace: workspace.to_string(),
            include_tree: false,
            include_git_status: true,
            include_diff: false,
            recent_commits: 0,
            searches: Vec::new(),
            read_paths: Vec::new(),
            known_tree_version: None,
            known_state_digest: None,
        },
        &mut blocked,
        &mut gates,
    )
    .await?;
    if !matches!(final_state, Response::WorkspaceState { .. }) {
        return unexpected(final_state);
    }
    transport_stats.observe(&conn);
    conn.close(0_u32.into(), b"agent workload complete");

    Ok(WorkloadMetrics {
        experiment: "agent-workload",
        system: "asp",
        application_round_trips: gates,
        transport_connections: 2,
        quic_tx_datagrams: transport_stats.tx_datagrams,
        quic_tx_bytes: transport_stats.tx_bytes,
        quic_rx_datagrams: transport_stats.rx_datagrams,
        quic_rx_bytes: transport_stats.rx_bytes,
        quic_lost_packets: transport_stats.lost_packets,
        quic_congestion_events: transport_stats.congestion_events,
        quic_last_path_rtt_us: transport_stats.last_path_rtt_us,
        application_payload_bytes: response_bytes,
        wall_ms: wall_started.elapsed().as_secs_f64() * 1_000.0,
        network_blocked_ms: blocked.as_secs_f64() * 1_000.0,
        recovery_ms: recovery.as_secs_f64() * 1_000.0,
        disconnect_seconds,
        summary_output,
        summary_tail_bytes,
        log_mode: log_mode.to_string(),
        resumed_events,
        persistent_process_observed,
    })
}

fn agent_log_command(mode: &str) -> Result<&'static str> {
    match mode {
        "compressible" => Ok("head -c 10485760 /dev/zero"),
        "incompressible" => Ok("head -c 10485760 /dev/urandom"),
        "mixed" => Ok("head -c 5242880 /dev/zero; head -c 5242880 /dev/urandom"),
        other => bail!(
            "invalid agent log mode {other:?}; expected compressible, incompressible, or mixed"
        ),
    }
}

/// Establish a client connection with a short bounded retry window. The
/// transport itself owns loss recovery and migration; this helper only covers
/// a transient handshake/path failure before an application request exists.
/// Callers still have request-level retries for an established session.
async fn connect_with_retry(
    server: &str,
    cert_path: &Path,
    auth_token: Option<&str>,
) -> Result<Connection> {
    let mut last_error = None;
    for attempt in 0_u32..=4 {
        if attempt > 0 {
            let delay = (100_u64.saturating_mul(1_u64 << attempt.min(3))).min(1_000);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        match connect(server, cert_path, auth_token).await {
            Ok(connection) => return Ok(connection),
            Err(error) if retryable_connection_error(&error) => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("connect attempts exhausted")))
}

async fn connect(server: &str, cert_path: &Path, auth_token: Option<&str>) -> Result<Connection> {
    let auth_token = current_auth_token(auth_token)?;
    if cached_server_version(server) == Some(LEGACY_PROTOCOL_VERSION) {
        match connect_for_version(
            server,
            cert_path,
            auth_token.as_deref(),
            LEGACY_PROTOCOL_VERSION,
        )
        .await
        {
            Ok(connection) => return Ok(connection),
            Err(error) if should_try_current_version(&error) => {
                // The endpoint may have been upgraded, or a load balancer
                // may have moved us to a v17-only peer. Forget the hint and
                // run the normal current-version probe below.
                forget_server_version(server);
            }
            Err(error) => return Err(error),
        }
    }
    match connect_for_version(server, cert_path, auth_token.as_deref(), PROTOCOL_VERSION).await {
        Ok(connection) => {
            remember_server_version(server, PROTOCOL_VERSION);
            Ok(connection)
        }
        Err(error) if should_try_legacy_version(&error) => {
            // A rolling deployment can leave an older v16 daemon behind a
            // load balancer. v17's AF envelope is deliberately not decoded
            // as a legacy payload; retry the same authenticated handshake
            // with the tested plain v16 framing instead. Authentication and
            // policy failures are excluded by should_try_legacy_version so a
            // bad credential is never retried as a second login attempt.
            match connect_for_version(
                server,
                cert_path,
                auth_token.as_deref(),
                LEGACY_PROTOCOL_VERSION,
            )
            .await
            {
                Ok(connection) => {
                    remember_server_version(server, LEGACY_PROTOCOL_VERSION);
                    Ok(connection)
                }
                Err(legacy_error) => Err(legacy_error).with_context(|| {
                    format!("v17 handshake failed; v16 fallback also failed: {error}")
                }),
            }
        }
        Err(error) => Err(error),
    }
}

/// Connect using an already configured Quinn endpoint. Keeping this separate
/// from [`connect`] lets long-lived adapters preserve the endpoint's UDP
/// socket and rustls session cache across reconnects, while one-shot callers
/// retain the existing fresh-endpoint behavior.
async fn connect_on_endpoint(
    endpoint: Endpoint,
    server: &str,
    auth_token: Option<&str>,
) -> Result<Connection> {
    let auth_token = current_auth_token(auth_token)?;
    if cached_server_version(server) == Some(LEGACY_PROTOCOL_VERSION) {
        match connect_for_version_on_endpoint(
            endpoint.clone(),
            server,
            auth_token.as_deref(),
            LEGACY_PROTOCOL_VERSION,
        )
        .await
        {
            Ok(connection) => return Ok(connection),
            Err(error) if should_try_current_version(&error) => {
                forget_server_version(server);
            }
            Err(error) => return Err(error),
        }
    }
    match connect_for_version_on_endpoint(
        endpoint.clone(),
        server,
        auth_token.as_deref(),
        PROTOCOL_VERSION,
    )
    .await
    {
        Ok(connection) => {
            remember_server_version(server, PROTOCOL_VERSION);
            Ok(connection)
        }
        Err(error) if should_try_legacy_version(&error) => {
            match connect_for_version_on_endpoint(
                endpoint,
                server,
                auth_token.as_deref(),
                LEGACY_PROTOCOL_VERSION,
            )
            .await
            {
                Ok(connection) => {
                    remember_server_version(server, LEGACY_PROTOCOL_VERSION);
                    Ok(connection)
                }
                Err(legacy_error) => Err(legacy_error).with_context(|| {
                    format!("v17 handshake failed; v16 fallback also failed: {error}")
                }),
            }
        }
        Err(error) => Err(error),
    }
}

async fn connect_for_version(
    server: &str,
    cert_path: &Path,
    auth_token: Option<&str>,
    version: u16,
) -> Result<Connection> {
    let endpoint = build_client_endpoint(cert_path)?;
    connect_for_version_on_endpoint(endpoint, server, auth_token, version).await
}

fn build_client_endpoint(cert_path: &Path) -> Result<Endpoint> {
    let mut roots = RootCertStore::empty();
    for cert in read_pinned_server_certificates(cert_path)? {
        roots.add(CertificateDer::from(cert)).with_context(|| {
            format!("parse pinned server certificate(s) {}", cert_path.display())
        })?;
    }
    let mut client_config =
        if let Some(identity) = CLIENT_IDENTITY.get().and_then(|identity| identity.as_ref()) {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let client_certificate = read_file_limited(&identity.cert, MAX_CLIENT_CERT_BYTES)
                .with_context(|| format!("read client certificate {}", identity.cert.display()))?;
            let client_key =
                read_private_file_limited(&identity.key, MAX_CLIENT_CERT_BYTES, "client key")
                    .with_context(|| format!("read client key {}", identity.key.display()))?;
            let tls_config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_client_auth_cert(
                    vec![CertificateDer::from(client_certificate)],
                    PrivateKeyDer::try_from(client_key)
                        .map_err(|error| anyhow!("invalid client key: {error}"))?,
                )?;
            quinn::ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(
                tls_config,
            )?))
        } else {
            quinn::ClientConfig::with_root_certificates(Arc::new(roots))?
        };
    let mut transport = TransportConfig::default();
    configure_quic_transport(&mut transport)?;
    client_config.transport_config(Arc::new(transport));
    let mut endpoint = Endpoint::client("[::]:0".parse()?)?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

/// Connect and negotiate one protocol version on an already configured Quinn
/// endpoint. Long-lived adapters pass the endpoint that owns their current
/// connection so reconnects keep the same UDP socket and rustls session
/// cache. One-shot callers use `connect_for_version`, which constructs a
/// fresh endpoint and retains the existing bounded close-drain behavior.
async fn connect_for_version_on_endpoint(
    endpoint: Endpoint,
    server: &str,
    auth_token: Option<&str>,
    version: u16,
) -> Result<Connection> {
    let remotes = resolve(server).await?;
    let server_name = TLS_SERVER_NAME
        .get()
        .map(String::as_str)
        .unwrap_or("localhost");
    let connection_deadline = Instant::now() + client_connect_timeout();
    let server = server.to_owned();
    let server_name = server_name.to_owned();
    let auth_token = auth_token.map(str::to_owned);
    let attempt_slots = Arc::new(Semaphore::new(MAX_PARALLEL_CONNECT_ATTEMPTS));
    let mut attempts = tokio::task::JoinSet::new();
    for (index, remote) in remotes.into_iter().enumerate() {
        // Keep a bounded number of attempts in flight. The small stagger
        // gives the first address a head start on healthy networks while
        // allowing a black-holed family to be bypassed quickly. All resolved
        // addresses are queued, so a failed first wave does not hide a
        // healthy fifth or later DNS result.
        let endpoint = endpoint.clone();
        let server = server.clone();
        let server_name = server_name.clone();
        let auth_token = auth_token.clone();
        let attempt_slots = Arc::clone(&attempt_slots);
        attempts.spawn(async move {
            let _slot = attempt_slots
                .acquire_owned()
                .await
                .context("connect attempt limiter closed")?;
            let delay = connect_attempt_delay(index);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let remaining = connection_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("connect to {server} ({remote}) timed out"));
            }
            let connecting = endpoint.connect(remote, &server_name).map_err(|error| {
                anyhow!(error).context(format!("connect to {server} ({remote})"))
            })?;
            let connection = match tokio::time::timeout(remaining, connecting).await {
                Ok(Ok(connection)) => connection,
                Ok(Err(error)) => {
                    return Err(anyhow!(error).context(format!("connect to {server} ({remote})")));
                }
                Err(_) => {
                    return Err(anyhow!("connect to {server} ({remote}) timed out"));
                }
            };
            if let Err(error) =
                check_version_for_version(&connection, auth_token.as_deref(), version).await
            {
                connection.close(0_u32.into(), b"protocol negotiation failed");
                return Err(error.context(format!("ASP protocol v{version} handshake failed")));
            }
            Ok(connection)
        });
    }

    let mut last_transport_error = None;
    while !attempts.is_empty() {
        let remaining = connection_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let joined = match tokio::time::timeout(remaining, attempts.join_next()).await {
            Ok(Some(joined)) => joined,
            Ok(None) => break,
            Err(_) => break,
        };
        match joined {
            Ok(Ok(connection)) => {
                // Dropping an unfinished Connecting future is not enough to
                // communicate intent to the peer; abort the losing tasks so
                // they cannot complete a second handshake after the winner
                // has been returned.
                attempts.abort_all();
                while attempts.join_next().await.is_some() {}
                remember_frame_version(&connection, version);
                remember_client_endpoint(&connection, &endpoint);
                // Keep the endpoint alive until this connection has drained.
                // Quinn documents that `Connection::close` only queues the
                // CONNECTION_CLOSE packet; dropping the endpoint immediately
                // can leave a short-lived CLI connection visible to the peer
                // until the full idle timeout.  A tiny monitor gives the
                // endpoint time to flush the close while retaining no
                // application state after the connection is gone.  This is
                // especially important for the server's per-principal
                // connection quota, where a burst of one-shot commands must
                // not consume slots for fifteen seconds after normal exit.
                let cleanup_connection = connection.clone();
                tokio::spawn(async move {
                    cleanup_connection.closed().await;
                    wait_for_endpoint_idle(endpoint).await;
                    forget_client_endpoint(&cleanup_connection);
                });
                return Ok(connection);
            }
            Ok(Err(error)) => {
                // Preserve the existing contract: application/auth/policy
                // failures are surfaced immediately, while transport errors
                // let another resolved address win the race.
                if !retryable_connection_error(&error) {
                    attempts.abort_all();
                    while attempts.join_next().await.is_some() {}
                    return Err(error);
                }
                last_transport_error = Some(error);
            }
            Err(error) => {
                last_transport_error = Some(anyhow!(error).context("connect attempt task failed"));
            }
        }
    }
    attempts.abort_all();
    while attempts.join_next().await.is_some() {}
    Err(last_transport_error.unwrap_or_else(|| anyhow!("connect to {server} failed")))
}

/// Decide whether a failed current-version handshake is plausibly caused by
/// an older daemon that cannot parse the v17 frame envelope. Keep this list
/// intentionally narrow: credentials, certificate validation, and server
/// policy errors must be returned directly instead of causing another login
/// attempt or masking the real diagnosis.
fn should_try_legacy_version(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    if [
        "authentication",
        "unauthorized",
        "forbidden",
        "certificate",
        "invalid peer",
        "server name",
        "invalid dns",
        "invalid token",
        "principal_",
        "rate limit",
        "server_busy",
        "invalid_features",
        "permission",
        "did not negotiate required features",
    ]
    .iter()
    .any(|marker| text.contains(marker))
    {
        return false;
    }
    [
        "protocol v17 handshake",
        "version_mismatch",
        "server closed response",
        "closed stream",
        "connection",
        "decode",
        "frame",
        "postcard",
        "unexpected end",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn should_try_current_version(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    if [
        "authentication",
        "unauthorized",
        "forbidden",
        "certificate",
        "invalid peer",
        "server name",
        "invalid dns",
        "invalid token",
        "principal_",
        "rate limit",
        "server_busy",
        "invalid_features",
        "permission",
        "did not negotiate required features",
    ]
    .iter()
    .any(|marker| text.contains(marker))
    {
        return false;
    }
    [
        "protocol v16 handshake",
        "version_mismatch",
        "server closed response",
        "closed stream",
        "connection",
        "decode",
        "frame",
        "postcard",
        "unexpected end",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

/// Read one pinned DER certificate or a small directory of DER pins.
///
/// A directory lets operators stage an overlapping old/new pin set before a
/// server SIGHUP certificate reload. Only regular `.der` files are accepted;
/// symlinks and oversized bundles fail closed, and the final file open still
/// uses `O_NOFOLLOW` to close a rename race between enumeration and parsing.
fn read_pinned_server_certificates(path: &Path) -> Result<Vec<Vec<u8>>> {
    reject_symlink(path)?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat pinned server certificate(s) {}", path.display()))?;
    if !metadata.is_dir() {
        return read_file_limited(path, MAX_CLIENT_CERT_BYTES)
            .map(|certificate| vec![certificate])
            .with_context(|| format!("read pinned server certificate {}", path.display()));
    }

    let mut paths = Vec::new();
    for entry in std::fs::read_dir(path)
        .with_context(|| format!("read pinned certificate directory {}", path.display()))?
    {
        let entry = entry?;
        let entry_path = entry.path();
        let entry_metadata = std::fs::symlink_metadata(&entry_path)?;
        if entry_metadata.file_type().is_symlink() {
            bail!(
                "refusing symlink in pinned certificate directory {}",
                entry_path.display()
            );
        }
        let is_der = entry_path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("der"));
        if !entry_metadata.is_file() || !is_der {
            continue;
        }
        paths.push(entry_path);
    }
    paths.sort();
    if paths.is_empty() {
        bail!(
            "pinned certificate directory {} contains no .der files",
            path.display()
        );
    }
    if paths.len() > MAX_CLIENT_CERT_BUNDLE_ENTRIES {
        bail!(
            "pinned certificate directory {} contains more than {} files",
            path.display(),
            MAX_CLIENT_CERT_BUNDLE_ENTRIES
        );
    }
    let mut total = 0_u64;
    let mut certificates = Vec::with_capacity(paths.len());
    for certificate_path in paths {
        let certificate = read_file_limited(&certificate_path, MAX_CLIENT_CERT_BYTES)
            .with_context(|| {
                format!(
                    "read pinned server certificate {}",
                    certificate_path.display()
                )
            })?;
        total = total
            .checked_add(certificate.len() as u64)
            .ok_or_else(|| anyhow!("pinned certificate bundle size overflow"))?;
        if total > MAX_CLIENT_CERT_BUNDLE_BYTES {
            bail!(
                "pinned certificate directory {} exceeds the {} byte limit",
                path.display(),
                MAX_CLIENT_CERT_BUNDLE_BYTES
            );
        }
        certificates.push(certificate);
    }
    Ok(certificates)
}

async fn open_session_with_retry(
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    request_id: Uuid,
) -> Result<Response> {
    let mut last_error = None;
    for attempt in 0_u32..=4 {
        if attempt > 0 {
            let delay = (100_u64.saturating_mul(1_u64 << attempt.min(3))).min(1_000);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        let connection = match connect(server, cert, auth_token).await {
            Ok(connection) => connection,
            Err(error) if retryable_connection_error(&error) => {
                last_error = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        match one(&connection, Request::OpenSession { request_id }).await {
            Ok(response) => {
                close_connection_and_wait(&connection, b"open session complete").await;
                return Ok(response);
            }
            Err(error) if retryable_connection_error(&error) => {
                close_connection_and_wait(&connection, b"open session retry").await;
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("open session attempts exhausted")))
}

async fn reconnect_with_retry(
    retry: RetryContext<'_>,
    state: &mut SavedSession,
) -> Result<Connection> {
    reconnect_on_endpoint(
        retry.endpoint.cloned(),
        retry.server,
        retry.cert,
        retry.auth_token,
        state,
    )
    .await
}

/// Reconnect an operation without replaying the session journal. Request
/// callers carry stable idempotency keys, offsets, or immutable digests, so a
/// fresh authenticated attachment is sufficient and avoids an extra
/// high-latency round trip before the operation is retried.
async fn reconnect_without_resume_with_retry(
    retry: RetryContext<'_>,
    state: &mut SavedSession,
) -> Result<Connection> {
    reconnect_without_resume_on_endpoint(
        retry.endpoint.cloned(),
        retry.server,
        retry.cert,
        retry.auth_token,
        state,
    )
    .await
}

async fn reconnect_on_endpoint(
    endpoint: Option<Endpoint>,
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    state: &mut SavedSession,
) -> Result<Connection> {
    reconnect_on_endpoint_with_policy(endpoint, server, cert, auth_token, state).await
}

/// Reconnect without replaying the event journal.  The session UUID remains
/// durable and request IDs/offsets make the operation safe to repeat, so a
/// HELLO followed immediately by the original request avoids an otherwise
/// redundant RESUME round trip on high-latency links.  The saved event cursor
/// is intentionally left unchanged; an explicit RESUME or event subscription
/// still reconstructs the complete journal view.
async fn reconnect_without_resume_on_endpoint(
    endpoint: Option<Endpoint>,
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    state: &mut SavedSession,
) -> Result<Connection> {
    reconnect_on_endpoint_with_policy(endpoint, server, cert, auth_token, state).await
}

async fn reconnect_on_endpoint_with_policy(
    endpoint: Option<Endpoint>,
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    _state: &mut SavedSession,
) -> Result<Connection> {
    let mut last_error = None;
    let deadline = Instant::now() + client_reconnect_timeout();
    let mut attempt = 0_u32;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        if attempt > 0 {
            let delay = (100_u64.saturating_mul(1_u64 << attempt.min(3))).min(1_000);
            tokio::time::sleep(Duration::from_millis(delay).min(remaining)).await;
            if deadline.saturating_duration_since(Instant::now()).is_zero() {
                break;
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let connected = tokio::time::timeout(remaining, async {
            match endpoint.as_ref() {
                Some(endpoint) => connect_on_endpoint(endpoint.clone(), server, auth_token).await,
                None => connect(server, cert, auth_token).await,
            }
        })
        .await;
        match connected {
            Ok(Ok(connection)) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    connection.close(0_u32.into(), b"resume retry window expired");
                    break;
                }
                // Request IDs, process-output offsets, and durable file
                // checkpoints make the attached operation safe to retry
                // directly after HELLO. Replaying the entire session journal
                // here would add a round trip and can be unbounded relative
                // to the request's actual result. Explicit event consumers
                // still use `resume`/`SubscribeEvents` when they need replay.
                return Ok(connection);
            }
            Ok(Err(error)) if retryable_connection_error(&error) => {
                eprintln!("reconnect attempt {attempt} failed: {error}");
                last_error = Some(error);
            }
            Err(_) => {
                let error = anyhow!("reconnect attempt timed out");
                eprintln!("reconnect attempt {attempt} failed: {error}");
                last_error = Some(error);
            }
            Ok(Err(error)) => {
                eprintln!("reconnect attempt {attempt} not retryable: {error}");
                return Err(error);
            }
        }
        attempt = attempt.saturating_add(1);
    }
    Err(last_error.unwrap_or_else(|| anyhow!("reconnect retry window exhausted")))
}

/// Keep an interactive PTY usable across an outage or address change. Unlike
/// one-shot request retries, a shell should not give up after a handful of
/// attempts: laptops routinely sleep longer than a bounded retry window. The
/// reconnect loop is cancellable with Ctrl-] (the same escape byte used by the
/// connected shell) and retries only transport/resume failures.
async fn reconnect_shell(
    retry: RetryContext<'_>,
    _state: &mut SavedSession,
    stdin: &mut tokio::io::Stdin,
) -> Result<Option<Connection>> {
    let mut attempt = 0_u32;
    let mut input = [0_u8; 4096];
    let reusable_endpoint = retry.endpoint.cloned();
    loop {
        if attempt > 0 {
            let delay_ms = (100_u64.saturating_mul(1_u64 << attempt.min(6))).min(5_000);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                read = stdin.read(&mut input) => {
                    let n = read?;
                    if n == 0 || input[..n].contains(&0x1d) || input[..n].contains(&0x03) {
                        return Ok(None);
                    }
                    eprintln!("discarded {n} input bytes while reconnecting PTY");
                }
            }
        }
        let connected = tokio::time::timeout(client_connect_timeout(), async {
            match reusable_endpoint.as_ref() {
                Some(endpoint) => {
                    connect_on_endpoint(endpoint.clone(), retry.server, retry.auth_token).await
                }
                None => connect(retry.server, retry.cert, retry.auth_token).await,
            }
        })
        .await;
        let connection = match connected {
            Ok(Ok(connection)) => connection,
            Ok(Err(error)) if retryable_connection_error(&error) => {
                eprintln!("shell reconnect attempt {attempt} failed: {error}");
                attempt = attempt.saturating_add(1);
                continue;
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                eprintln!("shell reconnect attempt {attempt} timed out");
                attempt = attempt.saturating_add(1);
                continue;
            }
        };
        // PTY_OPEN below supplies an authoritative screen snapshot and
        // generation. The shell therefore reconnects directly after HELLO;
        // replaying the session journal first would add latency without
        // improving PTY recovery.
        return Ok(Some(connection));
    }
}

/// Retry a request whose semantics are safe to repeat. Side-effecting
/// requests carry a stable request ID, so a lost response cannot cause a
/// second process/file mutation; read-only requests are naturally repeatable.
async fn retry_request(
    conn: &mut Connection,
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    state: &mut SavedSession,
    request: Request,
) -> Result<Response> {
    let mut attempt = 0_u8;
    let mut auth_refresh_attempted = false;
    loop {
        match one(conn, request.clone()).await {
            Ok(Response::Error { code, .. })
                if code == "authentication_required" && !auth_refresh_attempted =>
            {
                auth_refresh_attempted = true;
                let endpoint = clone_client_endpoint(conn);
                conn.close(0_u32.into(), b"credentials rotated");
                *conn =
                    reconnect_without_resume_on_endpoint(endpoint, server, cert, auth_token, state)
                        .await?;
            }
            Ok(Response::Error { code, message }) if code == "server_busy" && attempt < 2 => {
                attempt += 1;
                let delay_ms = 100_u64.saturating_mul(1_u64 << attempt);
                eprintln!(
                    "ASP request temporarily busy; retrying in {delay_ms}ms (attempt {attempt}/2): {message}"
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            Ok(response) => return Ok(response),
            Err(error) if attempt < 2 && retryable_connection_error(&error) => {
                attempt += 1;
                let endpoint = clone_client_endpoint(conn);
                conn.close(0_u32.into(), b"request retry");
                *conn =
                    reconnect_without_resume_on_endpoint(endpoint, server, cert, auth_token, state)
                        .await?;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Authenticate and negotiate one concrete wire version. The request is
/// written directly instead of going through `one`, because the connection's
/// framing mode is not known until this very first request succeeds.
async fn check_version_for_version(
    conn: &Connection,
    auth_token: Option<&str>,
    version: u16,
) -> Result<()> {
    if !protocol_version_supported(version) {
        bail!("unsupported ASP protocol version requested: {version}");
    }
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    let request = Request::HelloFeatures {
        version,
        auth_token: auth_token.map(str::to_owned),
        features: requested_features(prefer_pty_state_delta()),
    };
    write_request_frame_for_version(&mut send, request, version).await?;
    send.finish()?;
    let response = tokio::time::timeout(
        client_connect_timeout(),
        read_frame_for_version(&mut recv, version),
    )
    .await
    .with_context(|| format!("ASP protocol v{version} handshake response timeout"))??;
    match response {
        Some(Response::HelloFeatures {
            version: response_version,
            features,
            ..
        }) if response_version == version => {
            let missing = SUPPORTED_FEATURES
                .iter()
                .filter(|feature| !features.iter().any(|value| value == **feature))
                .copied()
                .collect::<Vec<_>>();
            if missing.is_empty() {
                remember_connection_features(conn, &features);
                Ok(())
            } else {
                bail!("server did not negotiate required features: {missing:?}");
            }
        }
        Some(Response::Hello {
            version: response_version,
            ..
        }) if response_version == version => {
            remember_connection_features(conn, &[]);
            bail!("server only supports legacy HELLO without feature negotiation")
        }
        Some(Response::HelloFeatures {
            version: response_version,
            ..
        })
        | Some(Response::Hello {
            version: response_version,
            ..
        }) => {
            bail!("server negotiated ASP protocol version {response_version}, expected {version}")
        }
        Some(Response::Error { code, message }) => bail!("{code}: {message}"),
        Some(other) => unexpected(other),
        None => bail!("server closed protocol handshake response"),
    }
}

fn load_auth_token(path: &Path, explicit: Option<&str>) -> Result<Option<String>> {
    if let Some(token) = explicit {
        if token.trim().len() < 32 {
            bail!("--auth-token is too short");
        }
        return Ok(Some(token.trim().to_owned()));
    }
    reject_symlink(path)?;
    match read_file_limited(path, 4096) {
        Ok(contents) => {
            require_private_mode(path, "auth token")?;
            let contents = String::from_utf8(contents)
                .with_context(|| format!("decode auth token file {}", path.display()))?;
            let token = contents.trim();
            if token.len() < 32 || token.len() > 4096 {
                bail!("auth token file {} has an invalid length", path.display());
            }
            Ok(Some(token.to_owned()))
        }
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Read the token file for every newly established connection. This lets a
/// long-lived CLI/agent adapter recover after an operator atomically rotates
/// the server credential. An explicitly supplied token deliberately bypasses
/// the file so scripted callers retain deterministic credentials.
fn current_auth_token(fallback: Option<&str>) -> Result<Option<String>> {
    if fallback.is_none() {
        return Ok(None);
    }
    if let Some(Some(path)) = AUTH_TOKEN_FILE.get() {
        return load_auth_token(path, None);
    }
    Ok(fallback.map(str::to_owned))
}

/// Bound QUIC stream admission separately from the connection handshake. A
/// peer can exhaust its advertised bidirectional-stream limit or flow-control
/// budget without closing the connection; every request path must fail and
/// retry rather than leaving an agent task waiting indefinitely.
async fn open_bi_with_timeout(conn: &Connection) -> Result<(SendStream, RecvStream)> {
    tokio::time::timeout(REQUEST_STREAM_OPEN_TIMEOUT, conn.open_bi())
        .await
        .context("request stream open timeout")?
        .context("open QUIC bidirectional request stream")
}

async fn one(conn: &Connection, request: Request) -> Result<Response> {
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    write_request_frame(conn, &mut send, request).await?;
    send.finish()?;
    let response = tokio::time::timeout(
        REQUEST_RESPONSE_TIMEOUT,
        read_response_frame(conn, &mut recv),
    )
    .await
    .context("one-shot response timeout")??;
    response.ok_or_else(|| anyhow!("server closed response stream"))
}

async fn ack_events_consumer(
    conn: &Connection,
    session_id: Uuid,
    consumer_id: &str,
    through_event_id: u64,
) -> Result<Option<u64>> {
    if !connection_supports_feature(conn, "event_consumer_leases") {
        return Ok(None);
    }
    let response = one(
        conn,
        Request::AckEventsConsumer {
            session_id,
            consumer_id: consumer_id.to_owned(),
            through_event_id,
        },
    )
    .await?;
    match response {
        Response::Acked {
            through_event_id: acknowledged,
        } => {
            if acknowledged < through_event_id {
                bail!("server returned an event ACK below the requested cursor");
            }
            Ok(Some(acknowledged))
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => unexpected(other),
    }
}

/// Coalesce high-rate event cursors and send acknowledgements on independent
/// QUIC streams.  Event delivery never waits on this task, so a 300 ms path
/// cannot add one RTT per output chunk; at most one bounded ACK request is in
/// flight and newer cursors replace queued values.
fn spawn_event_ack_worker(
    conn: Connection,
    session_id: Uuid,
    consumer_id: String,
) -> Option<mpsc::Sender<u64>> {
    if !connection_supports_feature(&conn, "event_consumer_leases") {
        return None;
    }
    let (sender, mut receiver) = mpsc::channel::<u64>(32);
    tokio::spawn(async move {
        while let Some(mut pending) = receiver.recv().await {
            let mut coalesce = Box::pin(tokio::time::sleep(EVENT_ACK_COALESCE));
            loop {
                tokio::select! {
                    next = receiver.recv() => match next {
                        Some(next) => pending = pending.max(next),
                        None => break,
                    },
                    _ = &mut coalesce => break,
                }
            }
            match ack_events_consumer(&conn, session_id, &consumer_id, pending).await {
                Ok(Some(_)) | Ok(None) => {}
                Err(error) => {
                    // A subscription reconnect creates a fresh worker. Keep
                    // this task non-fatal for transient losses and avoid
                    // turning an advisory retention heartbeat into a stream
                    // delivery failure.
                    eprintln!("event consumer acknowledgement failed: {error}");
                }
            }
        }
    });
    Some(sender)
}

/// Apply the same Quinn stream class on client-originated request streams that
/// the server applies to their responses. Bulk uploads/downloads therefore do
/// not monopolize a connection carrying interactive PTY/control traffic.
async fn write_request_frame(
    connection: &Connection,
    send: &mut SendStream,
    request: Request,
) -> Result<()> {
    write_request_frame_for_version(send, request, frame_version_for_connection(connection)).await
}

async fn write_request_frame_for_version(
    send: &mut SendStream,
    request: Request,
    version: u16,
) -> Result<()> {
    let _ = send.set_priority(quic_stream_priority(&request));
    let payload = encode_request_frame_payload(request, version).await?;
    let length = u32::try_from(payload.len()).context("request frame length exceeds u32")?;
    let timeout = request_frame_write_timeout(payload.len());
    tokio::time::timeout(timeout, async {
        send.write_all(&length.to_be_bytes()).await?;
        send.write_all(&payload).await?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .with_context(|| format!("request frame write timeout after {timeout:?}"))??;
    Ok(())
}

/// Bound a client write when a peer stops consuming a stream without closing
/// the QUIC connection. The minimum-rate contract mirrors the server's body
/// receive deadline: small control frames get a ten-second floor, while large
/// uploads receive proportional time up to five minutes.
fn request_frame_write_timeout(payload_bytes: usize) -> Duration {
    let proportional_seconds = (payload_bytes as u64)
        .saturating_add(REQUEST_FRAME_MIN_RATE_BYTES_PER_SECOND - 1)
        / REQUEST_FRAME_MIN_RATE_BYTES_PER_SECOND;
    let seconds = proportional_seconds
        .max(REQUEST_FRAME_MIN_WRITE_TIMEOUT.as_secs())
        .min(REQUEST_FRAME_MAX_WRITE_TIMEOUT.as_secs());
    Duration::from_secs(seconds)
}

/// Keep small control requests on the fast inline path, while moving the
/// potentially expensive serialization of body-bearing requests to Tokio's
/// blocking pool.  Serializing a multi-megabyte FILE/ARTIFACT/PATCH value
/// before the existing codec offload would otherwise monopolize the client
/// reactor and delay PTY/control traffic sharing the same connection.
async fn encode_request_frame_payload(value: Request, version: u16) -> Result<Vec<u8>> {
    if request_has_large_body(&value) {
        return tokio::task::spawn_blocking(move || {
            let message = encode_message(&value)?;
            encode_frame_payload_for_version(&message, version)
        })
        .await
        .context("request frame codec task failed")?;
    }
    let message = encode_message(&value)?;
    if !should_attempt_frame_compression(&message) && message.len() < FRAME_CODEC_OFFLOAD_MIN_BYTES
    {
        return encode_frame_payload_for_version(&message, version);
    }
    tokio::task::spawn_blocking(move || encode_frame_payload_for_version(&message, version))
        .await
        .context("request frame codec task failed")?
}

fn request_has_large_body(request: &Request) -> bool {
    match request {
        Request::FilePut { data, .. }
        | Request::FilePutStreamChunk { data, .. }
        | Request::FilePatch {
            replacement: data, ..
        }
        | Request::ArtifactPutStreamChunk { data, .. }
        | Request::PtyInput { data, .. }
        | Request::PtyInputSequenced { data, .. } => data.len() >= REQUEST_BODY_OFFLOAD_THRESHOLD,
        Request::Exec { command, .. }
        | Request::ExecSummary { command, .. }
        | Request::Spawn { command, .. } => command.len() >= REQUEST_BODY_OFFLOAD_THRESHOLD,
        Request::WorkspaceState {
            workspace,
            searches,
            read_paths,
            ..
        } => {
            workspace
                .len()
                .saturating_add(searches.iter().map(String::len).sum::<usize>())
                .saturating_add(read_paths.iter().map(String::len).sum::<usize>())
                >= REQUEST_BODY_OFFLOAD_THRESHOLD
        }
        Request::FilePatchRanges { ranges, .. } => {
            ranges.iter().fold(0usize, |total, range| {
                total.saturating_add(range.replacement.len())
            }) >= REQUEST_BODY_OFFLOAD_THRESHOLD
        }
        _ => false,
    }
}

async fn read_response_frame(
    connection: &Connection,
    recv: &mut RecvStream,
) -> Result<Option<Response>> {
    let version = frame_version_for_connection(connection);
    let Some(payload) = read_frame_payload_for_version(recv, version).await? else {
        return Ok(None);
    };
    // Large responses can spend noticeable CPU in zlib/Postcard. Decode those
    // bytes off the Tokio reactor so a bulk log/file response cannot delay
    // interactive control traffic on another stream. Small plain and legacy
    // frames retain the direct path.
    if should_offload_response_codec(&payload, version) {
        return tokio::task::spawn_blocking(move || {
            let decoded = decode_frame_payload_for_version(&payload, version)?;
            Ok::<Option<Response>, anyhow::Error>(Some(decode_message(&decoded)?))
        })
        .await
        .context("response frame decompression task failed")?;
    }
    let decoded = decode_frame_payload_for_version(&payload, version)?;
    Ok(Some(decode_message(&decoded)?))
}

fn should_offload_response_codec(payload: &[u8], version: u16) -> bool {
    let compressed =
        version == PROTOCOL_VERSION && payload.len() >= FRAME_HEADER_BYTES && payload[2] == 1;
    compressed || payload.len() >= PLAIN_RESPONSE_CODEC_OFFLOAD_MIN_BYTES
}

async fn local_file_sha256(path: &Path) -> Result<(u64, String)> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; FILE_STREAM_CHUNK_BYTES];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| anyhow!("file size overflow"))?;
        if total > STREAM_FILE_MAX_BYTES {
            bail!("file exceeds streamed limit of {STREAM_FILE_MAX_BYTES} bytes");
        }
        hasher.update(&buffer[..read]);
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.as_bytes().iter().all(u8::is_ascii_hexdigit)
}

fn reject_symlink(path: &Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        bail!(
            "refusing to follow download sidecar symlink {}",
            path.display()
        );
    }
    Ok(())
}

fn read_file_limited(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    reject_symlink(path)?;
    // The preflight symlink check is useful for diagnostics, but the
    // descriptor must also refuse a final-component replacement that races
    // it. This protects pinned certificates, auth tokens, and saved cursors.
    let file = {
        #[cfg(unix)]
        {
            let mut options = OpenOptions::new();
            options.read(true).custom_flags(libc::O_NOFOLLOW);
            options.open(path)?
        }
        #[cfg(not(unix))]
        {
            std::fs::File::open(path)?
        }
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!(
            "client state path is not a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > maximum {
        bail!(
            "client state file {} exceeds the {} byte safety limit",
            path.display(),
            maximum
        );
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len()).context("client state file is too large")?,
    );
    let mut limited = file.take(maximum.saturating_add(1));
    limited.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        bail!(
            "client state file {} grew beyond the {} byte safety limit",
            path.display(),
            maximum
        );
    }
    Ok(bytes)
}

fn read_private_file_limited(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>> {
    let bytes = read_file_limited(path, maximum)?;
    require_private_mode(path, label)?;
    Ok(bytes)
}

fn require_private_mode(path: &Path, label: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("{label} {} is not a regular file", path.display());
        }
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "{label} {} must not be group/world accessible (mode {:o})",
                path.display(),
                mode & 0o777
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, label);
    }
    Ok(())
}

async fn read_file_limited_async(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    reject_symlink(path)?;
    // Match the synchronous reader's no-follow guarantee for checkpoint and
    // upload/download metadata. A path-only check can be raced by a rename.
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path).await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() {
        bail!(
            "client state path is not a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > maximum {
        bail!(
            "client state file {} exceeds the {} byte safety limit",
            path.display(),
            maximum
        );
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len()).context("client state file is too large")?,
    );
    let mut limited = file.take(maximum.saturating_add(1));
    limited.read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > maximum {
        bail!(
            "client state file {} grew beyond the {} byte safety limit",
            path.display(),
            maximum
        );
    }
    Ok(bytes)
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Persist a download checkpoint atomically. A torn metadata file must never
/// make the next invocation append bytes using an untrusted offset or digest.
async fn write_download_checkpoint(path: &Path, checkpoint: &DownloadCheckpoint) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let result = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary).await?;
        let contents = serde_json::to_vec(checkpoint)?;
        file.write_all(&contents).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .await?;
        }
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, path).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

async fn write_artifact_download_checkpoint(
    path: &Path,
    checkpoint: &ArtifactDownloadCheckpoint,
) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    reject_symlink(path)?;
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let result = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary).await?;
        let contents = serde_json::to_vec(checkpoint)?;
        file.write_all(&contents).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .await?;
        }
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, path).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

fn upload_checkpoint_paths(local: &Path) -> (PathBuf, PathBuf) {
    (
        local.with_extension("asp-upload"),
        local.with_extension("asp-upload.lock"),
    )
}

/// Open the per-destination upload lock and recover a stable request ID when
/// a previous client process left a checkpoint behind. Keeping the request ID
/// across processes is what lets the server's durable prefix survive a client
/// crash, not only a transient QUIC reconnect.
#[allow(clippy::too_many_arguments)]
fn prepare_upload_checkpoint(
    local: &Path,
    server: &str,
    session_id: Uuid,
    remote: &str,
    total_size: u64,
    sha256: &str,
    expected_sha256: Option<&str>,
    allow_blind: bool,
) -> Result<(UploadCheckpoint, std::fs::File, bool)> {
    let (metadata_path, lock_path) = upload_checkpoint_paths(local);
    reject_symlink(&metadata_path)?;
    reject_symlink(&lock_path)?;
    if let Some(parent) = local
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let lock = {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        options
            .open(&lock_path)
            .with_context(|| format!("open upload lock {}", lock_path.display()))?
    };
    lock.lock_exclusive()
        .with_context(|| format!("lock upload source {}", local.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }

    let matches = match read_file_limited(&metadata_path, MAX_CLIENT_METADATA_BYTES) {
        Ok(data) => serde_json::from_slice::<UploadCheckpoint>(&data)
            .ok()
            .filter(|checkpoint| {
                checkpoint.server == server
                    && checkpoint.session_id == session_id
                    && checkpoint.remote == remote
                    && checkpoint.total_size == total_size
                    && checkpoint.sha256 == sha256
                    && checkpoint.expected_sha256.as_deref() == expected_sha256
                    && checkpoint.allow_blind == allow_blind
            }),
        Err(error) if is_not_found(&error) => None,
        Err(error) => return Err(error),
    };
    if let Some(checkpoint) = matches {
        return Ok((checkpoint, lock, true));
    }

    let checkpoint = UploadCheckpoint {
        server: server.to_owned(),
        session_id,
        remote: remote.to_owned(),
        total_size,
        sha256: sha256.to_owned(),
        request_id: Uuid::new_v4(),
        expected_sha256: expected_sha256.map(str::to_owned),
        allow_blind,
    };
    write_upload_checkpoint(&metadata_path, &checkpoint)?;
    Ok((checkpoint, lock, false))
}

fn write_upload_checkpoint(path: &Path, checkpoint: &UploadCheckpoint) -> Result<()> {
    let data = serde_json::to_vec(checkpoint)?;
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        reject_symlink(&temporary)?;
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("create upload checkpoint {}", temporary.display()))?;
        file.write_all(&data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::File::open(parent)?.sync_data()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn clear_upload_checkpoint(path: &Path) -> Result<()> {
    reject_symlink(path)?;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::File::open(parent)?.sync_data()?;
    }
    Ok(())
}

fn artifact_upload_checkpoint_paths(local: &Path) -> (PathBuf, PathBuf) {
    (
        local.with_extension("asp-artifact-upload"),
        local.with_extension("asp-artifact-upload.lock"),
    )
}

fn prepare_artifact_upload_checkpoint(
    local: &Path,
    server: &str,
    session_id: Uuid,
    artifact_id: &str,
    total_size: u64,
    name: Option<&str>,
) -> Result<(ArtifactUploadCheckpoint, std::fs::File, bool)> {
    let (metadata_path, lock_path) = artifact_upload_checkpoint_paths(local);
    reject_symlink(&metadata_path)?;
    reject_symlink(&lock_path)?;
    if let Some(parent) = local
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let lock = {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        options
            .open(&lock_path)
            .with_context(|| format!("open artifact upload lock {}", local.display()))?
    };
    lock.lock_exclusive()
        .with_context(|| format!("lock artifact upload source {}", local.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let artifact_id = artifact_id.to_ascii_lowercase();
    let matches = match read_file_limited(&metadata_path, MAX_CLIENT_METADATA_BYTES) {
        Ok(data) => serde_json::from_slice::<ArtifactUploadCheckpoint>(&data)
            .ok()
            .filter(|checkpoint| {
                checkpoint.server == server
                    && checkpoint.session_id == session_id
                    && checkpoint.artifact_id == artifact_id
                    && checkpoint.total_size == total_size
                    && checkpoint.name.as_deref() == name
            }),
        Err(error) if is_not_found(&error) => None,
        Err(error) => return Err(error),
    };
    if let Some(checkpoint) = matches {
        return Ok((checkpoint, lock, true));
    }
    let checkpoint = ArtifactUploadCheckpoint {
        server: server.to_owned(),
        session_id,
        artifact_id,
        total_size,
        request_id: Uuid::new_v4(),
        name: name.map(str::to_owned),
    };
    write_artifact_upload_checkpoint(&metadata_path, &checkpoint)?;
    Ok((checkpoint, lock, false))
}

fn write_artifact_upload_checkpoint(
    path: &Path,
    checkpoint: &ArtifactUploadCheckpoint,
) -> Result<()> {
    let data = serde_json::to_vec(checkpoint)?;
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        reject_symlink(&temporary)?;
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary).with_context(|| {
            format!("create artifact upload checkpoint {}", temporary.display())
        })?;
        file.write_all(&data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::File::open(parent)?.sync_data()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn clear_artifact_upload_checkpoint(path: &Path) -> Result<()> {
    clear_upload_checkpoint(path)
}

async fn receive_file_stream(
    conn: &Connection,
    session_id: Uuid,
    remote: &str,
    offset: u64,
    expected: Option<(u64, &str)>,
    temporary: &Path,
    metadata_path: &Path,
) -> Result<StreamFileInfo> {
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    write_request_frame(
        conn,
        &mut send,
        Request::FileGetStream {
            session_id,
            path: (*remote).to_owned(),
            offset,
            length: None,
        },
    )
    .await?;
    send.finish()?;
    let first = read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed file stream before BEGIN"))?;
    let info = match first {
        Response::FileStreamBegin {
            path,
            total_size,
            offset,
            length,
            sha256,
            ..
        } => {
            if path != remote {
                bail!("file stream path mismatch");
            }
            if total_size > STREAM_FILE_MAX_BYTES
                || offset > total_size
                || length > total_size.saturating_sub(offset)
            {
                bail!("invalid file stream bounds");
            }
            if let Some((expected_size, expected_sha256)) = expected
                && (expected_size != total_size || expected_sha256 != sha256)
            {
                bail!("remote file changed while resuming");
            }
            if sha256.len() != 64 || !sha256.as_bytes().iter().all(u8::is_ascii_hexdigit) {
                bail!("invalid SHA-256 in file stream header");
            }
            let checkpoint = DownloadCheckpoint {
                remote: remote.to_owned(),
                total_size,
                sha256: sha256.clone(),
            };
            write_download_checkpoint(metadata_path, &checkpoint).await?;
            StreamFileInfo {
                total_size,
                offset,
                length,
                sha256,
            }
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected file stream response: {other:?}"),
    };
    if info.offset != offset {
        bail!("file stream resumed at {}, expected {offset}", info.offset);
    }
    let mut output = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(temporary)
        .await?;
    let mut received = 0_u64;
    loop {
        let response = read_response_frame(conn, &mut recv)
            .await?
            .ok_or_else(|| anyhow!("server closed file stream before END"))?;
        match response {
            Response::FileStreamChunk { offset, data } => {
                if data.is_empty() || data.len() > FILE_STREAM_CHUNK_BYTES {
                    bail!("invalid streamed file chunk");
                }
                if offset != info.offset + received {
                    bail!("file stream chunk offset mismatch");
                }
                received = received
                    .checked_add(data.len() as u64)
                    .ok_or_else(|| anyhow!("file stream byte count overflow"))?;
                if received > info.length {
                    bail!("file stream exceeded declared range");
                }
                output.write_all(&data).await?;
            }
            Response::FileStreamEnd { bytes, sha256 } => {
                if bytes != received || received != info.length || sha256 != info.sha256 {
                    bail!("file stream end metadata mismatch");
                }
                output.sync_data().await?;
                return Ok(info);
            }
            Response::Error { code, message } => bail!("{code}: {message}"),
            other => bail!("unexpected response in file stream: {other:?}"),
        }
    }
}

async fn download_file_with_retry(
    conn: &mut Connection,
    retry: &RetryContext<'_>,
    state: &mut SavedSession,
    remote: &str,
    local: &Path,
) -> Result<StreamFileInfo> {
    // FILE_GET_STREAM carries its own hash/offset checkpoint. A reconnect
    // therefore needs only HELLO before requesting the next range.
    let temporary = local.with_extension("asp-download");
    let metadata_path = local.with_extension("asp-download.meta");
    let lock_path = local.with_extension("asp-download.lock");
    reject_symlink(&temporary)?;
    reject_symlink(&metadata_path)?;
    reject_symlink(&lock_path)?;
    if let Some(parent) = local
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open download lock {}", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("lock download destination {}", local.display()))?;
    let mut attempts = 0_u8;
    loop {
        let (offset, expected) =
            match read_file_limited_async(&metadata_path, MAX_CLIENT_METADATA_BYTES).await {
                Ok(metadata) => {
                    let checkpoint = match serde_json::from_slice::<DownloadCheckpoint>(&metadata) {
                        Ok(checkpoint) => checkpoint,
                        Err(error) => {
                            let _ = tokio::fs::remove_file(&temporary).await;
                            let _ = tokio::fs::remove_file(&metadata_path).await;
                            return Err(anyhow!("invalid download metadata: {error}"));
                        }
                    };
                    if checkpoint.remote != remote
                        || checkpoint.total_size > STREAM_FILE_MAX_BYTES
                        || checkpoint.sha256.len() != 64
                        || !checkpoint
                            .sha256
                            .as_bytes()
                            .iter()
                            .all(u8::is_ascii_hexdigit)
                    {
                        let _ = tokio::fs::remove_file(&temporary).await;
                        let _ = tokio::fs::remove_file(&metadata_path).await;
                        (0, None)
                    } else {
                        match tokio::fs::metadata(&temporary).await {
                            Ok(metadata) if metadata.len() <= checkpoint.total_size => (
                                metadata.len(),
                                Some((checkpoint.total_size, checkpoint.sha256)),
                            ),
                            Ok(_) => {
                                let _ = tokio::fs::remove_file(&temporary).await;
                                let _ = tokio::fs::remove_file(&metadata_path).await;
                                (0, None)
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                let _ = tokio::fs::remove_file(&temporary).await;
                                let _ = tokio::fs::remove_file(&metadata_path).await;
                                (0, None)
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
                Err(error) if is_not_found(&error) => {
                    // A temporary file can survive a crash before its checkpoint
                    // was published. Never append to that untrusted prefix; start
                    // the transfer from a clean file.
                    let _ = tokio::fs::remove_file(&temporary).await;
                    (0, None)
                }
                Err(error) => return Err(error),
            };
        let expected_ref = expected
            .as_ref()
            .map(|(size, digest)| (*size, digest.as_str()));
        match receive_file_stream(
            conn,
            state.session_id,
            remote,
            offset,
            expected_ref,
            &temporary,
            &metadata_path,
        )
        .await
        {
            Ok(info) => {
                let actual_size = tokio::fs::metadata(&temporary).await?.len();
                if actual_size != info.total_size {
                    bail!(
                        "download ended at {actual_size} bytes, expected {}",
                        info.total_size
                    );
                }
                let (_, actual_digest) = local_file_sha256(&temporary).await?;
                if actual_digest != info.sha256 {
                    bail!("download SHA-256 mismatch");
                }
                if let Some(parent) = local
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::rename(&temporary, local).await?;
                let _ = tokio::fs::remove_file(&metadata_path).await;
                return Ok(info);
            }
            Err(error) if attempts < 3 && retryable_connection_error(&error) => {
                attempts += 1;
                conn.close(0_u32.into(), b"file download retry");
                *conn = reconnect_without_resume_with_retry(*retry, state).await?;
            }
            Err(error) => {
                if !retryable_connection_error(&error) {
                    let _ = tokio::fs::remove_file(&temporary).await;
                    let _ = tokio::fs::remove_file(&metadata_path).await;
                }
                return Err(error);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn receive_artifact_stream(
    conn: &Connection,
    server: &str,
    session_id: Uuid,
    artifact_id: &str,
    offset: u64,
    length: Option<u64>,
    expected: Option<(u64, &str)>,
    max_length: Option<u64>,
    temporary: &Path,
    metadata_path: Option<&Path>,
) -> Result<ArtifactStreamInfo> {
    let artifact_id = artifact_id.to_ascii_lowercase();
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    write_request_frame(
        conn,
        &mut send,
        Request::ArtifactGetStream {
            session_id,
            artifact_id: artifact_id.clone(),
            offset,
            length,
        },
    )
    .await?;
    send.finish()?;
    let first = read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed artifact stream before BEGIN"))?;
    let info = match first {
        Response::ArtifactStreamBegin {
            artifact_id: received_id,
            total_size,
            offset,
            length,
            sha256,
            name,
        } => {
            if received_id != artifact_id || sha256 != artifact_id {
                bail!("artifact stream identity mismatch");
            }
            if total_size > ARTIFACT_MAX_BYTES
                || offset > total_size
                || length > total_size.saturating_sub(offset)
            {
                bail!("invalid artifact stream bounds");
            }
            if max_length.is_some_and(|maximum| length > maximum) {
                bail!("artifact range exceeds client response limit");
            }
            if let Some((expected_size, expected_id)) = expected
                && (expected_size != total_size || expected_id != artifact_id)
            {
                bail!("artifact changed while resuming");
            }
            if let Some(metadata_path) = metadata_path {
                let checkpoint = ArtifactDownloadCheckpoint {
                    server: server.to_owned(),
                    session_id,
                    artifact_id: artifact_id.clone(),
                    total_size,
                };
                write_artifact_download_checkpoint(metadata_path, &checkpoint).await?;
            }
            ArtifactStreamInfo {
                artifact_id,
                total_size,
                offset,
                length,
                name,
            }
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected response in artifact stream: {other:?}"),
    };
    if info.offset != offset {
        bail!(
            "artifact stream resumed at {}, expected {offset}",
            info.offset
        );
    }
    reject_symlink(temporary)?;
    let mut output_options = tokio::fs::OpenOptions::new();
    output_options.create(true).append(true);
    #[cfg(unix)]
    output_options.custom_flags(libc::O_NOFOLLOW);
    let mut output = output_options.open(temporary).await?;
    let mut received = 0_u64;
    loop {
        let response = read_response_frame(conn, &mut recv)
            .await?
            .ok_or_else(|| anyhow!("server closed artifact stream before END"))?;
        match response {
            Response::ArtifactStreamChunk { offset, data } => {
                if data.is_empty() || data.len() > FILE_STREAM_CHUNK_BYTES {
                    bail!("invalid streamed artifact chunk");
                }
                if offset != info.offset + received {
                    bail!("artifact stream chunk offset mismatch");
                }
                received = received
                    .checked_add(data.len() as u64)
                    .ok_or_else(|| anyhow!("artifact stream byte count overflow"))?;
                if received > info.length {
                    bail!("artifact stream exceeded declared range");
                }
                output.write_all(&data).await?;
            }
            Response::ArtifactStreamEnd { bytes, sha256 } => {
                if bytes != received || received != info.length || sha256 != info.artifact_id {
                    bail!("artifact stream end metadata mismatch");
                }
                output.sync_data().await?;
                return Ok(info);
            }
            Response::Error { code, message } => bail!("{code}: {message}"),
            other => bail!("unexpected response in artifact stream: {other:?}"),
        }
    }
}

async fn download_artifact_with_retry(
    conn: &mut Connection,
    retry: &RetryContext<'_>,
    state: &mut SavedSession,
    artifact_id: &str,
    local: &Path,
    offset: u64,
    length: Option<u64>,
) -> Result<ArtifactStreamInfo> {
    // Immutable artifact ranges are independently addressed by digest and
    // offset; do not pay a session-journal replay on stream retry.
    let artifact_id = artifact_id.to_ascii_lowercase();
    if !valid_sha256(&artifact_id) {
        bail!("artifact_id must be a 64-character hexadecimal SHA-256 digest");
    }
    // Explicit ranges are one-shot materializations. Full-object downloads
    // use durable sidecars and resume after a client/transport interruption.
    if offset != 0 || length.is_some() {
        if let Some(parent) = local
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let temporary = local.with_extension("asp-artifact-range");
        reject_symlink(&temporary)?;
        let _ = tokio::fs::remove_file(&temporary).await;
        let info = receive_artifact_stream(
            conn,
            retry.server,
            state.session_id,
            &artifact_id,
            offset,
            length,
            None,
            None,
            &temporary,
            None,
        )
        .await?;
        reject_symlink(local)?;
        tokio::fs::rename(&temporary, local).await?;
        return Ok(info);
    }

    let temporary = local.with_extension("asp-artifact-download");
    let metadata_path = local.with_extension("asp-artifact-download.meta");
    let lock_path = local.with_extension("asp-artifact-download.lock");
    reject_symlink(&temporary)?;
    reject_symlink(&metadata_path)?;
    reject_symlink(&lock_path)?;
    if let Some(parent) = local
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open artifact download lock {}", local.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("lock artifact download destination {}", local.display()))?;
    let mut attempts = 0_u8;
    loop {
        let (offset, expected) =
            match read_file_limited_async(&metadata_path, MAX_CLIENT_METADATA_BYTES).await {
                Ok(metadata) => {
                    let checkpoint =
                        match serde_json::from_slice::<ArtifactDownloadCheckpoint>(&metadata) {
                            Ok(checkpoint) => checkpoint,
                            Err(error) => {
                                let _ = tokio::fs::remove_file(&temporary).await;
                                let _ = tokio::fs::remove_file(&metadata_path).await;
                                return Err(anyhow!("invalid artifact download metadata: {error}"));
                            }
                        };
                    if checkpoint.server != retry.server
                        || checkpoint.session_id != state.session_id
                        || checkpoint.artifact_id != artifact_id
                        || checkpoint.total_size > ARTIFACT_MAX_BYTES
                    {
                        let _ = tokio::fs::remove_file(&temporary).await;
                        let _ = tokio::fs::remove_file(&metadata_path).await;
                        (0, None)
                    } else {
                        match tokio::fs::metadata(&temporary).await {
                            Ok(metadata) if metadata.len() <= checkpoint.total_size => (
                                metadata.len(),
                                Some((checkpoint.total_size, checkpoint.artifact_id)),
                            ),
                            Ok(_) => {
                                let _ = tokio::fs::remove_file(&temporary).await;
                                let _ = tokio::fs::remove_file(&metadata_path).await;
                                (0, None)
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                let _ = tokio::fs::remove_file(&temporary).await;
                                let _ = tokio::fs::remove_file(&metadata_path).await;
                                (0, None)
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
                Err(error) if is_not_found(&error) => {
                    let _ = tokio::fs::remove_file(&temporary).await;
                    (0, None)
                }
                Err(error) => return Err(error),
            };
        let expected_ref = expected
            .as_ref()
            .map(|(size, artifact_id)| (*size, artifact_id.as_str()));
        match receive_artifact_stream(
            conn,
            retry.server,
            state.session_id,
            &artifact_id,
            offset,
            None,
            expected_ref,
            None,
            &temporary,
            Some(&metadata_path),
        )
        .await
        {
            Ok(info) => {
                let actual_size = tokio::fs::metadata(&temporary).await?.len();
                if actual_size != info.total_size {
                    bail!(
                        "artifact download ended at {actual_size} bytes, expected {}",
                        info.total_size
                    );
                }
                let (_, actual_digest) = local_file_sha256(&temporary).await?;
                if actual_digest != info.artifact_id {
                    bail!("artifact download SHA-256 mismatch");
                }
                tokio::fs::rename(&temporary, local).await?;
                let _ = tokio::fs::remove_file(&metadata_path).await;
                return Ok(info);
            }
            Err(error) if attempts < 3 && retryable_connection_error(&error) => {
                attempts += 1;
                conn.close(0_u32.into(), b"artifact download retry");
                *conn = reconnect_without_resume_with_retry(*retry, state).await?;
            }
            Err(error) => {
                if !retryable_connection_error(&error) {
                    let _ = tokio::fs::remove_file(&temporary).await;
                    let _ = tokio::fs::remove_file(&metadata_path).await;
                }
                return Err(error);
            }
        }
    }
}

async fn send_file_stream(conn: &Connection, transfer: &UploadTransfer<'_>) -> Result<Response> {
    let UploadTransfer {
        session_id,
        request_id,
        remote,
        local,
        total_size,
        digest,
        expected_sha256,
        allow_blind,
        resume,
    } = transfer;
    // Open the source before advertising a new upload stream. If the local
    // file disappeared between digesting and sending, fail locally instead of
    // leaving a server-side request waiting for a timeout.
    let mut input = tokio::fs::File::open(local).await?;
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    let begin = if *resume {
        Request::FilePutStreamResumeBegin {
            session_id: *session_id,
            request_id: *request_id,
            path: (*remote).to_owned(),
            total_size: *total_size,
            sha256: (*digest).to_owned(),
            expected_sha256: expected_sha256.map(str::to_owned),
            allow_blind: *allow_blind,
        }
    } else {
        Request::FilePutStreamBegin {
            session_id: *session_id,
            request_id: *request_id,
            path: (*remote).to_owned(),
            total_size: *total_size,
            sha256: (*digest).to_owned(),
            expected_sha256: expected_sha256.map(str::to_owned),
            allow_blind: *allow_blind,
        }
    };
    write_request_frame(conn, &mut send, begin).await?;
    let mut offset = 0_u64;
    if *resume {
        let response = read_response_frame(conn, &mut recv)
            .await?
            .ok_or_else(|| anyhow!("server closed resumable upload stream"))?;
        match response {
            Response::FileUploadReady {
                path,
                total_size: server_size,
                offset: server_offset,
                sha256: server_digest,
            } => {
                if path != *remote
                    || server_size != *total_size
                    || server_digest != *digest
                    || server_offset > *total_size
                {
                    bail!("server resumable upload metadata mismatch");
                }
                input.seek(std::io::SeekFrom::Start(server_offset)).await?;
                offset = server_offset;
            }
            Response::FileStored { .. } => {
                send.finish()?;
                return Ok(response);
            }
            Response::Error { code, message } => bail!("{code}: {message}"),
            other => bail!("unexpected resumable upload response: {other:?}"),
        }
    }
    // The begin/ready exchange is control traffic; the remaining chunks are
    // bulk. Set the class once before the high-rate loop rather than taking a
    // Quinn state lock for every 64 KiB continuation frame.
    let _ = send.set_priority(QUIC_STREAM_PRIORITY_BULK);
    if *resume {
        // Let the server return from its durable-prefix scan before the first
        // continuation burst. This is a scheduling handoff, not transport
        // flow control; Quinn remains responsible for pacing and recovery.
        tokio::task::yield_now().await;
    }
    let mut buffer = vec![0_u8; FILE_STREAM_CHUNK_BYTES];
    let mut resumed_chunks = 0_usize;
    loop {
        let read = input.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        write_request_frame(
            conn,
            &mut send,
            Request::FilePutStreamChunk {
                offset,
                data: buffer[..read].to_vec(),
            },
        )
        .await?;
        offset += read as u64;
        if *resume {
            resumed_chunks = resumed_chunks.saturating_add(1);
            if resumed_chunks >= RESUMED_UPLOAD_PACING_CHUNKS {
                tokio::time::sleep(RESUMED_UPLOAD_PACING).await;
                resumed_chunks = 0;
            }
        }
    }
    write_request_frame(conn, &mut send, Request::FilePutStreamEnd).await?;
    send.finish()?;
    read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed file upload stream"))
}

#[allow(clippy::too_many_arguments)]
async fn send_artifact_stream(
    conn: &Connection,
    session_id: Uuid,
    request_id: Uuid,
    artifact_id: &str,
    total_size: u64,
    name: Option<&str>,
    local: &Path,
    resume: bool,
) -> Result<Response> {
    let mut input = tokio::fs::File::open(local).await?;
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    let begin = if resume {
        Request::ArtifactPutStreamResumeBegin {
            session_id,
            request_id,
            artifact_id: artifact_id.to_owned(),
            total_size,
            name: name.map(str::to_owned),
        }
    } else {
        Request::ArtifactPutStreamBegin {
            session_id,
            request_id,
            artifact_id: artifact_id.to_owned(),
            total_size,
            name: name.map(str::to_owned),
        }
    };
    write_request_frame(conn, &mut send, begin).await?;
    let response = read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed artifact upload stream before readiness"))?;
    let mut offset = match response {
        Response::ArtifactUploadReady {
            artifact_id: server_id,
            total_size: server_size,
            offset: server_offset,
        } => {
            if server_id != artifact_id || server_size != total_size || server_offset > total_size {
                bail!("server artifact upload metadata mismatch");
            }
            if !resume && server_offset != 0 {
                bail!("fresh artifact upload returned a nonzero offset");
            }
            input.seek(std::io::SeekFrom::Start(server_offset)).await?;
            server_offset
        }
        Response::ArtifactStored { .. } => {
            send.finish()?;
            return Ok(response);
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected artifact upload response: {other:?}"),
    };
    // The readiness frame is emitted by the server before it re-enters its
    // receive loop.  Yield once after a resumable handshake so a large local
    // source cannot fill Quinn's receive assembler with a burst of
    // continuation packets while the server task is still returning from
    // hashing/validating the durable prefix.  This is a scheduling handoff,
    // not application-level flow control; QUIC remains responsible for
    // pacing and loss recovery once the stream is active.
    if resume {
        tokio::task::yield_now().await;
    }
    let _ = send.set_priority(QUIC_STREAM_PRIORITY_BULK);
    let mut buffer = vec![0_u8; FILE_STREAM_CHUNK_BYTES];
    let mut resumed_chunks = 0_usize;
    loop {
        let read = input.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        write_request_frame(
            conn,
            &mut send,
            Request::ArtifactPutStreamChunk {
                offset,
                data: buffer[..read].to_vec(),
            },
        )
        .await?;
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| anyhow!("artifact upload offset overflow"))?;
        if resume {
            // A resumed stream starts after the server has hashed a durable
            // prefix. Give its receive task a short scheduling window before
            // the next burst; without this, a fast loopback sender can fill
            // Quinn's bounded out-of-order assembler and trigger its
            // `too many gaps in stream buffer` transport guard.
            resumed_chunks = resumed_chunks.saturating_add(1);
            if resumed_chunks >= RESUMED_UPLOAD_PACING_CHUNKS {
                tokio::time::sleep(RESUMED_UPLOAD_PACING).await;
                resumed_chunks = 0;
            }
        }
    }
    write_request_frame(conn, &mut send, Request::ArtifactPutStreamEnd).await?;
    send.finish()?;
    read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed artifact upload stream"))
}

async fn upload_artifact_with_retry(
    conn: &mut Connection,
    retry: &RetryContext<'_>,
    state: &mut SavedSession,
    local: &Path,
    name: Option<String>,
) -> Result<Response> {
    let (total_size, artifact_id) = local_file_sha256(local).await?;
    if total_size > ARTIFACT_MAX_BYTES {
        bail!("artifact exceeds streamed limit of {ARTIFACT_MAX_BYTES} bytes");
    }
    let (checkpoint, _lock, checkpoint_exists) = prepare_artifact_upload_checkpoint(
        local,
        retry.server,
        state.session_id,
        &artifact_id,
        total_size,
        name.as_deref(),
    )?;
    let metadata_path = artifact_upload_checkpoint_paths(local).0;
    let mut attempts = 0_u8;
    loop {
        match send_artifact_stream(
            conn,
            state.session_id,
            checkpoint.request_id,
            &artifact_id,
            total_size,
            name.as_deref(),
            local,
            checkpoint_exists || attempts > 0,
        )
        .await
        {
            Ok(response) => {
                if let Err(error) = clear_artifact_upload_checkpoint(&metadata_path) {
                    eprintln!(
                        "warning: artifact upload completed but checkpoint cleanup failed: {error}"
                    );
                }
                return Ok(response);
            }
            Err(error) if attempts < 3 && retryable_connection_error(&error) => {
                attempts += 1;
                eprintln!(
                    "artifact upload connection interrupted; resuming (attempt {attempts}/3): {error}"
                );
                conn.close(0_u32.into(), b"artifact upload retry");
                *conn = reconnect_with_retry(*retry, state).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn upload_artifact_bytes(
    conn: &Connection,
    session_id: Uuid,
    request_id: Uuid,
    data: &[u8],
    name: Option<&str>,
) -> Result<Response> {
    let artifact_id = format!("{:x}", Sha256::digest(data));
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    write_request_frame(
        conn,
        &mut send,
        Request::ArtifactPutStreamBegin {
            session_id,
            request_id,
            artifact_id: artifact_id.clone(),
            total_size: data.len() as u64,
            name: name.map(str::to_owned),
        },
    )
    .await?;
    let response = read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed artifact upload stream before readiness"))?;
    let start_offset = match response {
        Response::ArtifactUploadReady {
            artifact_id: server_id,
            total_size: server_size,
            offset,
        } if server_id == artifact_id && server_size == data.len() as u64 && offset == 0 => offset,
        Response::ArtifactStored { .. } => {
            send.finish()?;
            return Ok(response);
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected artifact upload response: {other:?}"),
    };
    let _ = send.set_priority(QUIC_STREAM_PRIORITY_BULK);
    for (index, chunk) in data.chunks(FILE_STREAM_CHUNK_BYTES).enumerate() {
        write_request_frame(
            conn,
            &mut send,
            Request::ArtifactPutStreamChunk {
                offset: start_offset + (index * FILE_STREAM_CHUNK_BYTES) as u64,
                data: chunk.to_vec(),
            },
        )
        .await?;
    }
    write_request_frame(conn, &mut send, Request::ArtifactPutStreamEnd).await?;
    send.finish()?;
    read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed artifact upload stream"))
}

async fn agent_artifact_put_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    request_id: Uuid,
    data: Vec<u8>,
    name: Option<String>,
) -> Result<()> {
    let mut attempts = 0_u8;
    let mut auth_refresh_attempted = false;
    loop {
        match upload_artifact_bytes(conn, state.session_id, request_id, &data, name.as_deref())
            .await
        {
            Ok(Response::ArtifactStored {
                artifact_id,
                total_size,
                name,
                event_id,
            }) => {
                emit_agent(serde_json::json!({
                    "type": "artifact_stored",
                    "id": request_label,
                    "request_id": request_id,
                    "artifact_id": artifact_id,
                    "bytes": total_size,
                    "name": name,
                    "event_id": event_id,
                }))?;
                return Ok(());
            }
            Ok(Response::Error { code, .. })
                if code == "authentication_required" && !auth_refresh_attempted =>
            {
                auth_refresh_attempted = true;
                eprintln!("agent artifact PUT credentials changed; reconnecting");
                conn.close(0_u32.into(), b"credentials rotated");
                *conn = reconnect_with_retry(retry, state).await?;
            }
            Ok(Response::Error { code, message }) => bail!("{code}: {message}"),
            Ok(other) => return unexpected(other),
            Err(error) if attempts < 3 && retryable_connection_error(&error) => {
                attempts += 1;
                conn.close(0_u32.into(), b"artifact put retry");
                *conn = reconnect_with_retry(retry, state).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn agent_artifact_get_with_retry(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    request_label: Option<&str>,
    artifact_id: &str,
    offset: u64,
    length: Option<u64>,
) -> Result<()> {
    // Artifact GET is immutable and offset-addressed, so transport retries
    // can reconnect directly after authentication.
    let temporary = std::env::temp_dir().join(format!("asp-agent-artifact-{}", Uuid::new_v4()));
    let mut attempts = 0_u8;
    let mut auth_refresh_attempted = false;
    let result = loop {
        let _ = tokio::fs::remove_file(&temporary).await;
        match receive_artifact_stream(
            conn,
            retry.server,
            state.session_id,
            artifact_id,
            offset,
            length,
            None,
            Some(AGENT_ARTIFACT_MAX_BYTES),
            &temporary,
            None,
        )
        .await
        {
            Ok(info) => {
                let data = tokio::fs::read(&temporary).await?;
                emit_agent(serde_json::json!({
                    "type": "artifact_data",
                    "id": request_label,
                    "artifact_id": info.artifact_id,
                    "name": info.name,
                    "offset": info.offset,
                    "bytes": data.len(),
                    "total_size": info.total_size,
                    "data_base64": BASE64.encode(data),
                }))?;
                break Ok(());
            }
            Err(error) if !auth_refresh_attempted && authentication_refresh_error(&error) => {
                auth_refresh_attempted = true;
                eprintln!("agent artifact GET credentials changed; reconnecting");
                conn.close(0_u32.into(), b"credentials rotated");
                *conn = reconnect_without_resume_with_retry(retry, state).await?;
            }
            Err(error) if attempts < 3 && retryable_connection_error(&error) => {
                attempts += 1;
                conn.close(0_u32.into(), b"artifact get retry");
                *conn = reconnect_without_resume_with_retry(retry, state).await?;
            }
            Err(error) => break Err(error),
        }
    };
    let _ = tokio::fs::remove_file(&temporary).await;
    result
}

async fn upload_file_with_retry(
    conn: &mut Connection,
    retry: &RetryContext<'_>,
    state: &mut SavedSession,
    remote: &str,
    local: &Path,
    expected_sha256: Option<&str>,
    allow_blind: bool,
) -> Result<Response> {
    let mut attempts = 0_u8;
    let (total_size, digest) = local_file_sha256(local).await?;
    let (checkpoint, _checkpoint_lock, checkpoint_exists) = prepare_upload_checkpoint(
        local,
        retry.server,
        state.session_id,
        remote,
        total_size,
        &digest,
        expected_sha256,
        allow_blind,
    )?;
    let request_id = checkpoint.request_id;
    let metadata_path = upload_checkpoint_paths(local).0;
    loop {
        match send_file_stream(
            conn,
            &UploadTransfer {
                session_id: state.session_id,
                request_id,
                remote,
                local,
                total_size,
                digest: &digest,
                expected_sha256,
                allow_blind,
                resume: checkpoint_exists || attempts > 0,
            },
        )
        .await
        {
            Ok(response) => {
                if let Err(error) = clear_upload_checkpoint(&metadata_path) {
                    eprintln!("warning: upload completed but checkpoint cleanup failed: {error}");
                }
                return Ok(response);
            }
            Err(error) if attempts < 2 && retryable_connection_error(&error) => {
                attempts += 1;
                conn.close(0_u32.into(), b"file upload retry");
                *conn = reconnect_with_retry(*retry, state).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn subscribe_events(
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    session_file: &Path,
    requested_after_event_id: Option<u64>,
    process_id: Option<Uuid>,
    include_output: bool,
) -> Result<()> {
    let mut conn = connect_with_retry(server, cert, auth_token).await?;
    // Event subscriptions are long-lived and commonly reconnect after a
    // laptop changes networks.  Keep the endpoint that owns the first
    // attachment when the client uses bearer-token authentication: this
    // preserves the UDP socket and rustls session cache for the next QUIC
    // handshake.  mTLS endpoints intentionally return `None` here so a
    // rotated client certificate is reloaded before reconnecting.
    let reusable_endpoint = clone_client_endpoint(&conn);
    let mut state = ensure_session(&mut conn, server, cert, auth_token, session_file).await?;
    let mut first_subscription = true;
    let mut last_persist = Instant::now();
    let mut reconnect_attempt = 0_u32;
    let consumer_id = client_consumer_id().map(str::to_owned);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    'subscription: loop {
        let after_event_id = if first_subscription {
            requested_after_event_id.unwrap_or(state.last_event_id)
        } else {
            state.last_event_id
        };
        first_subscription = false;
        let mut stream_ready = false;
        let ack_sender = consumer_id.as_ref().and_then(|consumer_id| {
            spawn_event_ack_worker(conn.clone(), state.session_id, consumer_id.clone())
        });
        match open_bi_with_timeout(&conn).await {
            Ok((mut send, mut recv)) => {
                let setup = async {
                    write_request_frame(
                        &conn,
                        &mut send,
                        Request::SubscribeEvents {
                            session_id: state.session_id,
                            after_event_id,
                            process_id,
                            include_output,
                        },
                    )
                    .await?;
                    send.finish()?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                match setup {
                    Ok(()) => {
                        stream_ready = true;
                        loop {
                            let response = tokio::select! {
                                _ = &mut ctrl_c => {
                                    if let Some(sender) = &ack_sender {
                                        let _ = sender.try_send(state.last_event_id);
                                    }
                                    save(session_file, server, state)?;
                                    conn.close(0_u32.into(), b"event subscription detached");
                                    return Ok(());
                                }
                                response = read_response_frame(&conn, &mut recv) => match response {
                                    Ok(Some(response)) => response,
                                    Ok(None) => {
                                        eprintln!("event subscription stream closed; reconnecting");
                                        break;
                                    }
                                    Err(error) if retryable_connection_error(&error) => {
                                        eprintln!("event subscription interrupted; reconnecting: {error}");
                                        break;
                                    }
                                    Err(error) => return Err(error),
                                },
                            };
                            match response {
                                Response::SubscriptionReady {
                                    compacted,
                                    through_event_id,
                                    ..
                                } => {
                                    reconnect_attempt = 0;
                                    if compacted {
                                        // The snapshot is the authoritative
                                        // replacement for the skipped
                                        // history. It is installed before the
                                        // client advances its cursor, so an
                                        // ACK at this boundary is safe.
                                        state.advance_event_cursor(through_event_id);
                                        save(session_file, server, state.clone())?;
                                        if let Some(sender) = &ack_sender {
                                            let _ = sender.try_send(through_event_id);
                                        }
                                        eprintln!(
                                            "event history was compacted; following from the current boundary"
                                        );
                                    } else if state.last_event_id >= through_event_id
                                        && let Some(sender) = &ack_sender
                                    {
                                        // No backlog was present at the
                                        // subscription boundary.
                                        let _ = sender.try_send(through_event_id);
                                    }
                                }
                                Response::SubscriptionCaughtUp { through_event_id } => {
                                    // This marker follows the captured
                                    // backlog. It is safe to advance across
                                    // events hidden by a process/output
                                    // filter because every event up to the
                                    // boundary has now been consumed from the
                                    // subscription stream.
                                    state.advance_event_cursor(through_event_id);
                                    save(session_file, server, state.clone())?;
                                    if let Some(sender) = &ack_sender {
                                        let _ = sender.try_send(through_event_id);
                                    }
                                }
                                Response::EventNotification { event } => {
                                    if event.id <= state.last_event_id {
                                        continue;
                                    }
                                    println!("{}", serde_json::to_string(&event)?);
                                    state.advance_event_cursor(event.id);
                                    if let Some(sender) = &ack_sender {
                                        let _ = sender.try_send(event.id);
                                    }
                                    // Process output can be high-rate; avoid fsyncing the local
                                    // cursor for every chunk while still bounding crash replay.
                                    if !matches!(event.kind, EventKind::ProcessOutput { .. })
                                        || last_persist.elapsed() >= Duration::from_millis(250)
                                    {
                                        save(session_file, server, state.clone())?;
                                        last_persist = Instant::now();
                                    }
                                }
                                Response::Error { code, message }
                                    if code == "subscription_lagged" =>
                                {
                                    eprintln!(
                                        "event subscription lagged; reconnecting from saved cursor: {message}"
                                    );
                                    break;
                                }
                                Response::Error { code, message } => bail!("{code}: {message}"),
                                other => bail!("unexpected event subscription response: {other:?}"),
                            }
                        }
                    }
                    Err(error) if retryable_connection_error(&error) => {
                        eprintln!("event subscription setup interrupted; reconnecting: {error}");
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => {
                if retryable_connection_error(&error) {
                    eprintln!("event subscription stream open interrupted; reconnecting: {error}");
                } else {
                    return Err(error);
                }
            }
        }

        conn.close(0_u32.into(), b"event subscription reconnect");
        if stream_ready {
            eprintln!("event subscription transport lost; reconnecting");
        }
        loop {
            let delay_ms = (100_u64.saturating_mul(1_u64 << reconnect_attempt.min(6))).min(5_000);
            tokio::select! {
                _ = &mut ctrl_c => {
                    save(session_file, server, state)?;
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            }
            let next = match reusable_endpoint.as_ref() {
                Some(endpoint) => connect_on_endpoint(endpoint.clone(), server, auth_token).await,
                None => connect(server, cert, auth_token).await,
            };
            match next {
                Ok(next) => {
                    conn = next;
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                    continue 'subscription;
                }
                Err(error) if retryable_connection_error(&error) => {
                    eprintln!("event subscription reconnect failed: {error}");
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn parse_output_stream(value: &str) -> Result<OutputStream> {
    match value.trim().to_ascii_lowercase().as_str() {
        "stdout" => Ok(OutputStream::Stdout),
        "stderr" => Ok(OutputStream::Stderr),
        _ => bail!("--stream must be stdout or stderr"),
    }
}

fn parse_signal(value: &str) -> Result<i32> {
    let normalized = value.trim().to_ascii_lowercase();
    let signal = match normalized.as_str() {
        "hup" | "sighup" | "1" => 1,
        "int" | "sigint" | "2" => 2,
        "kill" | "sigkill" | "9" => 9,
        "term" | "sigterm" | "15" => 15,
        _ => bail!("--signal must be HUP(1), INT(2), KILL(9), or TERM(15)"),
    };
    Ok(signal)
}

/// Fetch one bounded, offset-addressed process-log range. The response is
/// written as raw bytes to the selected local stream so this command composes
/// with `tail`, agent parsers, and scripts without a JSON envelope.
async fn get_process_output(
    conn: &Connection,
    session_id: Uuid,
    process_id: Uuid,
    stream: OutputStream,
    offset: u64,
    requested_length: Option<u64>,
) -> Result<()> {
    if requested_length.is_some_and(|length| length > PROCESS_LOG_RANGE_MAX_BYTES) {
        bail!("requested process log range exceeds {PROCESS_LOG_RANGE_MAX_BYTES} bytes");
    }
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    write_request_frame(
        conn,
        &mut send,
        Request::ProcessOutputStream {
            session_id,
            process_id,
            stream: stream.clone(),
            offset,
            length: requested_length,
        },
    )
    .await?;
    send.finish()?;

    let first = read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed process log stream before BEGIN"))?;
    let (_total_size, begin_offset, length) = match first {
        Response::ProcessOutputStreamBegin {
            process_id: actual_process,
            stream: actual_stream,
            total_size,
            offset: begin_offset,
            length,
        } => {
            if actual_process != process_id || actual_stream != stream {
                bail!("process log stream metadata mismatch");
            }
            if total_size > PROCESS_OUTPUT_MAX_BYTES
                || begin_offset != offset
                || begin_offset > total_size
                || length > total_size.saturating_sub(begin_offset)
                || length > PROCESS_LOG_RANGE_MAX_BYTES
            {
                bail!("invalid process log stream bounds");
            }
            if requested_length.is_some_and(|requested| requested != length) {
                bail!("server changed requested process log range");
            }
            (total_size, begin_offset, length)
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected process log response: {other:?}"),
    };

    let mut received = 0_u64;
    loop {
        let response = read_response_frame(conn, &mut recv)
            .await?
            .ok_or_else(|| anyhow!("server closed process log stream before END"))?;
        match response {
            Response::ProcessOutputStreamChunk {
                offset: chunk_offset,
                data,
            } => {
                if data.is_empty() || data.len() > FILE_STREAM_CHUNK_BYTES {
                    bail!("invalid process log chunk");
                }
                if chunk_offset != begin_offset + received {
                    bail!("process log chunk offset mismatch");
                }
                received = received
                    .checked_add(data.len() as u64)
                    .ok_or_else(|| anyhow!("process log byte count overflow"))?;
                if received > length {
                    bail!("process log stream exceeded declared range");
                }
                match stream {
                    OutputStream::Stdout => {
                        std::io::stdout().write_all(&data)?;
                        std::io::stdout().flush()?;
                    }
                    OutputStream::Stderr => {
                        std::io::stderr().write_all(&data)?;
                        std::io::stderr().flush()?;
                    }
                }
            }
            Response::ProcessOutputStreamEnd { bytes, complete } => {
                if bytes != received || received != length || !complete {
                    bail!(
                        "process log stream ended incomplete: bytes={bytes} received={received} expected={length}"
                    );
                }
                return Ok(());
            }
            Response::Error { code, message } => bail!("{code}: {message}"),
            other => bail!("unexpected response in process log stream: {other:?}"),
        }
    }
}

/// Convert a requested tail into an offset-addressed range. The process
/// snapshot is the observation point: callers get at most `tail_bytes` from
/// the bytes known at that point, even if a live process appends more output
/// between the state query and the range stream.
fn process_log_tail_range(total_size: u64, tail_bytes: u64) -> Result<(u64, u64)> {
    if total_size > PROCESS_OUTPUT_MAX_BYTES {
        bail!("process log exceeds {PROCESS_OUTPUT_MAX_BYTES} byte limit");
    }
    if tail_bytes > PROCESS_LOG_RANGE_MAX_BYTES {
        bail!("requested process log tail exceeds {PROCESS_LOG_RANGE_MAX_BYTES} bytes");
    }
    let length = tail_bytes.min(total_size);
    Ok((total_size - length, length))
}

/// Read the durable process counters once and resolve a bounded tail without
/// adding a new wire operation. `retry_request` preserves the existing
/// reconnect/auth-rotation behavior used by other read-only agent requests.
async fn resolve_process_log_tail(
    conn: &mut Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
    process_id: Uuid,
    stream: &OutputStream,
    tail_bytes: u64,
) -> Result<(u64, Option<u64>)> {
    let session_id = state.session_id;
    let response = retry_request(
        conn,
        retry.server,
        retry.cert,
        retry.auth_token,
        state,
        Request::ProcessState {
            session_id,
            process_id,
        },
    )
    .await?;
    match response {
        Response::ProcessState { snapshot } => {
            if snapshot.process_id != process_id {
                bail!("process state response ID mismatch");
            }
            let total_size = match stream {
                OutputStream::Stdout => snapshot.stdout_bytes,
                OutputStream::Stderr => snapshot.stderr_bytes,
            };
            let (offset, length) = process_log_tail_range(total_size, tail_bytes)?;
            Ok((offset, Some(length)))
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => unexpected(other),
    }
}

/// Load the durable session cursor, creating a session only when no local
/// cursor exists. The first `OPEN_SESSION` is retried with one stable request
/// ID: if a network drop happens after the server commits the session but
/// before the response arrives, replaying the request returns the same UUID
/// instead of orphaning a newly-created session. Reconnecting here also makes
/// the very first daily CLI invocation tolerant of a transient path flap.
async fn ensure_session(
    conn: &mut Connection,
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    path: &Path,
) -> Result<SavedSession> {
    // A saved UUID is already sufficient to address a durable server-side
    // session. Avoid paying a RESUME round trip before every one-shot CLI
    // operation; callers that need the journal cursor use the explicit
    // `resume` command or reconnect path. Crucially, only a genuinely missing
    // server entry creates a new session. A malformed or unreadable local
    // session file must fail loudly instead of silently orphaning the durable
    // remote session behind a fresh UUID.
    if let Some(state) = saved_session(path, server)? {
        return Ok(state);
    }
    let lock = acquire_session_lock(path).await?;
    // Another process may have completed OPEN_SESSION while this process was
    // waiting for the lock. Re-read under the lock before creating anything.
    if let Some(state) = saved_session(path, server)? {
        return Ok(state);
    }
    let request_id = Uuid::new_v4();
    let mut last_error = None;
    for attempt in 0_u32..=4 {
        if attempt > 0 {
            let delay = (100_u64.saturating_mul(1_u64 << attempt.min(3))).min(1_000);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        match one(conn, Request::OpenSession { request_id }).await {
            Ok(Response::SessionOpened {
                session_id,
                event_id,
            }) => {
                let state = SavedSession {
                    session_id,
                    last_event_id: event_id,
                };
                save_locked(path, server, state.clone(), &lock)?;
                return Ok(state);
            }
            Ok(Response::Error { code, message }) if code == "server_busy" => {
                last_error = Some(anyhow!("{code}: {message}"));
            }
            Ok(Response::Error { code, message }) => {
                bail!("{code}: {message}");
            }
            Ok(response) => return unexpected(response),
            Err(error) if retryable_connection_error(&error) => {
                last_error = Some(error);
                conn.close(0_u32.into(), b"open session retry");
                match connect(server, cert, auth_token).await {
                    Ok(next) => *conn = next,
                    Err(error) if retryable_connection_error(&error) => {
                        last_error = Some(error);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("open session attempts exhausted")))
}

async fn resume(
    conn: &Connection,
    server: &str,
    path: &Path,
    state: &mut SavedSession,
    display: bool,
) -> Result<()> {
    let ResumeResult {
        snapshot,
        events,
        compacted,
    } = resume_stream(conn, state.session_id, state.last_event_id).await?;
    if display {
        if compacted {
            eprintln!("event history was compacted; using current snapshot");
        }
        for event in &events {
            match &event.kind {
                EventKind::ProcessOutput { stream, data, .. } => match stream {
                    OutputStream::Stdout => std::io::stdout().write_all(data)?,
                    OutputStream::Stderr => std::io::stderr().write_all(data)?,
                },
                kind => eprintln!("{} {:?}", event.id, kind),
            }
        }
        for process in &snapshot.processes {
            eprintln!(
                "process {} running={} command={:?}",
                process.process_id, process.running, process.command
            );
        }
    }
    state.advance_event_cursor(snapshot.latest_event_id);
    save(path, server, state.clone())?;
    if let Some(consumer_id) = client_consumer_id()
        && let Err(error) =
            ack_events_consumer(conn, state.session_id, consumer_id, state.last_event_id).await
    {
        eprintln!("warning: event consumer ACK was not persisted: {error}");
    }
    Ok(())
}

/// Retry a one-shot RESUME when the transport disappears while the bounded
/// replay is in flight. `resume` advances the local cursor only after a
/// complete `RESUME_END`, so replaying the same cursor is safe and cannot
/// duplicate partially-rendered events.
async fn resume_with_retry(
    conn: &mut Connection,
    server: &str,
    cert: &Path,
    auth_token: Option<&str>,
    session_file: &Path,
    state: &mut SavedSession,
    display: bool,
) -> Result<()> {
    let mut attempt = 0_u8;
    loop {
        match resume(conn, server, session_file, state, display).await {
            Ok(()) => return Ok(()),
            Err(error) if attempt < 8 && retryable_connection_error(&error) => {
                attempt += 1;
                eprintln!("resume interrupted; reconnecting (attempt {attempt}/8): {error}");
                conn.close(0_u32.into(), b"resume retry");
                *conn = connect_with_retry(server, cert, auth_token).await?;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Resume over a sequence of bounded frames. A single giant `Resumed` frame
/// remains available for older peers, but current clients use this path so a
/// large journal tail cannot monopolize one allocation or hit frame limits.
async fn resume_stream(
    conn: &Connection,
    session_id: Uuid,
    last_event_id: u64,
) -> Result<ResumeResult> {
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    write_request_frame(
        conn,
        &mut send,
        Request::ResumeSessionStream {
            session_id,
            last_event_id,
        },
    )
    .await?;
    send.finish()?;

    let first = read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("server closed resume stream before BEGIN"))?;
    let (snapshot, compacted, event_count) = match first {
        Response::ResumeBegin {
            snapshot,
            compacted,
            event_count,
            ..
        } => (snapshot, compacted, event_count),
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => bail!("unexpected resume response: {other:?}"),
    };
    const MAX_RESUME_EVENTS: u64 = 100_000;
    if event_count > MAX_RESUME_EVENTS {
        bail!("resume stream contains too many events ({event_count})");
    }
    let mut events = Vec::with_capacity(event_count as usize);
    let mut previous_event_id = last_event_id;
    loop {
        let response = read_response_frame(conn, &mut recv)
            .await?
            .ok_or_else(|| anyhow!("server closed resume stream before END"))?;
        match response {
            Response::ResumeEvent { event } => {
                if events.len() as u64 >= event_count {
                    bail!("resume stream sent more events than BEGIN promised");
                }
                if event.id <= previous_event_id {
                    bail!("resume stream event IDs are not strictly increasing");
                }
                previous_event_id = event.id;
                events.push(event);
            }
            Response::ResumeEnd { through_event_id } => {
                if events.len() as u64 != event_count {
                    bail!(
                        "resume stream ended after {} events; expected {event_count}",
                        events.len()
                    );
                }
                if through_event_id < previous_event_id
                    || through_event_id < snapshot.latest_event_id
                {
                    bail!("resume stream end cursor is behind the snapshot");
                }
                return Ok(ResumeResult {
                    snapshot,
                    events,
                    compacted,
                });
            }
            Response::Error { code, message } => bail!("{code}: {message}"),
            other => bail!("unexpected response in resume stream: {other:?}"),
        }
    }
}

async fn open_pty(
    conn: &Connection,
    session_id: Uuid,
    rows: u16,
    cols: u16,
    include_scrollback: bool,
) -> Result<(
    SendStream,
    RecvStream,
    PtyReadySnapshot,
    Option<PtyScrollbackSnapshot>,
)> {
    let (mut send, mut recv) = open_bi_with_timeout(conn).await?;
    write_request_frame(
        conn,
        &mut send,
        Request::PtyOpen {
            session_id,
            rows,
            cols,
        },
    )
    .await?;
    let ready = read_response_frame(conn, &mut recv)
        .await?
        .ok_or_else(|| anyhow!("PTY stream closed"))?;
    let snapshot = match ready {
        Response::PtyReady { snapshot } => PtyReadySnapshot::Plain(snapshot),
        Response::PtyReadyRich { snapshot } => PtyReadySnapshot::Rich(snapshot),
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => return unexpected(other),
    };
    let scrollback = if include_scrollback {
        let response = read_response_frame(conn, &mut recv)
            .await?
            .ok_or_else(|| anyhow!("PTY stream closed before scrollback snapshot"))?;
        match response {
            Response::PtyReadyScrollback { snapshot } => {
                validate_pty_scrollback_snapshot(&snapshot)?;
                Some(snapshot)
            }
            Response::Error { code, message } => bail!("{code}: {message}"),
            other => {
                return Err(anyhow!(
                    "unexpected response before PTY scrollback snapshot: {other:?}"
                ));
            }
        }
    } else {
        None
    };
    Ok((send, recv, snapshot, scrollback))
}

enum PtyReadySnapshot {
    Plain(PtySnapshot),
    Rich(PtyRichSnapshot),
}

impl PtyReadySnapshot {
    fn generation(&self) -> u64 {
        match self {
            Self::Plain(snapshot) => snapshot.generation,
            Self::Rich(snapshot) => snapshot.generation,
        }
    }
}

fn validate_pty_scrollback_snapshot(snapshot: &PtyScrollbackSnapshot) -> Result<()> {
    if snapshot.rows == 0 || snapshot.cols == 0 {
        bail!("PTY scrollback snapshot has invalid geometry");
    }
    if snapshot.lines.len() > PTY_SCROLLBACK_MAX_LINES {
        bail!("PTY scrollback snapshot contains too many lines");
    }
    let mut total_bytes = 0usize;
    for line in &snapshot.lines {
        if line.len() > PTY_SCROLLBACK_MAX_LINE_BYTES || line.chars().any(char::is_control) {
            bail!("PTY scrollback snapshot contains an invalid line");
        }
        total_bytes = total_bytes.saturating_add(line.len());
    }
    if total_bytes > PTY_SCROLLBACK_MAX_BYTES {
        bail!("PTY scrollback snapshot exceeds its byte bound");
    }
    Ok(())
}

/// Client-side base for the optional plain PTY row-delta datagram.  Reliable
/// PTY output remains the authoritative byte stream; this state exists only
/// to validate and apply replaceable screen updates without replaying an
/// entire screen for every frame.
#[derive(Debug, Clone)]
struct PlainPtyScreenState {
    generation: u64,
    rows: u16,
    cols: u16,
    screen: Vec<String>,
    cursor_row: u16,
    cursor_col: u16,
}

impl PlainPtyScreenState {
    fn from_snapshot(snapshot: &PtySnapshot) -> Self {
        Self {
            generation: snapshot.generation,
            rows: snapshot.rows,
            cols: snapshot.cols,
            screen: snapshot.screen.clone(),
            cursor_row: snapshot.cursor_row,
            cursor_col: snapshot.cursor_col,
        }
    }

    /// Apply a delta only when its base and geometry match exactly.  The
    /// strict row ordering and bounded row text prevent malformed datagrams
    /// from causing ambiguous screen updates; a mismatch is recoverable by
    /// the server's periodic full checkpoint.
    fn apply_delta(&mut self, delta: &PtyStateDeltaDatagram) -> Result<Option<bool>> {
        const MAX_ROW_BYTES: usize = 64 * 1024;
        if delta.base_generation != self.generation
            || delta.generation <= delta.base_generation
            || delta.rows != self.rows
            || delta.cols != self.cols
            || delta.changes.len() > self.screen.len()
        {
            return Ok(None);
        }
        let mut previous_row = None;
        for change in &delta.changes {
            if change.row >= self.rows
                || change.row as usize >= self.screen.len()
                || change.text.len() > MAX_ROW_BYTES
                || previous_row.is_some_and(|previous| change.row <= previous)
            {
                return Ok(None);
            }
            previous_row = Some(change.row);
        }
        for change in &delta.changes {
            self.screen[change.row as usize] = change.text.clone();
        }
        let changed = !delta.changes.is_empty()
            || self.cursor_row != delta.cursor_row
            || self.cursor_col != delta.cursor_col;
        self.generation = delta.generation;
        self.cursor_row = delta.cursor_row;
        self.cursor_col = delta.cursor_col;
        Ok(Some(changed))
    }
}

async fn shell(
    initial_conn: Connection,
    retry: RetryContext<'_>,
    state: &mut SavedSession,
) -> Result<()> {
    let _terminal_guard = TerminalGuard::enter();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut input = [0_u8; 4096];
    let mut conn = initial_conn;
    #[cfg(unix)]
    let mut window_change =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();
    #[cfg(not(unix))]
    let mut window_change = ();
    loop {
        let (mut rows, mut cols) = terminal_size();
        let include_scrollback = connection_supports_feature(&conn, "pty_scrollback");
        let (mut send, mut recv, snapshot, scrollback) = loop {
            match open_pty(&conn, state.session_id, rows, cols, include_scrollback).await {
                Ok(attachment) => break attachment,
                Err(error) if retryable_connection_error(&error) => {
                    eprintln!("PTY attach interrupted; reconnecting");
                    conn.close(0_u32.into(), b"pty attach retry");
                    match reconnect_shell(retry, state, &mut stdin).await? {
                        Some(connection) => conn = connection,
                        None => return Ok(()),
                    }
                }
                Err(error) => return Err(error),
            }
        };
        let mut newest_generation = snapshot.generation();
        if let Some(scrollback) = scrollback.as_ref() {
            render_pty_scrollback_to(&mut stdout, scrollback).await?;
        }
        render_pty_ready_snapshot_to(&mut stdout, &snapshot).await?;
        let mut plain_screen_state = match &snapshot {
            PtyReadySnapshot::Plain(snapshot) => Some(PlainPtyScreenState::from_snapshot(snapshot)),
            PtyReadySnapshot::Rich(_) => None,
        };
        let mut input_sequence = 0_u64;
        let disconnected = loop {
            let window_event = wait_for_window_change(&mut window_change);
            tokio::select! {
                read = stdin.read(&mut input) => {
                    let n = read?;
                    if n == 0 || input[..n].contains(&0x1d) {
                        break false;
                    }
                    match write_request_frame(&conn, &mut send, Request::PtyInputSequenced { session_id: state.session_id, sequence: input_sequence, data: input[..n].to_vec() }).await {
                        Ok(()) => input_sequence = input_sequence.saturating_add(1),
                        Err(error) if retryable_connection_error(&error) => break true,
                        Err(error) => return Err(error),
                    }
                }
                response = read_response_frame(&conn, &mut recv) => {
                    match response {
                        Ok(Some(Response::PtyOutput { generation, data })) => {
                            // Reliable PTY output and replaceable screen
                            // datagrams share the same monotonic parser
                            // generation.  Advance the guard for every
                            // output frame so a delayed/lost datagram cannot
                            // repaint an older screen over newer live bytes.
                            record_pty_generation(&mut newest_generation, generation);
                            stdout.write_all(&data).await?;
                            stdout.flush().await?;
                        }
                        Ok(Some(Response::PtyInputAck { .. })) => {}
                        Ok(Some(Response::PtyReady { snapshot })) => {
                            // A lag-recovery snapshot is reliable, but it
                            // can still arrive after newer PTY output or a
                            // replaceable DATAGRAM on the same attachment.
                            // Treat it exactly like the lossy state form:
                            // only a strictly newer generation may repaint
                            // the local terminal.
                            if record_pty_generation(
                                &mut newest_generation,
                                snapshot.generation,
                            ) {
                                render_pty_snapshot_to(&mut stdout, &snapshot).await?;
                                plain_screen_state = Some(PlainPtyScreenState::from_snapshot(&snapshot));
                            }
                        }
                        Ok(Some(Response::PtyReadyRich { snapshot })) => {
                            if record_pty_generation(
                                &mut newest_generation,
                                snapshot.generation,
                            ) {
                                render_pty_rich_snapshot_to(&mut stdout, &snapshot).await?;
                                plain_screen_state = None;
                            }
                        }
                        Ok(Some(Response::Error { code, message })) => bail!("{code}: {message}"),
                        Ok(Some(other)) => return unexpected(other),
                        Ok(None) => break true,
                        Err(error) if retryable_connection_error(&error) => break true,
                        Err(error) => return Err(error),
                    }
                }
                datagram = conn.read_datagram() => {
                    match datagram {
                        Ok(payload) => {
                            if connection_supports_feature(&conn, "pty_state_delta") {
                                if let Ok(Some(delta)) = decode_pty_state_delta_datagram(&payload) {
                                    if delta.session_id == state.session_id
                                        && delta.generation >= newest_generation
                                        && let Some(screen) = plain_screen_state.as_mut()
                                        && let Some(changed) = screen.apply_delta(&delta)?
                                    {
                                        newest_generation = newest_generation.max(delta.generation);
                                        if changed {
                                            render_pty_delta_to(&mut stdout, &delta).await?;
                                        }
                                    }
                                    continue;
                                }
                            }
                            if let Ok(Some(datagram)) = decode_pty_rich_datagram(
                                &payload,
                                connection_supports_feature(
                                    &conn,
                                    "pty_rich_compression",
                                ),
                            ) {
                                if datagram.session_id == state.session_id
                                    && record_pty_generation(
                                        &mut newest_generation,
                                        datagram.generation,
                                    )
                                {
                                    render_pty_rich_snapshot_to(
                                        &mut stdout,
                                        &PtyRichSnapshot {
                                            generation: datagram.generation,
                                            rows: datagram.rows,
                                            cols: datagram.cols,
                                            screen: datagram.screen,
                                            cursor_row: datagram.cursor_row,
                                            cursor_col: datagram.cursor_col,
                                        },
                                    )
                                    .await?;
                                }
                                continue;
                            }
                            let Ok(datagram) = decode_pty_state_datagram(&payload) else {
                                continue;
                            };
                            if datagram.session_id == state.session_id
                                && record_pty_generation(&mut newest_generation, datagram.generation)
                            {
                                plain_screen_state = Some(PlainPtyScreenState {
                                    generation: datagram.generation,
                                    rows: datagram.rows,
                                    cols: datagram.cols,
                                    screen: datagram.screen.clone(),
                                    cursor_row: datagram.cursor_row,
                                    cursor_col: datagram.cursor_col,
                                });
                                render_pty_snapshot_to(&mut stdout, &PtySnapshot {
                                    generation: datagram.generation,
                                    rows: datagram.rows,
                                    cols: datagram.cols,
                                    screen: datagram.screen,
                                    cursor_row: datagram.cursor_row,
                                    cursor_col: datagram.cursor_col,
                                    tail: Vec::new(),
                                }).await?;
                            }
                        }
                        Err(error) => {
                            let error: anyhow::Error = error.into();
                            if retryable_connection_error(&error) {
                                break true;
                            }
                            return Err(error);
                        }
                    }
                }
                _ = window_event => {
                    let (new_rows, new_cols) = terminal_size();
                    if (new_rows, new_cols) != (rows, cols) {
                        rows = new_rows;
                        cols = new_cols;
                        match write_request_frame(&conn, &mut send, Request::PtyResize { session_id: state.session_id, rows, cols }).await {
                            Ok(()) => {}
                            Err(error) if retryable_connection_error(&error) => break true,
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
        };
        if !disconnected {
            conn.close(0_u32.into(), b"shell complete");
            return Ok(());
        }
        conn.close(0_u32.into(), b"pty connection interrupted");
        eprintln!("PTY connection interrupted; reconnecting");
        match reconnect_shell(retry, state, &mut stdin).await? {
            Some(connection) => conn = connection,
            None => return Ok(()),
        }
    }
}

/// Advance a PTY attachment's replaceable-state generation only when the
/// candidate is newer. Reliable output and lossy screen snapshots share this
/// guard, so an older DATAGRAM can never repaint the terminal after a newer
/// output frame has already been rendered.
fn record_pty_generation(current: &mut u64, candidate: u64) -> bool {
    if candidate <= *current {
        return false;
    }
    *current = candidate;
    true
}

#[cfg(unix)]
async fn wait_for_window_change(signal: &mut Option<tokio::signal::unix::Signal>) {
    if let Some(signal) = signal {
        let _ = signal.recv().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(not(unix))]
async fn wait_for_window_change(_: &mut ()) {
    std::future::pending::<()>().await;
}

async fn render_pty_scrollback_to<W: tokio::io::AsyncWrite + Unpin>(
    output: &mut W,
    snapshot: &PtyScrollbackSnapshot,
) -> Result<()> {
    if snapshot.lines.is_empty() {
        return Ok(());
    }
    // History is deliberately plain text and has already passed the protocol
    // bounds/validation check. Prefix each row with a carriage return so a
    // client restarted while the local terminal cursor was mid-line cannot
    // concatenate old history with the current prompt. The following current
    // screen redraw clears only the viewport; these rows remain in terminal
    // scrollback just as if they had arrived while the client was attached.
    let capacity = snapshot
        .lines
        .iter()
        .map(|line| line.len().saturating_add(2))
        .sum::<usize>();
    let mut rendered = Vec::with_capacity(capacity);
    for line in &snapshot.lines {
        rendered.extend_from_slice(b"\r");
        rendered.extend_from_slice(line.as_bytes());
        rendered.extend_from_slice(b"\n");
    }
    output.write_all(&rendered).await?;
    output.flush().await?;
    Ok(())
}

async fn render_pty_snapshot_to<W: tokio::io::AsyncWrite + Unpin>(
    output: &mut W,
    snapshot: &asp_protocol::PtySnapshot,
) -> Result<()> {
    // A reconnect snapshot can contain hundreds of rows. Build one bounded
    // frame before writing so PTY resume does not perform an async write and
    // scheduler handoff for every row; live incremental PTY output remains
    // streamed separately for interactive latency.
    let mut rendered = Vec::with_capacity(
        4_usize
            .saturating_add(
                snapshot
                    .screen
                    .iter()
                    .map(|row| row.len().saturating_add(16))
                    .sum::<usize>(),
            )
            .saturating_add(32),
    );
    rendered.extend_from_slice(b"\x1b[2J");
    for (index, row) in snapshot.screen.iter().enumerate() {
        let prefix = format!("\x1b[{};1H\x1b[2K", index + 1);
        rendered.extend_from_slice(prefix.as_bytes());
        rendered.extend_from_slice(row.as_bytes());
    }
    let cursor = format!(
        "\x1b[{};{}H",
        snapshot.cursor_row.saturating_add(1),
        snapshot.cursor_col.saturating_add(1)
    );
    rendered.extend_from_slice(cursor.as_bytes());
    output.write_all(&rendered).await?;
    output.flush().await?;
    Ok(())
}

async fn render_pty_ready_snapshot_to<W: tokio::io::AsyncWrite + Unpin>(
    output: &mut W,
    snapshot: &PtyReadySnapshot,
) -> Result<()> {
    match snapshot {
        PtyReadySnapshot::Plain(snapshot) => render_pty_snapshot_to(output, snapshot).await,
        PtyReadySnapshot::Rich(snapshot) => render_pty_rich_snapshot_to(output, snapshot).await,
    }
}

async fn render_pty_rich_snapshot_to<W: tokio::io::AsyncWrite + Unpin>(
    output: &mut W,
    snapshot: &PtyRichSnapshot,
) -> Result<()> {
    // `vt100::Screen::contents_formatted` emits a complete ANSI redraw,
    // including attributes, cursor state, and screen clearing. It is a
    // replaceable snapshot: reliable PTY bytes remain authoritative for live
    // output, while this path repairs a reconnect without replaying history.
    output.write_all(&snapshot.screen).await?;
    output.flush().await?;
    Ok(())
}

/// Render only the rows carried by a base-relative PTY state datagram.  The
/// local terminal is already displaying the reliable byte stream, so this
/// bounded cursor/row update repairs the replaceable view without emitting a
/// full-screen clear and redraw for every datagram.
async fn render_pty_delta_to<W: tokio::io::AsyncWrite + Unpin>(
    output: &mut W,
    delta: &PtyStateDeltaDatagram,
) -> Result<()> {
    let mut rendered = Vec::with_capacity(
        delta
            .changes
            .iter()
            .map(|change| change.text.len().saturating_add(16))
            .sum::<usize>()
            .saturating_add(32),
    );
    for change in &delta.changes {
        let prefix = format!("\x1b[{};1H\x1b[2K", change.row as usize + 1);
        rendered.extend_from_slice(prefix.as_bytes());
        rendered.extend_from_slice(change.text.as_bytes());
    }
    let cursor = format!(
        "\x1b[{};{}H",
        delta.cursor_row.saturating_add(1),
        delta.cursor_col.saturating_add(1)
    );
    rendered.extend_from_slice(cursor.as_bytes());
    output.write_all(&rendered).await?;
    output.flush().await?;
    Ok(())
}

struct TerminalGuard(Option<String>);

impl TerminalGuard {
    fn enter() -> Self {
        let saved = std::process::Command::new("stty")
            .arg("-g")
            .output()
            .ok()
            .and_then(|output| {
                output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            });
        if saved.is_some() {
            let _ = std::process::Command::new("stty")
                .args(["raw", "-echo"])
                .status();
        }
        Self(saved)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            let _ = std::process::Command::new("stty").arg(saved).status();
        }
    }
}

fn terminal_size() -> (u16, u16) {
    std::process::Command::new("stty")
        .arg("size")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let value = String::from_utf8_lossy(&output.stdout);
            let mut values = value
                .split_whitespace()
                .filter_map(|part| part.parse::<u16>().ok());
            Some((values.next()?, values.next()?))
        })
        .unwrap_or((24, 80))
}

async fn resolve(server: &str) -> Result<Vec<SocketAddr>> {
    let addresses = tokio::time::timeout(client_connect_timeout(), tokio::net::lookup_host(server))
        .await
        .with_context(|| format!("resolve {server} timed out"))?
        .with_context(|| format!("resolve {server}"))?;
    let mut unique = Vec::new();
    for address in addresses {
        if !unique.contains(&address) {
            unique.push(address);
        }
        if unique.len() > MAX_RESOLVED_ADDRESSES {
            bail!("resolve {server} returned more than {MAX_RESOLVED_ADDRESSES} addresses");
        }
    }
    if unique.is_empty() {
        bail!("no address for {server}");
    }
    Ok(unique)
}

fn validate_server_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 255
        || name.as_bytes().contains(&0)
        || name.chars().any(char::is_whitespace)
    {
        bail!("--server-name must be 1..255 bytes with no whitespace or NUL");
    }
    Ok(())
}

const READY_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const READY_CHECK_MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Parse the explicit local HTTP readiness URL accepted by `asp doctor`.
/// Requiring a literal loopback `SocketAddr` avoids turning a diagnostic
/// option into a DNS-rebinding/SSRF primitive; the daemon itself enforces the
/// same loopback-only policy for its health listener.
fn parse_ready_url(url: &str) -> Result<SocketAddr> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("--ready-url must use http://LOOPBACK:PORT/ready"))?;
    let (authority, path) = rest
        .split_once('/')
        .ok_or_else(|| anyhow!("--ready-url must end with /ready"))?;
    if authority.is_empty()
        || path != "ready"
        || authority
            .chars()
            .any(|character| matches!(character, '?' | '#'))
    {
        bail!("--ready-url must be http://LOOPBACK:PORT/ready");
    }
    let address = authority
        .parse::<SocketAddr>()
        .with_context(|| format!("parse loopback readiness address {authority:?}"))?;
    if !address.ip().is_loopback() {
        bail!("--ready-url must target a loopback address, got {address}");
    }
    Ok(address)
}

fn parse_ready_http_status(response: &[u8]) -> Result<(u16, &[u8])> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("readiness endpoint returned an incomplete HTTP response"))?;
    let status_line_end = response
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or_else(|| anyhow!("readiness endpoint returned no HTTP status line"))?;
    let status_line = std::str::from_utf8(&response[..status_line_end])
        .context("readiness endpoint returned a non-UTF-8 HTTP status line")?;
    let mut fields = status_line.split_whitespace();
    let protocol = fields.next().unwrap_or_default();
    if protocol != "HTTP/1.1" && protocol != "HTTP/1.0" {
        bail!("readiness endpoint returned an invalid HTTP status line");
    }
    let status = fields
        .next()
        .ok_or_else(|| anyhow!("readiness endpoint returned no HTTP status code"))?
        .parse::<u16>()
        .context("readiness endpoint returned an invalid HTTP status code")?;
    Ok((status, &response[header_end + 4..]))
}

/// Check the operator-owned loopback readiness endpoint with a bounded HTTP
/// request. A successful response is deliberately silent; `asp doctor` keeps
/// its existing machine-readable HEALTH JSON on stdout. A non-200 response
/// includes a short body excerpt so a failed disk/audit/launcher/drain gate is
/// actionable without requiring a second curl invocation.
async fn check_ready_endpoint(url: &str) -> Result<()> {
    let address = parse_ready_url(url)?;
    let response = tokio::time::timeout(READY_CHECK_TIMEOUT, async {
        let mut stream = TcpStream::connect(address).await?;
        stream
            .write_all(b"GET /ready HTTP/1.1\r\nHost: asp-readiness\r\nConnection: close\r\n\r\n")
            .await?;
        stream.shutdown().await?;
        let mut response = Vec::with_capacity(1024);
        let mut chunk = [0_u8; 4096];
        loop {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            if response.len().saturating_add(read) > READY_CHECK_MAX_RESPONSE_BYTES {
                bail!("readiness endpoint response exceeds {READY_CHECK_MAX_RESPONSE_BYTES} bytes");
            }
            response.extend_from_slice(&chunk[..read]);
        }
        Ok::<Vec<u8>, anyhow::Error>(response)
    })
    .await
    .with_context(|| format!("readiness check timed out for {url}"))??;
    let (status, body) = parse_ready_http_status(&response)?;
    if status != 200 {
        let mut excerpt = String::from_utf8_lossy(body).trim().to_owned();
        if excerpt.len() > 512 {
            excerpt.truncate(512);
            excerpt.push_str("...");
        }
        if excerpt.is_empty() {
            bail!("readiness check failed for {url}: HTTP {status}");
        }
        bail!("readiness check failed for {url}: HTTP {status}: {excerpt}");
    }
    Ok(())
}

/// Apply the client-side portion of the production health gate. The
/// authenticated HEALTH response intentionally stays small and portable, so
/// the strict doctor checks only invariants it can actually observe: a
/// negotiated protocol the client understands, authentication enabled on the
/// server, and the durable tmux PTY backend. Filesystem headroom, audit sink,
/// launcher identity, and supervisor health remain owned by the server's
/// loopback `/ready` probe and are not guessed here.
fn validate_strict_doctor(
    protocol_version: u16,
    auth_required: bool,
    pty_backend: &str,
) -> Result<()> {
    let mut failures = Vec::new();
    if !protocol_version_supported(protocol_version) {
        failures.push(format!("unsupported protocol version {protocol_version}"));
    }
    if !auth_required {
        failures.push("server authentication is disabled".to_owned());
    }
    if pty_backend != "tmux" {
        failures.push(format!(
            "durable PTY backend is {pty_backend:?}, expected tmux"
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("strict doctor failed: {}", failures.join("; "))
    }
}

fn validate_consumer_id(consumer_id: Option<&str>) -> Result<()> {
    let Some(consumer_id) = consumer_id else {
        return Ok(());
    };
    if consumer_id.is_empty()
        || consumer_id.len() > 128
        || consumer_id
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        bail!("--consumer-id must be 1..128 bytes with no whitespace or control characters");
    }
    Ok(())
}

fn resolve_session_file(path: PathBuf) -> PathBuf {
    if path != PathBuf::from(LEGACY_SESSION_FILE) || path.exists() {
        return path;
    }
    default_session_file()
}

/// Return a durable per-user cursor location outside any remote workspace.
/// The legacy `.asp-session` path remains supported when it already exists or
/// when the caller passes an explicit path.
fn default_session_file() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            #[cfg(target_os = "macos")]
            {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join("Library").join("Application Support"))
            }
            #[cfg(target_os = "windows")]
            {
                std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local").join("state"))
            }
            #[cfg(not(any(unix, target_os = "windows")))]
            {
                None
            }
        });
    base.map(|base| base.join("asp").join("sessions.json"))
        .unwrap_or_else(|| PathBuf::from(LEGACY_SESSION_FILE))
}

fn load(path: &Path) -> Result<SavedSessions> {
    reject_symlink(path)?;
    match read_file_limited(path, MAX_CLIENT_METADATA_BYTES) {
        Ok(data) => Ok(serde_json::from_slice(&data)?),
        Err(error) if is_not_found(&error) => Ok(SavedSessions::default()),
        Err(error) => Err(error),
    }
}

fn client_consumer_id() -> Option<&'static str> {
    CLIENT_CONSUMER_ID.get().and_then(Option::as_deref)
}

fn lookup_saved_session(
    sessions: &SavedSessions,
    server: &str,
    consumer_id: Option<&str>,
) -> Option<SavedSession> {
    if let Some(consumer_id) = consumer_id
        && let Some(state) = sessions
            .consumers
            .get(server)
            .and_then(|consumers| consumers.get(consumer_id))
            .cloned()
    {
        return Some(state);
    }
    // A newly named consumer may attach to a session created by the legacy
    // per-server cursor. Once it receives or applies anything, save_locked
    // materializes an independent entry and subsequent updates stay isolated.
    sessions.servers.get(server).cloned()
}

fn require_saved(path: &Path, server: &str) -> Result<SavedSession> {
    saved_session(path, server)?
        .ok_or_else(|| anyhow!("no saved session for {server}; run `asp connect {server}`"))
}

fn saved_session(path: &Path, server: &str) -> Result<Option<SavedSession>> {
    let sessions = load(path)?;
    Ok(lookup_saved_session(
        &sessions,
        server,
        client_consumer_id(),
    ))
}

fn prepare_session_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // `create_dir_all` is convenient for first use, but a path-only
            // permission repair would leave a check/use race and would follow
            // a final-component symlink. Re-open the directory with
            // descriptor-level no-follow semantics before tightening it.
            let mut options = OpenOptions::new();
            options
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
            let directory = options
                .open(parent)
                .with_context(|| format!("open session state directory {}", parent.display()))?;
            let metadata = directory.metadata()?;
            if !metadata.is_dir() {
                bail!(
                    "session state parent is not a regular directory: {}",
                    parent.display()
                );
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    Ok(())
}

fn open_session_lock(path: &Path) -> Result<std::fs::File> {
    prepare_session_parent(path)?;
    let lock_path = session_lock_path(path);
    reject_symlink(&lock_path)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    options
        .open(&lock_path)
        .with_context(|| format!("open session lock {}", lock_path.display()))
}

/// Acquire the client cursor lock without blocking a Tokio worker thread. The
/// lock is held by the caller across the bounded first-session network retry;
/// this prevents two freshly-started agents from creating two remote sessions
/// before either can publish the shared local cursor.
async fn acquire_session_lock(path: &Path) -> Result<std::fs::File> {
    let lock = open_session_lock(path)?;
    for attempt in 0_u32..=100 {
        match lock.try_lock_exclusive() {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                }
                return Ok(lock);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && attempt < 100 => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("lock session state {}", path.display()));
            }
        }
    }
    unreachable!("bounded session lock loop must return")
}

fn insert_saved_session(
    sessions: &mut SavedSessions,
    server: &str,
    consumer_id: Option<&str>,
    state: SavedSession,
) {
    // Multiple agent processes may share the default per-user cursor file.
    // They each hold the file lock while saving, but an older in-memory
    // cursor can still arrive after a newer process has persisted one. Never
    // let that stale writer move the durable resume point backwards for the
    // same session. Explicit consumer IDs get independent monotonic cursors;
    // an `asp connect` that creates a new session may replace its own entry.
    if let Some(consumer_id) = consumer_id {
        let consumers = sessions.consumers.entry(server.to_string()).or_default();
        let state = match consumers.get(consumer_id) {
            Some(existing) if existing.session_id == state.session_id => SavedSession {
                session_id: state.session_id,
                last_event_id: existing.last_event_id.max(state.last_event_id),
            },
            _ => state,
        };
        consumers.insert(consumer_id.to_owned(), state);
    } else {
        let state = match sessions.servers.get(server) {
            Some(existing) if existing.session_id == state.session_id => SavedSession {
                session_id: state.session_id,
                last_event_id: existing.last_event_id.max(state.last_event_id),
            },
            _ => state,
        };
        sessions.servers.insert(server.to_string(), state);
    }
}

fn save_locked(path: &Path, server: &str, state: SavedSession, lock: &std::fs::File) -> Result<()> {
    #[cfg(not(unix))]
    let _ = lock;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let mut sessions = load(path)?;
    insert_saved_session(&mut sessions, server, client_consumer_id(), state);
    let data = serde_json::to_vec_pretty(&sessions)?;
    if data.len() as u64 > MAX_CLIENT_METADATA_BYTES {
        bail!(
            "client session metadata exceeds the {MAX_CLIENT_METADATA_BYTES}-byte limit; remove stale server or consumer entries before retrying"
        );
    }
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Apply the private mode through the descriptor before publishing
            // the cursor path; a pathname chmod is vulnerable to a local
            // rename/symlink race.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::File::open(parent)?.sync_data()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Persist the durable session/consumer cursor. Attachment progress is
/// deliberately process-local and callers must not invoke this for every
/// filtered response: doing so would add a synchronous atomic write and
/// fsync to the short-command hot path.
fn save(path: &Path, server: &str, state: SavedSession) -> Result<()> {
    let lock = open_session_lock(path)?;
    lock.lock_exclusive()
        .with_context(|| format!("lock session state {}", path.display()))?;
    save_locked(path, server, state, &lock)
}

fn session_lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

fn join_command(parts: Vec<String>) -> Result<String> {
    if parts.is_empty() {
        bail!("command must not be empty");
    }
    Ok(parts.join(" "))
}

fn join_command_option(parts: Vec<String>, command_text: Option<String>) -> Result<String> {
    match command_text {
        Some(command) => join_command(vec![command]),
        None => join_command(parts),
    }
}

fn resolve_server_endpoint(
    positional_server: Option<String>,
    server_option: Option<String>,
) -> Result<String> {
    positional_server
        .or(server_option)
        .ok_or_else(|| anyhow!("server endpoint is required (pass SERVER or --server/ASP_SERVER)"))
}

fn resolve_process_target(
    positional_server: Option<String>,
    positional_process_id: Option<Uuid>,
    server_option: Option<String>,
) -> Result<(String, Uuid)> {
    if let Some(process_id) = positional_process_id {
        return Ok((
            resolve_server_endpoint(positional_server, server_option)?,
            process_id,
        ));
    }

    let process_id_text = positional_server.ok_or_else(|| {
        anyhow!("process ID is required (pass SERVER PROCESS_ID or --server PROCESS_ID)")
    })?;
    let process_id = Uuid::parse_str(&process_id_text)
        .with_context(|| format!("invalid process ID: {process_id_text}"))?;
    let server = server_option.ok_or_else(|| {
        anyhow!(
            "server endpoint is required (pass SERVER PROCESS_ID or --server PROCESS_ID/ASP_SERVER)"
        )
    })?;
    Ok((server, process_id))
}

fn resolve_single_path_args(
    positional_server: Option<String>,
    positional_path: Option<PathBuf>,
    server_option: Option<String>,
    path_label: &str,
) -> Result<(String, PathBuf)> {
    match positional_path {
        Some(path) => Ok((
            resolve_server_endpoint(positional_server, server_option)?,
            path,
        )),
        None => Ok((
            server_option.ok_or_else(|| {
                anyhow!(
                    "server endpoint is required (pass SERVER PATH or --server PATH/ASP_SERVER)"
                )
            })?,
            PathBuf::from(positional_server.ok_or_else(|| anyhow!("{path_label} is required"))?),
        )),
    }
}

fn resolve_artifact_get_args(
    positional_server: Option<String>,
    positional_artifact_id: Option<String>,
    positional_local: Option<PathBuf>,
    server_option: Option<String>,
) -> Result<(String, String, PathBuf)> {
    if let Some(local) = positional_local {
        return Ok((
            resolve_server_endpoint(positional_server, server_option)?,
            positional_artifact_id.ok_or_else(|| anyhow!("artifact ID is required"))?,
            local,
        ));
    }

    let artifact_id = positional_server.ok_or_else(|| anyhow!("artifact ID is required"))?;
    let local = positional_artifact_id.ok_or_else(|| anyhow!("local artifact path is required"))?;
    let server = server_option.ok_or_else(|| {
        anyhow!(
            "server endpoint is required (pass SERVER ARTIFACT_ID LOCAL or --server ARTIFACT_ID LOCAL/ASP_SERVER)"
        )
    })?;
    Ok((server, artifact_id, PathBuf::from(local)))
}

fn resolve_get_args(
    positional_server: Option<String>,
    positional_remote: Option<String>,
    positional_local: Option<PathBuf>,
    server_option: Option<String>,
) -> Result<(String, String, PathBuf)> {
    if let Some(local) = positional_local {
        return Ok((
            resolve_server_endpoint(positional_server, server_option)?,
            positional_remote.ok_or_else(|| anyhow!("remote path is required"))?,
            local,
        ));
    }

    let remote = positional_server.ok_or_else(|| anyhow!("remote path is required"))?;
    let local = positional_remote.ok_or_else(|| anyhow!("local path is required"))?;
    let server = server_option.ok_or_else(|| {
        anyhow!(
            "server endpoint is required (pass SERVER REMOTE LOCAL or --server REMOTE LOCAL/ASP_SERVER)"
        )
    })?;
    Ok((server, remote, PathBuf::from(local)))
}

fn path_argument_to_string(path: PathBuf, label: &str) -> Result<String> {
    path.into_os_string()
        .into_string()
        .map_err(|_| anyhow!("{label} must be valid UTF-8"))
}

fn resolve_put_args(
    positional_server: Option<String>,
    positional_local: Option<PathBuf>,
    positional_remote: Option<String>,
    server_option: Option<String>,
) -> Result<(String, PathBuf, String)> {
    if let Some(remote) = positional_remote {
        return Ok((
            resolve_server_endpoint(positional_server, server_option)?,
            positional_local.ok_or_else(|| anyhow!("local path is required"))?,
            remote,
        ));
    }

    let local = PathBuf::from(positional_server.ok_or_else(|| anyhow!("local path is required"))?);
    let remote = path_argument_to_string(
        positional_local.ok_or_else(|| anyhow!("remote path is required"))?,
        "remote path",
    )?;
    let server = server_option.ok_or_else(|| {
        anyhow!(
            "server endpoint is required (pass SERVER LOCAL REMOTE or --server LOCAL REMOTE/ASP_SERVER)"
        )
    })?;
    Ok((server, local, remote))
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b)
        .take_while(|(left, right)| left == right)
        .count()
}

fn common_suffix(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
}

/// Return whether a contiguous prefix/suffix patch is expected to beat a full
/// FILE_PUT body. The fixed allowance covers patch metadata (hashes and two
/// lengths) plus a margin for framing; compression is intentionally not part
/// of this decision because both request variants use the same v17 codec.
const FILE_PATCH_FIXED_OVERHEAD_BYTES: usize = 128;
/// Multi-range patches carry one offset/removal pair for each changed run.
/// Keep the selection deliberately conservative; the wire codec may compress
/// either variant, but this preflight must still be a win for source edits
/// before compression is considered.
const FILE_PATCH_RANGES_FIXED_OVERHEAD_BYTES: usize = 160;
const FILE_PATCH_RANGE_OVERHEAD_BYTES: usize = 24;
const FILE_PATCH_RANGE_COALESCE_GAP_BYTES: usize = 32;
// Line-aware matching is useful for source edits that insert/delete text at
// several distant locations, but an unbounded LCS table would turn a large
// generated file into a client-side latency/memory spike.  Above these
// limits the caller retains the single contiguous replacement fallback.
const FILE_PATCH_LINE_MAX_LINES: usize = 2_048;
const FILE_PATCH_LINE_MAX_LCS_CELLS: usize = 4_000_000;
const FILE_PATCH_MAX_DERIVED_RANGES: usize = 4_096;

fn should_use_file_patch(replacement_len: usize, full_file_len: usize) -> bool {
    replacement_len.saturating_add(FILE_PATCH_FIXED_OVERHEAD_BYTES) < full_file_len
}

fn file_patch_ranges_replacement_len(ranges: &[FilePatchRange]) -> usize {
    ranges.iter().fold(0usize, |total, range| {
        total.saturating_add(range.replacement.len())
    })
}

fn estimated_file_patch_ranges_wire_bytes(ranges: &[FilePatchRange]) -> usize {
    FILE_PATCH_RANGES_FIXED_OVERHEAD_BYTES
        .saturating_add(ranges.len().saturating_mul(FILE_PATCH_RANGE_OVERHEAD_BYTES))
        .saturating_add(file_patch_ranges_replacement_len(ranges))
}

/// Return whether a range patch is expected to beat a complete FILE_PUT. A
/// single range deliberately stays on the legacy prefix/suffix operation,
/// whose representation is smaller and already handles insert/delete edits.
fn should_use_file_patch_ranges(ranges: &[FilePatchRange], full_file_len: usize) -> bool {
    ranges.len() > 1
        && estimated_file_patch_ranges_wire_bytes(ranges) < full_file_len
        && file_patch_ranges_replacement_len(ranges) < full_file_len
}

/// Compute byte-aligned replacements for scattered edits. Equal-length
/// changed runs become independent ranges; small unchanged gaps are coalesced
/// to avoid turning a handful of adjacent edits into excessive metadata. For
/// length-changing source files, a bounded line-level LCS recovers independent
/// insertion/deletion/replacement ranges when the edit is naturally line
/// oriented. If matching would exceed the explicit CPU/memory bounds, one
/// contiguous range remains the unambiguous and conservative representation.
fn derive_file_patch_ranges(old: &[u8], new: &[u8]) -> Vec<FilePatchRange> {
    let prefix = common_prefix(old, new);
    let suffix = common_suffix(&old[prefix..], &new[prefix..]);
    let old_end = old.len().saturating_sub(suffix);
    let new_end = new.len().saturating_sub(suffix);
    let old_middle = &old[prefix..old_end];
    let new_middle = &new[prefix..new_end];
    if old_middle.len() != new_middle.len() {
        if let Some(ranges) = derive_line_patch_ranges(old, new) {
            return ranges;
        }
        return vec![FilePatchRange {
            offset: prefix as u64,
            remove_len: old_middle.len() as u64,
            replacement: new_middle.to_vec(),
        }];
    }

    let mut runs = Vec::<(usize, usize)>::new();
    let mut index = 0usize;
    while index < old_middle.len() {
        if old_middle[index] == new_middle[index] {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < old_middle.len() && old_middle[index] != new_middle[index] {
            index += 1;
        }
        runs.push((start, index));
    }
    if runs.is_empty() {
        return Vec::new();
    }

    let mut coalesced = Vec::<(usize, usize)>::with_capacity(runs.len());
    for (start, end) in runs {
        if let Some((_, previous_end)) = coalesced.last_mut()
            && start.saturating_sub(*previous_end) <= FILE_PATCH_RANGE_COALESCE_GAP_BYTES
        {
            *previous_end = end;
        } else {
            coalesced.push((start, end));
        }
    }
    coalesced
        .into_iter()
        .map(|(start, end)| FilePatchRange {
            offset: (prefix + start) as u64,
            remove_len: (end - start) as u64,
            replacement: new_middle[start..end].to_vec(),
        })
        .collect()
}

/// Return line-oriented ranges for a length-changing edit when the bounded
/// dynamic-programming table is affordable. The ranges address the original
/// byte coordinates, so the server can apply them in one pass without later
/// offsets shifting. A `None` result asks the caller to use the contiguous
/// prefix/suffix representation instead.
fn derive_line_patch_ranges(old: &[u8], new: &[u8]) -> Option<Vec<FilePatchRange>> {
    let old_lines = split_patch_lines(old);
    let new_lines = split_patch_lines(new);
    if old_lines.len() > FILE_PATCH_LINE_MAX_LINES || new_lines.len() > FILE_PATCH_LINE_MAX_LINES {
        return None;
    }
    let width = new_lines.len().checked_add(1)?;
    let cells = old_lines.len().checked_add(1)?.checked_mul(width)?;
    if cells > FILE_PATCH_LINE_MAX_LCS_CELLS {
        return None;
    }

    let mut lcs = vec![0_u32; cells];
    for old_index in (0..old_lines.len()).rev() {
        for new_index in (0..new_lines.len()).rev() {
            let cell = old_index * width + new_index;
            lcs[cell] = if old[old_lines[old_index].0..old_lines[old_index].1]
                == new[new_lines[new_index].0..new_lines[new_index].1]
            {
                1 + lcs[(old_index + 1) * width + new_index + 1]
            } else {
                lcs[(old_index + 1) * width + new_index].max(lcs[old_index * width + new_index + 1])
            };
        }
    }

    // Each tuple is (old start, old end, new start, new end). Keeping both
    // coordinate systems until after coalescing lets a merged range include
    // the unchanged gap bytes exactly once even when an insertion shifts the
    // following new-line offset.
    let mut edits = Vec::<(usize, usize, usize, usize)>::new();
    let (mut old_index, mut new_index) = (0usize, 0usize);
    let (mut old_anchor, mut new_anchor) = (0usize, 0usize);
    while old_index < old_lines.len() && new_index < new_lines.len() {
        let old_line = &old[old_lines[old_index].0..old_lines[old_index].1];
        let new_line = &new[new_lines[new_index].0..new_lines[new_index].1];
        if old_line == new_line {
            if old_index != old_anchor || new_index != new_anchor {
                edits.push((
                    patch_line_offset(&old_lines, old_anchor, old.len()),
                    patch_line_offset(&old_lines, old_index, old.len()),
                    patch_line_offset(&new_lines, new_anchor, new.len()),
                    patch_line_offset(&new_lines, new_index, new.len()),
                ));
            }
            old_index += 1;
            new_index += 1;
            old_anchor = old_index;
            new_anchor = new_index;
        } else if lcs[(old_index + 1) * width + new_index] >= lcs[old_index * width + new_index + 1]
        {
            old_index += 1;
        } else {
            new_index += 1;
        }
    }
    if old_index != old_anchor || new_index != new_anchor {
        edits.push((
            patch_line_offset(&old_lines, old_anchor, old.len()),
            patch_line_offset(&old_lines, old_index, old.len()),
            patch_line_offset(&new_lines, new_anchor, new.len()),
            patch_line_offset(&new_lines, new_index, new.len()),
        ));
    }

    if edits.len() > FILE_PATCH_MAX_DERIVED_RANGES {
        return None;
    }
    let mut coalesced = Vec::<(usize, usize, usize, usize)>::with_capacity(edits.len());
    for edit in edits {
        if edit.0 == edit.1 && edit.2 == edit.3 {
            continue;
        }
        if let Some(previous) = coalesced.last_mut()
            && edit.0.saturating_sub(previous.1) <= FILE_PATCH_RANGE_COALESCE_GAP_BYTES
        {
            previous.1 = edit.1;
            previous.3 = edit.3;
        } else {
            coalesced.push(edit);
        }
    }

    Some(
        coalesced
            .into_iter()
            .map(|(old_start, old_end, new_start, new_end)| FilePatchRange {
                offset: old_start as u64,
                remove_len: (old_end - old_start) as u64,
                replacement: new[new_start..new_end].to_vec(),
            })
            .collect(),
    )
}

fn split_patch_lines(data: &[u8]) -> Vec<(usize, usize)> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (index, byte) in data.iter().enumerate() {
        if *byte == b'\n' {
            lines.push((start, index + 1));
            start = index + 1;
        }
    }
    if start < data.len() {
        lines.push((start, data.len()));
    }
    lines
}

fn patch_line_offset(lines: &[(usize, usize)], index: usize, data_len: usize) -> usize {
    lines.get(index).map_or(data_len, |line| line.0)
}

/// Derive a hash-guarded contiguous patch from a file retained by a prior
/// semantic workspace inspection. The adapter never fetches a remote base
/// solely to make a patch: if the base is not already cached, the caller
/// keeps the ordinary FILE_PUT path. The conservative byte threshold avoids
/// replacing a small file with metadata that is larger than its body.
#[derive(Debug, PartialEq, Eq)]
enum CachedFilePatch {
    /// The caller's replacement bytes are already the inspected base. This
    /// can use a zero-byte metadata check, not a reason to fall back to a
    /// full FILE_PUT body.
    Unchanged {
        sha256: String,
    },
    Patch {
        prefix_len: u64,
        suffix_len: u64,
        replacement: Vec<u8>,
    },
    Ranges {
        ranges: Vec<FilePatchRange>,
    },
}

#[cfg(test)]
fn cached_file_patch(
    cache: &AgentWorkspaceCache,
    requested_path: &str,
    expected_sha256: &str,
    replacement_file: &[u8],
) -> Option<CachedFilePatch> {
    cached_file_patch_with_ranges(
        cache,
        requested_path,
        expected_sha256,
        replacement_file,
        false,
    )
}

fn cached_file_patch_with_ranges(
    cache: &AgentWorkspaceCache,
    requested_path: &str,
    expected_sha256: &str,
    replacement_file: &[u8],
    allow_ranges: bool,
) -> Option<CachedFilePatch> {
    for (key, state) in &cache.entries {
        for file in &state.files {
            let cached_path = if key.workspace == "." || key.workspace.is_empty() {
                file.path.clone()
            } else {
                format!("{}/{}", key.workspace.trim_end_matches('/'), file.path)
            };
            let cached_path = cached_path.strip_prefix("./").unwrap_or(&cached_path);
            let requested_path = requested_path.strip_prefix("./").unwrap_or(requested_path);
            if cached_path != requested_path || file.sha256 != expected_sha256 {
                continue;
            }
            if file.data == replacement_file {
                return Some(CachedFilePatch::Unchanged {
                    sha256: file.sha256.clone(),
                });
            }
            let ranges = derive_file_patch_ranges(&file.data, replacement_file);
            if allow_ranges && should_use_file_patch_ranges(&ranges, replacement_file.len()) {
                return Some(CachedFilePatch::Ranges { ranges });
            }
            let prefix = common_prefix(&file.data, replacement_file);
            let suffix = common_suffix(&file.data[prefix..], &replacement_file[prefix..]);
            let replacement = replacement_file[prefix..replacement_file.len() - suffix].to_vec();
            // Postcard carries the raw replacement, while FILE_PUT carries
            // the complete file. Leave a generous allowance for the patch's
            // fixed fields and hashes so this remains a bandwidth win even
            // before any frame compression is applied.
            if should_use_file_patch(replacement.len(), replacement_file.len()) {
                return Some(CachedFilePatch::Patch {
                    prefix_len: prefix as u64,
                    suffix_len: suffix as u64,
                    replacement,
                });
            }
        }
    }
    None
}

fn print_file_response(response: Response) -> Result<()> {
    match response {
        Response::FileStored {
            path,
            version,
            sha256,
        } => {
            println!("{path} version={version} sha256={sha256}");
            Ok(())
        }
        Response::Error { code, message } => bail!("{code}: {message}"),
        other => unexpected(other),
    }
}

fn unexpected<T>(response: Response) -> Result<T> {
    Err(anyhow!("unexpected response: {response:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asp_protocol::PtyRowDelta;
    use clap::CommandFactory;

    #[test]
    fn client_connection_options_publish_environment_defaults() {
        let command = Args::command();
        let env_name = |id: &str| {
            command
                .get_arguments()
                .find(|argument| argument.get_id().as_str() == id)
                .and_then(|argument| argument.get_env())
                .and_then(|value| value.to_str())
        };
        assert_eq!(env_name("cert"), Some("ASP_CERT"));
        assert_eq!(env_name("server_name"), Some("ASP_SERVER_NAME"));
        assert_eq!(env_name("session_file"), Some("ASP_SESSION_FILE"));
        assert_eq!(env_name("consumer_id"), Some("ASP_CONSUMER_ID"));
        assert_eq!(env_name("auth_token_file"), Some("ASP_AUTH_TOKEN_FILE"));
        assert_eq!(env_name("auth_token"), Some("ASP_AUTH_TOKEN"));
        assert_eq!(env_name("prefer_pty_delta"), Some("ASP_PREFER_PTY_DELTA"));
        assert_eq!(
            env_name("connect_timeout_ms"),
            Some("ASP_CONNECT_TIMEOUT_MS")
        );
        assert_eq!(
            env_name("reconnect_timeout_ms"),
            Some("ASP_RECONNECT_TIMEOUT_MS")
        );
        assert_eq!(env_name("client_cert"), Some("ASP_CLIENT_CERT"));
        assert_eq!(env_name("client_key"), Some("ASP_CLIENT_KEY"));

        for subcommand_name in [
            "doctor",
            "connect",
            "resume",
            "events",
            "logs",
            "status",
            "artifact-get",
            "artifact-put",
            "exec",
            "batch",
            "agent",
            "agent-listen",
            "spawn",
            "signal",
            "shell",
            "forward",
            "get",
            "put",
            "patch",
            "inspect",
            "agent-workload",
        ] {
            let subcommand = command
                .find_subcommand(subcommand_name)
                .expect("server-facing subcommand should be present");
            let server_env = subcommand.get_arguments().find_map(|argument| {
                if matches!(argument.get_id().as_str(), "server" | "server_option") {
                    argument.get_env().and_then(|value| value.to_str())
                } else {
                    None
                }
            });
            assert_eq!(server_env, Some("ASP_SERVER"), "{subcommand_name}");
        }
    }

    #[test]
    fn server_option_forms_recover_endpoint_before_positional_operands() {
        let process_id = Uuid::new_v4();
        let (server, parsed_process_id) = resolve_process_target(
            Some(process_id.to_string()),
            None,
            Some("127.0.0.1:4657".to_owned()),
        )
        .expect("server option should disambiguate a process ID");
        assert_eq!(server, "127.0.0.1:4657");
        assert_eq!(parsed_process_id, process_id);

        let (server, artifact_id, local) = resolve_artifact_get_args(
            Some("deadbeef".to_owned()),
            Some("artifact.bin".to_owned()),
            None,
            Some("127.0.0.1:4657".to_owned()),
        )
        .expect("server option should disambiguate artifact operands");
        assert_eq!(server, "127.0.0.1:4657");
        assert_eq!(artifact_id, "deadbeef");
        assert_eq!(local, PathBuf::from("artifact.bin"));

        let (server, remote, local) = resolve_get_args(
            Some("remote.txt".to_owned()),
            Some("local.txt".to_owned()),
            None,
            Some("127.0.0.1:4657".to_owned()),
        )
        .expect("server option should disambiguate file operands");
        assert_eq!(server, "127.0.0.1:4657");
        assert_eq!(remote, "remote.txt");
        assert_eq!(local, PathBuf::from("local.txt"));
    }

    #[test]
    fn exec_and_spawn_support_explicit_command_option() {
        let exec = Args::try_parse_from([
            "asp",
            "exec",
            "127.0.0.1:4657",
            "--summary",
            "--command",
            "printf agent-ok",
        ])
        .expect("exec command option should parse");
        let Command::Exec {
            server,
            summary,
            command_text,
            command,
            ..
        } = exec.command
        else {
            panic!("expected exec command");
        };
        assert_eq!(server, "127.0.0.1:4657");
        assert!(summary);
        assert_eq!(command_text.as_deref(), Some("printf agent-ok"));
        assert!(command.is_empty());

        let spawn =
            Args::try_parse_from(["asp", "spawn", "127.0.0.1:4657", "--command", "sleep 1"])
                .expect("spawn command option should parse");
        let Command::Spawn {
            server,
            command_text,
            command,
        } = spawn.command
        else {
            panic!("expected spawn command");
        };
        assert_eq!(server, "127.0.0.1:4657");
        assert_eq!(command_text.as_deref(), Some("sleep 1"));
        assert!(command.is_empty());
    }

    #[test]
    fn strict_doctor_accepts_authenticated_supported_tmux_endpoint() {
        assert!(validate_strict_doctor(PROTOCOL_VERSION, true, "tmux").is_ok());
        assert!(validate_strict_doctor(LEGACY_PROTOCOL_VERSION, true, "tmux").is_ok());
    }

    #[test]
    fn strict_doctor_rejects_insecure_or_unusable_endpoint() {
        let insecure = validate_strict_doctor(PROTOCOL_VERSION, false, "tmux").unwrap_err();
        assert!(insecure.to_string().contains("authentication is disabled"));

        let no_pty = validate_strict_doctor(PROTOCOL_VERSION, true, "unavailable").unwrap_err();
        assert!(no_pty.to_string().contains("durable PTY backend"));

        let unsupported = validate_strict_doctor(u16::MAX, true, "tmux").unwrap_err();
        assert!(
            unsupported
                .to_string()
                .contains("unsupported protocol version")
        );
    }

    #[test]
    fn readiness_url_is_literal_loopback_and_strictly_shaped() {
        assert_eq!(
            parse_ready_url("http://127.0.0.1:9443/ready").unwrap(),
            "127.0.0.1:9443".parse().unwrap()
        );
        assert_eq!(
            parse_ready_url("http://[::1]:9443/ready").unwrap(),
            "[::1]:9443".parse().unwrap()
        );
        assert!(parse_ready_url("https://127.0.0.1:9443/ready").is_err());
        assert!(parse_ready_url("http://127.0.0.1:9443/health").is_err());
        assert!(parse_ready_url("http://192.0.2.1:9443/ready").is_err());
        assert!(parse_ready_url("http://127.0.0.1:9443/ready?debug=1").is_err());
    }

    #[test]
    fn readiness_http_status_parser_requires_complete_response() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nok\n";
        let (status, body) = parse_ready_http_status(response).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"ok\n");
        assert!(parse_ready_http_status(b"HTTP/1.1 503 Service Unavailable\r\n").is_err());
        assert!(parse_ready_http_status(b"NOTHTTP 200 OK\r\n\r\n").is_err());
    }

    #[test]
    fn pty_generation_guard_rejects_stale_replaceable_or_reliable_state() {
        let mut current = 7;
        assert!(!record_pty_generation(&mut current, 6));
        assert_eq!(current, 7);
        assert!(record_pty_generation(&mut current, 8));
        assert_eq!(current, 8);
        assert!(!record_pty_generation(&mut current, 8));
        assert_eq!(current, 8);
    }

    #[test]
    fn pty_delta_preference_keeps_required_features_and_drops_rich_markers() {
        let rich = requested_features(false);
        assert!(rich.iter().any(|feature| feature == "pty_rich_state"));
        assert!(rich.iter().any(|feature| feature == "pty_state_delta"));

        let delta = requested_features(true);
        assert!(delta.iter().any(|feature| feature == "pty_state_delta"));
        assert!(!delta.iter().any(|feature| feature == "pty_rich_state"));
        assert!(
            !delta
                .iter()
                .any(|feature| feature == "pty_rich_compression")
        );
        for required in SUPPORTED_FEATURES {
            assert!(delta.iter().any(|feature| feature == required));
        }
    }

    #[test]
    fn plain_pty_delta_applies_changed_rows_and_advances_empty_generations() {
        let snapshot = PtySnapshot {
            generation: 4,
            rows: 3,
            cols: 12,
            screen: vec!["first".into(), "second".into(), "third".into()],
            cursor_row: 1,
            cursor_col: 2,
            tail: Vec::new(),
        };
        let mut state = PlainPtyScreenState::from_snapshot(&snapshot);
        let delta = PtyStateDeltaDatagram {
            session_id: Uuid::nil(),
            base_generation: 4,
            generation: 5,
            rows: 3,
            cols: 12,
            changes: vec![PtyRowDelta {
                row: 1,
                text: "updated".into(),
            }],
            cursor_row: 2,
            cursor_col: 7,
        };
        assert_eq!(state.apply_delta(&delta).unwrap(), Some(true));
        assert_eq!(state.generation, 5);
        assert_eq!(state.screen[1], "updated");
        assert_eq!((state.cursor_row, state.cursor_col), (2, 7));

        let invisible = PtyStateDeltaDatagram {
            base_generation: 5,
            generation: 6,
            rows: 3,
            cols: 12,
            changes: Vec::new(),
            cursor_row: 2,
            cursor_col: 7,
            ..delta.clone()
        };
        assert_eq!(state.apply_delta(&invisible).unwrap(), Some(false));
        assert_eq!(state.generation, 6);
    }

    #[test]
    fn plain_pty_delta_rejects_stale_geometry_and_ambiguous_rows() {
        let snapshot = PtySnapshot {
            generation: 9,
            rows: 3,
            cols: 12,
            screen: vec!["one".into(), "two".into(), "three".into()],
            cursor_row: 0,
            cursor_col: 0,
            tail: Vec::new(),
        };
        let mut state = PlainPtyScreenState::from_snapshot(&snapshot);
        let base = PtyStateDeltaDatagram {
            session_id: Uuid::nil(),
            base_generation: 8,
            generation: 10,
            rows: 3,
            cols: 12,
            changes: vec![PtyRowDelta {
                row: 0,
                text: "new".into(),
            }],
            cursor_row: 0,
            cursor_col: 0,
        };
        assert_eq!(state.apply_delta(&base).unwrap(), None);

        let geometry = PtyStateDeltaDatagram {
            base_generation: 9,
            rows: 4,
            ..base.clone()
        };
        assert_eq!(state.apply_delta(&geometry).unwrap(), None);

        let unsorted = PtyStateDeltaDatagram {
            base_generation: 9,
            changes: vec![
                PtyRowDelta {
                    row: 2,
                    text: "last".into(),
                },
                PtyRowDelta {
                    row: 1,
                    text: "middle".into(),
                },
            ],
            ..base.clone()
        };
        assert_eq!(state.apply_delta(&unsorted).unwrap(), None);

        let out_of_range = PtyStateDeltaDatagram {
            base_generation: 9,
            changes: vec![PtyRowDelta {
                row: 3,
                text: "invalid".into(),
            }],
            ..base
        };
        assert_eq!(state.apply_delta(&out_of_range).unwrap(), None);
        assert_eq!(state.generation, 9);
    }

    #[test]
    fn agent_log_commands_are_explicit_and_binary_safe() {
        assert_eq!(
            agent_log_command("compressible").unwrap(),
            "head -c 10485760 /dev/zero"
        );
        assert_eq!(
            agent_log_command("incompressible").unwrap(),
            "head -c 10485760 /dev/urandom"
        );
        assert_eq!(
            agent_log_command("mixed").unwrap(),
            "head -c 5242880 /dev/zero; head -c 5242880 /dev/urandom"
        );
        assert!(agent_log_command("unknown").is_err());
    }

    #[test]
    fn prefix_suffix_patch_is_minimal_for_middle_edit() {
        let old = b"abc OLD xyz";
        let new = b"abc NEW xyz";
        let prefix = common_prefix(old, new);
        let suffix = common_suffix(&old[prefix..], &new[prefix..]);
        assert_eq!(&new[prefix..new.len() - suffix], b"NEW");
    }

    #[test]
    fn adaptive_file_patch_prefers_delta_only_when_it_is_materially_smaller() {
        assert!(should_use_file_patch(8, 1024));
        assert!(!should_use_file_patch(896, 1024));
        assert!(!should_use_file_patch(1024, 1024));
        assert!(!should_use_file_patch(usize::MAX, 1));
    }

    #[test]
    fn scattered_file_patch_ranges_reconstruct_the_new_bytes() {
        let old = vec![b'a'; 4096];
        let mut new = old.clone();
        new[100..108].copy_from_slice(b"one-edit");
        new[1800..1811].copy_from_slice(b"second-edit");
        new[3500..3508].copy_from_slice(b"third-ed");
        let ranges = derive_file_patch_ranges(&old, &new);
        assert_eq!(ranges.len(), 3);
        assert!(
            ranges
                .windows(2)
                .all(|pair| pair[0].offset + pair[0].remove_len <= pair[1].offset)
        );
        let mut reconstructed = Vec::with_capacity(new.len());
        let mut cursor = 0usize;
        for range in &ranges {
            let offset = range.offset as usize;
            let end = offset + range.remove_len as usize;
            reconstructed.extend_from_slice(&old[cursor..offset]);
            reconstructed.extend_from_slice(&range.replacement);
            cursor = end;
        }
        reconstructed.extend_from_slice(&old[cursor..]);
        assert_eq!(reconstructed, new);
        assert!(should_use_file_patch_ranges(&ranges, new.len()));
    }

    #[test]
    fn length_changing_line_edits_use_multiple_ranges_and_reconstruct() {
        let old = b"fn alpha() {\n    old_alpha();\n}\n\nfn beta() {\n    old_beta();\n}\n\nfn gamma() {\n    old_gamma();\n}\n";
        let new = b"fn alpha() {\n    new_alpha();\n    extra_alpha();\n}\n\nfn beta() {\n    old_beta();\n}\n\nfn gamma() {\n    new_gamma();\n}\n";
        let ranges = derive_file_patch_ranges(old, new);
        assert!(
            ranges.len() >= 2,
            "expected independent line edits: {ranges:?}"
        );
        assert!(
            ranges
                .windows(2)
                .all(|pair| pair[0].offset + pair[0].remove_len <= pair[1].offset)
        );

        let mut reconstructed = Vec::with_capacity(new.len());
        let mut cursor = 0usize;
        for range in &ranges {
            let offset = range.offset as usize;
            let end = offset + range.remove_len as usize;
            reconstructed.extend_from_slice(&old[cursor..offset]);
            reconstructed.extend_from_slice(&range.replacement);
            cursor = end;
        }
        reconstructed.extend_from_slice(&old[cursor..]);
        assert_eq!(reconstructed, new);
    }

    #[test]
    fn large_length_changing_source_edits_choose_ranges() {
        let old = (0..512)
            .map(|index| format!("fn item_{index}() {{ old_{index}(); }}\n"))
            .collect::<String>();
        let mut new = old.clone();
        new = new.replacen(
            "fn item_20() { old_20(); }\n",
            "fn item_20() { new_20(); extra_20(); }\n",
            1,
        );
        new = new.replacen(
            "fn item_240() { old_240(); }\n",
            "fn item_240() { new_240(); }\ninserted_240();\n",
            1,
        );
        new = new.replacen(
            "fn item_460() { old_460(); }\n",
            "fn item_460() { new_460(); }\n",
            1,
        );
        let ranges = derive_file_patch_ranges(old.as_bytes(), new.as_bytes());
        assert!(
            ranges.len() >= 3,
            "expected independent source ranges: {ranges:?}"
        );
        assert!(should_use_file_patch_ranges(&ranges, new.len()));
    }

    #[test]
    fn line_patch_matching_has_bounded_fallback() {
        let old = (0..(FILE_PATCH_LINE_MAX_LINES + 1))
            .map(|index| format!("old-{index}\n"))
            .collect::<String>();
        let new = old.replace("old-1\n", "new-1\nextra-1\n");
        let ranges = derive_file_patch_ranges(old.as_bytes(), new.as_bytes());
        assert_eq!(ranges.len(), 1);
        let offset = ranges[0].offset as usize;
        let end = offset + ranges[0].remove_len as usize;
        let mut reconstructed = Vec::with_capacity(new.len());
        reconstructed.extend_from_slice(&old.as_bytes()[..offset]);
        reconstructed.extend_from_slice(&ranges[0].replacement);
        reconstructed.extend_from_slice(&old.as_bytes()[end..]);
        assert_eq!(reconstructed, new.as_bytes());
    }

    #[test]
    fn range_patch_selection_is_conservative_for_broad_rewrites() {
        let old = vec![b'a'; 4096];
        let new = vec![b'b'; 4096];
        let ranges = derive_file_patch_ranges(&old, &new);
        assert_eq!(ranges.len(), 1);
        assert!(!should_use_file_patch_ranges(&ranges, new.len()));
        assert!(!should_use_file_patch_ranges(
            &[FilePatchRange {
                offset: 0,
                remove_len: 1,
                replacement: vec![b'b'; 1],
            }],
            4096,
        ));
    }

    #[test]
    fn connect_reuses_by_default_and_requires_explicit_new_flag() {
        let args = Args::try_parse_from(["asp", "connect", "server.example"]).unwrap();
        assert!(matches!(args.command, Command::Connect { new: false, .. }));

        let args = Args::try_parse_from(["asp", "connect", "server.example", "--new"]).unwrap();
        assert!(matches!(args.command, Command::Connect { new: true, .. }));
    }

    #[test]
    fn resume_accepts_explicit_session_identity_and_cursor() {
        let session_id = Uuid::new_v4();
        let session_arg = session_id.to_string();
        let args = Args::try_parse_from([
            "asp",
            "resume",
            "server.example",
            "--session-id",
            session_arg.as_str(),
            "--after-event-id",
            "42",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            Command::Resume {
                session_id: Some(parsed),
                after_event_id: 42,
                ..
            } if parsed == session_id
        ));
    }

    #[test]
    fn batch_summary_is_bounded_and_requires_explicit_opt_in() {
        let args = Args::try_parse_from([
            "asp",
            "batch",
            "server.example",
            "--summary",
            "--tail-bytes",
            "4096",
            "--command",
            "cargo test",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            Command::Batch {
                summary: true,
                tail_bytes: 4096,
                commands,
                stdin: false,
                ..
            } if commands == vec!["cargo test"]
        ));

        let error = Args::try_parse_from([
            "asp",
            "batch",
            "server.example",
            "--tail-bytes",
            "4096",
            "--command",
            "cargo test",
        ])
        .expect_err("tail bytes without --summary must be rejected");
        assert!(error.to_string().contains("--summary"));
    }

    #[test]
    fn batch_parallel_is_explicit_zero_tail_summary_mode() {
        let args = Args::try_parse_from([
            "asp",
            "batch",
            "server.example",
            "--summary",
            "--tail-bytes",
            "0",
            "--parallel",
            "4",
            "--command",
            "git status",
            "--command",
            "cargo test --no-run",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            Command::Batch {
                summary: true,
                tail_bytes: 0,
                parallel: 4,
                commands,
                stdin: false,
                ..
            } if commands == vec!["git status", "cargo test --no-run"]
        ));
    }

    #[test]
    fn cached_file_patch_uses_inspected_base_without_fetching_it() {
        let mut old = vec![b'a'; 256];
        old.extend_from_slice(b"old body");
        old.extend_from_slice(&[b'z'; 256]);
        let mut new = vec![b'a'; 256];
        new.extend_from_slice(b"new body");
        new.extend_from_slice(&[b'z'; 256]);
        let old_sha256 = format!("{:x}", Sha256::digest(&old));
        let key = WorkspaceQueryKey::new(".", true, true, false, 0, &[], &["src/lib.rs".into()]);
        let mut cache = AgentWorkspaceCache::default();
        cache.insert(
            key,
            CachedWorkspaceState {
                digest: "d".repeat(64),
                tree_version: None,
                tree: Vec::new(),
                git_status: None,
                diff: None,
                recent_commits: Vec::new(),
                search_hits: Vec::new(),
                files: vec![WorkspaceFile {
                    path: "src/lib.rs".into(),
                    sha256: old_sha256.clone(),
                    data: old,
                }],
                bytes: 0,
            },
        );

        assert_eq!(
            cached_file_patch(&cache, "src/lib.rs", &old_sha256, &new),
            Some(CachedFilePatch::Patch {
                prefix_len: 256,
                suffix_len: 261,
                replacement: b"new".to_vec(),
            })
        );
    }

    #[test]
    fn cached_file_patch_keeps_small_or_uncached_files_on_full_put() {
        let old = b"same base".to_vec();
        let new = b"same face".to_vec();
        let key = WorkspaceQueryKey::new(".", true, true, false, 0, &[], &["small.txt".into()]);
        let mut cache = AgentWorkspaceCache::default();
        cache.insert(
            key,
            CachedWorkspaceState {
                digest: "e".repeat(64),
                tree_version: None,
                tree: Vec::new(),
                git_status: None,
                diff: None,
                recent_commits: Vec::new(),
                search_hits: Vec::new(),
                files: vec![WorkspaceFile {
                    path: "small.txt".into(),
                    sha256: format!("{:x}", Sha256::digest(&old)),
                    data: old,
                }],
                bytes: 0,
            },
        );
        assert!(
            cached_file_patch(
                &cache,
                "small.txt",
                &format!("{:x}", Sha256::digest(b"same base")),
                &new,
            )
            .is_none()
        );
        assert!(cached_file_patch(&cache, "missing.txt", &"f".repeat(64), b"new data").is_none());
    }

    #[test]
    fn cached_file_patch_reports_exact_base_as_noop() {
        let data = b"already current".to_vec();
        let digest = format!("{:x}", Sha256::digest(&data));
        let key = WorkspaceQueryKey::new(".", true, true, false, 0, &[], &["same.txt".into()]);
        let mut cache = AgentWorkspaceCache::default();
        cache.insert(
            key,
            CachedWorkspaceState {
                digest: "f".repeat(64),
                tree_version: None,
                tree: Vec::new(),
                git_status: None,
                diff: None,
                recent_commits: Vec::new(),
                search_hits: Vec::new(),
                files: vec![WorkspaceFile {
                    path: "same.txt".into(),
                    sha256: digest.clone(),
                    data: data.clone(),
                }],
                bytes: data.len(),
            },
        );
        assert_eq!(
            cached_file_patch(&cache, "same.txt", &digest, &data),
            Some(CachedFilePatch::Unchanged { sha256: digest })
        );
    }

    #[test]
    fn cached_file_patch_uses_ranges_for_scattered_equal_length_edits() {
        let mut old = vec![b'a'; 48 * 1024];
        let mut new = old.clone();
        for (offset, replacement) in [
            (512, b"ORIG-000".as_slice()),
            (16_384, b"ORIG-001".as_slice()),
            (40_960, b"ORIG-002".as_slice()),
        ] {
            old[offset..offset + replacement.len()].copy_from_slice(replacement);
        }
        for (offset, replacement) in [
            (512, b"EDIT-001".as_slice()),
            (16_384, b"EDIT-002".as_slice()),
            (40_960, b"EDIT-003".as_slice()),
        ] {
            new[offset..offset + replacement.len()].copy_from_slice(replacement);
        }
        let digest = format!("{:x}", Sha256::digest(&old));
        let key = WorkspaceQueryKey::new(".", false, false, false, 0, &[], &["ranges.txt".into()]);
        let mut cache = AgentWorkspaceCache::default();
        cache.insert(
            key,
            CachedWorkspaceState {
                digest: "r".repeat(64),
                tree_version: None,
                tree: Vec::new(),
                git_status: None,
                diff: None,
                recent_commits: Vec::new(),
                search_hits: Vec::new(),
                files: vec![WorkspaceFile {
                    path: "ranges.txt".into(),
                    sha256: digest.clone(),
                    data: old,
                }],
                bytes: 0,
            },
        );
        let Some(CachedFilePatch::Ranges { ranges }) =
            cached_file_patch_with_ranges(&cache, "ranges.txt", &digest, &new, true)
        else {
            panic!("expected multi-range cached patch");
        };
        assert_eq!(ranges.len(), 3);
        assert_eq!(file_patch_ranges_replacement_len(&ranges), 24);
    }

    #[test]
    fn consumer_cursor_lookup_prefers_isolated_entry_and_falls_back_for_bootstrap() {
        let legacy = SavedSession {
            session_id: Uuid::new_v4(),
            last_event_id: 7,
        };
        let isolated = SavedSession {
            session_id: legacy.session_id,
            last_event_id: 11,
        };
        let mut sessions = SavedSessions::default();
        sessions.servers.insert("server".into(), legacy.clone());
        sessions
            .consumers
            .entry("server".into())
            .or_default()
            .insert("agent-a".into(), isolated.clone());

        assert_eq!(
            lookup_saved_session(&sessions, "server", Some("agent-a"))
                .unwrap()
                .last_event_id,
            11
        );
        assert_eq!(
            lookup_saved_session(&sessions, "server", Some("agent-b"))
                .unwrap()
                .last_event_id,
            7
        );
        assert_eq!(
            lookup_saved_session(&sessions, "server", None)
                .unwrap()
                .last_event_id,
            7
        );
    }

    #[test]
    fn consumer_cursor_writes_are_independent_and_monotonic() {
        let session_id = Uuid::new_v4();
        let mut sessions = SavedSessions::default();
        insert_saved_session(
            &mut sessions,
            "server",
            Some("agent-a"),
            SavedSession {
                session_id,
                last_event_id: 10,
            },
        );
        insert_saved_session(
            &mut sessions,
            "server",
            Some("agent-a"),
            SavedSession {
                session_id,
                last_event_id: 4,
            },
        );
        insert_saved_session(
            &mut sessions,
            "server",
            Some("agent-b"),
            SavedSession {
                session_id,
                last_event_id: 3,
            },
        );

        assert_eq!(sessions.consumers["server"]["agent-a"].last_event_id, 10);
        assert_eq!(sessions.consumers["server"]["agent-b"].last_event_id, 3);
    }

    #[test]
    fn filtered_results_do_not_advance_durable_events() {
        let mut state = SavedSession {
            session_id: Uuid::new_v4(),
            last_event_id: 7,
        };

        // A process/result stream can expose a high event ID while unrelated
        // file or process events before it were not delivered on that stream.
        // Filtered result streams therefore have no authority to advance the
        // durable event-consumer cursor.
        assert_eq!(state.last_event_id, 7);

        // Only an explicit replay/subscription boundary may move the durable
        // consumer cursor.
        state.advance_event_cursor(50);
        assert_eq!(state.last_event_id, 50);
    }

    #[test]
    fn consumer_cursor_schema_reads_legacy_files_and_rejects_unsafe_ids() {
        let legacy = format!(
            r#"{{"servers":{{"server":{{"session_id":"{}","last_event_id":3}}}}}}"#,
            Uuid::new_v4()
        );
        let parsed: SavedSessions = serde_json::from_str(&legacy).unwrap();
        assert!(parsed.consumers.is_empty());
        assert_eq!(parsed.servers["server"].last_event_id, 3);
        assert!(validate_consumer_id(Some("agent-a")).is_ok());
        assert!(validate_consumer_id(Some("agent a")).is_err());
        assert!(validate_consumer_id(Some("agent\n-a")).is_err());
        assert!(validate_consumer_id(Some(&"x".repeat(129))).is_err());
    }

    #[test]
    fn saved_session_file_is_replaced_atomically() {
        let root = std::env::temp_dir().join(format!("asp-client-save-{}", Uuid::new_v4()));
        let path = root.join("sessions.json");
        let session_id = Uuid::new_v4();
        save(
            &path,
            "server",
            SavedSession {
                session_id,
                last_event_id: 4,
            },
        )
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.servers["server"].last_event_id, 4);
        // A second adapter can finish a stale request after another adapter
        // has advanced the shared cursor. The durable value must remain the
        // maximum cursor observed for this session.
        save(
            &path,
            "server",
            SavedSession {
                session_id,
                last_event_id: 2,
            },
        )
        .unwrap();
        assert_eq!(load(&path).unwrap().servers["server"].last_event_id, 4);
        save(
            &path,
            "server",
            SavedSession {
                session_id,
                last_event_id: 9,
            },
        )
        .unwrap();
        assert_eq!(load(&path).unwrap().servers["server"].last_event_id, 9);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(session_lock_path(&path).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saved_session_metadata_is_bounded_before_publish() {
        let root = std::env::temp_dir().join(format!("asp-client-save-limit-{}", Uuid::new_v4()));
        let path = root.join("sessions.json");
        let oversized_server = "s".repeat(MAX_CLIENT_METADATA_BYTES as usize);
        let error = save(
            &path,
            &oversized_server,
            SavedSession {
                session_id: Uuid::new_v4(),
                last_event_id: 1,
            },
        )
        .expect_err("oversized client metadata must fail closed");
        assert!(error.to_string().contains("session metadata exceeds"));
        assert!(!path.exists(), "an oversized cursor must not be published");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn session_lock_acquisition_is_async_and_serialized() {
        let root = std::env::temp_dir().join(format!("asp-client-lock-{}", Uuid::new_v4()));
        let path = root.join("sessions.json");
        let first = acquire_session_lock(&path).await.unwrap();
        let waiter_path = path.clone();
        let waiter = tokio::spawn(async move { acquire_session_lock(&waiter_path).await });
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(!waiter.is_finished());
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_session_state_is_not_treated_as_a_missing_session() {
        let root = std::env::temp_dir().join(format!("asp-client-corrupt-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("sessions.json");
        std::fs::write(&path, b"{not-json").unwrap();
        assert!(saved_session(&path, "server").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_credentials_reject_permissive_modes() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("asp-client-private-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("credential");
        std::fs::write(&path, b"credential").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private_file_limited(&path, 1024, "credential").is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_private_file_limited(&path, 1024, "credential").unwrap(),
            b"credential"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn session_parent_permissions_are_repaired_through_a_descriptor() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("asp-client-session-parent-{}", Uuid::new_v4()));
        let parent = root.join("state");
        let path = parent.join("sessions.json");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();

        prepare_session_parent(&path).unwrap();

        assert_eq!(
            std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn session_parent_rejects_a_final_component_symlink() {
        let root =
            std::env::temp_dir().join(format!("asp-client-session-symlink-{}", Uuid::new_v4()));
        let target = root.join("target");
        let link = root.join("state");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(prepare_session_parent(&link.join("sessions.json")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pinned_certificate_directory_is_bounded_and_sorted() {
        let root = std::env::temp_dir().join(format!("asp-client-cert-bundle-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("b.der"), b"second").unwrap();
        std::fs::write(root.join("a.der"), b"first").unwrap();
        std::fs::write(root.join("notes.txt"), b"ignored").unwrap();
        let certificates = read_pinned_server_certificates(&root).unwrap();
        assert_eq!(certificates, vec![b"first".to_vec(), b"second".to_vec()]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pinned_certificate_directory_rejects_symlinks() {
        let root = std::env::temp_dir().join(format!("asp-client-cert-symlink-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("target.der");
        let link = root.join("link.der");
        std::fs::write(&target, b"certificate").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_pinned_server_certificates(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upload_checkpoint_reuses_request_id_across_processes() {
        let root = std::env::temp_dir().join(format!("asp-client-upload-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let local = root.join("source.txt");
        std::fs::write(&local, b"upload body").unwrap();
        let digest = format!("{:x}", Sha256::digest(b"upload body"));
        let (first, lock, existed) = prepare_upload_checkpoint(
            &local,
            "server:4433",
            Uuid::new_v4(),
            "remote/source.txt",
            11,
            &digest,
            None,
            false,
        )
        .unwrap();
        assert!(!existed);
        drop(lock);
        let (second, lock, existed) = prepare_upload_checkpoint(
            &local,
            "server:4433",
            first.session_id,
            "remote/source.txt",
            11,
            &digest,
            None,
            false,
        )
        .unwrap();
        assert!(existed);
        assert_eq!(first.request_id, second.request_id);
        drop(lock);
        clear_upload_checkpoint(&upload_checkpoint_paths(&local).0).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upload_checkpoint_binds_precondition_and_blind_policy() {
        let root =
            std::env::temp_dir().join(format!("asp-client-upload-policy-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let local = root.join("source.txt");
        std::fs::write(&local, b"upload body").unwrap();
        let digest = format!("{:x}", Sha256::digest(b"upload body"));
        let expected = "a".repeat(64);
        let (guarded, lock, existed) = prepare_upload_checkpoint(
            &local,
            "server:4433",
            Uuid::new_v4(),
            "remote/source.txt",
            11,
            &digest,
            Some(&expected),
            false,
        )
        .unwrap();
        assert!(!existed);
        assert_eq!(guarded.expected_sha256.as_deref(), Some(expected.as_str()));
        assert!(!guarded.allow_blind);
        drop(lock);

        let (resumed, lock, existed) = prepare_upload_checkpoint(
            &local,
            "server:4433",
            guarded.session_id,
            "remote/source.txt",
            11,
            &digest,
            Some(&expected),
            false,
        )
        .unwrap();
        assert!(existed);
        assert_eq!(resumed.request_id, guarded.request_id);
        drop(lock);

        let (new_policy, lock, existed) = prepare_upload_checkpoint(
            &local,
            "server:4433",
            guarded.session_id,
            "remote/source.txt",
            11,
            &digest,
            None,
            true,
        )
        .unwrap();
        assert!(!existed);
        assert_ne!(new_policy.request_id, guarded.request_id);
        assert!(new_policy.allow_blind);
        assert!(new_policy.expected_sha256.is_none());
        drop(lock);

        clear_upload_checkpoint(&upload_checkpoint_paths(&local).0).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expected_sha256_validation_is_strict() {
        assert!(valid_sha256(&"a".repeat(64)));
        assert!(!valid_sha256(&"a".repeat(63)));
        assert!(!valid_sha256(&format!("{}g", "a".repeat(63))));
    }

    #[test]
    fn tls_server_name_validation_rejects_unsafe_values() {
        assert!(validate_server_name("localhost").is_ok());
        assert!(validate_server_name("asp.example.test").is_ok());
        assert!(validate_server_name("127.0.0.1").is_ok());
        assert!(validate_server_name("").is_err());
        assert!(validate_server_name("asp example").is_err());
    }

    #[test]
    fn output_replay_suffix_is_deduplicated_and_gaps_are_rejected() {
        assert_eq!(unseen_suffix(0, b"first", 0).unwrap(), Some(&b"first"[..]));
        assert_eq!(unseen_suffix(0, b"first", 5).unwrap(), None);
        assert_eq!(
            unseen_suffix(5, b"second", 5).unwrap(),
            Some(&b"second"[..])
        );
        assert!(unseen_suffix(7, b"gap", 5).is_err());
    }

    #[test]
    fn process_log_stream_names_are_strict() {
        assert_eq!(parse_output_stream("stdout").unwrap(), OutputStream::Stdout);
        assert_eq!(
            parse_output_stream(" StDeRr ").unwrap(),
            OutputStream::Stderr
        );
        assert!(parse_output_stream("combined").is_err());
    }

    #[test]
    fn process_log_tail_range_is_bounded_and_snapshot_relative() {
        assert_eq!(process_log_tail_range(100, 25).unwrap(), (75, 25));
        assert_eq!(process_log_tail_range(100, 200).unwrap(), (0, 100));
        assert_eq!(process_log_tail_range(100, 0).unwrap(), (100, 0));
        assert!(process_log_tail_range(100, PROCESS_LOG_RANGE_MAX_BYTES + 1).is_err());
        assert!(process_log_tail_range(PROCESS_OUTPUT_MAX_BYTES + 1, 1).is_err());
    }

    #[test]
    fn expected_agent_socket_disconnects_are_limited_to_shutdown_errors() {
        assert!(expected_agent_socket_disconnect(&std::io::Error::from(
            std::io::ErrorKind::BrokenPipe,
        )));
        assert!(expected_agent_socket_disconnect(&std::io::Error::from(
            std::io::ErrorKind::ConnectionReset,
        )));
        assert!(expected_agent_socket_disconnect(&std::io::Error::from(
            std::io::ErrorKind::NotConnected,
        )));
        assert!(!expected_agent_socket_disconnect(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )));
    }

    #[test]
    fn signal_names_are_strict_and_case_insensitive() {
        assert_eq!(parse_signal("TERM").unwrap(), 15);
        assert_eq!(parse_signal("sigint").unwrap(), 2);
        assert_eq!(parse_signal("9").unwrap(), 9);
        assert!(parse_signal("USR1").is_err());
    }

    #[test]
    fn agent_requests_are_strict_and_preserve_ids() {
        let request: AgentRequest = serde_json::from_str(
            r#"{"id":"inspect-7","op":"exec_summary","request_id":"00000000-0000-0000-0000-000000000007","command":"cargo test","tail_bytes":4096}"#,
        )
        .unwrap();
        assert_eq!(request.id.as_deref(), Some("inspect-7"));
        assert_eq!(request.op, "exec_summary");
        assert_eq!(
            request.request_id.unwrap().to_string(),
            "00000000-0000-0000-0000-000000000007"
        );
        assert_eq!(request.tail_bytes, Some(4096));
        let spawn: AgentRequest = serde_json::from_str(
            r#"{"id":"spawn-1","op":"spawn","request_id":"00000000-0000-0000-0000-000000000009","command":"python -m http.server 8000"}"#,
        )
        .unwrap();
        assert_eq!(spawn.op, "spawn");
        assert_eq!(spawn.command.as_deref(), Some("python -m http.server 8000"));
        let logs: AgentRequest = serde_json::from_str(
            r#"{"id":"logs-1","op":"logs","process_id":"00000000-0000-0000-0000-000000000009","stream":"stderr","offset":128,"length":4096}"#,
        )
        .unwrap();
        assert_eq!(logs.op, "logs");
        assert_eq!(logs.stream.as_deref(), Some("stderr"));
        assert_eq!(logs.offset, 128);
        assert_eq!(logs.length, Some(4096));
        let tail: AgentRequest = serde_json::from_str(
            r#"{"op":"logs","process_id":"00000000-0000-0000-0000-000000000009","stream":"stdout","tail_bytes":8192}"#,
        )
        .unwrap();
        assert_eq!(tail.tail_bytes, Some(8192));
        assert!(
            serde_json::from_str::<AgentRequest>(r#"{"op":"ping","unexpected":true}"#).is_err()
        );
    }

    #[tokio::test]
    async fn bounded_agent_lines_drain_oversized_input_without_allocating_it() {
        let mut bytes = vec![b'x'; AGENT_INPUT_MAX_BYTES + 1];
        bytes.push(b'\n');
        bytes.extend_from_slice(br#"{"op":"ping"}"#);
        bytes.push(b'\n');
        let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(bytes));
        let mut line = Vec::new();
        assert_eq!(
            read_bounded_agent_line(&mut reader, &mut line, AGENT_INPUT_MAX_BYTES)
                .await
                .unwrap(),
            Some(true)
        );
        assert_eq!(
            read_bounded_agent_line(&mut reader, &mut line, AGENT_INPUT_MAX_BYTES)
                .await
                .unwrap(),
            Some(false)
        );
        assert_eq!(std::str::from_utf8(&line).unwrap(), "{\"op\":\"ping\"}\n");
        assert_eq!(
            read_bounded_agent_line(&mut reader, &mut line, AGENT_INPUT_MAX_BYTES)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn agent_output_queue_accounts_bytes_until_writer_consumes_them() {
        let (sender, mut receiver) = mpsc::channel(2);
        let output = AgentOutput {
            sender,
            memory: Arc::new(Semaphore::new(1)),
        };
        // One permit represents one 16 KiB accounting unit in this focused
        // test. A queued line keeps it held until the writer drops the
        // message, so a second line is refused rather than growing memory.
        output.enqueue(vec![b'x']).unwrap();
        assert!(output.enqueue(vec![b'y']).is_err());
        let message = receiver.recv().await.expect("queued output line");
        drop(message);
        output.enqueue(vec![b'z']).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn agent_socket_requires_private_absolute_parent() {
        use std::os::unix::fs::PermissionsExt;

        let root = PathBuf::from(format!("/tmp/asp-agent-socket-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("agent.sock");
        assert!(validate_agent_socket_path(Path::new("agent.sock"), false).is_err());
        assert!(validate_agent_socket_path(&path, false).is_ok());

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(validate_agent_socket_path(&path, false).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_socket_refuses_active_listener_and_removes_cleanly() {
        let root = PathBuf::from(format!("/tmp/asp-agent-socket-live-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("agent.sock");
        let listener = bind_agent_socket(&path).await.unwrap();
        assert!(bind_agent_socket(&path).await.is_err());
        drop(listener);
        remove_agent_socket(&path).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn agent_socket_cleanup_is_idempotent_when_listener_path_is_missing() {
        let root = PathBuf::from(format!("/tmp/asp-agent-socket-missing-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("agent.sock");
        // The listener's common error/shutdown path may race a supervisor or
        // operator that already removed the endpoint. Treating that state as
        // clean keeps recovery from requiring a manual stale-socket delete.
        assert!(remove_agent_socket(&path).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_workspace_and_file_fields_decode() {
        let workspace: AgentRequest = serde_json::from_str(
            r#"{"op":"inspect","workspace":".","searches":["TODO"],"read_paths":["src/lib.rs"],"diff":true,"recent_commits":3,"known_state_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
        )
        .unwrap();
        assert_eq!(workspace.workspace.as_deref(), Some("."));
        assert!(workspace.include_tree);
        assert!(workspace.include_git_status);
        assert_eq!(workspace.searches, vec!["TODO"]);
        assert_eq!(workspace.read_paths, vec!["src/lib.rs"]);
        assert!(workspace.diff);
        assert_eq!(workspace.recent_commits, 3);
        assert!(workspace.known_state_digest.is_some());

        let no_tree = Args::try_parse_from(["asp", "inspect", "server", "--no-tree"]).unwrap();
        assert!(matches!(
            no_tree.command,
            Command::Inspect { no_tree: true, .. }
        ));

        let no_git_status =
            Args::try_parse_from(["asp", "inspect", "server", "--no-git-status"]).unwrap();
        assert!(matches!(
            no_git_status.command,
            Command::Inspect {
                no_git_status: true,
                ..
            }
        ));

        let no_git_status_request: AgentRequest =
            serde_json::from_str(r#"{"op":"inspect","include_git_status":false}"#).unwrap();
        assert!(!no_git_status_request.include_git_status);

        let patch: AgentRequest = serde_json::from_str(
            r#"{"op":"file_patch","path":"src/lib.rs","request_id":"00000000-0000-0000-0000-000000000008","expected_sha256":"abc","prefix_len":4,"suffix_len":2,"replacement_base64":"bmV3"}"#,
        )
        .unwrap();
        assert_eq!(patch.path.as_deref(), Some("src/lib.rs"));
        assert_eq!(patch.prefix_len, Some(4));
        assert_eq!(patch.suffix_len, Some(2));
        assert_eq!(patch.replacement_base64.as_deref(), Some("bmV3"));

        let range_patch: AgentRequest = serde_json::from_str(
            r#"{"op":"file_patch_ranges","path":"src/lib.rs","expected_sha256":"abc","ranges":[{"offset":4,"remove_len":2,"replacement_base64":"bmV3"}]}"#,
        )
        .unwrap();
        assert_eq!(range_patch.ranges.len(), 1);
        assert_eq!(range_patch.ranges[0].offset, 4);
        assert_eq!(range_patch.ranges[0].remove_len, 2);
        assert_eq!(range_patch.ranges[0].replacement_base64, "bmV3");

        let put: AgentRequest = serde_json::from_str(
            r#"{"op":"file_put","path":"src/lib.rs","expected_sha256":"def","force":false,"data_base64":"bmV3"}"#,
        )
        .unwrap();
        assert_eq!(put.expected_sha256.as_deref(), Some("def"));
        assert!(!put.force);

        let signal: AgentRequest = serde_json::from_str(
            r#"{"op":"signal","process_id":"00000000-0000-0000-0000-000000000009","signal":"KILL"}"#,
        )
        .unwrap();
        assert_eq!(
            signal.process_id.unwrap().to_string(),
            "00000000-0000-0000-0000-000000000009"
        );
        assert_eq!(signal.signal.as_deref(), Some("KILL"));
    }

    #[test]
    fn workspace_cache_is_bounded_and_replaces_entries() {
        let mut cache = AgentWorkspaceCache::default();
        for index in 0..(AGENT_WORKSPACE_CACHE_MAX_ENTRIES + 4) {
            let key = WorkspaceQueryKey::new(
                &format!("workspace-{index}"),
                true,
                true,
                false,
                0,
                &[],
                &[],
            );
            cache.insert(
                key,
                CachedWorkspaceState {
                    digest: format!("{index:0>64}"),
                    tree_version: None,
                    tree: Vec::new(),
                    git_status: None,
                    diff: None,
                    recent_commits: Vec::new(),
                    search_hits: Vec::new(),
                    files: Vec::new(),
                    bytes: 0,
                },
            );
        }
        assert!(cache.entries.len() <= AGENT_WORKSPACE_CACHE_MAX_ENTRIES);
        assert!(cache.bytes <= AGENT_WORKSPACE_CACHE_MAX_BYTES);

        let key = WorkspaceQueryKey::new("replace", true, true, false, 0, &[], &[]);
        let state = |digest: String| CachedWorkspaceState {
            digest,
            tree_version: None,
            tree: Vec::new(),
            git_status: None,
            diff: None,
            recent_commits: Vec::new(),
            search_hits: Vec::new(),
            files: Vec::new(),
            bytes: 0,
        };
        cache.insert(key.clone(), state("a".repeat(64)));
        cache.insert(key.clone(), state("b".repeat(64)));
        assert!(
            cache
                .get(&key)
                .is_some_and(|entry| entry.digest == "b".repeat(64))
        );
    }

    #[test]
    fn resume_cursor_refresh_invalidates_workspace_hints() {
        let mut versions = HashMap::from([(
            ".".to_owned(),
            WorkspaceVersion {
                epoch: Uuid::new_v4(),
                generation: 1,
            },
        )]);
        let mut cache = AgentWorkspaceCache::default();
        cache.insert(
            WorkspaceQueryKey::new(".", true, true, false, 0, &[], &[]),
            CachedWorkspaceState {
                digest: "a".repeat(64),
                tree_version: None,
                tree: Vec::new(),
                git_status: None,
                diff: None,
                recent_commits: Vec::new(),
                search_hits: Vec::new(),
                files: Vec::new(),
                bytes: 0,
            },
        );
        assert!(!versions.is_empty());
        assert!(!cache.entries.is_empty());

        invalidate_workspace_caches(&mut versions, &mut cache);

        assert!(versions.is_empty());
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn restart_errors_are_retryable_but_application_errors_are_not() {
        assert!(retryable_connection_error(&anyhow!(
            "connect to host: aborted by peer: the server refused to accept a new connection"
        )));
        assert!(retryable_connection_error(&anyhow!("connect to host")));
        assert!(retryable_connection_error(&anyhow!(
            "closed by peer: server shutdown (code 0)"
        )));
        assert!(retryable_connection_error(&anyhow!(
            "closed by peer: application closed (code 0)"
        )));
        assert!(!retryable_connection_error(&anyhow!(
            "authentication_failed: client auth token is invalid"
        )));
        assert!(!retryable_connection_error(&anyhow!(
            "connect to host: invalid peer certificate"
        )));
        assert!(!retryable_connection_error(&anyhow!(
            "connect to host: invalid server name"
        )));
        assert!(!retryable_connection_error(&anyhow!(
            "invalid_cursor: resume cursor is ahead of journal head"
        )));
        assert!(!retryable_connection_error(&anyhow!(
            "principal_byte_budget: request byte budget exceeded"
        )));
        assert!(!retryable_connection_error(&anyhow!(
            "principal_response_budget: response byte budget exceeded"
        )));
        assert!(!retryable_connection_error(&anyhow!(
            "process_spawn_failed: resource limit rejected"
        )));
        assert!(!retryable_connection_error(&anyhow!(
            "exec_timeout: command exceeded its configured wall-clock limit"
        )));
        assert!(!retryable_connection_error(&anyhow!(
            "idempotency_capacity: request table is full"
        )));
        assert!(retryable_connection_error(&anyhow!(
            "server_busy: request limit reached"
        )));
        assert!(retryable_connection_error(&anyhow!(
            "request stream open timeout"
        )));
        assert!(retryable_connection_error(&anyhow!(
            "open QUIC bidirectional request stream"
        )));
        assert!(retryable_connection_error(&anyhow!(
            "one-shot response timeout"
        )));
        assert!(is_server_busy_error(&anyhow!(
            "server_busy: response memory temporarily exhausted"
        )));
        assert!(!is_server_busy_error(&anyhow!(
            "connection closed before response"
        )));
    }

    #[test]
    fn connect_timeout_is_bounded_and_configurable() {
        assert_eq!(
            validate_connect_timeout_ms(1).unwrap(),
            Duration::from_millis(1)
        );
        assert_eq!(
            validate_connect_timeout_ms(DEFAULT_CONNECT_TIMEOUT_MS).unwrap(),
            CONNECT_TIMEOUT
        );
        assert_eq!(
            validate_connect_timeout_ms(MAX_CONNECT_TIMEOUT_MS).unwrap(),
            Duration::from_millis(MAX_CONNECT_TIMEOUT_MS)
        );
        assert!(validate_connect_timeout_ms(0).is_err());
        assert!(validate_connect_timeout_ms(MAX_CONNECT_TIMEOUT_MS + 1).is_err());
    }

    #[test]
    fn connect_attempts_are_staggered_without_unbounded_delay() {
        assert_eq!(connect_attempt_delay(0), Duration::ZERO);
        assert_eq!(connect_attempt_delay(1), CONNECT_ATTEMPT_STAGGER);
        assert_eq!(connect_attempt_delay(3), Duration::from_millis(150));
        assert!(connect_attempt_delay(usize::MAX) >= connect_attempt_delay(3));
    }

    #[test]
    fn reconnect_timeout_is_bounded_and_configurable() {
        assert_eq!(
            validate_reconnect_timeout_ms(1).unwrap(),
            Duration::from_millis(1)
        );
        assert_eq!(
            validate_reconnect_timeout_ms(DEFAULT_RECONNECT_TIMEOUT_MS).unwrap(),
            RECONNECT_RETRY_WINDOW
        );
        assert_eq!(
            validate_reconnect_timeout_ms(MAX_RECONNECT_TIMEOUT_MS).unwrap(),
            Duration::from_millis(MAX_RECONNECT_TIMEOUT_MS)
        );
        assert!(validate_reconnect_timeout_ms(0).is_err());
        assert!(validate_reconnect_timeout_ms(MAX_RECONNECT_TIMEOUT_MS + 1).is_err());
    }

    #[test]
    fn request_frame_write_deadline_has_rate_floor_and_cap() {
        assert_eq!(
            request_frame_write_timeout(0),
            REQUEST_FRAME_MIN_WRITE_TIMEOUT
        );
        assert_eq!(
            request_frame_write_timeout(REQUEST_FRAME_MIN_RATE_BYTES_PER_SECOND as usize),
            REQUEST_FRAME_MIN_WRITE_TIMEOUT
        );
        assert_eq!(
            request_frame_write_timeout((REQUEST_FRAME_MIN_RATE_BYTES_PER_SECOND * 11) as usize),
            Duration::from_secs(11)
        );
        assert_eq!(
            request_frame_write_timeout(usize::MAX),
            REQUEST_FRAME_MAX_WRITE_TIMEOUT
        );
    }

    #[tokio::test]
    async fn async_resolution_accepts_literal_socket_addresses() {
        assert_eq!(
            resolve("127.0.0.1:4433").await.unwrap(),
            vec!["127.0.0.1:4433".parse::<SocketAddr>().unwrap()]
        );
    }

    #[tokio::test]
    async fn request_frame_codec_preserves_v16_and_v17_contracts() {
        let request = Request::Exec {
            session_id: Uuid::nil(),
            request_id: Uuid::from_u128(7),
            command: "x".repeat(asp_protocol::FRAME_COMPRESSION_MIN_BYTES * 2),
        };
        let current = encode_request_frame_payload(request.clone(), PROTOCOL_VERSION)
            .await
            .unwrap();
        assert_eq!(&current[..2], b"AF");
        let current_decoded =
            asp_protocol::decode_frame_payload_for_version(&current, PROTOCOL_VERSION).unwrap();
        assert_eq!(
            asp_protocol::decode_message::<Request>(&current_decoded).unwrap(),
            request
        );

        let legacy = encode_request_frame_payload(request.clone(), LEGACY_PROTOCOL_VERSION)
            .await
            .unwrap();
        let legacy_decoded =
            asp_protocol::decode_frame_payload_for_version(&legacy, LEGACY_PROTOCOL_VERSION)
                .unwrap();
        assert_eq!(
            asp_protocol::decode_message::<Request>(&legacy_decoded).unwrap(),
            request
        );
    }

    #[test]
    fn large_request_body_classifier_keeps_typing_inline() {
        let session_id = Uuid::nil();
        let small = Request::PtyInput {
            session_id,
            data: vec![b'x'; REQUEST_BODY_OFFLOAD_THRESHOLD - 1],
        };
        assert!(!request_has_large_body(&small));

        let large = Request::FilePutStreamChunk {
            offset: 0,
            data: vec![b'x'; REQUEST_BODY_OFFLOAD_THRESHOLD],
        };
        assert!(request_has_large_body(&large));

        let large_command = Request::Exec {
            session_id,
            request_id: Uuid::from_u128(8),
            command: "x".repeat(REQUEST_BODY_OFFLOAD_THRESHOLD),
        };
        assert!(request_has_large_body(&large_command));

        let large_workspace = Request::WorkspaceState {
            session_id,
            workspace: ".".into(),
            include_tree: false,
            include_git_status: false,
            include_diff: false,
            recent_commits: 0,
            searches: vec!["q".repeat(REQUEST_BODY_OFFLOAD_THRESHOLD)],
            read_paths: Vec::new(),
            known_tree_version: None,
            known_state_digest: None,
        };
        assert!(request_has_large_body(&large_workspace));
    }

    #[test]
    fn response_codec_offload_avoids_task_per_transfer_chunk() {
        let plain_chunk = vec![0_u8; FILE_STREAM_CHUNK_BYTES];
        assert!(!should_offload_response_codec(
            &plain_chunk,
            PROTOCOL_VERSION
        ));
        assert!(!should_offload_response_codec(
            &plain_chunk,
            LEGACY_PROTOCOL_VERSION
        ));

        let plain_large = vec![0_u8; PLAIN_RESPONSE_CODEC_OFFLOAD_MIN_BYTES];
        assert!(should_offload_response_codec(
            &plain_large,
            PROTOCOL_VERSION
        ));
        assert!(should_offload_response_codec(
            &plain_large,
            LEGACY_PROTOCOL_VERSION
        ));

        let compressed_header = vec![b'A', b'F', 1, 0, 0, 0, 1];
        assert!(should_offload_response_codec(
            &compressed_header,
            PROTOCOL_VERSION
        ));
        assert!(!should_offload_response_codec(
            &compressed_header,
            LEGACY_PROTOCOL_VERSION
        ));
    }

    #[test]
    fn legacy_fallback_only_triggers_for_protocol_handshake_failures() {
        assert!(should_try_legacy_version(&anyhow!(
            "ASP protocol v17 handshake failed: server closed protocol handshake response"
        )));
        assert!(should_try_legacy_version(&anyhow!(
            "ASP protocol v17 handshake failed: ASP protocol v17 handshake response timeout"
        )));
        assert!(should_try_legacy_version(&anyhow!(
            "ASP protocol v17 handshake failed: decode frame envelope marker"
        )));
        assert!(should_try_legacy_version(&anyhow!(
            "ASP protocol v17 handshake failed: version_mismatch"
        )));
        assert!(!should_try_legacy_version(&anyhow!(
            "ASP protocol v17 handshake failed: authentication_failed"
        )));
        assert!(!should_try_legacy_version(&anyhow!(
            "connect to host timed out"
        )));
        assert!(!should_try_legacy_version(&anyhow!(
            "ASP protocol v17 handshake failed: server did not negotiate required features"
        )));
        assert!(!should_try_legacy_version(&anyhow!(
            "ASP protocol v17 handshake failed: server_busy"
        )));
    }

    #[test]
    fn cached_legacy_version_is_reprobed_only_for_protocol_failures() {
        assert!(should_try_current_version(&anyhow!(
            "ASP protocol v16 handshake failed: version_mismatch"
        )));
        assert!(should_try_current_version(&anyhow!(
            "ASP protocol v16 handshake failed: server closed response"
        )));
        assert!(!should_try_current_version(&anyhow!(
            "ASP protocol v16 handshake failed: authentication_failed"
        )));
        assert!(!should_try_current_version(&anyhow!(
            "connect to host timed out"
        )));
    }

    #[test]
    fn endpoint_version_hint_can_be_invalidated() {
        let server = format!("cache-test-{}", Uuid::new_v4());
        assert_eq!(cached_server_version(&server), None);
        remember_server_version(&server, LEGACY_PROTOCOL_VERSION);
        assert_eq!(
            cached_server_version(&server),
            Some(LEGACY_PROTOCOL_VERSION)
        );
        forget_server_version(&server);
        assert_eq!(cached_server_version(&server), None);
    }

    #[test]
    fn forward_listener_defaults_to_loopback() {
        assert!(validate_forward_listener("127.0.0.1:8080".parse().unwrap(), false).is_ok());
        assert!(validate_forward_listener("[::1]:8080".parse().unwrap(), false).is_ok());
        assert!(validate_forward_listener("0.0.0.0:8080".parse().unwrap(), false).is_err());
        assert!(validate_forward_listener("0.0.0.0:8080".parse().unwrap(), true).is_ok());
    }
}
