//! Wire messages and framing for ASP v0.

use anyhow::{Context, Result, bail};
use flate2::{Compression, write::ZlibEncoder};
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::io::{Read as _, Write as _};
use std::marker::PhantomData;
use uuid::Uuid;

// Changing an enum variant's fields, the stream-frame envelope, or adding a
// durable event shape changes the wire contract and therefore requires a new
// negotiated protocol version. An appended, capability-gated enum variant is
// safe without a version bump when it is sent only after the peer advertises
// that capability; older peers continue decoding the original discriminants.
// Peers on unsupported versions must fail closed rather than treating
// workspace digest, compression, or artifact-retention fields as if they were
// absent.
pub const PROTOCOL_VERSION: u16 = 17;
/// v16 used the same Postcard message shapes with a plain length-prefixed
/// payload. It remains readable during a v17 rolling deployment; the current
/// v17 client still prefers the current envelope and never guesses a legacy
/// shape unless the connection explicitly negotiates v16.
pub const LEGACY_PROTOCOL_VERSION: u16 = 16;
/// Versions this binary can decode without an explicit translation layer.
/// Keep this list explicit: accepting an unknown Postcard enum shape would
/// turn a rolling upgrade into silent state corruption. Add a version only
/// after a compatibility test covers every changed variant.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[u16] = &[LEGACY_PROTOCOL_VERSION, PROTOCOL_VERSION];

pub fn protocol_version_supported(version: u16) -> bool {
    SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
}

/// Return whether it is worth paying the zlib invocation cost for a payload.
/// Small frames always follow the existing size threshold. For larger frames,
/// sample three separated windows and skip only data with near-uniform byte
/// diversity. This targets compressed artifacts and random blobs while keeping
/// normal source/log text on the compression path.
pub fn should_attempt_frame_compression(payload: &[u8]) -> bool {
    if payload.len() < FRAME_COMPRESSION_MIN_BYTES {
        return false;
    }
    if payload.len() < FRAME_COMPRESSION_HEURISTIC_MIN_BYTES {
        return true;
    }

    let window = FRAME_COMPRESSION_SAMPLE_BYTES.min(payload.len());
    let starts = [
        0,
        payload.len().saturating_sub(window) / 2,
        payload.len().saturating_sub(window),
    ];
    let mut histogram = [0usize; 256];
    let mut sampled = 0usize;
    for start in starts {
        let end = start.saturating_add(window).min(payload.len());
        for &byte in &payload[start..end] {
            histogram[byte as usize] = histogram[byte as usize].saturating_add(1);
            sampled = sampled.saturating_add(1);
        }
    }
    let distinct = histogram.iter().filter(|&&count| count != 0).count();
    let largest_bucket = histogram.iter().copied().max().unwrap_or(0);
    !(distinct >= FRAME_COMPRESSION_HIGH_DIVERSITY_BYTES && largest_bucket <= sampled / 32)
}

pub const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;
/// Every stream frame has a fixed marker/encoding/original-length header. The
/// logical message limit above remains the decompressed limit; the wire limit
/// includes this small header so a maximum-size plain message is still legal.
pub const FRAME_HEADER_BYTES: usize = 7;
pub const MAX_WIRE_FRAME_BYTES: usize = MAX_FRAME_BYTES + FRAME_HEADER_BYTES;
const FRAME_MAGIC: [u8; 2] = *b"AF";
const FRAME_ENCODING_PLAIN: u8 = 0;
const FRAME_ENCODING_ZLIB: u8 = 1;
/// Small control messages are not worth a codec invocation. Larger frames
/// use zlib only when it actually saves bytes; incompressible data keeps the
/// plain representation and pays no decompression cost.
pub const FRAME_COMPRESSION_MIN_BYTES: usize = 1024;
/// Above this size, a cheap sample avoids invoking zlib for payloads that look
/// like already-compressed/binary data. The decision is conservative: a
/// payload is skipped only when sampled byte diversity is extremely high and
/// no single byte repeats often. A false negative costs compression savings;
/// it never changes the wire contract or correctness.
pub const FRAME_COMPRESSION_HEURISTIC_MIN_BYTES: usize = 64 * 1024;
const FRAME_COMPRESSION_SAMPLE_BYTES: usize = 4096;
const FRAME_COMPRESSION_HIGH_DIVERSITY_BYTES: usize = 240;
/// Postcard serialization/deserialization of a large plain frame can still
/// consume a Tokio worker even when compression is intentionally skipped.
/// Keep small control messages inline, but move bodies at or above this size
/// to the blocking pool on both endpoints so bulk artifacts/logs do not delay
/// interactive requests sharing the connection.
pub const FRAME_CODEC_OFFLOAD_MIN_BYTES: usize = 64 * 1024;
// Quinn's defaults target roughly 100 Mbps/100 ms. These bounded windows
// preserve that behavior while avoiding a small per-stream window throttling
// large file/log transfers on higher-BDP private links. The connection-wide
// cap keeps the worst-case receive buffering bounded even with many streams.
pub const QUIC_STREAM_WINDOW_BYTES: u32 = 8 * 1024 * 1024;
pub const QUIC_CONNECTION_WINDOW_BYTES: u32 = 32 * 1024 * 1024;
pub const QUIC_SEND_WINDOW_BYTES: u64 = 32 * 1024 * 1024;
/// Bound each endpoint's application-datagram queue. This is deliberately
/// separate from the reliable stream windows: PTY screen/presence datagrams
/// are replaceable and must not be allowed to grow without bound when a peer
/// is slow or temporarily unreachable.
pub const QUIC_DATAGRAM_BUFFER_BYTES: usize = 1024 * 1024;
// Keepalive probes still preserve healthy idle PTY/event attachments. This
// bound controls how quickly a dead path is declared lost so clients can
// reconnect/resume instead of waiting Quinn's 30-second default.
pub const QUIC_MAX_IDLE_TIMEOUT_SECONDS: u64 = 15;
// Quinn schedules locally buffered send streams by priority. Keep interactive
// PTY/control traffic ahead of bulk logs, files, and workspace snapshots when
// they share a congested connection. These are transport hints only; QUIC's
// congestion control, pacing, and flow control remain authoritative.
pub const QUIC_STREAM_PRIORITY_INTERACTIVE: i32 = 10;
pub const QUIC_STREAM_PRIORITY_CONTROL: i32 = 5;
pub const QUIC_STREAM_PRIORITY_BULK: i32 = -5;
pub const SUPPORTED_FEATURES: &[&str] = &[
    "resume_stream",
    "file_stream",
    "file_upload_resume",
    "workspace_state",
    "pty_datagram",
    "principal_scopes",
    "event_subscriptions",
    "port_forward",
    "exec_summary",
    "process_log_stream",
    "file_preconditions",
    "workspace_index",
    "workspace_digest",
    "artifact_stream",
];

/// Capabilities that may be used when both peers advertise them, but are not
/// required for the v0 client/server contract. Keeping this list separate from
/// `SUPPORTED_FEATURES` lets a new daemon add capabilities such as durable
/// event-consumer leases or attribute-preserving PTY snapshots without making
/// an older v17 peer fail its HELLO handshake.
pub const OPTIONAL_FEATURES: &[&str] = &[
    "event_consumer_leases",
    "pty_rich_state",
    "pty_rich_compression",
    "file_patch_ranges",
    "pty_state_delta",
    "pty_scrollback",
];

pub fn feature_supported(feature: &str) -> bool {
    SUPPORTED_FEATURES.contains(&feature) || OPTIONAL_FEATURES.contains(&feature)
}

/// Apply the bounded Quinn transport profile used by both ASP endpoints.
/// Keeping this in the protocol crate prevents the client and server from
/// accidentally diverging in flow-control, keepalive, datagram, or stream
/// scheduling behavior. Quinn remains the authority for congestion control,
/// loss recovery, migration, and pacing.
pub fn configure_quic_transport(transport: &mut quinn::TransportConfig) -> Result<()> {
    transport.max_concurrent_bidi_streams(128_u32.into());
    transport.max_concurrent_uni_streams(128_u32.into());
    transport.stream_receive_window(QUIC_STREAM_WINDOW_BYTES.into());
    transport.receive_window(QUIC_CONNECTION_WINDOW_BYTES.into());
    transport.send_window(QUIC_SEND_WINDOW_BYTES);
    transport.max_idle_timeout(Some(
        std::time::Duration::from_secs(QUIC_MAX_IDLE_TIMEOUT_SECONDS)
            .try_into()
            .context("invalid QUIC idle timeout")?,
    ));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    transport.datagram_receive_buffer_size(Some(QUIC_DATAGRAM_BUFFER_BYTES));
    transport.datagram_send_buffer_size(QUIC_DATAGRAM_BUFFER_BYTES);
    // Explicitly retain Quinn's fair queue for streams at the same priority;
    // the ASP classes above provide strict ordering between interactive,
    // control, and bulk streams while equal-class work remains fair.
    transport.send_fairness(true);
    Ok(())
}

/// Stable operation labels used by the machine-readable schema registry and
/// audit/metrics integrations. Continuation frames are listed explicitly so
/// an implementation can validate the complete request surface without
/// guessing from Rust enum discriminants.
pub const SUPPORTED_OPERATIONS: &[&str] = &[
    "hello",
    "hello_features",
    "health",
    "open_session",
    "resume_session",
    "resume_session_stream",
    "ack_events",
    "ack_events_consumer",
    "subscribe_events",
    "exec",
    "exec_summary",
    "process_output_stream",
    "process_state",
    "spawn",
    "signal",
    "pty_open",
    "pty_input",
    "pty_input_sequenced",
    "pty_resize",
    "file_get",
    "file_get_stream",
    "file_put",
    "file_put_stream_begin",
    "file_put_stream_resume_begin",
    "file_put_stream_chunk",
    "file_put_stream_end",
    "file_patch",
    "file_patch_ranges",
    "workspace_state",
    "port_open",
    "artifact_get_stream",
    "artifact_put_stream_begin",
    "artifact_put_stream_resume_begin",
    "artifact_put_stream_chunk",
    "artifact_put_stream_end",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionCreated,
    ProcessStarted {
        process_id: Uuid,
        command: String,
        pid: Option<u32>,
        request_id: Option<Uuid>,
    },
    ProcessOutput {
        process_id: Uuid,
        stream: OutputStream,
        offset: u64,
        data: Vec<u8>,
    },
    ProcessExited {
        process_id: Uuid,
        code: Option<i32>,
    },
    PtyStarted,
    PtyStateAdvanced {
        generation: u64,
    },
    FileChanged {
        path: String,
        version: u64,
    },
    /// Durable result metadata for a file mutation. This is separate from
    /// `FileChanged` so older event readers can still materialize file
    /// versions while v5 clients gain persisted idempotency.
    FileMutation {
        request_id: Uuid,
        request_hash: String,
        path: String,
        version: u64,
        sha256: String,
    },
    SignalApplied {
        request_id: Uuid,
        request_hash: String,
        process_id: Uuid,
        signal: i32,
        success: bool,
        message: Option<String>,
    },
    /// Immutable content-addressed object committed to the session artifact
    /// store. The payload itself lives outside the event journal; the event
    /// makes the object discoverable and replayable after reconnect.
    ArtifactCreated {
        request_id: Uuid,
        request_hash: String,
        artifact_id: String,
        total_size: u64,
        name: Option<String>,
    },
    /// Durable tombstone for an artifact removed by retention/GC. The object
    /// itself is stored outside the journal; replaying this event removes its
    /// metadata so a restart cannot resurrect a collected object.
    ArtifactDeleted {
        artifact_id: String,
        total_size: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub id: u64,
    pub unix_ms: u64,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtySnapshot {
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub screen: Vec<String>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    /// Bounded raw output retained for diagnostics and clients without screen rendering.
    pub tail: Vec<u8>,
}

/// Bounded terminal history sent on a reliable PTY attachment for peers that
/// negotiate `pty_scrollback`.  The current screen remains in `PtySnapshot`
/// or `PtyRichSnapshot`; these plain lines are only the most recent history
/// page needed to make a fresh client process feel continuous after a restart.
/// Formatting is intentionally omitted so shell output cannot inject terminal
/// control sequences through the replaceable history view.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtyScrollbackSnapshot {
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub lines: Vec<String>,
}

/// Maximum history page emitted by the optional PTY scrollback capability.
/// These bounds apply before framing and keep a reconnect from turning a
/// terminal attachment into an unbounded log transfer.
pub const PTY_SCROLLBACK_MAX_LINES: usize = 256;
pub const PTY_SCROLLBACK_MAX_LINE_BYTES: usize = 64 * 1024;
pub const PTY_SCROLLBACK_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessSnapshot {
    pub process_id: Uuid,
    pub command: String,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub session_id: Uuid,
    pub latest_event_id: u64,
    pub processes: Vec<ProcessSnapshot>,
    pub pty: Option<PtySnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceTreeEntry {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceSearchHit {
    pub query: String,
    pub path: String,
    pub line: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceFile {
    pub path: String,
    pub sha256: String,
    pub data: Vec<u8>,
}

/// A process-epoch-scoped workspace index version. The epoch prevents a
/// client from treating a token from before a daemon restart as current; the
/// generation advances whenever the watcher-backed index is rebuilt after a
/// filesystem invalidation. It is a cache validator, not a file-version
/// clock or an authorization token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceVersion {
    pub epoch: Uuid,
    pub generation: u64,
}

/// One byte-range replacement in a hash-guarded multi-range file patch.
/// `offset` and `remove_len` address the pre-edit file. Ranges must be sorted,
/// non-overlapping, and may be applied in one pass without shifting later
/// offsets. A range with `remove_len == 0` is an insertion; an empty
/// replacement is a deletion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilePatchRange {
    pub offset: u64,
    pub remove_len: u64,
    pub replacement: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u16,
        auth_token: Option<String>,
    },
    /// Extended HELLO that negotiates optional protocol capabilities while
    /// retaining the original HELLO variant for older peers.
    HelloFeatures {
        version: u16,
        auth_token: Option<String>,
        features: Vec<String>,
    },
    Health,
    OpenSession {
        request_id: Uuid,
    },
    ResumeSession {
        session_id: Uuid,
        last_event_id: u64,
    },
    /// Streaming variant of RESUME. It avoids constructing a single large
    /// response frame when a disconnected agent has a substantial journal
    /// tail to catch up.
    ResumeSessionStream {
        session_id: Uuid,
        last_event_id: u64,
    },
    AckEvents {
        session_id: Uuid,
        through_event_id: u64,
    },
    /// Open a reliable, cursor-based event subscription. The server sends the
    /// retained backlog first, then follows the live journal until the stream
    /// is closed or the subscriber falls too far behind.
    SubscribeEvents {
        session_id: Uuid,
        after_event_id: u64,
        process_id: Option<Uuid>,
        include_output: bool,
    },
    Exec {
        session_id: Uuid,
        request_id: Uuid,
        command: String,
    },
    /// Execute a command while returning only bounded aggregate counts and
    /// the final tail of each output stream. The process and full output
    /// remain durable in the session journal for later resume/subscription.
    ExecSummary {
        session_id: Uuid,
        request_id: Uuid,
        command: String,
        tail_bytes: u32,
    },
    /// Read a bounded, snapshot-length range from a durable process log. This
    /// remains useful after the corresponding journal output events compact.
    ProcessOutputStream {
        session_id: Uuid,
        process_id: Uuid,
        stream: OutputStream,
        offset: u64,
        length: Option<u64>,
    },
    /// Read the current durable state of one process without replaying the
    /// entire session snapshot. This is useful for detached agents and
    /// operators that need a cheap running/exited check before fetching logs.
    ProcessState {
        session_id: Uuid,
        process_id: Uuid,
    },
    Spawn {
        session_id: Uuid,
        request_id: Uuid,
        command: String,
    },
    Signal {
        session_id: Uuid,
        request_id: Uuid,
        process_id: Uuid,
        signal: i32,
    },
    PtyOpen {
        session_id: Uuid,
        rows: u16,
        cols: u16,
    },
    PtyInput {
        session_id: Uuid,
        data: Vec<u8>,
    },
    /// Ordered PTY input for clients that need duplicate detection. Sequence
    /// numbers start at zero for each PTY attachment stream; a reconnect starts
    /// a fresh input epoch instead of guessing whether an unacknowledged byte
    /// reached the shell.
    PtyInputSequenced {
        session_id: Uuid,
        sequence: u64,
        data: Vec<u8>,
    },
    PtyResize {
        session_id: Uuid,
        rows: u16,
        cols: u16,
    },
    FileGet {
        session_id: Uuid,
        path: String,
    },
    /// Range-capable file download. The response is a bounded sequence of
    /// `FILE_STREAM_*` frames on the same reliable stream.
    FileGetStream {
        session_id: Uuid,
        path: String,
        offset: u64,
        length: Option<u64>,
    },
    FilePut {
        session_id: Uuid,
        request_id: Uuid,
        path: String,
        /// SHA-256 of the file the caller edited. `None` is an explicit
        /// create-only request unless `allow_blind` is true.
        expected_sha256: Option<String>,
        /// Permit replacing an existing file without a hash precondition.
        /// This is intentionally explicit so concurrent agents cannot
        /// silently lose an edit.
        allow_blind: bool,
        data: Vec<u8>,
    },
    /// Starts a resumable file upload. The client follows this with ordered
    /// `FilePutStreamChunk` frames and `FilePutStreamEnd`.
    FilePutStreamBegin {
        session_id: Uuid,
        request_id: Uuid,
        path: String,
        total_size: u64,
        sha256: String,
        expected_sha256: Option<String>,
        allow_blind: bool,
    },
    /// Resume a previously staged upload. The server responds with the
    /// durable prefix offset before the client sends more chunks.
    FilePutStreamResumeBegin {
        session_id: Uuid,
        request_id: Uuid,
        path: String,
        total_size: u64,
        sha256: String,
        expected_sha256: Option<String>,
        allow_blind: bool,
    },
    FilePutStreamChunk {
        offset: u64,
        data: Vec<u8>,
    },
    FilePutStreamEnd,
    FilePatch {
        session_id: Uuid,
        request_id: Uuid,
        path: String,
        expected_sha256: String,
        prefix_len: u64,
        suffix_len: u64,
        replacement: Vec<u8>,
    },
    WorkspaceState {
        session_id: Uuid,
        workspace: String,
        include_tree: bool,
        include_git_status: bool,
        include_diff: bool,
        recent_commits: u16,
        searches: Vec<String>,
        read_paths: Vec<String>,
        /// Optional validator for the tree/search index. The server only
        /// omits an unchanged tree when this token matches a stable cached
        /// index; other requested fields are still evaluated normally.
        known_tree_version: Option<WorkspaceVersion>,
        /// Optional digest of the complete requested workspace result. When
        /// it matches, the server can return `state_unchanged=true` and omit
        /// all repeated tree/Git/search/file payloads.
        known_state_digest: Option<String>,
    },
    /// Open one bidirectional stream to a service on the server host. v0
    /// intentionally permits only loopback targets; this is a dev-server
    /// forward, not a general-purpose VPN primitive.
    PortOpen {
        session_id: Uuid,
        host: String,
        port: u16,
    },
    /// Read a bounded range from an immutable content-addressed artifact.
    /// The response is a sequence of `ARTIFACT_STREAM_*` frames on this
    /// reliable stream and can be retried from a later offset.
    ArtifactGetStream {
        session_id: Uuid,
        artifact_id: String,
        offset: u64,
        length: Option<u64>,
    },
    /// Starts a content-addressed artifact upload. The caller supplies the
    /// final SHA-256 so the object can be installed atomically without a
    /// second naming or metadata round trip.
    ArtifactPutStreamBegin {
        session_id: Uuid,
        request_id: Uuid,
        artifact_id: String,
        total_size: u64,
        name: Option<String>,
    },
    /// Resume a staged artifact upload after a transport/client restart.
    ArtifactPutStreamResumeBegin {
        session_id: Uuid,
        request_id: Uuid,
        artifact_id: String,
        total_size: u64,
        name: Option<String>,
    },
    ArtifactPutStreamChunk {
        offset: u64,
        data: Vec<u8>,
    },
    ArtifactPutStreamEnd,
    /// Cumulative acknowledgement for one named event consumer.  This is an
    /// additive v0 variant gated by `event_consumer_leases`; peers that do not
    /// advertise the feature continue to use `ACK_EVENTS`.
    AckEventsConsumer {
        session_id: Uuid,
        consumer_id: String,
        through_event_id: u64,
    },
    /// Apply several non-overlapping replacements against one hash-guarded
    /// base file. This is an additive, optional v17 capability: clients must
    /// negotiate `file_patch_ranges` before sending it, and servers must
    /// reject it with `unsupported_feature` otherwise. It is deliberately
    /// appended so all pre-existing Postcard enum discriminants remain stable.
    FilePatchRanges {
        session_id: Uuid,
        request_id: Uuid,
        path: String,
        expected_sha256: String,
        ranges: Vec<FilePatchRange>,
    },
}

/// Return the local QUIC send priority for a request/response stream. A
/// response stream inherits the request's class on the server, while clients
/// use the same class for their request bytes. Quinn's scheduler only applies
/// this to data already buffered locally, so this does not create a second
/// reliability or congestion-control layer.
pub fn quic_stream_priority(request: &Request) -> i32 {
    match request {
        Request::PtyOpen { .. }
        | Request::PtyInput { .. }
        | Request::PtyInputSequenced { .. }
        | Request::PtyResize { .. } => QUIC_STREAM_PRIORITY_INTERACTIVE,
        // Summary mode deliberately does not forward the full transcript.
        // Keep its bounded verdict on the control lane so a command result
        // does not wait behind a bulk log/file stream.  Exact EXEC remains
        // bulk because its response can contain arbitrarily large output.
        Request::ExecSummary { .. } => QUIC_STREAM_PRIORITY_CONTROL,
        // A legacy whole-file request is normally a small control operation,
        // but it can carry a 16 MiB body.  Once the body is large enough that
        // codec work is offloaded, classify the stream as bulk so it cannot
        // starve PTY input or session control on a shared connection.  The
        // same adaptive rule applies to a large prefix/suffix replacement.
        Request::FilePut { data, .. } if data.len() >= FRAME_CODEC_OFFLOAD_MIN_BYTES => {
            QUIC_STREAM_PRIORITY_BULK
        }
        Request::FilePatch { replacement, .. }
            if replacement.len() >= FRAME_CODEC_OFFLOAD_MIN_BYTES =>
        {
            QUIC_STREAM_PRIORITY_BULK
        }
        Request::FilePatchRanges { ranges, .. }
            if ranges.iter().fold(0usize, |total, range| {
                total.saturating_add(range.replacement.len())
            }) >= FRAME_CODEC_OFFLOAD_MIN_BYTES =>
        {
            QUIC_STREAM_PRIORITY_BULK
        }
        Request::Hello { .. }
        | Request::HelloFeatures { .. }
        | Request::Health
        | Request::OpenSession { .. }
        | Request::AckEvents { .. }
        | Request::AckEventsConsumer { .. }
        | Request::Spawn { .. }
        | Request::Signal { .. }
        | Request::ProcessState { .. }
        | Request::FilePut { .. }
        | Request::FilePutStreamBegin { .. }
        | Request::FilePutStreamResumeBegin { .. }
        | Request::FilePatch { .. }
        | Request::FilePatchRanges { .. } => QUIC_STREAM_PRIORITY_CONTROL,
        Request::FilePutStreamChunk { .. } | Request::FilePutStreamEnd => QUIC_STREAM_PRIORITY_BULK,
        Request::ResumeSession { .. }
        | Request::ResumeSessionStream { .. }
        | Request::SubscribeEvents { .. }
        | Request::Exec { .. }
        | Request::ProcessOutputStream { .. }
        | Request::FileGet { .. }
        | Request::FileGetStream { .. }
        | Request::WorkspaceState { .. }
        | Request::PortOpen { .. }
        | Request::ArtifactGetStream { .. }
        | Request::ArtifactPutStreamBegin { .. }
        | Request::ArtifactPutStreamResumeBegin { .. } => QUIC_STREAM_PRIORITY_BULK,
        Request::ArtifactPutStreamChunk { .. } | Request::ArtifactPutStreamEnd => {
            QUIC_STREAM_PRIORITY_BULK
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Hello {
        version: u16,
        server: String,
    },
    HelloFeatures {
        version: u16,
        server: String,
        features: Vec<String>,
    },
    Health {
        protocol_version: u16,
        server: String,
        sessions: u32,
        running_processes: u32,
        active_connections: u32,
        event_log_bytes: u64,
        uptime_ms: u64,
        requests_total: u64,
        request_failures: u64,
        auth_required: bool,
        pty_backend: String,
    },
    SessionOpened {
        session_id: Uuid,
        event_id: u64,
    },
    Resumed {
        snapshot: SessionSnapshot,
        events: Vec<Event>,
        compacted: bool,
        retained_from_event_id: u64,
    },
    ResumeBegin {
        snapshot: SessionSnapshot,
        compacted: bool,
        retained_from_event_id: u64,
        event_count: u64,
    },
    ResumeEvent {
        event: Event,
    },
    ResumeEnd {
        through_event_id: u64,
    },
    Acked {
        through_event_id: u64,
    },
    SubscriptionReady {
        snapshot: SessionSnapshot,
        through_event_id: u64,
        retained_from_event_id: u64,
        compacted: bool,
    },
    EventNotification {
        event: Event,
    },
    ProcessAccepted {
        process_id: Uuid,
        event_id: u64,
    },
    ProcessOutput {
        process_id: Uuid,
        event_id: u64,
        stream: OutputStream,
        offset: u64,
        data: Vec<u8>,
    },
    /// Bounded result metadata for `EXEC_SUMMARY`. The event id is the
    /// corresponding durable PROCESS_EXITED event; a following
    /// `PROCESS_EXITED` frame preserves the normal lifecycle contract.
    ProcessSummary {
        process_id: Uuid,
        event_id: u64,
        stdout_bytes: u64,
        stderr_bytes: u64,
        stdout_tail: Vec<u8>,
        stderr_tail: Vec<u8>,
        stdout_truncated: bool,
        stderr_truncated: bool,
    },
    ProcessOutputStreamBegin {
        process_id: Uuid,
        stream: OutputStream,
        total_size: u64,
        offset: u64,
        length: u64,
    },
    ProcessOutputStreamChunk {
        offset: u64,
        data: Vec<u8>,
    },
    ProcessOutputStreamEnd {
        bytes: u64,
        complete: bool,
    },
    /// Current state for one process, including durable output byte counts and
    /// the last known exit code. The response is a point-in-time read and does
    /// not create an idempotency record.
    ProcessState {
        snapshot: ProcessSnapshot,
    },
    ProcessExited {
        process_id: Uuid,
        event_id: u64,
        code: Option<i32>,
    },
    PtyReady {
        snapshot: PtySnapshot,
    },
    PtyOutput {
        generation: u64,
        data: Vec<u8>,
    },
    PtyInputAck {
        sequence: u64,
    },
    FileData {
        path: String,
        version: u64,
        sha256: String,
        data: Vec<u8>,
    },
    FileStreamBegin {
        path: String,
        version: u64,
        total_size: u64,
        offset: u64,
        length: u64,
        sha256: String,
    },
    FileStreamChunk {
        offset: u64,
        data: Vec<u8>,
    },
    FileStreamEnd {
        bytes: u64,
        sha256: String,
    },
    /// Server-side durable upload staging offset. This response is emitted
    /// only for `FILE_PUT_STREAM_RESUME_BEGIN`.
    FileUploadReady {
        path: String,
        total_size: u64,
        offset: u64,
        sha256: String,
    },
    FileStored {
        path: String,
        version: u64,
        sha256: String,
    },
    WorkspaceState {
        workspace: String,
        tree_version: Option<WorkspaceVersion>,
        tree_unchanged: bool,
        tree: Vec<WorkspaceTreeEntry>,
        git_status: Option<String>,
        diff: Option<String>,
        recent_commits: Vec<String>,
        search_hits: Vec<WorkspaceSearchHit>,
        files: Vec<WorkspaceFile>,
        /// SHA-256 over the complete requested semantic result. It is a
        /// validator for the next identical query, never an authorization
        /// token. An unchanged response omits the repeated payload fields.
        state_digest: String,
        state_unchanged: bool,
    },
    PortReady {
        host: String,
        port: u16,
    },
    ArtifactUploadReady {
        artifact_id: String,
        total_size: u64,
        offset: u64,
    },
    ArtifactStored {
        artifact_id: String,
        total_size: u64,
        name: Option<String>,
        event_id: u64,
    },
    ArtifactStreamBegin {
        artifact_id: String,
        total_size: u64,
        offset: u64,
        length: u64,
        sha256: String,
        name: Option<String>,
    },
    ArtifactStreamChunk {
        offset: u64,
        data: Vec<u8>,
    },
    ArtifactStreamEnd {
        bytes: u64,
        sha256: String,
    },
    Error {
        code: String,
        message: String,
    },
    /// Marks the end of the backlog captured by `SUBSCRIBE_EVENTS`. This
    /// additive response is sent only when `event_consumer_leases` was
    /// negotiated, allowing a filtered subscriber to acknowledge the whole
    /// boundary even when no matching event was delivered.
    SubscriptionCaughtUp {
        through_event_id: u64,
    },
    /// Additive PTY attachment response for peers that negotiate
    /// `pty_rich_state`. Kept at the end of the enum so existing variant
    /// discriminants remain stable for v16/v17 peers.
    PtyReadyRich {
        snapshot: PtyRichSnapshot,
    },
    /// Additive PTY history response gated by `pty_scrollback`.  It is kept
    /// at the end of the enum so existing v16/v17 discriminants remain stable.
    PtyReadyScrollback {
        snapshot: PtyScrollbackSnapshot,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtyStateDatagram {
    pub session_id: Uuid,
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub screen: Vec<String>,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

/// Replaceable PTY datagrams have their own bounds because QUIC DATAGRAM
/// payloads do not pass through the reliable stream-frame memory admission
/// path. The bounded wire visitors below also prevent Postcard from reserving
/// a peer-advertised sequence length before it sees that the body is truncated.
pub const PTY_DATAGRAM_MAX_DECODED_BYTES: usize = 8 * 1024 * 1024;
pub const PTY_DATAGRAM_MAX_ROWS: usize = 4096;

/// One changed row in a base-relative plain PTY screen update.  Rows are
/// complete replacements rather than byte patches: applying a delta is
/// deterministic and does not require a second terminal parser in the
/// client.  The reliable PTY byte stream and full snapshot remain
/// authoritative when a datagram is lost or its base is unavailable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtyRowDelta {
    pub row: u16,
    pub text: String,
}

/// Optional base-relative replaceable PTY state.  A client may apply this
/// datagram only when its current plain screen generation equals
/// `base_generation`; otherwise it waits for a periodic full snapshot or a
/// reliable `PTY_READY` replacement.  This lets the server skip unchanged
/// rows without pretending QUIC DATAGRAMs are reliable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtyStateDeltaDatagram {
    pub session_id: Uuid,
    pub base_generation: u64,
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub changes: Vec<PtyRowDelta>,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

/// Prefix identifying the optional base-relative plain PTY DATAGRAM form.
/// The prefix keeps it distinguishable from the legacy postcard-encoded
/// `PtyStateDatagram` payload on peers that do not negotiate the feature.
pub const PTY_DELTA_DATAGRAM_MAGIC: [u8; 2] = *b"PD";
/// Delta state is bounded to the same order of magnitude as rich snapshots;
/// the server normally emits far less, while the decoder never accepts an
/// arbitrarily large forged datagram before validating its shape.
pub const PTY_DELTA_DATAGRAM_MAX_DECODED_BYTES: usize = 8 * 1024 * 1024;
pub const PTY_DELTA_MAX_ROWS: usize = 4096;

/// Encode a base-relative PTY state update when it fits the peer's QUIC
/// DATAGRAM budget.  No compression is needed for row deltas in the common
/// case; the caller can fall back to a full or rich representation when the
/// changed rows are broad or incompressible.
pub fn encode_pty_state_delta_datagram(
    datagram: &PtyStateDeltaDatagram,
    max_size: usize,
) -> Result<Option<Vec<u8>>> {
    if datagram.changes.len() > PTY_DELTA_MAX_ROWS {
        return Ok(None);
    }
    let encoded = encode_message(datagram)?;
    let total = PTY_DELTA_DATAGRAM_MAGIC.len().saturating_add(encoded.len());
    if encoded.len() > PTY_DELTA_DATAGRAM_MAX_DECODED_BYTES || total > max_size {
        return Ok(None);
    }
    let mut payload = Vec::with_capacity(total);
    payload.extend_from_slice(&PTY_DELTA_DATAGRAM_MAGIC);
    payload.extend_from_slice(&encoded);
    Ok(Some(payload))
}

/// Decode an optional base-relative PTY state update.  Unknown prefixes are
/// deliberately ignored so old peers can continue trying the legacy full
/// snapshot decoder; recognized malformed payloads fail closed.
pub fn decode_pty_state_delta_datagram(payload: &[u8]) -> Result<Option<PtyStateDeltaDatagram>> {
    if !payload.starts_with(&PTY_DELTA_DATAGRAM_MAGIC) {
        return Ok(None);
    }
    let body = &payload[PTY_DELTA_DATAGRAM_MAGIC.len()..];
    if body.len() > PTY_DELTA_DATAGRAM_MAX_DECODED_BYTES {
        bail!("PTY delta datagram exceeds {PTY_DELTA_DATAGRAM_MAX_DECODED_BYTES} decoded bytes");
    }
    let wire: PtyStateDeltaDatagramWire = decode_message(body)?;
    Ok(Some(PtyStateDeltaDatagram {
        session_id: wire.session_id,
        base_generation: wire.base_generation,
        generation: wire.generation,
        rows: wire.rows,
        cols: wire.cols,
        changes: wire.changes.0,
        cursor_row: wire.cursor_row,
        cursor_col: wire.cursor_col,
    }))
}

/// Attribute-preserving replaceable PTY state. This is an additive response
/// selected only when both peers negotiate `pty_rich_state`; older peers keep
/// receiving the plain `PtySnapshot` shape above. The screen bytes are the
/// terminal parser's bounded ANSI redraw, so cell colors and text attributes
/// survive a reconnect without inventing a second terminal emulator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtyRichSnapshot {
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub screen: Vec<u8>,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

/// Datagram form of [`PtyRichSnapshot`]. A short magic prefix is added by the
/// sender because QUIC DATAGRAMs are not stream-framed and must remain
/// distinguishable from the legacy `PtyStateDatagram` bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtyStateRichDatagram {
    pub session_id: Uuid,
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub screen: Vec<u8>,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

/// Prefix identifying the optional attribute-preserving PTY DATAGRAM form.
pub const PTY_RICH_DATAGRAM_MAGIC: [u8; 2] = *b"PR";
/// Prefix identifying a zlib-compressed attribute-preserving PTY DATAGRAM.
/// The payload after this prefix is the normal version-17 `AF` frame envelope;
/// it is only emitted when both peers negotiate `pty_rich_compression`.
pub const PTY_RICH_COMPRESSED_DATAGRAM_MAGIC: [u8; 2] = *b"PZ";
/// Rich PTY datagrams are replaceable screen state, not arbitrary file
/// transfer. Keep decompression materially below the general 128 MiB stream
/// ceiling so a forged compressed datagram cannot force a large client-side
/// allocation before terminal dimensions are validated.
pub const PTY_RICH_DATAGRAM_MAX_DECODED_BYTES: usize = 8 * 1024 * 1024;

struct BoundedVec<T, const MAX: usize>(Vec<T>);

struct BoundedVecVisitor<T, const MAX: usize>(PhantomData<T>);

impl<'de, T, const MAX: usize> Visitor<'de> for BoundedVecVisitor<T, MAX>
where
    T: Deserialize<'de>,
{
    type Value = BoundedVec<T, MAX>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "a sequence with at most {MAX} elements")
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<T>()? {
            if values.len() >= MAX {
                return Err(de::Error::custom(format!(
                    "sequence exceeds the {MAX}-element limit"
                )));
            }
            values.push(value);
        }
        Ok(BoundedVec(values))
    }
}

impl<'de, T, const MAX: usize> Deserialize<'de> for BoundedVec<T, MAX>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(BoundedVecVisitor(PhantomData))
    }
}

#[derive(Deserialize)]
struct PtyStateDatagramWire {
    session_id: Uuid,
    generation: u64,
    rows: u16,
    cols: u16,
    screen: BoundedVec<String, PTY_DATAGRAM_MAX_ROWS>,
    cursor_row: u16,
    cursor_col: u16,
}

#[derive(Deserialize)]
struct PtyStateDeltaDatagramWire {
    session_id: Uuid,
    base_generation: u64,
    generation: u64,
    rows: u16,
    cols: u16,
    changes: BoundedVec<PtyRowDelta, PTY_DELTA_MAX_ROWS>,
    cursor_row: u16,
    cursor_col: u16,
}

#[derive(Deserialize)]
struct PtyStateRichDatagramWire {
    session_id: Uuid,
    generation: u64,
    rows: u16,
    cols: u16,
    screen: BoundedVec<u8, PTY_RICH_DATAGRAM_MAX_DECODED_BYTES>,
    cursor_row: u16,
    cursor_col: u16,
}

pub fn encode_message<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    postcard::to_allocvec(value).context("serialize ASP message")
}

pub fn decode_message<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Result<T> {
    postcard::from_bytes(payload).context("decode ASP message")
}

/// Decode the legacy plain PTY state form with bounded sequence allocation.
/// The client uses this for untrusted QUIC DATAGRAM input; the server only
/// emits the form and therefore continues to use the ordinary serializer.
pub fn decode_pty_state_datagram(payload: &[u8]) -> Result<PtyStateDatagram> {
    if payload.len() > PTY_DATAGRAM_MAX_DECODED_BYTES {
        bail!("PTY state datagram exceeds {PTY_DATAGRAM_MAX_DECODED_BYTES} decoded bytes");
    }
    let wire: PtyStateDatagramWire = decode_message(payload)?;
    Ok(PtyStateDatagram {
        session_id: wire.session_id,
        generation: wire.generation,
        rows: wire.rows,
        cols: wire.cols,
        screen: wire.screen.0,
        cursor_row: wire.cursor_row,
        cursor_col: wire.cursor_col,
    })
}

/// Encode one rich PTY state datagram for the peer's bounded QUIC DATAGRAM
/// budget. Plain rich datagrams remain the compatibility form. If the plain
/// representation does not fit, or if zlib makes it strictly smaller, the
/// compressed `PZ` form is selected only when the peer negotiated
/// `pty_rich_compression`. `None` means that neither representation fits;
/// reliable PTY output and the next reconnect snapshot remain authoritative.
pub fn encode_pty_rich_datagram(
    datagram: &PtyStateRichDatagram,
    max_size: usize,
    allow_compression: bool,
) -> Result<Option<Vec<u8>>> {
    let encoded = encode_message(datagram)?;
    if encoded.len() > PTY_RICH_DATAGRAM_MAX_DECODED_BYTES {
        return Ok(None);
    }
    let plain_len = PTY_RICH_DATAGRAM_MAGIC.len().saturating_add(encoded.len());
    let mut compressed = None;
    if allow_compression && encoded.len() >= FRAME_COMPRESSION_MIN_BYTES {
        let framed = encode_frame_payload(&encoded)?;
        // `encode_frame_payload` can return a plain AF frame when a payload
        // is incompressible. A PZ marker must never claim that representation
        // is compressed, otherwise a peer could spend a codec pass for no
        // benefit and a mixed implementation could misinterpret the body.
        if framed.len() >= FRAME_HEADER_BYTES && framed[FRAME_MAGIC.len()] == FRAME_ENCODING_ZLIB {
            let compressed_len = PTY_RICH_COMPRESSED_DATAGRAM_MAGIC
                .len()
                .saturating_add(framed.len());
            if compressed_len < plain_len && compressed_len <= max_size {
                let mut payload = Vec::with_capacity(compressed_len);
                payload.extend_from_slice(&PTY_RICH_COMPRESSED_DATAGRAM_MAGIC);
                payload.extend_from_slice(&framed);
                compressed = Some(payload);
            }
        }
    }
    if let Some(payload) = compressed {
        return Ok(Some(payload));
    }
    if plain_len > max_size {
        return Ok(None);
    }
    let mut payload = Vec::with_capacity(plain_len);
    payload.extend_from_slice(&PTY_RICH_DATAGRAM_MAGIC);
    payload.extend_from_slice(&encoded);
    Ok(Some(payload))
}

/// Decode a rich PTY DATAGRAM. Unknown prefixes return `None` so callers can
/// continue trying legacy plain PTY state. Malformed recognized payloads fail
/// closed; compressed bodies inherit the bounded `AF` decoder and therefore
/// cannot allocate beyond the protocol frame limit before validation.
pub fn decode_pty_rich_datagram(
    payload: &[u8],
    allow_compression: bool,
) -> Result<Option<PtyStateRichDatagram>> {
    if payload.starts_with(&PTY_RICH_DATAGRAM_MAGIC) {
        if payload.len() - PTY_RICH_DATAGRAM_MAGIC.len() > PTY_RICH_DATAGRAM_MAX_DECODED_BYTES {
            bail!("rich PTY datagram exceeds {PTY_RICH_DATAGRAM_MAX_DECODED_BYTES} decoded bytes");
        }
        let wire: PtyStateRichDatagramWire =
            decode_message(&payload[PTY_RICH_DATAGRAM_MAGIC.len()..])?;
        return Ok(Some(PtyStateRichDatagram {
            session_id: wire.session_id,
            generation: wire.generation,
            rows: wire.rows,
            cols: wire.cols,
            screen: wire.screen.0,
            cursor_row: wire.cursor_row,
            cursor_col: wire.cursor_col,
        }));
    }
    if payload.starts_with(&PTY_RICH_COMPRESSED_DATAGRAM_MAGIC) {
        if !allow_compression {
            return Ok(None);
        }
        let frame = &payload[PTY_RICH_COMPRESSED_DATAGRAM_MAGIC.len()..];
        let decoded_len = frame_decoded_len(
            frame
                .get(..FRAME_HEADER_BYTES)
                .ok_or_else(|| anyhow::anyhow!("compressed rich PTY datagram is truncated"))?,
        )?;
        if decoded_len > PTY_RICH_DATAGRAM_MAX_DECODED_BYTES {
            bail!(
                "compressed rich PTY datagram exceeds {PTY_RICH_DATAGRAM_MAX_DECODED_BYTES} decoded bytes"
            );
        }
        let decoded = decode_frame_payload(frame)?;
        let wire: PtyStateRichDatagramWire = decode_message(decoded.as_ref())?;
        return Ok(Some(PtyStateRichDatagram {
            session_id: wire.session_id,
            generation: wire.generation,
            rows: wire.rows,
            cols: wire.cols,
            screen: wire.screen.0,
            cursor_row: wire.cursor_row,
            cursor_col: wire.cursor_col,
        }));
    }
    Ok(None)
}

/// Encode a serialized message using the framing contract for an explicitly
/// negotiated protocol version. v16 has no envelope; its length prefix is
/// written by the caller and the returned payload is the Postcard bytes.
pub fn encode_frame_payload_for_version(payload: &[u8], version: u16) -> Result<Vec<u8>> {
    match version {
        LEGACY_PROTOCOL_VERSION => {
            if payload.len() > MAX_FRAME_BYTES {
                bail!("ASP legacy frame exceeds {MAX_FRAME_BYTES} bytes");
            }
            Ok(payload.to_vec())
        }
        PROTOCOL_VERSION => encode_frame_payload(payload),
        _ => bail!("unsupported ASP frame protocol version {version}"),
    }
}

/// Decode a payload using the framing contract for an explicitly negotiated
/// protocol version. Legacy payloads borrow their input and are bounded by
/// the same logical message limit as decompressed v17 frames.
pub fn decode_frame_payload_for_version<'a>(
    payload: &'a [u8],
    version: u16,
) -> Result<Cow<'a, [u8]>> {
    match version {
        LEGACY_PROTOCOL_VERSION => {
            if payload.len() > MAX_FRAME_BYTES {
                bail!("ASP legacy frame exceeds {MAX_FRAME_BYTES} bytes");
            }
            Ok(Cow::Borrowed(payload))
        }
        PROTOCOL_VERSION => decode_frame_payload(payload),
        _ => bail!("unsupported ASP frame protocol version {version}"),
    }
}

/// Wrap one serialized message in the version-17 stream-frame envelope.
///
/// The envelope is deliberately independent of QUIC: reliable streams still
/// provide ordering, flow control, retransmission, and congestion control.
/// ASP only chooses whether a complete message should be represented as plain
/// bytes or as a zlib payload. The advertised uncompressed length gives the
/// receiver a hard allocation/decompression bound before it touches the
/// codec, which is important for an agent-facing endpoint that accepts many
/// concurrent streams.
pub fn encode_frame_payload(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > MAX_FRAME_BYTES {
        bail!("ASP frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    let original_len = u32::try_from(payload.len()).context("ASP frame length exceeds u32")?;
    if should_attempt_frame_compression(payload) {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder
            .write_all(payload)
            .context("compress ASP frame payload")?;
        let compressed = encoder.finish().context("finish ASP frame compression")?;
        // The header is the same size for both representations. Requiring a
        // strict byte win avoids spending CPU on a tie or a one-byte saving.
        if compressed.len() < payload.len() {
            let mut framed = Vec::with_capacity(FRAME_HEADER_BYTES + compressed.len());
            framed.extend_from_slice(&FRAME_MAGIC);
            framed.push(FRAME_ENCODING_ZLIB);
            framed.extend_from_slice(&original_len.to_be_bytes());
            framed.extend_from_slice(&compressed);
            if framed.len() <= MAX_WIRE_FRAME_BYTES {
                return Ok(framed);
            }
        }
    }

    let mut framed = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    framed.extend_from_slice(&FRAME_MAGIC);
    framed.push(FRAME_ENCODING_PLAIN);
    framed.extend_from_slice(&original_len.to_be_bytes());
    framed.extend_from_slice(payload);
    Ok(framed)
}

/// Parse and, when necessary, decompress a stream-frame payload. Plain frames
/// borrow the input, avoiding a second allocation on the common small-control
/// path. Compressed frames are capped by their advertised length and must
/// produce exactly that many bytes; trailing or truncated codec output fails
/// closed instead of being handed to Postcard.
pub fn decode_frame_payload(payload: &[u8]) -> Result<Cow<'_, [u8]>> {
    if payload.len() < FRAME_HEADER_BYTES {
        bail!("ASP frame is shorter than its envelope");
    }
    if payload[..FRAME_MAGIC.len()] != FRAME_MAGIC {
        bail!("ASP frame has an invalid envelope marker");
    }
    let encoding = payload[FRAME_MAGIC.len()];
    let original_start = FRAME_MAGIC.len() + 1;
    let original_end = original_start + std::mem::size_of::<u32>();
    let original_len =
        u32::from_be_bytes(payload[original_start..original_end].try_into()?) as usize;
    if original_len > MAX_FRAME_BYTES {
        bail!("ASP frame exceeds {MAX_FRAME_BYTES} bytes after decompression");
    }
    let body = &payload[FRAME_HEADER_BYTES..];
    match encoding {
        FRAME_ENCODING_PLAIN => {
            if body.len() != original_len {
                bail!("ASP plain frame length does not match its envelope");
            }
            Ok(Cow::Borrowed(body))
        }
        FRAME_ENCODING_ZLIB => {
            if body.is_empty() {
                bail!("ASP compressed frame has an empty payload");
            }
            let mut decoder = flate2::read::ZlibDecoder::new(body);
            // Do not reserve the complete advertised decoded size up front.
            // The header is attacker-controlled until the caller's admission
            // budget has been applied, and a tiny malformed body could
            // otherwise force a large allocation (up to MAX_FRAME_BYTES)
            // before zlib proves that it cannot produce that many bytes. A
            // bounded estimate keeps normal compressed text fast while
            // making malformed/hostile frames pay only for bytes actually
            // decoded.
            let initial_capacity = original_len.min(
                body.len()
                    .saturating_mul(8)
                    .max(FRAME_COMPRESSION_MIN_BYTES),
            );
            let mut decoded = Vec::with_capacity(initial_capacity);
            // The one-byte allowance detects an expansion beyond the
            // advertised size without permitting an attacker to allocate an
            // unbounded output buffer.
            let mut limited = std::io::Read::take(&mut decoder, original_len as u64 + 1);
            limited
                .read_to_end(&mut decoded)
                .context("decompress ASP frame payload")?;
            if decoded.len() != original_len {
                bail!("ASP compressed frame length does not match its envelope");
            }
            Ok(Cow::Owned(decoded))
        }
        _ => bail!("ASP frame has an unknown encoding {encoding}"),
    }
}

/// Return the advertised decoded message length without invoking the codec.
/// The server uses this to charge both wire and decompressed allocations to
/// its aggregate frame-memory budget before reading the body into memory.
pub fn frame_decoded_len(payload_header: &[u8]) -> Result<usize> {
    if payload_header.len() < FRAME_HEADER_BYTES {
        bail!("ASP frame is shorter than its envelope");
    }
    if payload_header[..FRAME_MAGIC.len()] != FRAME_MAGIC {
        bail!("ASP frame has an invalid envelope marker");
    }
    if !matches!(
        payload_header[FRAME_MAGIC.len()],
        FRAME_ENCODING_PLAIN | FRAME_ENCODING_ZLIB
    ) {
        bail!(
            "ASP frame has an unknown encoding {}",
            payload_header[FRAME_MAGIC.len()]
        );
    }
    let original_start = FRAME_MAGIC.len() + 1;
    let original_end = original_start + std::mem::size_of::<u32>();
    let original_len =
        u32::from_be_bytes(payload_header[original_start..original_end].try_into()?) as usize;
    if original_len > MAX_FRAME_BYTES {
        bail!("ASP frame exceeds {MAX_FRAME_BYTES} bytes after decompression");
    }
    Ok(original_len)
}

pub async fn write_frame<T: Serialize>(send: &mut quinn::SendStream, value: &T) -> Result<()> {
    write_frame_for_version(send, value, PROTOCOL_VERSION).await
}

/// Write one complete frame for an explicitly negotiated protocol version.
/// Keeping the version argument at this boundary makes legacy support
/// auditable and prevents callers from silently mixing v16/v17 payloads.
pub async fn write_frame_for_version<T: Serialize>(
    send: &mut quinn::SendStream,
    value: &T,
    version: u16,
) -> Result<()> {
    let payload = encode_frame_payload_for_version(&encode_message(value)?, version)?;
    let length = u32::try_from(payload.len()).context("ASP wire frame length exceeds u32")?;
    send.write_all(&length.to_be_bytes()).await?;
    send.write_all(&payload).await?;
    Ok(())
}

pub async fn read_frame<T: for<'de> Deserialize<'de>>(
    recv: &mut quinn::RecvStream,
) -> Result<Option<T>> {
    read_frame_for_version(recv, PROTOCOL_VERSION).await
}

/// Read one complete length-prefixed frame payload without decoding it.
/// Callers that own an async runtime can use this boundary to move expensive
/// decompression/Postcard work to a blocking pool while keeping QUIC I/O
/// responsive. The returned bytes are still bounded by the negotiated wire
/// version; framing validation happens in `decode_frame_payload_for_version`.
pub async fn read_frame_payload_for_version(
    recv: &mut quinn::RecvStream,
    version: u16,
) -> Result<Option<Vec<u8>>> {
    let mut len = [0_u8; 4];
    match recv.read_exact(&mut len).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    let maximum = if version == LEGACY_PROTOCOL_VERSION {
        MAX_FRAME_BYTES
    } else {
        MAX_WIRE_FRAME_BYTES
    };
    if len > maximum {
        bail!("peer wire frame exceeds {maximum} bytes");
    }
    let mut payload = vec![0_u8; len];
    recv.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

/// Read one complete frame for an explicitly negotiated protocol version.
pub async fn read_frame_for_version<T: for<'de> Deserialize<'de>>(
    recv: &mut quinn::RecvStream,
    version: u16,
) -> Result<Option<T>> {
    let Some(payload) = read_frame_payload_for_version(recv, version).await? else {
        return Ok(None);
    };
    let decoded = decode_frame_payload_for_version(&payload, version)?;
    Ok(Some(decode_message(&decoded)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_json_is_self_describing() {
        let event = Event {
            id: 7,
            unix_ms: 9,
            kind: EventKind::ProcessExited {
                process_id: Uuid::nil(),
                code: Some(0),
            },
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(encoded.contains("process_exited"));
        assert_eq!(serde_json::from_str::<Event>(&encoded).unwrap(), event);
    }

    #[test]
    fn binary_wire_encoding_keeps_byte_payloads_compact() {
        let request = Request::FilePut {
            session_id: Uuid::nil(),
            request_id: Uuid::new_v4(),
            path: "large.bin".into(),
            expected_sha256: None,
            allow_blind: true,
            data: vec![0_u8; 1024 * 1024],
        };
        let encoded = postcard::to_allocvec(&request).unwrap();
        assert!(encoded.len() < 1024 * 1024 + 128);
        assert_eq!(postcard::from_bytes::<Request>(&encoded).unwrap(), request);
    }

    #[test]
    fn stream_frame_compression_is_bounded_and_only_used_for_a_byte_win() {
        let repetitive = vec![b'x'; 64 * 1024];
        let framed = encode_frame_payload(&repetitive).unwrap();
        assert!(framed.len() < repetitive.len());
        assert_eq!(framed[FRAME_MAGIC.len()], FRAME_ENCODING_ZLIB);
        assert_eq!(
            frame_decoded_len(&framed[..FRAME_HEADER_BYTES]).unwrap(),
            repetitive.len()
        );
        assert_eq!(
            decode_frame_payload(&framed).unwrap().as_ref(),
            repetitive.as_slice()
        );

        let incompressible = (0..64 * 1024).map(|index| index as u8).collect::<Vec<_>>();
        assert!(!should_attempt_frame_compression(&incompressible));
        let framed = encode_frame_payload(&incompressible).unwrap();
        assert_eq!(framed[FRAME_MAGIC.len()], FRAME_ENCODING_PLAIN);
        assert_eq!(
            decode_frame_payload(&framed).unwrap().as_ref(),
            incompressible.as_slice()
        );
        assert!(framed.len() <= incompressible.len() + FRAME_HEADER_BYTES);
    }

    #[test]
    fn stream_frame_rejects_bad_markers_lengths_and_expansion() {
        assert!(decode_frame_payload(&[0; FRAME_HEADER_BYTES]).is_err());
        let mut framed = encode_frame_payload(b"small payload").unwrap();
        framed[0] = b'X';
        assert!(decode_frame_payload(&framed).is_err());

        let mut framed = encode_frame_payload(b"small payload").unwrap();
        framed[3..7].copy_from_slice(&(1_u32).to_be_bytes());
        assert!(decode_frame_payload(&framed).is_err());

        let too_large = (MAX_FRAME_BYTES as u32).saturating_add(1).to_be_bytes();
        let mut header = Vec::from(FRAME_MAGIC);
        header.push(FRAME_ENCODING_PLAIN);
        header.extend_from_slice(&too_large);
        header.push(0);
        assert!(decode_frame_payload(&header).is_err());
    }

    #[test]
    fn compressed_frame_does_not_eagerly_reserve_advertised_limit() {
        // A peer controls the decoded-length field.  The body below is a
        // valid, tiny zlib stream that cannot produce the advertised 128 MiB
        // payload.  Decoding must reject it without first reserving the full
        // limit; otherwise a few forged packets could create avoidable memory
        // pressure before the length mismatch is discovered.
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(b"x").unwrap();
        let body = encoder.finish().unwrap();
        let mut framed = Vec::with_capacity(FRAME_HEADER_BYTES + body.len());
        framed.extend_from_slice(&FRAME_MAGIC);
        framed.push(FRAME_ENCODING_ZLIB);
        framed.extend_from_slice(&(MAX_FRAME_BYTES as u32).to_be_bytes());
        framed.extend_from_slice(&body);
        assert!(decode_frame_payload(&framed).is_err());
    }

    #[test]
    fn rich_pty_datagram_compression_is_bounded_and_negotiated() {
        let datagram = PtyStateRichDatagram {
            session_id: Uuid::new_v4(),
            generation: 9,
            rows: 24,
            cols: 80,
            screen: vec![b'x'; 8 * 1024],
            cursor_row: 2,
            cursor_col: 3,
        };
        let encoded = encode_pty_rich_datagram(&datagram, 1_200, true)
            .unwrap()
            .expect("repetitive rich state should fit when compressed");
        assert!(encoded.starts_with(&PTY_RICH_COMPRESSED_DATAGRAM_MAGIC));
        assert!(encoded.len() <= 1_200);
        assert_eq!(
            decode_pty_rich_datagram(&encoded, true).unwrap(),
            Some(datagram.clone())
        );
        // A peer that did not negotiate the additive compression feature must
        // not decode or use the marker. It can still keep its legacy state.
        assert_eq!(decode_pty_rich_datagram(&encoded, false).unwrap(), None);
    }

    #[test]
    fn rich_pty_datagram_falls_back_to_plain_or_none() {
        let datagram = PtyStateRichDatagram {
            session_id: Uuid::nil(),
            generation: 1,
            rows: 2,
            cols: 3,
            screen: b"small".to_vec(),
            cursor_row: 0,
            cursor_col: 0,
        };
        let plain = encode_pty_rich_datagram(&datagram, 128, true)
            .unwrap()
            .expect("small rich state should use compatibility form");
        assert!(plain.starts_with(&PTY_RICH_DATAGRAM_MAGIC));
        assert_eq!(
            decode_pty_rich_datagram(&plain, true).unwrap(),
            Some(datagram)
        );

        let oversized = encode_pty_rich_datagram(
            &PtyStateRichDatagram {
                screen: vec![0_u8; 32 * 1024],
                ..datagram_from_nil()
            },
            64,
            false,
        )
        .unwrap();
        assert_eq!(oversized, None);

        let mut forged_frame = Vec::from(FRAME_MAGIC);
        forged_frame.push(FRAME_ENCODING_ZLIB);
        forged_frame
            .extend_from_slice(&(PTY_RICH_DATAGRAM_MAX_DECODED_BYTES as u32 + 1).to_be_bytes());
        forged_frame.push(0);
        let mut forged = Vec::from(PTY_RICH_COMPRESSED_DATAGRAM_MAGIC);
        forged.extend_from_slice(&forged_frame);
        assert!(decode_pty_rich_datagram(&forged, true).is_err());
    }

    #[test]
    fn pty_state_delta_datagram_round_trips_and_is_bounded() {
        let datagram = PtyStateDeltaDatagram {
            session_id: Uuid::new_v4(),
            base_generation: 10,
            generation: 13,
            rows: 24,
            cols: 80,
            changes: vec![
                PtyRowDelta {
                    row: 1,
                    text: "cargo test".into(),
                },
                PtyRowDelta {
                    row: 22,
                    text: "ready".into(),
                },
            ],
            cursor_row: 1,
            cursor_col: 10,
        };
        let encoded = encode_pty_state_delta_datagram(&datagram, 1_200)
            .unwrap()
            .expect("small row delta should fit");
        assert!(encoded.starts_with(&PTY_DELTA_DATAGRAM_MAGIC));
        assert_eq!(
            decode_pty_state_delta_datagram(&encoded).unwrap(),
            Some(datagram.clone())
        );
        assert_eq!(
            encode_pty_state_delta_datagram(&datagram, 2).unwrap(),
            None,
            "a peer MTU that cannot carry the delta must trigger a full-state fallback"
        );

        let too_many = PtyStateDeltaDatagram {
            changes: (0..=PTY_DELTA_MAX_ROWS)
                .map(|row| PtyRowDelta {
                    row: row as u16,
                    text: "x".into(),
                })
                .collect(),
            ..datagram
        };
        assert_eq!(
            encode_pty_state_delta_datagram(&too_many, usize::MAX).unwrap(),
            None
        );
    }

    #[test]
    fn pty_state_delta_decoder_rejects_oversized_body_and_unknown_prefix() {
        assert_eq!(
            decode_pty_state_delta_datagram(b"XXfuture-state").unwrap(),
            None
        );
        let mut forged = Vec::from(PTY_DELTA_DATAGRAM_MAGIC);
        forged.extend(std::iter::repeat_n(
            0_u8,
            PTY_DELTA_DATAGRAM_MAX_DECODED_BYTES + 1,
        ));
        assert!(decode_pty_state_delta_datagram(&forged).is_err());
    }

    #[test]
    fn pty_datagram_decoders_bound_sequence_lengths() {
        let legacy = PtyStateDatagram {
            session_id: Uuid::nil(),
            generation: 1,
            rows: 24,
            cols: 80,
            screen: (0..=PTY_DATAGRAM_MAX_ROWS)
                .map(|row| format!("row {row}"))
                .collect(),
            cursor_row: 0,
            cursor_col: 0,
        };
        let encoded = encode_message(&legacy).unwrap();
        assert!(decode_pty_state_datagram(&encoded).is_err());

        // The delta body contains a valid sequence count but no row bodies.
        // The bounded visitor must fail closed without asking Postcard to
        // reserve the forged count.
        let mut forged_delta = Vec::from(PTY_DELTA_DATAGRAM_MAGIC);
        forged_delta.extend_from_slice(&[0; 16]); // UUID
        forged_delta.push(1); // base generation
        forged_delta.push(2); // generation
        forged_delta.push(24); // rows
        forged_delta.push(80); // columns
        forged_delta.extend_from_slice(&[0xff; 9]); // huge sequence count
        assert!(decode_pty_state_delta_datagram(&forged_delta).is_err());
    }

    fn datagram_from_nil() -> PtyStateRichDatagram {
        PtyStateRichDatagram {
            session_id: Uuid::nil(),
            generation: 1,
            rows: 2,
            cols: 3,
            screen: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    #[test]
    fn streamed_file_messages_round_trip_with_bounded_chunks() {
        let session_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let begin = Request::FilePutStreamBegin {
            session_id,
            request_id,
            path: "src/main.rs".into(),
            total_size: 131_072,
            sha256: "a".repeat(64),
            expected_sha256: Some("b".repeat(64)),
            allow_blind: false,
        };
        let chunk = Request::FilePutStreamChunk {
            offset: 65_536,
            data: vec![0x5a; 65_536],
        };
        let response = Response::FileStreamEnd {
            bytes: 65_536,
            sha256: "b".repeat(64),
        };
        for message in [
            encode_message(&begin).unwrap(),
            encode_message(&chunk).unwrap(),
            encode_message(&response).unwrap(),
        ] {
            assert!(message.len() <= MAX_FRAME_BYTES);
        }
        assert_eq!(
            decode_message::<Request>(&encode_message(&begin).unwrap()).unwrap(),
            begin
        );
        assert_eq!(
            decode_message::<Request>(&encode_message(&chunk).unwrap()).unwrap(),
            chunk
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&response).unwrap()).unwrap(),
            response
        );
        let resume = Request::FilePutStreamResumeBegin {
            session_id,
            request_id,
            path: "src/main.rs".into(),
            total_size: 131_072,
            sha256: "a".repeat(64),
            expected_sha256: Some("b".repeat(64)),
            allow_blind: false,
        };
        let ready = Response::FileUploadReady {
            path: "src/main.rs".into(),
            total_size: 131_072,
            offset: 65_536,
            sha256: "a".repeat(64),
        };
        assert_eq!(
            decode_message::<Request>(&encode_message(&resume).unwrap()).unwrap(),
            resume
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&ready).unwrap()).unwrap(),
            ready
        );
    }

    #[test]
    fn feature_handshake_round_trips_and_keeps_unknown_capabilities_explicit() {
        let request = Request::HelloFeatures {
            version: PROTOCOL_VERSION,
            auth_token: Some("token".into()),
            features: vec!["file_stream".into(), "future_feature".into()],
        };
        let response = Response::HelloFeatures {
            version: PROTOCOL_VERSION,
            server: "aspd/0.1".into(),
            features: vec!["file_stream".into()],
        };
        assert_eq!(
            decode_message::<Request>(&encode_message(&request).unwrap()).unwrap(),
            request
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&response).unwrap()).unwrap(),
            response
        );
        let caught_up = Response::SubscriptionCaughtUp {
            through_event_id: 41,
        };
        assert_eq!(
            decode_message::<Response>(&encode_message(&caught_up).unwrap()).unwrap(),
            caught_up
        );
        let rich = Response::PtyReadyRich {
            snapshot: PtyRichSnapshot {
                generation: 7,
                rows: 24,
                cols: 80,
                screen: b"\x1b[2J\x1b[31mred\x1b[0m".to_vec(),
                cursor_row: 0,
                cursor_col: 3,
            },
        };
        assert_eq!(
            decode_message::<Response>(&encode_message(&rich).unwrap()).unwrap(),
            rich
        );
        let scrollback = Response::PtyReadyScrollback {
            snapshot: PtyScrollbackSnapshot {
                generation: 7,
                rows: 24,
                cols: 80,
                lines: vec!["previous command".into(), "previous result".into()],
            },
        };
        assert_eq!(
            decode_message::<Response>(&encode_message(&scrollback).unwrap()).unwrap(),
            scrollback
        );
    }

    #[test]
    fn event_subscription_messages_round_trip() {
        let request = Request::SubscribeEvents {
            session_id: Uuid::new_v4(),
            after_event_id: 41,
            process_id: Some(Uuid::new_v4()),
            include_output: false,
        };
        assert_eq!(
            decode_message::<Request>(&encode_message(&request).unwrap()).unwrap(),
            request
        );
        let response = Response::SubscriptionReady {
            snapshot: SessionSnapshot {
                session_id: Uuid::nil(),
                latest_event_id: 41,
                processes: Vec::new(),
                pty: None,
            },
            through_event_id: 41,
            retained_from_event_id: 1,
            compacted: false,
        };
        assert_eq!(
            decode_message::<Response>(&encode_message(&response).unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn optional_consumer_ack_round_trips() {
        assert!(feature_supported("event_consumer_leases"));
        assert!(feature_supported("pty_rich_state"));
        assert!(feature_supported("pty_rich_compression"));
        assert!(feature_supported("pty_state_delta"));
        assert!(!SUPPORTED_FEATURES.contains(&"event_consumer_leases"));
        let request = Request::AckEventsConsumer {
            session_id: Uuid::new_v4(),
            consumer_id: "agent-a".into(),
            through_event_id: 99,
        };
        assert_eq!(
            decode_message::<Request>(&encode_message(&request).unwrap()).unwrap(),
            request
        );
    }

    #[test]
    fn sequenced_pty_input_and_ack_round_trip() {
        let request = Request::PtyInputSequenced {
            session_id: Uuid::new_v4(),
            sequence: 42,
            data: b"ls\r".to_vec(),
        };
        let response = Response::PtyInputAck { sequence: 42 };
        assert_eq!(
            decode_message::<Request>(&encode_message(&request).unwrap()).unwrap(),
            request
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&response).unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn loopback_port_forward_messages_round_trip() {
        let request = Request::PortOpen {
            session_id: Uuid::new_v4(),
            host: "127.0.0.1".into(),
            port: 3000,
        };
        let response = Response::PortReady {
            host: "127.0.0.1".into(),
            port: 3000,
        };
        assert_eq!(
            decode_message::<Request>(&encode_message(&request).unwrap()).unwrap(),
            request
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&response).unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn exec_summary_messages_round_trip_with_bounded_tails() {
        let request = Request::ExecSummary {
            session_id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            command: "cargo test".into(),
            tail_bytes: 4096,
        };
        let response = Response::ProcessSummary {
            process_id: Uuid::new_v4(),
            event_id: 17,
            stdout_bytes: 8192,
            stderr_bytes: 32,
            stdout_tail: b"ok\n".to_vec(),
            stderr_tail: Vec::new(),
            stdout_truncated: true,
            stderr_truncated: false,
        };
        assert_eq!(
            decode_message::<Request>(&encode_message(&request).unwrap()).unwrap(),
            request
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&response).unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn process_log_stream_messages_round_trip_with_ranges() {
        let request = Request::ProcessOutputStream {
            session_id: Uuid::new_v4(),
            process_id: Uuid::new_v4(),
            stream: OutputStream::Stderr,
            offset: 4096,
            length: Some(8192),
        };
        let begin = Response::ProcessOutputStreamBegin {
            process_id: Uuid::new_v4(),
            stream: OutputStream::Stdout,
            total_size: 65_536,
            offset: 4096,
            length: 8192,
        };
        let chunk = Response::ProcessOutputStreamChunk {
            offset: 4096,
            data: vec![0x41; 4096],
        };
        let end = Response::ProcessOutputStreamEnd {
            bytes: 8192,
            complete: true,
        };
        for message in [
            encode_message(&request).unwrap(),
            encode_message(&begin).unwrap(),
            encode_message(&chunk).unwrap(),
            encode_message(&end).unwrap(),
        ] {
            assert!(message.len() <= MAX_FRAME_BYTES);
        }
        assert_eq!(
            decode_message::<Request>(&encode_message(&request).unwrap()).unwrap(),
            request
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&begin).unwrap()).unwrap(),
            begin
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&chunk).unwrap()).unwrap(),
            chunk
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&end).unwrap()).unwrap(),
            end
        );
    }

    #[test]
    fn process_state_messages_round_trip() {
        let process_id = Uuid::new_v4();
        let request = Request::ProcessState {
            session_id: Uuid::new_v4(),
            process_id,
        };
        let response = Response::ProcessState {
            snapshot: ProcessSnapshot {
                process_id,
                command: "cargo test".into(),
                running: true,
                exit_code: None,
                stdout_bytes: 128,
                stderr_bytes: 4,
            },
        };
        assert_eq!(
            decode_message::<Request>(&encode_message(&request).unwrap()).unwrap(),
            request
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&response).unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn file_patch_ranges_round_trip() {
        let request = Request::FilePatchRanges {
            session_id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            path: "src/lib.rs".into(),
            expected_sha256: "a".repeat(64),
            ranges: vec![
                FilePatchRange {
                    offset: 10,
                    remove_len: 3,
                    replacement: b"new".to_vec(),
                },
                FilePatchRange {
                    offset: 4096,
                    remove_len: 0,
                    replacement: b"inserted".to_vec(),
                },
            ],
        };
        let encoded = encode_message(&request).unwrap();
        assert!(encoded.len() <= MAX_FRAME_BYTES);
        assert_eq!(decode_message::<Request>(&encoded).unwrap(), request);
    }

    #[test]
    fn artifact_stream_messages_round_trip_with_content_addressed_ranges() {
        let session_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let artifact_id = "a".repeat(64);
        let begin = Request::ArtifactPutStreamBegin {
            session_id,
            request_id,
            artifact_id: artifact_id.clone(),
            total_size: 131_072,
            name: Some("test-output.log".into()),
        };
        let resume = Request::ArtifactPutStreamResumeBegin {
            session_id,
            request_id,
            artifact_id: artifact_id.clone(),
            total_size: 131_072,
            name: Some("test-output.log".into()),
        };
        let get = Request::ArtifactGetStream {
            session_id,
            artifact_id: artifact_id.clone(),
            offset: 65_536,
            length: Some(4096),
        };
        let chunk = Request::ArtifactPutStreamChunk {
            offset: 65_536,
            data: vec![0x5a; 4096],
        };
        let ready = Response::ArtifactUploadReady {
            artifact_id: artifact_id.clone(),
            total_size: 131_072,
            offset: 65_536,
        };
        let stream_begin = Response::ArtifactStreamBegin {
            artifact_id: artifact_id.clone(),
            total_size: 131_072,
            offset: 65_536,
            length: 4096,
            sha256: artifact_id.clone(),
            name: Some("test-output.log".into()),
        };
        let stream_chunk = Response::ArtifactStreamChunk {
            offset: 65_536,
            data: vec![0x5a; 4096],
        };
        let stream_end = Response::ArtifactStreamEnd {
            bytes: 4096,
            sha256: artifact_id.clone(),
        };
        let stored = Response::ArtifactStored {
            artifact_id: artifact_id.clone(),
            total_size: 131_072,
            name: Some("test-output.log".into()),
            event_id: 9,
        };
        for message in [
            encode_message(&begin).unwrap(),
            encode_message(&resume).unwrap(),
            encode_message(&get).unwrap(),
            encode_message(&chunk).unwrap(),
            encode_message(&ready).unwrap(),
            encode_message(&stream_begin).unwrap(),
            encode_message(&stream_chunk).unwrap(),
            encode_message(&stream_end).unwrap(),
            encode_message(&stored).unwrap(),
        ] {
            assert!(message.len() <= MAX_FRAME_BYTES);
        }
        assert_eq!(
            decode_message::<Request>(&encode_message(&begin).unwrap()).unwrap(),
            begin
        );
        assert_eq!(
            decode_message::<Request>(&encode_message(&resume).unwrap()).unwrap(),
            resume
        );
        assert_eq!(
            decode_message::<Request>(&encode_message(&get).unwrap()).unwrap(),
            get
        );
        assert_eq!(
            decode_message::<Request>(&encode_message(&chunk).unwrap()).unwrap(),
            chunk
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&ready).unwrap()).unwrap(),
            ready
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&stream_begin).unwrap()).unwrap(),
            stream_begin
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&stream_chunk).unwrap()).unwrap(),
            stream_chunk
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&stream_end).unwrap()).unwrap(),
            stream_end
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&stored).unwrap()).unwrap(),
            stored
        );
        let deleted = Event {
            id: 10,
            unix_ms: 123,
            kind: EventKind::ArtifactDeleted {
                artifact_id: artifact_id.clone(),
                total_size: 131_072,
            },
        };
        assert_eq!(
            decode_message::<Event>(&encode_message(&deleted).unwrap()).unwrap(),
            deleted
        );
    }

    #[test]
    fn workspace_index_version_and_conditional_tree_round_trip() {
        let version = WorkspaceVersion {
            epoch: Uuid::new_v4(),
            generation: 7,
        };
        let request = Request::WorkspaceState {
            session_id: Uuid::new_v4(),
            workspace: ".".into(),
            include_tree: true,
            include_git_status: true,
            include_diff: false,
            recent_commits: 0,
            searches: Vec::new(),
            read_paths: Vec::new(),
            known_tree_version: Some(version.clone()),
            known_state_digest: None,
        };
        let response = Response::WorkspaceState {
            workspace: ".".into(),
            tree_version: Some(version),
            tree_unchanged: true,
            tree: Vec::new(),
            git_status: Some("".into()),
            diff: None,
            recent_commits: Vec::new(),
            search_hits: Vec::new(),
            files: Vec::new(),
            state_digest: "a".repeat(64),
            state_unchanged: true,
        };
        assert_eq!(
            decode_message::<Request>(&encode_message(&request).unwrap()).unwrap(),
            request
        );
        assert_eq!(
            decode_message::<Response>(&encode_message(&response).unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn quic_stream_priorities_keep_interactive_control_ahead_of_bulk() {
        let session_id = Uuid::new_v4();
        let pty = Request::PtyOpen {
            session_id,
            rows: 24,
            cols: 80,
        };
        let health = Request::Health;
        let exec = Request::Exec {
            session_id,
            request_id: Uuid::new_v4(),
            command: "cargo test".into(),
        };
        let summary = Request::ExecSummary {
            session_id,
            request_id: Uuid::new_v4(),
            command: "cargo test".into(),
            tail_bytes: 8 * 1024,
        };
        let small_file = Request::FilePut {
            session_id,
            request_id: Uuid::new_v4(),
            path: "small.txt".into(),
            expected_sha256: None,
            allow_blind: false,
            data: vec![1, 2, 3],
        };
        let large_file = Request::FilePut {
            session_id,
            request_id: Uuid::new_v4(),
            path: "large.bin".into(),
            expected_sha256: None,
            allow_blind: false,
            data: vec![0; FRAME_CODEC_OFFLOAD_MIN_BYTES],
        };
        let upload_chunk = Request::FilePutStreamChunk {
            offset: 0,
            data: vec![1, 2, 3],
        };
        let artifact_chunk = Request::ArtifactPutStreamChunk {
            offset: 0,
            data: vec![1, 2, 3],
        };
        let ranges = Request::FilePatchRanges {
            session_id,
            request_id: Uuid::new_v4(),
            path: "scattered.rs".into(),
            expected_sha256: "a".repeat(64),
            ranges: vec![FilePatchRange {
                offset: 1,
                remove_len: 1,
                replacement: vec![0; FRAME_CODEC_OFFLOAD_MIN_BYTES],
            }],
        };
        assert_eq!(quic_stream_priority(&pty), QUIC_STREAM_PRIORITY_INTERACTIVE);
        assert_eq!(quic_stream_priority(&health), QUIC_STREAM_PRIORITY_CONTROL);
        assert_eq!(quic_stream_priority(&exec), QUIC_STREAM_PRIORITY_BULK);
        assert_eq!(quic_stream_priority(&summary), QUIC_STREAM_PRIORITY_CONTROL);
        assert_eq!(
            quic_stream_priority(&small_file),
            QUIC_STREAM_PRIORITY_CONTROL
        );
        assert_eq!(quic_stream_priority(&large_file), QUIC_STREAM_PRIORITY_BULK);
        assert_eq!(
            quic_stream_priority(&upload_chunk),
            QUIC_STREAM_PRIORITY_BULK
        );
        assert_eq!(
            quic_stream_priority(&artifact_chunk),
            QUIC_STREAM_PRIORITY_BULK
        );
        assert_eq!(quic_stream_priority(&ranges), QUIC_STREAM_PRIORITY_BULK);
        assert!(quic_stream_priority(&pty) > quic_stream_priority(&health));
        assert!(quic_stream_priority(&health) > quic_stream_priority(&exec));
    }

    #[test]
    fn machine_readable_schema_registry_matches_wire_surface() {
        // Keep a crate-local copy so the publishable wire-format crate's own
        // tests remain runnable from a `cargo package` tarball.  In the
        // workspace, compare it byte-for-byte with the documentation copy so
        // the two public registries cannot silently drift apart.
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let crate_path = manifest_dir.join("schema.json");
        let crate_schema =
            std::fs::read_to_string(&crate_path).expect("crate schema registry must be present");
        let workspace_path = manifest_dir.join("../../docs/schema.json");
        if workspace_path.is_file() {
            let workspace_schema = std::fs::read_to_string(&workspace_path)
                .expect("workspace schema registry must be readable");
            assert_eq!(workspace_schema, crate_schema, "schema registries drifted");
        }
        let registry: serde_json::Value =
            serde_json::from_str(&crate_schema).expect("schema registry must be valid JSON");
        assert_eq!(
            registry["protocol_version"].as_u64(),
            Some(PROTOCOL_VERSION as u64)
        );
        let versions = registry["supported_protocol_versions"]
            .as_array()
            .expect("schema registry supported versions must be an array")
            .iter()
            .map(|value| value.as_u64().expect("supported version must be numeric") as u16)
            .collect::<Vec<_>>();
        assert_eq!(versions.as_slice(), SUPPORTED_PROTOCOL_VERSIONS);
        let features = registry["features"]
            .as_array()
            .expect("schema registry features must be an array")
            .iter()
            .map(|value| value.as_str().expect("feature must be a string"))
            .collect::<Vec<_>>();
        assert_eq!(features.as_slice(), SUPPORTED_FEATURES);
        let optional_features = registry["optional_features"]
            .as_array()
            .expect("schema registry optional_features must be an array")
            .iter()
            .map(|value| value.as_str().expect("optional feature must be a string"))
            .collect::<Vec<_>>();
        assert_eq!(optional_features.as_slice(), OPTIONAL_FEATURES);
        let operations = registry["operations"]
            .as_array()
            .expect("schema registry operations must be an array")
            .iter()
            .map(|value| {
                assert!(value["wire_name"].as_str().is_some());
                assert!(value["transport"].as_str().is_some());
                assert!(value["idempotency"].as_str().is_some());
                value["name"]
                    .as_str()
                    .expect("operation name must be a string")
            })
            .collect::<Vec<_>>();
        assert_eq!(operations.as_slice(), SUPPORTED_OPERATIONS);
    }

    #[test]
    fn protocol_version_policy_is_explicit_and_fail_closed() {
        assert!(protocol_version_supported(PROTOCOL_VERSION));
        assert!(!protocol_version_supported(
            LEGACY_PROTOCOL_VERSION.saturating_sub(1)
        ));
        assert!(!protocol_version_supported(
            PROTOCOL_VERSION.saturating_add(1)
        ));
        assert!(protocol_version_supported(LEGACY_PROTOCOL_VERSION));
        assert_eq!(
            SUPPORTED_PROTOCOL_VERSIONS,
            &[LEGACY_PROTOCOL_VERSION, PROTOCOL_VERSION]
        );
    }

    #[test]
    fn legacy_v16_frame_is_plain_and_bounded() {
        let message = Request::Health;
        let encoded = encode_message(&message).unwrap();
        let framed = encode_frame_payload_for_version(&encoded, LEGACY_PROTOCOL_VERSION).unwrap();
        assert_eq!(framed, encoded);
        assert_eq!(
            decode_frame_payload_for_version(&framed, LEGACY_PROTOCOL_VERSION)
                .unwrap()
                .as_ref(),
            encoded.as_slice()
        );
        assert!(
            encode_frame_payload_for_version(
                &vec![0_u8; MAX_FRAME_BYTES + 1],
                LEGACY_PROTOCOL_VERSION
            )
            .is_err()
        );
        assert!(encode_frame_payload_for_version(&encoded, PROTOCOL_VERSION + 1).is_err());
    }

    #[test]
    fn malformed_wire_payloads_fail_closed_without_panicking() {
        // A deterministic corpus exercises truncated varints, invalid enum
        // discriminants, frame envelopes, PTY datagrams, and arbitrary byte
        // strings without pulling a fuzzing runtime into the production
        // dependency graph. The same decoder is suitable for cargo-fuzz; this
        // regression keeps malformed input from becoming a process-level
        // failure in ordinary CI.
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let mut state = 0x9e37_79b9_u32;
        for length in 0..=256_usize {
            for _ in 0..8 {
                let mut payload = Vec::with_capacity(length);
                for _ in 0..length {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    payload.push((state >> 24) as u8);
                }
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let _ = decode_message::<Request>(&payload);
                    let _ = decode_message::<Response>(&payload);
                    let _ = decode_message::<Event>(&payload);
                    let _ = decode_frame_payload(&payload);
                    let _ = decode_pty_state_datagram(&payload);
                    let _ = decode_pty_state_delta_datagram(&payload);
                    let _ = decode_pty_rich_datagram(&payload, true);
                }));
                assert!(result.is_ok(), "malformed decoder corpus caused a panic");
            }
        }
    }
}
