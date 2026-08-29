use anyhow::{Context, Result, bail};
use asp_protocol::{
    Event, EventKind, FRAME_HEADER_BYTES, FilePatchRange, LEGACY_PROTOCOL_VERSION,
    PROTOCOL_VERSION, PtyRowDelta, PtyStateDatagram, PtyStateDeltaDatagram, PtyStateRichDatagram,
    Request, Response, SUPPORTED_FEATURES, configure_quic_transport, decode_frame_payload,
    decode_frame_payload_for_version, decode_message, decode_pty_rich_datagram,
    decode_pty_state_datagram, decode_pty_state_delta_datagram, encode_frame_payload,
    encode_frame_payload_for_version, encode_message, encode_pty_rich_datagram,
    encode_pty_state_delta_datagram, read_frame_for_version, write_frame_for_version,
};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use quinn::{Endpoint, ServerConfig};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use serde::Serialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::time::Instant as TokioInstant;

#[derive(Parser)]
#[command(name = "asp-bench")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// In-process experiment proving Quinn streams, DATAGRAMs, stats, and client rebinding.
    QuinnSmoke,
    /// Measure the bounded stream-frame codec on repetitive and
    /// incompressible payloads without conflating it with network latency.
    FrameCompression,
    /// Measure MTU fitting for the optional compressed rich PTY state form.
    PtyDatagramCompression,
    /// Compare complete plain PTY snapshots with base-relative row deltas on
    /// deterministic localized and broad-screen fixtures.
    PtyStateDelta,
    /// Compare exact v17 encoded FILE_PUT and prefix/suffix FILE_PATCH sizes
    /// for deterministic source-edit fixtures. This is a codec measurement,
    /// not an end-to-end network benchmark.
    FileSync,
    /// Forward UDP between an explicit local listener and target while
    /// applying deterministic delay, jitter, loss, and optional rate shaping.
    /// This is benchmark tooling, not a production relay or security boundary.
    UdpProxy {
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: SocketAddr,
        #[arg(long)]
        target: SocketAddr,
        #[arg(long, default_value_t = 0)]
        delay_ms: u64,
        #[arg(long, default_value_t = 0)]
        jitter_ms: u64,
        #[arg(long, default_value_t = 0)]
        loss_percent: u8,
        /// Decimal megabits per second in each direction; zero is unlimited.
        #[arg(long, default_value_t = 0)]
        rate_mbit: u64,
    },
    /// Exercise the legacy v16 plain framing against a running aspd.
    LegacySmoke {
        #[arg(long, default_value = "127.0.0.1:4433")]
        server: String,
        #[arg(long, default_value = ".asp/server-cert.der")]
        cert: PathBuf,
        #[arg(long, default_value = ".asp/auth-token")]
        auth_token_file: PathBuf,
    },
    /// Run a deterministic bounded mutation corpus through every public wire
    /// decoder. This is a fast CI regression guard; it is not a replacement
    /// for an independent time-limited fuzzing campaign.
    ProtocolFuzz {
        /// Number of mutated inputs. Keep the default small enough for every
        /// pull request while allowing a larger operator-controlled soak.
        #[arg(long, default_value_t = 10_000)]
        iterations: usize,
        /// Maximum generated input size. Decoder limits remain authoritative;
        /// this bound prevents the benchmark itself from becoming a memory
        /// pressure source.
        #[arg(long, default_value_t = 4096)]
        max_bytes: usize,
        /// Deterministic mutation seed, useful for reproducing a failure.
        #[arg(long, default_value_t = 0x4153_505f_4655_5a5a_u64)]
        seed: u64,
    },
}

#[derive(Serialize)]
struct SmokeResult {
    experiment: &'static str,
    handshake_us: u128,
    stream_rtt_us: u128,
    datagram_rtt_us: u128,
    rebind_stream_rtt_us: u128,
    udp_tx: u64,
    udp_rx: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().command {
        Command::QuinnSmoke => quinn_smoke().await,
        Command::FrameCompression => frame_compression(),
        Command::PtyDatagramCompression => pty_datagram_compression(),
        Command::PtyStateDelta => pty_state_delta(),
        Command::FileSync => file_sync(),
        Command::UdpProxy {
            listen,
            target,
            delay_ms,
            jitter_ms,
            loss_percent,
            rate_mbit,
        } => {
            udp_proxy(UdpProxyConfig {
                listen,
                target,
                delay_ms,
                jitter_ms,
                loss_percent,
                rate_mbit,
            })
            .await
        }
        Command::LegacySmoke {
            server,
            cert,
            auth_token_file,
        } => legacy_smoke(&server, &cert, &auth_token_file).await,
        Command::ProtocolFuzz {
            iterations,
            max_bytes,
            seed,
        } => protocol_fuzz(iterations, max_bytes, seed),
    }
}

#[derive(Serialize)]
struct ProtocolFuzzResult {
    experiment: &'static str,
    seed: u64,
    iterations: usize,
    max_bytes: usize,
    inputs: usize,
    decoder_calls: u64,
    panics: u64,
}

/// A tiny deterministic PRNG is sufficient for a reproducible mutation
/// corpus and avoids adding a dependency to the benchmark binary. It is not
/// intended to provide cryptographic randomness.
struct FuzzRng {
    state: u64,
}

impl FuzzRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    fn next(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }

    fn bounded(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() as usize) % bound
        }
    }
}

fn protocol_fuzz(iterations: usize, max_bytes: usize, seed: u64) -> Result<()> {
    if iterations == 0 {
        bail!("--iterations must be greater than zero");
    }
    if iterations > MAX_PROTOCOL_FUZZ_ITERATIONS {
        bail!(
            "--iterations must be at most {MAX_PROTOCOL_FUZZ_ITERATIONS} to keep the harness bounded"
        );
    }
    if !(1..=1024 * 1024).contains(&max_bytes) {
        bail!("--max-bytes must be between 1 and 1048576");
    }

    let valid_event = encode_message(&Event {
        id: 7,
        unix_ms: 9,
        kind: EventKind::SessionCreated,
    })?;
    let valid_request = encode_message(&Request::Health)?;
    let valid_frame = encode_frame_payload(&valid_request)?;
    let valid_response = encode_message(&Response::Error {
        code: "fuzz".to_owned(),
        message: "fixture".to_owned(),
    })?;
    let valid_pty = encode_message(&PtyStateDatagram {
        session_id: uuid::Uuid::nil(),
        generation: 1,
        rows: 24,
        cols: 80,
        screen: vec!["fuzz".to_owned()],
        cursor_row: 0,
        cursor_col: 0,
    })?;
    let valid_delta = encode_pty_state_delta_datagram(
        &PtyStateDeltaDatagram {
            session_id: uuid::Uuid::nil(),
            base_generation: 1,
            generation: 2,
            rows: 24,
            cols: 80,
            changes: vec![PtyRowDelta {
                row: 0,
                text: "fuzz".to_owned(),
            }],
            cursor_row: 0,
            cursor_col: 0,
        },
        usize::MAX,
    )?
    .ok_or_else(|| anyhow::anyhow!("valid PTY delta fixture unexpectedly rejected"))?;
    let valid_rich = encode_pty_rich_datagram(
        &PtyStateRichDatagram {
            session_id: uuid::Uuid::nil(),
            generation: 1,
            rows: 24,
            cols: 80,
            screen: b"fuzz".to_vec(),
            cursor_row: 0,
            cursor_col: 0,
        },
        usize::MAX,
        true,
    )?
    .ok_or_else(|| anyhow::anyhow!("valid rich PTY fixture unexpectedly rejected"))?;
    let seeds = [
        Vec::new(),
        vec![0],
        vec![0xff; 7],
        valid_event,
        valid_request,
        valid_frame,
        valid_response,
        valid_pty,
        valid_delta,
        valid_rich,
    ];

    let mut rng = FuzzRng::new(seed);
    let mut decoder_calls = 0_u64;
    for iteration in 0..iterations {
        let mut input = if iteration < seeds.len() {
            seeds[iteration].clone()
        } else {
            let length = rng.bounded(max_bytes.saturating_add(1));
            (0..length).map(|_| rng.next() as u8).collect()
        };
        if input.len() > max_bytes {
            input.truncate(max_bytes);
        }
        // Keep the seed corpus intact so recognized prefixes and valid
        // messages are always exercised. Mutate only generated inputs;
        // retaining the first bytes occasionally also explores recognized
        // envelope paths with malformed bodies.
        if iteration >= seeds.len() {
            let mutations = 1 + rng.bounded(8);
            for _ in 0..mutations {
                if input.is_empty() {
                    input.push(rng.next() as u8);
                    continue;
                }
                let index = rng.bounded(input.len());
                input[index] ^= (rng.next() as u8).max(1);
            }
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = decode_message::<Request>(&input);
            let _ = decode_message::<Response>(&input);
            let _ = decode_message::<Event>(&input);
            let _ = decode_frame_payload(&input);
            let _ = decode_frame_payload_for_version(&input, PROTOCOL_VERSION);
            let _ = decode_frame_payload_for_version(&input, LEGACY_PROTOCOL_VERSION);
            let _ = decode_pty_state_datagram(&input);
            let _ = decode_pty_state_delta_datagram(&input);
            let _ = decode_pty_rich_datagram(&input, false);
            let _ = decode_pty_rich_datagram(&input, true);
        }));
        decoder_calls = decoder_calls.saturating_add(10);
        if result.is_err() {
            bail!(
                "protocol decoder panic at iteration {iteration}, input_len={}, seed={seed:#x}",
                input.len()
            );
        }
    }

    println!(
        "{}",
        serde_json::to_string(&ProtocolFuzzResult {
            experiment: "protocol-fuzz",
            seed,
            iterations,
            max_bytes,
            inputs: iterations,
            decoder_calls,
            panics: 0,
        })?
    );
    Ok(())
}

const FILE_PATCH_FIXED_OVERHEAD_BYTES: usize = 128;
const FILE_PATCH_RANGES_FIXED_OVERHEAD_BYTES: usize = 160;
const FILE_PATCH_RANGE_OVERHEAD_BYTES: usize = 24;
const FILE_PATCH_RANGE_COALESCE_GAP_BYTES: usize = 32;
const FILE_PATCH_LINE_MAX_LINES: usize = 2_048;
const FILE_PATCH_LINE_MAX_LCS_CELLS: usize = 4_000_000;
const FILE_PATCH_MAX_DERIVED_RANGES: usize = 4_096;
const FRAME_LENGTH_PREFIX_BYTES: usize = std::mem::size_of::<u32>();
const MAX_PROTOCOL_FUZZ_ITERATIONS: usize = 1_000_000;

#[derive(Serialize)]
struct FileSyncResult {
    experiment: &'static str,
    protocol_version: u16,
    cases: Vec<FileSyncCase>,
}

#[derive(Serialize)]
struct FileSyncCase {
    name: &'static str,
    base_bytes: usize,
    new_bytes: usize,
    prefix_bytes: usize,
    suffix_bytes: usize,
    replacement_bytes: usize,
    full_wire_bytes: usize,
    patch_wire_bytes: usize,
    raw_policy_chooses_patch: bool,
    patch_wire_saves_bytes: isize,
    patch_is_wire_win: bool,
    range_count: usize,
    range_replacement_bytes: usize,
    range_wire_bytes: usize,
    raw_policy_chooses_ranges: bool,
    range_wire_saves_bytes: isize,
    range_is_wire_win: bool,
}

fn file_sync() -> Result<()> {
    let mut cases = Vec::new();

    let mut localized_old = vec![b'a'; 256];
    localized_old.extend_from_slice(b"old body");
    localized_old.extend_from_slice(&[b'z'; 256]);
    let mut localized_new = vec![b'a'; 256];
    localized_new.extend_from_slice(b"new body");
    localized_new.extend_from_slice(&[b'z'; 256]);
    cases.push(file_sync_case(
        "localized-source-edit",
        &localized_old,
        &localized_new,
    )?);

    let mut scattered_old = Vec::with_capacity(128 * 1024);
    for line in 0..1_024 {
        scattered_old
            .extend_from_slice(format!("line {line:04}: original source text\n").as_bytes());
    }
    let mut scattered_new = scattered_old.clone();
    for (needle, replacement) in [
        (
            b"line 0010: original source text".as_slice(),
            b"line 0010: edited source text".as_slice(),
        ),
        (
            b"line 0512: original source text".as_slice(),
            b"line 0512: edited source text".as_slice(),
        ),
        (
            b"line 1000: original source text".as_slice(),
            b"line 1000: edited source text".as_slice(),
        ),
    ] {
        let Some(start) = scattered_new
            .windows(needle.len())
            .position(|window| window == needle)
        else {
            bail!("file-sync fixture needle missing");
        };
        scattered_new.splice(start..start + needle.len(), replacement.iter().copied());
    }
    cases.push(file_sync_case(
        "scattered-source-edits",
        &scattered_old,
        &scattered_new,
    )?);

    // Equal-length edits are the case where a multi-range patch preserves the
    // scattered structure instead of collapsing the whole middle into one
    // legacy prefix/suffix replacement.
    let mut scattered_equal_old = vec![b'a'; 96 * 1024];
    let mut scattered_equal_new = scattered_equal_old.clone();
    for (offset, replacement) in [
        (1_024, b"EDIT-001".as_slice()),
        (32_768, b"EDIT-002".as_slice()),
        (81_920, b"EDIT-003".as_slice()),
    ] {
        scattered_equal_old[offset..offset + replacement.len()].copy_from_slice(b"ORIG-000");
        scattered_equal_new[offset..offset + replacement.len()].copy_from_slice(replacement);
    }
    cases.push(file_sync_case(
        "scattered-equal-length-source-edits",
        &scattered_equal_old,
        &scattered_equal_new,
    )?);

    let broad_old = vec![b'a'; 128 * 1024];
    let broad_new = vec![b'b'; 128 * 1024];
    cases.push(file_sync_case(
        "compressible-broad-rewrite",
        &broad_old,
        &broad_new,
    )?);

    let mut random_old = Vec::with_capacity(128 * 1024);
    let mut state = 0x9e37_79b9_u64;
    for _ in 0..128 * 1024 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        random_old.push((state >> 24) as u8);
    }
    let mut random_new = random_old.clone();
    random_new[48 * 1024..80 * 1024].fill(b'x');
    cases.push(file_sync_case(
        "incompressible-middle-rewrite",
        &random_old,
        &random_new,
    )?);

    println!(
        "{}",
        serde_json::to_string(&FileSyncResult {
            experiment: "file-sync-wire",
            protocol_version: PROTOCOL_VERSION,
            cases,
        })?
    );
    Ok(())
}

fn file_sync_case(name: &'static str, old: &[u8], new: &[u8]) -> Result<FileSyncCase> {
    let prefix = common_prefix(old, new);
    let suffix = common_suffix(&old[prefix..], &new[prefix..]);
    let replacement = &new[prefix..new.len() - suffix];
    let expected_sha256 = "a".repeat(64);
    let full = Request::FilePut {
        session_id: uuid::Uuid::nil(),
        request_id: uuid::Uuid::nil(),
        path: "fixture.txt".to_owned(),
        expected_sha256: Some(expected_sha256.clone()),
        allow_blind: false,
        data: new.to_vec(),
    };
    let patch = Request::FilePatch {
        session_id: uuid::Uuid::nil(),
        request_id: uuid::Uuid::nil(),
        path: "fixture.txt".to_owned(),
        expected_sha256,
        prefix_len: prefix as u64,
        suffix_len: suffix as u64,
        replacement: replacement.to_vec(),
    };
    let ranges = derive_file_patch_ranges(old, new);
    let ranges_request = Request::FilePatchRanges {
        session_id: uuid::Uuid::nil(),
        request_id: uuid::Uuid::nil(),
        path: "fixture.txt".to_owned(),
        expected_sha256: "a".repeat(64),
        ranges: ranges.clone(),
    };
    let full_wire_bytes = FRAME_LENGTH_PREFIX_BYTES.saturating_add(
        encode_frame_payload_for_version(&encode_message(&full)?, PROTOCOL_VERSION)?.len(),
    );
    let patch_wire_bytes = FRAME_LENGTH_PREFIX_BYTES.saturating_add(
        encode_frame_payload_for_version(&encode_message(&patch)?, PROTOCOL_VERSION)?.len(),
    );
    let range_wire_bytes = if ranges.is_empty() {
        0
    } else {
        FRAME_LENGTH_PREFIX_BYTES.saturating_add(
            encode_frame_payload_for_version(&encode_message(&ranges_request)?, PROTOCOL_VERSION)?
                .len(),
        )
    };
    let raw_policy_chooses_patch = replacement
        .len()
        .saturating_add(FILE_PATCH_FIXED_OVERHEAD_BYTES)
        < new.len();
    Ok(FileSyncCase {
        name,
        base_bytes: old.len(),
        new_bytes: new.len(),
        prefix_bytes: prefix,
        suffix_bytes: suffix,
        replacement_bytes: replacement.len(),
        full_wire_bytes,
        patch_wire_bytes,
        raw_policy_chooses_patch,
        patch_wire_saves_bytes: full_wire_bytes as isize - patch_wire_bytes as isize,
        patch_is_wire_win: patch_wire_bytes < full_wire_bytes,
        range_count: ranges.len(),
        range_replacement_bytes: ranges.iter().fold(0usize, |total, range| {
            total.saturating_add(range.replacement.len())
        }),
        range_wire_bytes,
        raw_policy_chooses_ranges: ranges.len() > 1
            && ranges
                .iter()
                .fold(0usize, |total, range| {
                    total.saturating_add(range.replacement.len())
                })
                .saturating_add(FILE_PATCH_RANGES_FIXED_OVERHEAD_BYTES)
                .saturating_add(ranges.len().saturating_mul(FILE_PATCH_RANGE_OVERHEAD_BYTES))
                < new.len(),
        range_wire_saves_bytes: if ranges.is_empty() {
            0
        } else {
            full_wire_bytes as isize - range_wire_bytes as isize
        },
        range_is_wire_win: range_wire_bytes != 0 && range_wire_bytes < full_wire_bytes,
    })
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

    let mut edits = Vec::<(usize, usize, usize, usize)>::new();
    let (mut old_index, mut new_index) = (0usize, 0usize);
    let (mut old_anchor, mut new_anchor) = (0usize, 0usize);
    while old_index < old_lines.len() && new_index < new_lines.len() {
        if old[old_lines[old_index].0..old_lines[old_index].1]
            == new[new_lines[new_index].0..new_lines[new_index].1]
        {
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

const MAX_PROXY_DELAY_MS: u64 = 3_600_000;
const MAX_PROXY_PENDING_PACKETS: usize = 1024;
const MAX_PROXY_PENDING_BYTES: usize = 32 * 1024 * 1024;
const MAX_PROXY_DATAGRAM_BYTES: usize = 65_535;

#[derive(Clone, Copy, Debug)]
struct UdpProxyConfig {
    listen: SocketAddr,
    target: SocketAddr,
    delay_ms: u64,
    jitter_ms: u64,
    loss_percent: u8,
    rate_mbit: u64,
}

#[derive(Debug, Serialize, Default, Clone, Copy)]
struct UdpProxyStats {
    received_packets: u64,
    received_bytes: u64,
    forwarded_packets: u64,
    forwarded_bytes: u64,
    lost_packets: u64,
    queue_dropped_packets: u64,
    replies_before_client_packets: u64,
}

#[derive(Debug)]
struct ScheduledProxyPacket {
    due: TokioInstant,
    order: u64,
    destination: SocketAddr,
    payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
enum ProxyDirection {
    ClientToServer,
    ServerToClient,
}

struct UdpProxyState {
    config: UdpProxyConfig,
    client: Option<SocketAddr>,
    pending: Vec<ScheduledProxyPacket>,
    pending_bytes: usize,
    next_client_send: TokioInstant,
    next_server_send: TokioInstant,
    random_state: u64,
    next_order: u64,
    stats: UdpProxyStats,
}

async fn udp_proxy(config: UdpProxyConfig) -> Result<()> {
    validate_udp_proxy_config(config)?;
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let stats = run_udp_proxy(config, shutdown, None).await?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "experiment": "udp-proxy",
            "status": "stopped",
            "listen": config.listen,
            "target": config.target,
            "delay_ms": config.delay_ms,
            "jitter_ms": config.jitter_ms,
            "loss_percent": config.loss_percent,
            "rate_mbit": config.rate_mbit,
            "stats": stats,
        }))?
    );
    Ok(())
}

fn validate_udp_proxy_config(config: UdpProxyConfig) -> Result<()> {
    if config.delay_ms > MAX_PROXY_DELAY_MS {
        bail!("--delay-ms must be at most {MAX_PROXY_DELAY_MS} (one hour)");
    }
    if config.jitter_ms > MAX_PROXY_DELAY_MS {
        bail!("--jitter-ms must be at most {MAX_PROXY_DELAY_MS} (one hour)");
    }
    if config.loss_percent > 100 {
        bail!("--loss-percent must be between 0 and 100");
    }
    config
        .rate_mbit
        .checked_mul(1_000_000)
        .context("--rate-mbit is too large")?;
    if config.listen == config.target {
        bail!("--listen and --target must be different UDP addresses");
    }
    Ok(())
}

/// Run one bounded UDP proxy loop. The optional readiness sender is used only
/// by the in-process forwarding test; the CLI prints its bound address before
/// entering this loop so a `:0` listener is still discoverable by callers.
async fn run_udp_proxy<F>(
    config: UdpProxyConfig,
    shutdown: F,
    ready: Option<oneshot::Sender<SocketAddr>>,
) -> Result<UdpProxyStats>
where
    F: std::future::Future<Output = ()> + Send,
{
    validate_udp_proxy_config(config)?;
    let socket = UdpSocket::bind(config.listen)
        .await
        .with_context(|| format!("bind UDP proxy listener {}", config.listen))?;
    let bound = socket.local_addr()?;
    if let Some(ready) = ready {
        let _ = ready.send(bound);
    } else {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "experiment": "udp-proxy",
                "status": "listening",
                "listen": bound,
                "target": config.target,
                "delay_ms": config.delay_ms,
                "jitter_ms": config.jitter_ms,
                "loss_percent": config.loss_percent,
                "rate_mbit": config.rate_mbit,
                "max_pending_packets": MAX_PROXY_PENDING_PACKETS,
                "max_pending_bytes": MAX_PROXY_PENDING_BYTES,
            }))?
        );
    }

    let now = TokioInstant::now();
    let mut state = UdpProxyState {
        config,
        client: None,
        pending: Vec::new(),
        pending_bytes: 0,
        next_client_send: now,
        next_server_send: now,
        // A fixed nonzero seed makes loss/jitter runs reproducible while the
        // direction and packet order still influence each draw.
        random_state: 0x9e37_79b9_7f4a_7c15,
        next_order: 0,
        stats: UdpProxyStats::default(),
    };
    let mut receive_buffer = [0_u8; MAX_PROXY_DATAGRAM_BYTES];
    tokio::pin!(shutdown);

    loop {
        flush_due_packets(&socket, &mut state).await?;
        let next_due = state.pending.iter().map(|packet| packet.due).min();
        if let Some(next_due) = next_due {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = tokio::time::sleep_until(next_due) => {},
                received = socket.recv_from(&mut receive_buffer) => {
                    let (length, peer) = received.context("receive UDP proxy datagram")?;
                    queue_proxy_datagram(&mut state, peer, &receive_buffer[..length]);
                }
            }
        } else {
            tokio::select! {
                _ = &mut shutdown => break,
                received = socket.recv_from(&mut receive_buffer) => {
                    let (length, peer) = received.context("receive UDP proxy datagram")?;
                    queue_proxy_datagram(&mut state, peer, &receive_buffer[..length]);
                }
            }
        }
    }

    // Do not keep delayed packets alive after shutdown. A benchmark process
    // should terminate promptly even if it was configured with a long delay.
    state.pending.clear();
    state.pending_bytes = 0;
    Ok(state.stats)
}

async fn flush_due_packets(socket: &UdpSocket, state: &mut UdpProxyState) -> Result<()> {
    loop {
        let now = TokioInstant::now();
        let Some(index) = state
            .pending
            .iter()
            .enumerate()
            .filter(|(_, packet)| packet.due <= now)
            .min_by_key(|(_, packet)| (packet.due, packet.order))
            .map(|(index, _)| index)
        else {
            return Ok(());
        };
        let packet = state.pending.swap_remove(index);
        state.pending_bytes = state.pending_bytes.saturating_sub(packet.payload.len());
        socket
            .send_to(&packet.payload, packet.destination)
            .await
            .with_context(|| format!("forward UDP proxy datagram to {}", packet.destination))?;
        state.stats.forwarded_packets = state.stats.forwarded_packets.saturating_add(1);
        state.stats.forwarded_bytes = state
            .stats
            .forwarded_bytes
            .saturating_add(packet.payload.len() as u64);
    }
}

fn queue_proxy_datagram(state: &mut UdpProxyState, peer: SocketAddr, payload: &[u8]) {
    let (destination, direction) = if peer == state.config.target {
        let Some(client) = state.client else {
            state.stats.replies_before_client_packets =
                state.stats.replies_before_client_packets.saturating_add(1);
            return;
        };
        (client, ProxyDirection::ServerToClient)
    } else {
        // Treat the most recent non-target sender as the client. This permits
        // Quinn's endpoint to rebind to a new source port while the proxy is
        // running, which is useful for migration experiments.
        state.client = Some(peer);
        (state.config.target, ProxyDirection::ClientToServer)
    };

    state.stats.received_packets = state.stats.received_packets.saturating_add(1);
    state.stats.received_bytes = state
        .stats
        .received_bytes
        .saturating_add(payload.len() as u64);
    if should_drop(state.config.loss_percent, &mut state.random_state) {
        state.stats.lost_packets = state.stats.lost_packets.saturating_add(1);
        return;
    }
    if state.pending.len() >= MAX_PROXY_PENDING_PACKETS
        || state.pending_bytes.saturating_add(payload.len()) > MAX_PROXY_PENDING_BYTES
    {
        state.stats.queue_dropped_packets = state.stats.queue_dropped_packets.saturating_add(1);
        return;
    }

    let next_send = match direction {
        ProxyDirection::ClientToServer => &mut state.next_client_send,
        ProxyDirection::ServerToClient => &mut state.next_server_send,
    };
    let due = shaped_due(
        TokioInstant::now(),
        state.config.delay_ms,
        state.config.jitter_ms,
        state.config.rate_mbit,
        payload.len(),
        next_send,
        &mut state.random_state,
    );
    let order = state.next_order;
    state.next_order = state.next_order.wrapping_add(1);
    state.pending.push(ScheduledProxyPacket {
        due,
        order,
        destination,
        payload: payload.to_vec(),
    });
    state.pending_bytes = state.pending_bytes.saturating_add(payload.len());
}

fn should_drop(loss_percent: u8, random_state: &mut u64) -> bool {
    loss_percent != 0 && next_random(random_state) % 100 < u64::from(loss_percent)
}

fn next_random(random_state: &mut u64) -> u64 {
    let mut value = *random_state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *random_state = value;
    value
}

fn shaped_due(
    now: TokioInstant,
    delay_ms: u64,
    jitter_ms: u64,
    rate_mbit: u64,
    packet_bytes: usize,
    next_send: &mut TokioInstant,
    random_state: &mut u64,
) -> TokioInstant {
    let jitter = if jitter_ms == 0 {
        0_i128
    } else {
        let span = jitter_ms.saturating_mul(2).saturating_add(1);
        i128::from(next_random(random_state) % span) - i128::from(jitter_ms)
    };
    let delay = i128::from(delay_ms) + jitter;
    let mut due = if delay <= 0 {
        now
    } else {
        now + Duration::from_millis(delay as u64)
    };
    if due < *next_send {
        due = *next_send;
    }
    if rate_mbit != 0 {
        *next_send = due + serialization_duration(rate_mbit, packet_bytes);
    } else {
        *next_send = due;
    }
    due
}

fn serialization_duration(rate_mbit: u64, packet_bytes: usize) -> Duration {
    if rate_mbit == 0 || packet_bytes == 0 {
        return Duration::ZERO;
    }
    let rate_bps = u128::from(rate_mbit) * 1_000_000;
    let nanos = (u128::from(packet_bytes as u64) * 8 * 1_000_000_000)
        .div_ceil(rate_bps)
        .max(1)
        .min(u128::from(u64::MAX));
    Duration::from_nanos(nanos as u64)
}

#[derive(Serialize)]
struct LegacySmokeResult {
    experiment: &'static str,
    protocol_version: u16,
    hello_ok: bool,
    health_ok: bool,
}

async fn legacy_smoke(server: &str, cert_path: &PathBuf, auth_token_path: &PathBuf) -> Result<()> {
    let cert = std::fs::read(cert_path)
        .with_context(|| format!("read pinned certificate {}", cert_path.display()))?;
    let token = std::fs::read_to_string(auth_token_path)
        .with_context(|| format!("read auth token {}", auth_token_path.display()))?;
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(cert))?;
    let mut endpoint = Endpoint::client("127.0.0.1:0".parse()?)?;
    let mut client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots))?;
    let mut transport = quinn::TransportConfig::default();
    configure_quic_transport(&mut transport)?;
    client_config.transport_config(Arc::new(transport));
    endpoint.set_default_client_config(client_config);
    let remote: std::net::SocketAddr = server.parse().context("parse legacy smoke server")?;
    let connection = endpoint.connect(remote, "localhost")?.await?;

    let (mut send, mut recv) = connection.open_bi().await?;
    write_frame_for_version(
        &mut send,
        &Request::HelloFeatures {
            version: LEGACY_PROTOCOL_VERSION,
            auth_token: Some(token.trim().to_owned()),
            features: SUPPORTED_FEATURES
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
        },
        LEGACY_PROTOCOL_VERSION,
    )
    .await?;
    send.finish()?;
    let hello = read_frame_for_version::<Response>(&mut recv, LEGACY_PROTOCOL_VERSION)
        .await?
        .ok_or_else(|| anyhow::anyhow!("legacy server closed HELLO stream"))?;
    if !matches!(
        hello,
        Response::HelloFeatures {
            version: LEGACY_PROTOCOL_VERSION,
            ..
        }
    ) {
        bail!("legacy HELLO did not negotiate v16: {hello:?}");
    }

    let (mut send, mut recv) = connection.open_bi().await?;
    write_frame_for_version(&mut send, &Request::Health, LEGACY_PROTOCOL_VERSION).await?;
    send.finish()?;
    let health = read_frame_for_version::<Response>(&mut recv, LEGACY_PROTOCOL_VERSION)
        .await?
        .ok_or_else(|| anyhow::anyhow!("legacy server closed HEALTH stream"))?;
    if !matches!(health, Response::Health { .. }) {
        bail!("legacy HEALTH did not return a health response: {health:?}");
    }
    connection.close(0_u32.into(), b"legacy smoke complete");
    println!(
        "{}",
        serde_json::to_string(&LegacySmokeResult {
            experiment: "legacy-v16-smoke",
            protocol_version: LEGACY_PROTOCOL_VERSION,
            hello_ok: true,
            health_ok: true,
        })?
    );
    Ok(())
}

#[derive(Serialize)]
struct FrameCompressionResult {
    experiment: &'static str,
    repetitive_input_bytes: usize,
    repetitive_wire_bytes: usize,
    repetitive_ratio: f64,
    incompressible_input_bytes: usize,
    incompressible_wire_bytes: usize,
    incompressible_ratio: f64,
    frame_header_bytes: usize,
}

fn frame_compression() -> Result<()> {
    const SAMPLE_BYTES: usize = 1024 * 1024;
    let repetitive = vec![b'x'; SAMPLE_BYTES];
    // Deterministic xorshift bytes keep this benchmark reproducible without
    // adding a random-number dependency, while avoiding the highly
    // compressible low-byte counter pattern.
    let mut state = 0x9e37_79b9_u64;
    let incompressible = (0..SAMPLE_BYTES)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect::<Vec<_>>();
    let repetitive_wire = encode_frame_payload(&repetitive)?.len();
    let incompressible_wire = encode_frame_payload(&incompressible)?.len();
    println!(
        "{}",
        serde_json::to_string(&FrameCompressionResult {
            experiment: "frame-compression",
            repetitive_input_bytes: repetitive.len(),
            repetitive_wire_bytes: repetitive_wire,
            repetitive_ratio: repetitive_wire as f64 / repetitive.len() as f64,
            incompressible_input_bytes: incompressible.len(),
            incompressible_wire_bytes: incompressible_wire,
            incompressible_ratio: incompressible_wire as f64 / incompressible.len() as f64,
            frame_header_bytes: FRAME_HEADER_BYTES,
        })?
    );
    Ok(())
}

#[derive(Serialize)]
struct PtyDatagramCompressionResult {
    experiment: &'static str,
    mtu_bytes: usize,
    rich_input_bytes: usize,
    plain_datagram_bytes: usize,
    plain_fits: bool,
    compressed_datagram_bytes: usize,
    compressed_fits: bool,
    decoded_matches: bool,
}

fn pty_datagram_compression() -> Result<()> {
    // A terminal redraw contains repeated cursor/clear sequences and source
    // text. This deterministic fixture is intentionally larger than a usual
    // QUIC DATAGRAM so the result answers the practical "does it fit?"
    // question rather than only reporting a compression ratio.
    let mut screen = Vec::new();
    for row in 0..80 {
        screen.extend_from_slice(format!("\x1b[{};1H\x1b[2K", row + 1).as_bytes());
        screen.extend_from_slice(
            b"fn build_workspace_state() -> Result<WorkspaceState> { /* cached semantic result */ }\n",
        );
    }
    let datagram = PtyStateRichDatagram {
        session_id: uuid::Uuid::nil(),
        generation: 7,
        rows: 80,
        cols: 120,
        screen,
        cursor_row: 3,
        cursor_col: 14,
    };
    let plain = encode_pty_rich_datagram(&datagram, usize::MAX, false)?
        .ok_or_else(|| anyhow::anyhow!("plain rich datagram unexpectedly rejected"))?;
    let mtu = 1_200;
    let compressed = encode_pty_rich_datagram(&datagram, mtu, true)?
        .ok_or_else(|| anyhow::anyhow!("compressed rich datagram did not fit the fixture MTU"))?;
    let decoded_matches = decode_pty_rich_datagram(&compressed, true)? == Some(datagram.clone());
    println!(
        "{}",
        serde_json::to_string(&PtyDatagramCompressionResult {
            experiment: "pty-rich-datagram-compression",
            mtu_bytes: mtu,
            rich_input_bytes: datagram.screen.len(),
            plain_datagram_bytes: plain.len(),
            plain_fits: plain.len() <= mtu,
            compressed_datagram_bytes: compressed.len(),
            compressed_fits: compressed.len() <= mtu,
            decoded_matches,
        })?
    );
    Ok(())
}

#[derive(Serialize)]
struct PtyStateDeltaResult {
    experiment: &'static str,
    rows: u16,
    cols: u16,
    base_generation: u64,
    full_datagram_bytes: usize,
    cases: Vec<PtyStateDeltaCase>,
}

#[derive(Serialize)]
struct PtyStateDeltaCase {
    name: &'static str,
    changed_rows: usize,
    delta_datagram_bytes: usize,
    chooses_delta: bool,
    wire_saves_bytes: isize,
}

fn pty_state_delta() -> Result<()> {
    const ROWS: u16 = 80;
    const COLS: u16 = 120;
    const BASE_GENERATION: u64 = 10;
    let base_screen = (0..ROWS)
        .map(|row| {
            format!(
                "row {row:02}: fn inspect_workspace_state() -> Result<State> {{ /* stable */ }}"
            )
        })
        .collect::<Vec<_>>();
    let full = PtyStateDatagram {
        session_id: uuid::Uuid::nil(),
        generation: BASE_GENERATION,
        rows: ROWS,
        cols: COLS,
        screen: base_screen.clone(),
        cursor_row: 0,
        cursor_col: 0,
    };
    let full_datagram_bytes = encode_message(&full)?.len();
    let cases = [
        ("cursor-only", Vec::new(), 3_u16, 14_u16),
        (
            "localized-row",
            vec![PtyRowDelta {
                row: 37,
                text: "row 37: fn inspect_workspace_state() -> Result<State> { /* changed */ }"
                    .into(),
            }],
            37_u16,
            28_u16,
        ),
        (
            "broad-screen",
            base_screen
                .iter()
                .enumerate()
                .map(|(row, _)| PtyRowDelta {
                    row: row as u16,
                    // A broad rewrite with long rows should make the
                    // conservative sender choose the full snapshot.
                    text: format!("row {row:02}: {}", "x".repeat(120)),
                })
                .collect::<Vec<_>>(),
            0_u16,
            0_u16,
        ),
    ]
    .into_iter()
    .map(
        |(name, changes, cursor_row, cursor_col)| -> Result<PtyStateDeltaCase> {
            let changed_rows = changes.len();
            let delta = PtyStateDeltaDatagram {
                session_id: uuid::Uuid::nil(),
                base_generation: BASE_GENERATION,
                generation: BASE_GENERATION + 1,
                rows: ROWS,
                cols: COLS,
                changes,
                cursor_row,
                cursor_col,
            };
            let encoded = encode_pty_state_delta_datagram(&delta, usize::MAX)?
                .ok_or_else(|| anyhow::anyhow!("unbounded delta fixture unexpectedly rejected"))?;
            let delta_datagram_bytes = encoded.len();
            Ok(PtyStateDeltaCase {
                name,
                changed_rows,
                delta_datagram_bytes,
                chooses_delta: delta_datagram_bytes < full_datagram_bytes,
                wire_saves_bytes: full_datagram_bytes as isize - delta_datagram_bytes as isize,
            })
        },
    )
    .collect::<Result<Vec<_>>>()?;
    println!(
        "{}",
        serde_json::to_string(&PtyStateDeltaResult {
            experiment: "pty-state-delta",
            rows: ROWS,
            cols: COLS,
            base_generation: BASE_GENERATION,
            full_datagram_bytes,
            cases,
        })?
    );
    Ok(())
}

async fn quinn_smoke() -> Result<()> {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let cert = CertificateDer::from(generated.cert);
    let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());
    let mut server_config = ServerConfig::with_single_cert(vec![cert.clone()], key.into())?;
    let transport = Arc::get_mut(&mut server_config.transport).expect("new config is unique");
    configure_quic_transport(transport)?;
    let server = Endpoint::server(server_config, "127.0.0.1:0".parse()?)?;
    let server_addr = server.local_addr()?;

    let server_task = tokio::spawn(async move {
        let conn = server
            .accept()
            .await
            .context("no incoming connection")?
            .await?;
        let (mut send, mut recv) = conn.accept_bi().await?;
        let data = recv.read_to_end(4096).await?;
        send.write_all(&data).await?;
        send.finish()?;
        let data = conn.read_datagram().await?;
        conn.send_datagram(data)?;
        let (mut send, mut recv) = conn.accept_bi().await?;
        let data = recv.read_to_end(4096).await?;
        send.write_all(&data).await?;
        send.finish()?;
        let _ = conn.closed().await;
        Result::<_>::Ok(())
    });

    let mut roots = RootCertStore::empty();
    roots.add(cert)?;
    let mut endpoint = Endpoint::client("127.0.0.1:0".parse()?)?;
    let mut client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots))?;
    let mut transport = quinn::TransportConfig::default();
    configure_quic_transport(&mut transport)?;
    client_config.transport_config(Arc::new(transport));
    endpoint.set_default_client_config(client_config);
    let started = Instant::now();
    let conn = endpoint.connect(server_addr, "localhost")?.await?;
    let handshake_us = started.elapsed().as_micros();

    let stream_started = Instant::now();
    stream_echo(&conn, b"reliable stream").await?;
    let stream_rtt_us = stream_started.elapsed().as_micros();

    let datagram_started = Instant::now();
    conn.send_datagram(Bytes::from_static(b"latest state"))?;
    let _ = conn.read_datagram().await?;
    let datagram_rtt_us = datagram_started.elapsed().as_micros();

    let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
    endpoint.rebind(socket)?;
    let rebind_started = Instant::now();
    stream_echo(&conn, b"after migration").await?;
    let rebind_stream_rtt_us = rebind_started.elapsed().as_micros();

    let stats = conn.stats();
    conn.close(0_u32.into(), b"done");
    server_task.await??;
    println!(
        "{}",
        serde_json::to_string(&SmokeResult {
            experiment: "quinn-smoke",
            handshake_us,
            stream_rtt_us,
            datagram_rtt_us,
            rebind_stream_rtt_us,
            udp_tx: stats.udp_tx.datagrams,
            udp_rx: stats.udp_rx.datagrams,
        })?
    );
    Ok(())
}

async fn stream_echo(conn: &quinn::Connection, message: &[u8]) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(message).await?;
    send.finish()?;
    let echo = recv.read_to_end(4096).await?;
    anyhow::ensure!(echo == message, "stream echo mismatch");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_fuzz_runner_exercises_bounded_decoder_corpus() {
        assert!(protocol_fuzz(32, 128, 0x4153_505f_4655_5a5a).is_ok());
    }

    #[test]
    fn file_sync_measurement_keeps_localized_edits_on_patch_path() {
        let mut old = vec![b'x'; 520];
        old[256..264].copy_from_slice(b"old body");
        let mut new = old.clone();
        new[256..264].copy_from_slice(b"new body");
        let case = file_sync_case("localized", &old, &new).unwrap();
        assert!(case.raw_policy_chooses_patch);
        assert!(case.patch_is_wire_win);
        assert!(case.patch_wire_saves_bytes > 0);
    }

    #[test]
    fn file_sync_measurement_uses_ranges_for_scattered_length_changes() {
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
        let case =
            file_sync_case("scattered-length-changing", old.as_bytes(), new.as_bytes()).unwrap();
        assert!(case.range_count >= 3);
        assert!(case.raw_policy_chooses_ranges);
        assert!(case.range_wire_saves_bytes > 0);
    }

    #[test]
    fn file_sync_measurement_keeps_broad_rewrites_conservative() {
        let old = vec![b'a'; 128 * 1024];
        let new = vec![b'b'; 128 * 1024];
        let case = file_sync_case("broad", &old, &new).unwrap();
        assert!(!case.raw_policy_chooses_patch);
    }

    #[test]
    fn pty_state_delta_measurement_prefers_localized_changes() {
        const ROWS: u16 = 40;
        const COLS: u16 = 100;
        let screen = (0..ROWS)
            .map(|row| format!("row {row:02}: stable source text"))
            .collect::<Vec<_>>();
        let full = encode_message(&PtyStateDatagram {
            session_id: uuid::Uuid::nil(),
            generation: 1,
            rows: ROWS,
            cols: COLS,
            screen,
            cursor_row: 0,
            cursor_col: 0,
        })
        .unwrap()
        .len();
        let delta = encode_pty_state_delta_datagram(
            &PtyStateDeltaDatagram {
                session_id: uuid::Uuid::nil(),
                base_generation: 1,
                generation: 2,
                rows: ROWS,
                cols: COLS,
                changes: vec![PtyRowDelta {
                    row: 12,
                    text: "row 12: edited source text".into(),
                }],
                cursor_row: 1,
                cursor_col: 2,
            },
            usize::MAX,
        )
        .unwrap()
        .unwrap()
        .len();
        assert!(delta < full);
    }

    #[test]
    fn udp_proxy_shaping_is_deterministic_and_rate_bounded() {
        let now = TokioInstant::now();
        let mut first_next = now;
        let mut second_next = now;
        let mut first_random = 7_u64;
        let mut second_random = 7_u64;
        let first = shaped_due(now, 50, 20, 1, 1_250, &mut first_next, &mut first_random);
        let second = shaped_due(now, 50, 20, 1, 1_250, &mut second_next, &mut second_random);
        assert_eq!(first, second);
        assert!(first >= now);
        assert_eq!(
            first_next.duration_since(first),
            serialization_duration(1, 1_250)
        );
    }

    #[test]
    fn udp_proxy_loss_is_deterministic_and_honors_extremes() {
        let mut random_state = 1_u64;
        assert!(!should_drop(0, &mut random_state));
        assert!(should_drop(100, &mut random_state));

        let mut first = 0xfeed_u64;
        let mut second = 0xfeed_u64;
        let first_draws = (0..32)
            .map(|_| should_drop(37, &mut first))
            .collect::<Vec<_>>();
        let second_draws = (0..32)
            .map(|_| should_drop(37, &mut second))
            .collect::<Vec<_>>();
        assert_eq!(first_draws, second_draws);
        assert!(first_draws.iter().any(|dropped| *dropped));
        assert!(first_draws.iter().any(|dropped| !*dropped));
    }

    #[tokio::test]
    async fn udp_proxy_forwards_bidirectionally_on_loopback() {
        let target = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let target_task = tokio::spawn(async move {
            let mut buffer = [0_u8; 128];
            let (length, peer) = target.recv_from(&mut buffer).await.unwrap();
            assert_eq!(&buffer[..length], b"ping");
            target.send_to(b"pong", peer).await.unwrap();
        });

        let (ready_sender, ready_receiver) = oneshot::channel();
        let (stop_sender, stop_receiver) = oneshot::channel();
        let proxy_task = tokio::spawn(run_udp_proxy(
            UdpProxyConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                target: target_addr,
                delay_ms: 0,
                jitter_ms: 0,
                loss_percent: 0,
                rate_mbit: 0,
            },
            async move {
                let _ = stop_receiver.await;
            },
            Some(ready_sender),
        ));
        let proxy_addr = ready_receiver.await.unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"ping", proxy_addr).await.unwrap();
        let mut response = [0_u8; 128];
        let (length, peer) =
            tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut response))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(&response[..length], b"pong");
        assert_eq!(peer, proxy_addr);
        let _ = stop_sender.send(());
        let stats = proxy_task.await.unwrap().unwrap();
        target_task.await.unwrap();
        assert_eq!(stats.received_packets, 2);
        assert_eq!(stats.forwarded_packets, 2);
        assert_eq!(stats.lost_packets, 0);
    }
}
