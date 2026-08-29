//! Persistent PTY wrapper. The reader thread and child outlive network connections.

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::VecDeque;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt as UnixCommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, broadcast};

const TAIL_LIMIT: usize = 64 * 1024;
const TMUX_SCROLLBACK_CAPTURE_MAX_BYTES: usize = 1024 * 1024;
const TMUX_SCROLLBACK_CAPTURE_TIMEOUT: Duration = Duration::from_millis(500);
const TMUX_SCROLLBACK_CAPTURE_RETRIES: usize = 3;
const TMUX_SCROLLBACK_CAPTURE_RETRY_DELAY: Duration = Duration::from_millis(20);
const TMUX_SCROLLBACK_CAPTURE_RETRY_WINDOW: Duration = Duration::from_millis(750);
const TMUX_SIZE_QUERY_MAX_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub screen: Vec<String>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub tail: Vec<u8>,
}

/// A reconnect-friendly terminal snapshot that preserves the parser's cell
/// attributes as ANSI SGR/control sequences.  The byte stream remains the
/// authoritative PTY output; this representation is only a replaceable view
/// used by peers that negotiate the optional `pty_rich_state` capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichSnapshot {
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub screen: Vec<u8>,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

/// Counters for the generation-keyed screen render caches.  Rendering is
/// deliberately lazy, so a hit means an attachment cloned an already
/// rendered generation while a render means it had to walk the terminal
/// parser.  The counters are process-local observability only; snapshots and
/// PTY output remain correct if they are reset or unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotCacheStats {
    pub snapshot_hits: u64,
    pub snapshot_renders: u64,
    pub rich_snapshot_hits: u64,
    pub rich_snapshot_renders: u64,
}

#[derive(Default)]
struct SnapshotCacheCounters {
    snapshot_hits: AtomicU64,
    snapshot_renders: AtomicU64,
    rich_snapshot_hits: AtomicU64,
    rich_snapshot_renders: AtomicU64,
}

impl SnapshotCacheCounters {
    fn snapshot(&self) -> SnapshotCacheStats {
        SnapshotCacheStats {
            snapshot_hits: self.snapshot_hits.load(Ordering::Relaxed),
            snapshot_renders: self.snapshot_renders.load(Ordering::Relaxed),
            rich_snapshot_hits: self.rich_snapshot_hits.load(Ordering::Relaxed),
            rich_snapshot_renders: self.rich_snapshot_renders.load(Ordering::Relaxed),
        }
    }
}

/// Optional operator-supplied executable boundary for the tmux process that
/// owns a durable PTY. The server validates the executable identity before
/// constructing this value; this crate only preserves the argv contract and
/// invokes the launcher with the absolute tmux command followed by its args.
/// A launcher should `exec` its arguments so tmux remains observable and can
/// survive an ASP daemon restart according to the deployment policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLauncher {
    pub program: PathBuf,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
struct TmuxContext {
    program: PathBuf,
    session_name: String,
    launcher: Option<CommandLauncher>,
}

struct State {
    generation: u64,
    tail: VecDeque<u8>,
    parser: vt100::Parser,
    // Screen rendering is substantially more expensive than reading the
    // generation or cloning the already-rendered rows.  Several clients can
    // attach to one durable PTY at once (for example an interactive shell and
    // an agent observer), so keep one generation-keyed cache in the PTY owner
    // instead of making every attachment walk the parser independently.
    snapshot_cache: Option<Snapshot>,
    rich_snapshot_cache: Option<RichSnapshot>,
}

pub struct PersistentPty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    // Keep the writer optional so a tmux attachment can relinquish it without
    // running portable-pty's UnixMasterWriter::drop. That drop writes a
    // newline plus VEOF to the child; on shutdown this would be interpreted
    // by the attached shell as an input EOF and could terminate the durable
    // tmux pane just as the daemon is restarting.
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    // portable-pty's writer is synchronous. Keep at most one blocking write
    // in flight per PTY so a timed-out attachment cannot spawn an unbounded
    // set of blocked Tokio worker tasks while a later attachment retries.
    write_gate: Arc<Semaphore>,
    state: Arc<Mutex<State>>,
    snapshot_cache_counters: SnapshotCacheCounters,
    output: broadcast::Sender<Vec<u8>>,
    tmux: Option<TmuxContext>,
}

impl PersistentPty {
    pub fn spawn(rows: u16, cols: u16) -> Result<Arc<Self>> {
        Self::spawn_command(
            rows,
            cols,
            {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
                let mut command = CommandBuilder::new(shell);
                command.env("TERM", "xterm-256color");
                command
            },
            0,
        )
    }

    /// Attach to a named tmux session. The tmux server owns the shell, so a
    /// restarted aspd can open a new PTY and reattach to the same process.
    pub fn spawn_tmux(rows: u16, cols: u16, session_name: &str) -> Result<Arc<Self>> {
        Self::spawn_tmux_with_generation(rows, cols, session_name, 0)
    }

    pub fn spawn_tmux_with_generation(
        rows: u16,
        cols: u16,
        session_name: &str,
        generation: u64,
    ) -> Result<Arc<Self>> {
        Self::spawn_tmux_with_generation_and_launcher(rows, cols, session_name, generation, None)
    }

    /// Attach to a named tmux session through an optional operator launcher.
    /// EXEC/SPAWN and PTY children therefore share the same configured
    /// boundary when the server is running in its fail-closed production
    /// profile. The launcher receives:
    ///
    /// ```text
    /// <launcher> <configured-args> <absolute-tmux> attach-session -t <name>
    /// ```
    pub fn spawn_tmux_with_generation_and_launcher(
        rows: u16,
        cols: u16,
        session_name: &str,
        generation: u64,
        launcher: Option<&CommandLauncher>,
    ) -> Result<Arc<Self>> {
        let tmux = resolve_tmux_program()?;
        let tmux_context = TmuxContext {
            program: tmux.clone(),
            session_name: session_name.to_owned(),
            launcher: launcher.cloned(),
        };
        ensure_tmux_session(&tmux_context)?;
        let mut command = if let Some(launcher) = launcher {
            let mut command = CommandBuilder::new(&launcher.program);
            command.args(&launcher.args);
            command.arg(&tmux);
            command
        } else {
            CommandBuilder::new(tmux)
        };
        // The tmux server/session is created detached before this PTY is
        // opened. `attach-session` is only a replaceable view: when aspd
        // exits, tmux still owns the shell and keeps the session available for
        // the next daemon instance.
        command.args(["attach-session", "-t", session_name]);
        command.env("TERM", "xterm-256color");
        Self::spawn_command_with_tmux(rows, cols, command, generation, Some(tmux_context))
    }

    fn spawn_command(
        rows: u16,
        cols: u16,
        command: CommandBuilder,
        generation: u64,
    ) -> Result<Arc<Self>> {
        Self::spawn_command_with_tmux(rows, cols, command, generation, None)
    }

    fn spawn_command_with_tmux(
        rows: u16,
        cols: u16,
        command: CommandBuilder,
        generation: u64,
        tmux: Option<TmuxContext>,
    ) -> Result<Arc<Self>> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open PTY")?;
        let _child = pair.slave.spawn_command(command).context("spawn shell")?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer = pair.master.take_writer().context("take PTY writer")?;
        let (output, _) = broadcast::channel(256);
        let state = Arc::new(Mutex::new(State {
            generation,
            tail: VecDeque::new(),
            parser: vt100::Parser::new(rows, cols, 1_000),
            snapshot_cache: None,
            rich_snapshot_cache: None,
        }));
        let pty = Arc::new(Self {
            master: Mutex::new(pair.master),
            writer: Mutex::new(Some(writer)),
            write_gate: Arc::new(Semaphore::new(1)),
            state: Arc::clone(&state),
            snapshot_cache_counters: SnapshotCacheCounters::default(),
            output: output.clone(),
            tmux,
        });

        thread::Builder::new()
            .name("asp-pty-reader".into())
            .spawn(move || {
                let mut buf = vec![0_u8; 8192];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let chunk = buf[..n].to_vec();
                    {
                        let mut state = state.lock().expect("PTY state poisoned");
                        state.generation += 1;
                        state.parser.process(&chunk);
                        state.tail.extend(chunk.iter().copied());
                        while state.tail.len() > TAIL_LIMIT {
                            state.tail.pop_front();
                        }
                        // Any output can change either representation.  The
                        // next attachment requesting a snapshot will render
                        // this generation once and repopulate both lazily.
                        state.snapshot_cache = None;
                        state.rich_snapshot_cache = None;
                    }
                    let _ = output.send(chunk);
                }
            })?;
        Ok(pty)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output.subscribe()
    }

    /// Return the current parser generation without rebuilding the screen
    /// rows.  The network server uses this on every output chunk for the
    /// reliable PTY stream; a full snapshot is only needed for reconnects or
    /// the throttled replaceable-state datagram.
    pub fn generation(&self) -> u64 {
        self.state.lock().expect("PTY state poisoned").generation
    }

    pub fn write(&self, data: &[u8]) -> Result<()> {
        let mut writer_guard = self.writer.lock().expect("PTY writer poisoned");
        let writer = writer_guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("PTY writer is closed"))?;
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    /// Write PTY input without blocking a Tokio reactor worker. The permit is
    /// moved into the blocking closure, so dropping the future after a caller
    /// timeout still leaves at most one in-flight writer for this PTY. A later
    /// attachment waits for that same permit instead of creating another
    /// potentially wedged blocking task.
    pub async fn write_async(self: &Arc<Self>, data: Vec<u8>) -> Result<()> {
        let permit = Arc::clone(&self.write_gate)
            .acquire_owned()
            .await
            .context("acquire PTY writer slot")?;
        let pty = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            pty.write(&data)
        })
        .await
        .context("PTY input writer task failed")??;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .lock()
            .expect("PTY master poisoned")
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        let mut state = self.state.lock().expect("PTY state poisoned");
        state.parser.screen_mut().set_size(rows, cols);
        state.snapshot_cache = None;
        state.rich_snapshot_cache = None;
        Ok(())
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut state = self.state.lock().expect("PTY state poisoned");
        if let Some(snapshot) = state.snapshot_cache.as_ref() {
            self.snapshot_cache_counters
                .snapshot_hits
                .fetch_add(1, Ordering::Relaxed);
            return snapshot.clone();
        }
        self.snapshot_cache_counters
            .snapshot_renders
            .fetch_add(1, Ordering::Relaxed);
        let terminal = state.parser.screen();
        let (rows, cols) = terminal.size();
        let (cursor_row, cursor_col) = terminal.cursor_position();
        let snapshot = Snapshot {
            generation: state.generation,
            rows,
            cols,
            screen: terminal.rows(0, cols).collect(),
            cursor_row,
            cursor_col,
            tail: state.tail.iter().copied().collect(),
        };
        state.snapshot_cache = Some(snapshot.clone());
        snapshot
    }

    /// Return the newest bounded page of plain scrollback rows.  The parser's
    /// scrollback cursor is restored before returning, so collecting history
    /// cannot change the live terminal view.  Callers should apply their own
    /// byte budget before putting these rows on a wire; this method bounds the
    /// number of rows and never allocates more than the parser's configured
    /// history window.
    pub fn scrollback(&self, max_lines: usize) -> Vec<String> {
        if max_lines == 0 {
            return Vec::new();
        }
        // tmux owns the durable pane history rather than forwarding it over
        // the attached PTY. Ask that same validated tmux executable for a
        // bounded plain-text page when this PTY came from a tmux session. If
        // the session disappeared or the helper is unavailable, fall back to
        // the parser's own history so a transient tmux failure never breaks
        // a PTY attachment.
        if let Some(tmux) = self.tmux.clone() {
            let rows = {
                let state = self.state.lock().expect("PTY state poisoned");
                state.parser.screen().size().0
            };
            // A newly attached client (or a tmux server recovering after a
            // hard daemon kill) can briefly expose only its current pane or
            // reject the control query while the old client is disappearing.
            // Retry both empty and failed captures inside a short total
            // window so a real history page is not turned into a misleading
            // zero-line snapshot, while an unavailable tmux server still
            // falls back to parser-local history promptly.
            let retry_deadline = Instant::now() + TMUX_SCROLLBACK_CAPTURE_RETRY_WINDOW;
            let mut last_capture = None;
            for attempt in 0..=TMUX_SCROLLBACK_CAPTURE_RETRIES {
                if let Some(lines) = capture_tmux_scrollback(&tmux, rows, max_lines) {
                    if !lines.is_empty() {
                        return lines;
                    }
                    last_capture = Some(lines);
                }
                if attempt == TMUX_SCROLLBACK_CAPTURE_RETRIES || Instant::now() >= retry_deadline {
                    break;
                }
                let remaining = retry_deadline.saturating_duration_since(Instant::now());
                thread::sleep(TMUX_SCROLLBACK_CAPTURE_RETRY_DELAY.min(remaining));
            }
            if let Some(lines) = last_capture {
                return lines;
            }
        }
        self.parser_scrollback(max_lines)
    }

    fn parser_scrollback(&self, max_lines: usize) -> Vec<String> {
        let mut state = self.state.lock().expect("PTY state poisoned");
        let terminal = state.parser.screen_mut();
        let original_offset = terminal.scrollback();
        // Setting the maximum value is a bounded query: vt100 clamps it to
        // the number of retained rows.  The resulting offset tells us how
        // much history exists without exposing parser internals.
        terminal.set_scrollback(usize::MAX);
        let available = terminal.scrollback();
        let requested = available.min(max_lines);
        if requested == 0 {
            terminal.set_scrollback(original_offset);
            return Vec::new();
        }
        let (rows, cols) = terminal.size();
        let page_rows = usize::from(rows).max(1);
        let mut remaining = requested;
        let mut lines = Vec::with_capacity(requested);
        // A larger scrollback offset exposes an older page. Walk from the
        // oldest part of the selected tail toward the newest so the returned
        // lines are in normal terminal order.
        while remaining > 0 {
            let offset = remaining;
            terminal.set_scrollback(offset);
            let take = remaining.min(page_rows);
            lines.extend(terminal.rows(0, cols).take(take));
            remaining -= take;
        }
        terminal.set_scrollback(original_offset);
        lines
    }

    /// Return the current screen with cell attributes preserved in the
    /// terminal's native ANSI representation.  This clones only the bounded
    /// rendered screen while holding the state lock; it does not affect the
    /// exact raw-output broadcast or durable tail.
    pub fn rich_snapshot(&self) -> RichSnapshot {
        let mut state = self.state.lock().expect("PTY state poisoned");
        if let Some(snapshot) = state.rich_snapshot_cache.as_ref() {
            self.snapshot_cache_counters
                .rich_snapshot_hits
                .fetch_add(1, Ordering::Relaxed);
            return snapshot.clone();
        }
        self.snapshot_cache_counters
            .rich_snapshot_renders
            .fetch_add(1, Ordering::Relaxed);
        let terminal = state.parser.screen();
        let (rows, cols) = terminal.size();
        let (cursor_row, cursor_col) = terminal.cursor_position();
        let snapshot = RichSnapshot {
            generation: state.generation,
            rows,
            cols,
            screen: terminal.contents_formatted(),
            cursor_row,
            cursor_col,
        };
        state.rich_snapshot_cache = Some(snapshot.clone());
        snapshot
    }

    /// Return process-local cache counters for health/metrics.  These values
    /// are monotonic for the lifetime of this PTY owner and intentionally do
    /// not affect reconnect or snapshot semantics.
    pub fn snapshot_cache_stats(&self) -> SnapshotCacheStats {
        self.snapshot_cache_counters.snapshot()
    }
}

impl Drop for PersistentPty {
    fn drop(&mut self) {
        // A PTY backed by tmux is an attachment, not the owner of the shell.
        // Do not let portable-pty's writer destructor inject VEOF into the
        // pane while aspd is shutting down. The operating system will close
        // the remaining PTY descriptors as the attachment disappears; tmux's
        // detached server keeps the shell/session alive for reattachment.
        if let Some(tmux) = &self.tmux {
            // Close the tmux client through the server before the PTY master
            // disappears. A raw PTY hangup can otherwise deliver SIGHUP to
            // the attached client while it is the foreground process, and
            // tmux may tear down the last pane before its detached owner can
            // retain the shell. `-s` is safe here because all ASP consumers
            // of one session share this single attachment owner.
            let detach_args = ["detach-client", "-s", tmux.session_name.as_str()];
            let _ = tmux_command_status(tmux, &detach_args);
            if let Ok(mut writer) = self.writer.lock() {
                let _ = writer.take();
            }
        }
    }
}

/// Capture the durable history owned by a tmux pane without sending a command
/// through the user's shell. The output is bounded before it is retained, and
/// the helper is killed if tmux does not answer promptly; a stale or missing
/// session therefore degrades to parser-local history instead of blocking a
/// reconnect indefinitely.
fn capture_tmux_scrollback(tmux: &TmuxContext, rows: u16, max_lines: usize) -> Option<Vec<String>> {
    let start = format!("-{max_lines}");
    let mut command = if let Some(launcher) = &tmux.launcher {
        let mut command = Command::new(&launcher.program);
        command.args(&launcher.args);
        command.arg(&tmux.program);
        command
    } else {
        Command::new(&tmux.program)
    };
    command
        .args(["capture-pane", "-p", "-S", &start, "-t", &tmux.session_name])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure_helper_process_group(&mut command);
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(TMUX_SCROLLBACK_CAPTURE_MAX_BYTES as u64)
            .read_to_end(&mut bytes);
        (result, bytes)
    });

    let deadline = Instant::now() + TMUX_SCROLLBACK_CAPTURE_TIMEOUT;
    while !reader.is_finished() {
        if Instant::now() >= deadline {
            kill_helper_process_group(&mut child);
            let _ = child.wait();
            let _ = reader.join();
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let (read_result, bytes) = reader.join().ok()?;
    if read_result.is_err() || bytes.len() >= TMUX_SCROLLBACK_CAPTURE_MAX_BYTES {
        kill_helper_process_group(&mut child);
        let _ = child.wait();
        return None;
    }
    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }

    let mut lines = String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let visible_rows = usize::from(rows).max(1);
    if lines.len() <= visible_rows {
        return Some(Vec::new());
    }
    let history_end = lines.len() - visible_rows;
    let history_start = history_end.saturating_sub(max_lines);
    Some(lines.drain(history_start..history_end).collect())
}

/// Run a small tmux control command with the same validated executable and
/// optional operator launcher used by the attached PTY. tmux normally answers
/// immediately, but a crashed or blocked supervisor must never stall a QUIC
/// control task indefinitely, so every status probe has a short bound.
fn tmux_command_status(tmux: &TmuxContext, args: &[&str]) -> Option<bool> {
    let mut command = if let Some(launcher) = &tmux.launcher {
        let mut command = Command::new(&launcher.program);
        command.args(&launcher.args);
        command.arg(&tmux.program);
        command
    } else {
        Command::new(&tmux.program)
    };
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_helper_process_group(&mut command);
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + TMUX_SCROLLBACK_CAPTURE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                kill_helper_process_group(&mut child);
                let _ = child.wait();
                return None;
            }
            Err(_) => {
                kill_helper_process_group(&mut child);
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Ensure that the durable tmux session exists independently of the PTY
/// attachment. Creating it detached is important: an attached `new-session
/// -A` client can make the session disappear when the daemon's PTY closes,
/// which would turn a reconnect into a silent fresh shell. The create/recheck
/// sequence tolerates two concurrent first attachments racing to create the
/// same named session.
fn ensure_tmux_session(tmux: &TmuxContext) -> Result<()> {
    let has_session = ["has-session", "-t", tmux.session_name.as_str()];
    if tmux_command_status(tmux, &has_session) == Some(true) {
        return Ok(());
    }

    let create_session = ["new-session", "-d", "-s", tmux.session_name.as_str()];
    if tmux_command_status(tmux, &create_session) == Some(true) {
        return Ok(());
    }

    if tmux_command_status(tmux, &has_session) == Some(true) {
        return Ok(());
    }

    anyhow::bail!(
        "could not create or find durable tmux session {}",
        tmux.session_name
    )
}

/// Resolve the tmux supervisor without assuming that a service manager has
/// populated an interactive user's PATH.  Homebrew's Apple-Silicon prefix is
/// included because launchd intentionally starts with a minimal environment;
/// `ASP_TMUX_PATH` remains available for Nix, package-manager, or custom
/// installations.  Only regular executable files that are not group/world
/// writable are accepted; a writable supervisor would let a workspace-local
/// attacker replace the process that owns durable PTYs.
fn resolve_tmux_program() -> Result<PathBuf> {
    if let Some(configured) = std::env::var_os("ASP_TMUX_PATH") {
        let path = PathBuf::from(configured);
        if !path.is_absolute() {
            anyhow::bail!("ASP_TMUX_PATH must be an absolute path");
        }
        if let Some(path) = resolve_executable_path(&path) {
            return Ok(path);
        }
        anyhow::bail!(
            "ASP_TMUX_PATH does not name a regular executable: {}",
            path.display()
        );
    }

    // Keep deterministic system paths ahead of PATH entries.  This makes the
    // launchd/systemd templates work even when PATH is absent or minimal.
    let standard = [
        "/usr/bin/tmux",
        "/bin/tmux",
        "/usr/local/bin/tmux",
        "/opt/homebrew/bin/tmux",
    ];
    for candidate in standard {
        let path = Path::new(candidate);
        if let Some(path) = resolve_executable_path(path) {
            return Ok(path);
        }
    }

    // Preserve support for user-managed installations after the known system
    // locations have been checked.  The daemon's environment is operator
    // controlled; callers that need a stricter trust policy should set the
    // absolute ASP_TMUX_PATH explicitly.
    if let Some(path_value) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path_value) {
            // An empty PATH component means the current working directory;
            // never resolve a supervisor executable from there.
            if directory.as_os_str().is_empty() {
                continue;
            }
            let path = directory.join("tmux");
            if let Some(path) = resolve_executable_path(&path) {
                return Ok(path);
            }
        }
    }

    anyhow::bail!(
        "tmux executable was not found; install tmux or set ASP_TMUX_PATH to an absolute path"
    )
}

/// Validate the durable PTY supervisor without starting a shell. Production
/// preflight uses this to fail before opening a listener when `tmux` is
/// missing; a later `PTY_OPEN` should not be the first indication that the
/// deployment cannot provide persistent terminals.
pub fn ensure_tmux_available() -> Result<()> {
    resolve_tmux_program().map(|_| ())
}

/// Read the existing tmux pane geometry without attaching a new client.
///
/// A daemon restart must not reset a durable shell to the recovery fallback
/// size before the real client has a chance to reconnect.  The query is
/// intentionally bounded and best-effort: a missing/stalled tmux server
/// returns `None`, allowing the caller to use a conservative default and
/// report the authoritative geometry on the next attachment.
pub fn tmux_session_size(
    session_name: &str,
    launcher: Option<&CommandLauncher>,
) -> Result<Option<(u16, u16)>> {
    let tmux = resolve_tmux_program()?;
    let mut command = if let Some(launcher) = launcher {
        let mut command = Command::new(&launcher.program);
        command.args(&launcher.args);
        command.arg(&tmux);
        command
    } else {
        Command::new(&tmux)
    };
    command
        .args([
            "display-message",
            "-p",
            "-t",
            session_name,
            "#{pane_height} #{pane_width}",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure_helper_process_group(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("query tmux geometry for {session_name}"))?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(anyhow::anyhow!("capture tmux geometry output"));
    };
    let reader = thread::spawn(move || {
        let mut bytes = Vec::with_capacity(TMUX_SIZE_QUERY_MAX_BYTES);
        let result = stdout
            .take(TMUX_SIZE_QUERY_MAX_BYTES as u64)
            .read_to_end(&mut bytes);
        (result, bytes)
    });

    let deadline = Instant::now() + TMUX_SCROLLBACK_CAPTURE_TIMEOUT;
    while !reader.is_finished() {
        if Instant::now() >= deadline {
            kill_helper_process_group(&mut child);
            let _ = child.wait();
            let _ = reader.join();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(5));
    }
    // The bounded reader can finish as soon as it reaches its byte cap. Keep
    // the child wait deadline independent so a query that writes too much or
    // never exits cannot leak a process during daemon startup.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(None) | Err(_) => {
                kill_helper_process_group(&mut child);
                let _ = child.wait();
                break None;
            }
        }
    };
    let (read_result, bytes) = reader
        .join()
        .map_err(|_| anyhow::anyhow!("tmux geometry reader thread panicked"))?;
    let Some(status) = status else {
        return Ok(None);
    };
    if !status.success() || read_result.is_err() || bytes.len() >= TMUX_SIZE_QUERY_MAX_BYTES {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut fields = text.split_whitespace();
    let rows = fields.next().and_then(|value| value.parse::<u16>().ok());
    let cols = fields.next().and_then(|value| value.parse::<u16>().ok());
    match (rows, cols) {
        (Some(rows), Some(cols)) if (1..=500).contains(&rows) && (1..=500).contains(&cols) => {
            Ok(Some((rows, cols)))
        }
        _ => Ok(None),
    }
}

/// Put short-lived tmux helper commands in a private process group.  The
/// stdout reader is deliberately blocking on a dedicated thread; killing only
/// the direct child is insufficient when a wrapper or descendant inherited
/// the pipe, because the timeout path would then block forever waiting for
/// that reader to observe EOF.  The operator launcher contract requires an
/// `exec`, but the group boundary keeps the helper safe even when a wrapper
/// violates it.
fn configure_helper_process_group(_command: &mut Command) {
    #[cfg(unix)]
    {
        // `process_group(0)` asks the child to become the leader of a fresh
        // group whose ID is its PID.  It is applied before spawn and does not
        // alter the long-lived tmux server's own group.
        _command.process_group(0);
    }
}

fn kill_helper_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        // A live Child handle proves that this PID has not been recycled. If
        // the direct child already exited, retain the safe handle-based kill
        // below rather than signalling a process group that could have been
        // reused; a compliant launcher keeps descendants in this group until
        // the direct child is reaped.
        let child_is_live = matches!(child.try_wait(), Ok(None));
        if pid > 0 && child_is_live {
            // The child was placed in a private group by
            // `configure_helper_process_group`; a negative PID targets that
            // group.  Keep the direct kill below as a fallback for platforms
            // where the group signal races process exit.
            let _ = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
        }
    }
    let _ = child.kill();
}

/// Resolve an executable through package-manager shims such as Homebrew's
/// `/opt/homebrew/bin/tmux` symlink, then return the canonical target. The
/// server validates the operator process launcher separately; canonicalizing
/// here avoids rejecting a normal managed installation while ensuring the
/// spawned PTY command does not depend on a mutable symlink path.
fn resolve_executable_path(path: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    let metadata = std::fs::metadata(&canonical).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        (mode & 0o111 != 0 && mode & 0o022 == 0).then_some(canonical)
    }
    #[cfg(not(unix))]
    {
        Some(canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(unix)]
    #[test]
    fn executable_resolver_rejects_group_or_world_writable_files() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!("asp-pty-resolver-{}", std::process::id()));
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(resolve_executable_path(&path).is_none());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let canonical = path.canonicalize().unwrap();
        assert_eq!(resolve_executable_path(&path), Some(canonical));
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn helper_process_group_kill_closes_inherited_capture_pipe() {
        use std::sync::mpsc;

        // Keep a descendant holding stdout open after the shell itself would
        // otherwise exit.  Killing only the direct child would leave the
        // reader blocked; the private process-group boundary must close the
        // inherited pipe promptly.
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 60 & wait"])
            .stdout(Stdio::piped());
        configure_helper_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout.take(1024).read_to_end(&mut bytes);
            let _ = done_tx.send(());
        });
        thread::sleep(Duration::from_millis(40));
        kill_helper_process_group(&mut child);
        let _ = child.wait();
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).is_ok());
        reader.join().unwrap();
    }

    #[tokio::test]
    async fn shell_output_reaches_persistent_snapshot() {
        let pty = PersistentPty::spawn(24, 80).unwrap();
        pty.write(b"printf 'asp-pty-marker\\n'\n").unwrap();
        for _ in 0..40 {
            let snapshot = pty.snapshot();
            if String::from_utf8_lossy(&snapshot.tail).contains("asp-pty-marker") {
                assert!(snapshot.generation > 0);
                assert!(
                    snapshot
                        .screen
                        .iter()
                        .any(|row| row.contains("asp-pty-marker"))
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("PTY marker did not appear in snapshot");
    }

    #[tokio::test]
    async fn async_write_reaches_persistent_snapshot() {
        let pty = PersistentPty::spawn(24, 80).unwrap();
        pty.write_async(b"printf 'asp-pty-async-marker\\n'\n".to_vec())
            .await
            .unwrap();
        for _ in 0..40 {
            let snapshot = pty.snapshot();
            if String::from_utf8_lossy(&snapshot.tail).contains("asp-pty-async-marker") {
                assert!(snapshot.generation > 0);
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("async PTY marker did not appear in snapshot");
    }

    #[tokio::test]
    async fn bounded_scrollback_returns_newest_history_without_changing_screen() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env("TERM", "xterm-256color");
        let pty = PersistentPty::spawn_command(4, 40, command, 0).unwrap();
        pty.write(
            b"i=0; while [ $i -lt 12 ]; do printf 'asp-history-%s\\n' \"$i\"; i=$((i+1)); done\n",
        )
        .unwrap();
        for _ in 0..80 {
            let snapshot = pty.snapshot();
            if String::from_utf8_lossy(&snapshot.tail).contains("asp-history-11") {
                let before = snapshot.screen.clone();
                let history = pty.scrollback(3);
                assert!(history.len() <= 3);
                assert!(history.iter().any(|line| line.contains("asp-history-6")));
                assert!(history.iter().any(|line| line.contains("asp-history-8")));
                assert_eq!(pty.snapshot().screen, before);
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("PTY history markers did not appear in snapshot");
    }

    #[tokio::test]
    async fn rich_snapshot_preserves_cell_attributes() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env("TERM", "xterm-256color");
        let pty = PersistentPty::spawn_command(4, 20, command, 0).unwrap();
        // Encode the marker as octal escapes so it is not present in the
        // shell's echoed command line. Otherwise the test can observe the
        // uncoloured source text before the parser has received the command's
        // coloured output, making the attribute assertion timing-dependent.
        pty.write(b"printf '\\033[31m\\122\\105\\104\\033[0m\\n'\n")
            .unwrap();
        for _ in 0..40 {
            let snapshot = pty.rich_snapshot();
            if snapshot.screen.windows(3).any(|window| window == b"RED") {
                assert!(
                    snapshot
                        .screen
                        .windows(5)
                        .any(|window| window == b"\x1b[31m")
                );
                let has_reset = snapshot.screen.windows(3).any(|window| window == b"\x1b[m")
                    || snapshot
                        .screen
                        .windows(4)
                        .any(|window| window == b"\x1b[0m");
                assert!(has_reset);
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("rich PTY marker did not appear in snapshot");
    }

    #[test]
    fn snapshot_cache_is_invalidated_by_resize() {
        let pty = PersistentPty::spawn(24, 80).unwrap();
        // Populate both lazy representations before changing the terminal
        // dimensions. A stale cache here would keep advertising the old
        // geometry to a newly attached client until the next output chunk.
        let _ = pty.snapshot();
        let _ = pty.rich_snapshot();
        pty.resize(12, 40).unwrap();
        let plain = pty.snapshot();
        let rich = pty.rich_snapshot();
        assert_eq!((plain.rows, plain.cols), (12, 40));
        assert_eq!((rich.rows, rich.cols), (12, 40));
    }

    #[test]
    fn snapshot_cache_stats_distinguish_hits_and_renders() {
        let pty = PersistentPty::spawn(24, 80).unwrap();
        assert_eq!(pty.snapshot_cache_stats(), SnapshotCacheStats::default());

        let _ = pty.snapshot();
        let _ = pty.snapshot();
        let _ = pty.rich_snapshot();
        let _ = pty.rich_snapshot();

        assert_eq!(
            pty.snapshot_cache_stats(),
            SnapshotCacheStats {
                snapshot_hits: 1,
                snapshot_renders: 1,
                rich_snapshot_hits: 1,
                rich_snapshot_renders: 1,
            }
        );

        pty.resize(12, 40).unwrap();
        let _ = pty.snapshot();
        let _ = pty.rich_snapshot();
        assert_eq!(
            pty.snapshot_cache_stats(),
            SnapshotCacheStats {
                snapshot_hits: 1,
                snapshot_renders: 2,
                rich_snapshot_hits: 1,
                rich_snapshot_renders: 2,
            }
        );
    }

    #[test]
    fn tmux_session_size_reads_existing_window_geometry() {
        let Ok(tmux) = resolve_tmux_program() else {
            eprintln!("tmux not installed; skipping tmux geometry test");
            return;
        };
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos()
        );
        let session_name = format!("asp-size-test-{suffix}");
        let status = Command::new(&tmux)
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "120",
                "-y",
                "40",
            ])
            .status()
            .expect("start tmux geometry test session");
        assert!(
            status.success(),
            "tmux failed to create geometry test session"
        );

        let size = tmux_session_size(&session_name, None).unwrap();
        let _ = Command::new(&tmux)
            .args(["kill-session", "-t", &session_name])
            .status();
        assert_eq!(size, Some((40, 120)));
    }

    #[tokio::test]
    async fn tmux_output_reaches_persistent_snapshot_when_available() {
        let Ok(tmux) = resolve_tmux_program() else {
            // Minimal CI images may intentionally omit tmux; the server will
            // report a clear PTY_OPEN error there, while deployments that
            // advertise durable PTYs should exercise this integration test.
            eprintln!("tmux not installed; skipping tmux PTY integration test");
            return;
        };
        let session_name = format!(
            "asp-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos()
        );
        let pty = PersistentPty::spawn_tmux_with_generation(24, 80, &session_name, 0).unwrap();
        pty.write(
            b"printf 'asp-tmux-marker\\n'; i=0; while [ $i -lt 40 ]; do printf 'asp-tmux-filler-%s\\n' \"$i\"; i=$((i+1)); done\n",
        )
        .unwrap();
        for _ in 0..40 {
            let snapshot = pty.snapshot();
            if String::from_utf8_lossy(&snapshot.tail).contains("asp-tmux-filler-39") {
                drop(pty);
                // Dropping an attachment must not destroy the durable tmux
                // session. Open a fresh attachment and verify that the pane's
                // history is still present before cleaning it up.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let reattached =
                    PersistentPty::spawn_tmux_with_generation(24, 80, &session_name, 0).unwrap();
                let mut resumed = false;
                for _ in 0..40 {
                    if reattached
                        .scrollback(256)
                        .iter()
                        .any(|line| line.contains("asp-tmux-marker"))
                    {
                        resumed = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                drop(reattached);
                let _ = std::process::Command::new(&tmux)
                    .args(["kill-session", "-t", &session_name])
                    .status();
                assert!(resumed, "tmux marker was not present after reattach");
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(pty);
        let _ = std::process::Command::new(&tmux)
            .args(["kill-session", "-t", &session_name])
            .status();
        panic!("tmux PTY marker did not appear in snapshot");
    }

    #[tokio::test]
    async fn tmux_scrollback_capture_returns_pane_history_when_available() {
        let Ok(tmux) = resolve_tmux_program() else {
            eprintln!("tmux not installed; skipping tmux scrollback integration test");
            return;
        };
        let session_name = format!(
            "asp-scrollback-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos()
        );
        let pty = PersistentPty::spawn_tmux_with_generation(4, 40, &session_name, 0).unwrap();
        pty.write(
            b"i=0; printf 'asp-tmux-history-marker\\n'; while [ $i -lt 40 ]; do printf 'asp-tmux-filler-%s\\n' \"$i\"; i=$((i+1)); done\n",
        )
        .unwrap();
        let mut output_ready = false;
        for _ in 0..80 {
            if String::from_utf8_lossy(&pty.snapshot().tail).contains("asp-tmux-filler-39") {
                output_ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let history = if output_ready {
            pty.scrollback(256)
        } else {
            Vec::new()
        };
        drop(pty);
        let _ = std::process::Command::new(&tmux)
            .args(["kill-session", "-t", &session_name])
            .status();
        assert!(output_ready, "tmux history command did not finish");
        assert!(
            history
                .iter()
                .any(|line| line.contains("asp-tmux-history-marker")),
            "tmux pane history did not contain the marker: {history:?}"
        );
    }

    #[tokio::test]
    async fn tmux_can_run_through_operator_launcher() {
        let Ok(tmux) = resolve_tmux_program() else {
            eprintln!("tmux not installed; skipping launcher PTY integration test");
            return;
        };
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos()
        );
        let launcher_path = std::env::temp_dir().join(format!("asp-pty-launcher-{suffix}.sh"));
        let log_path = std::env::temp_dir().join(format!("asp-pty-launcher-{suffix}.log"));
        std::fs::write(
            &launcher_path,
            b"#!/bin/sh\nset -eu\nlog=$1\nshift\nprintf '%s\\n' \"$1\" >\"$log\"\nexec \"$@\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&launcher_path, std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let launcher = CommandLauncher {
            program: launcher_path.clone(),
            args: vec![log_path.to_string_lossy().into_owned()],
        };
        let session_name = format!("asp-launcher-test-{suffix}");
        let pty = PersistentPty::spawn_tmux_with_generation_and_launcher(
            24,
            80,
            &session_name,
            0,
            Some(&launcher),
        )
        .unwrap();
        for _ in 0..40 {
            if std::fs::read_to_string(&log_path)
                .map(|contents| !contents.trim().is_empty())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Avoid matching the marker in the terminal's local input echo: the
        // shell constructs the output from two fragments instead.
        pty.write(b"printf 'asp-tmux-'$(printf 'launcher-marker')'\\n'\n")
            .unwrap();
        let mut marker_seen = false;
        for _ in 0..40 {
            let snapshot = pty.snapshot();
            if String::from_utf8_lossy(&snapshot.tail).contains("asp-tmux-launcher-marker") {
                marker_seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(pty);
        let _ = std::process::Command::new(&tmux)
            .args(["kill-session", "-t", &session_name])
            .status();
        let logged = std::fs::read_to_string(&log_path).unwrap_or_default();
        let _ = std::fs::remove_file(&launcher_path);
        let _ = std::fs::remove_file(&log_path);
        assert!(marker_seen, "tmux marker did not appear through launcher");
        assert_eq!(logged.trim(), tmux.to_string_lossy());
    }
}
