//! A single managed CLI session: a child process under a PTY, plus an in-process
//! VT emulator capturing its rendered screen. This is the proven core from the
//! ConPTY spike, generalized. No tmux — the PTY lives in this process.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize, PtySystem, SlavePty};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::jsonl::{cc_project_dir, JsonlWatcher};
use crate::parser::SessionState;

pub struct Session {
    pub name: String,
    pub state: SessionState,
    /// Last full capture, for naive chunk-diffing.
    pub last: String,
    /// Assistant-message count captured at send time, for completion detection.
    pub baseline_msgs: usize,
    /// True between a send and its completion.
    pub observing: bool,
    /// Wall-clock of the last screen change and of turn start — the quiescence
    /// fallback measures from a clock that is reset at each turn start.
    pub last_change: Instant,
    pub turn_started: Instant,
    /// The text of the in-flight user message (needle for extraction).
    pub sent: String,
    /// Set when a message has been typed/pasted into the prompt and is awaiting
    /// submission. The poll loop presses Enter while the prompt stays idle and
    /// clears this once the turn starts — robust against the paste/Enter race
    /// under ConPTY (a single timed Enter landed nondeterministically).
    pub pending_submit: Option<Instant>,
    /// Wall-clock of the last submit-Enter attempt (to space out retries).
    pub last_submit_try: Option<Instant>,
    /// Last TUI-scraped response text emitted as a streaming `replace` (dedup).
    pub last_streamed: String,
    /// The previous turn's final scraped response, captured when `last_streamed`
    /// is wiped at `send`. The first post-send scrape often still shows the prior
    /// response (the TUI hasn't redrawn yet), and with `last_streamed` cleared it
    /// would pass the dedup and stream as a stale "duplicate-of-last-message"
    /// opening frame. We suppress emitting any `replace` equal to this until real
    /// new content diverges. Kept separate from `last_streamed` so the think-gap
    /// shimmer branch (which keys on `last_streamed.is_empty()`) still fires.
    pub prev_final: String,
    /// Last spinner line relayed as a `thinking` event during the think-gap (dedup —
    /// a ticking timer re-emits, but identical frames don't). Cleared each turn.
    pub last_thinking: Option<String>,
    /// The CLI model this session launched with (from --model), for model-switch logic.
    pub model: Option<String>,
    /// Watches Claude Code's JSONL transcript for clean content + tool events.
    pub jsonl: Option<JsonlWatcher>,
    writer: Box<dyn Write + Send>,
    /// Set when the CLI requests win32-input-mode (`ESC[?9001h`) — on Windows the
    /// interactive Claude TUI negotiates this and ignores legacy VT keystrokes, so
    /// we must encode input as win32 input records. Cleared on `ESC[?9001l`.
    win32: Arc<AtomicBool>,
    screen: Arc<Mutex<vt100::Parser>>,
    /// Live tee of the raw PTY output, for terminal-attach clients (the escape hatch).
    output_tx: broadcast::Sender<Vec<u8>>,
    // Kept alive for the lifetime of the session; dropping closes the PTY.
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

/// One win32-input-mode key event (down + up): `ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`.
fn w32_record(vk: u16, sc: u16, uc: u32, cs: u32) -> String {
    format!(
        "\x1b[{vk};{sc};{uc};1;{cs};1_\x1b[{vk};{sc};{uc};0;{cs};1_",
        vk = vk, sc = sc, uc = uc, cs = cs
    )
}

/// Map a named key to a win32-input-mode record. ENHANCED_KEY (0x0100) flags the
/// arrow keys. Returns None for keys we don't special-case.
fn key_w32(key: &str) -> Option<String> {
    const ENH: u32 = 0x0100;
    let (vk, sc, uc, cs) = match key {
        "Enter" => (0x0D, 0x1C, 0x0D, 0),
        "Down" => (0x28, 0x50, 0, ENH),
        "Up" => (0x26, 0x48, 0, ENH),
        "Left" => (0x25, 0x4B, 0, ENH),
        "Right" => (0x27, 0x4D, 0, ENH),
        "Esc" => (0x1B, 0x01, 0x1B, 0),
        "Tab" => (0x09, 0x0F, 0x09, 0),
        "Space" => (0x20, 0x39, 0x20, 0),
        _ => return None,
    };
    Some(w32_record(vk, sc, uc, cs))
}

impl Session {
    pub fn win32_active(&self) -> bool {
        self.win32.load(Ordering::Relaxed)
    }

    pub fn spawn(
        name: String,
        exe: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &HashMap<String, String>,
        rows: u16,
        cols: u16,
    ) -> Result<Session> {
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        // IMPORTANT: spawn the real executable directly. On Windows that means
        // claude.exe, NOT the npm .cmd shim via cmd.exe (which breaks TTY detection
        // and renders nothing — learned the hard way in the spike).
        let mut cmd = CommandBuilder::new(exe);
        for a in args {
            cmd.arg(a);
        }
        if let Some(d) = cwd {
            cmd.cwd(d);
        }
        // Strip Claude Code's nested-invocation markers. If lit-bridge-rs is itself
        // launched from within a Claude session, the spawned `claude` would inherit
        // CLAUDECODE=1 and treat itself as a child — and a child invocation writes NO
        // session transcript, which silently breaks the JSONL clean-content path.
        // Clearing these makes the spawned session a top-level one that persists.
        for k in [
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_SSE_PORT",
        ] {
            cmd.env_remove(k);
        }
        // Advertise a real color terminal. tmux did this for us implicitly (its
        // default-terminal + 256-color advertisement inside the pane); under a raw PTY
        // the child inherits the daemon's env, which as a service is often TERM=dumb or
        // unset — so Claude downgrades to plain ASCII with no color or unicode spinner
        // animations. Set sane defaults BEFORE the caller's env so `create.env` can override.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd)?;

        // Feed PTY output into the VT emulator on a blocking reader thread.
        let screen = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(512);
        let mut reader = pair.master.try_clone_reader()?;
        let win32 = Arc::new(AtomicBool::new(false));
        {
            let s = screen.clone();
            let tee = output_tx.clone();
            let win32_r = win32.clone();
            // Diagnostic: dump raw PTY output to a file when LIT_BRIDGE_RS_RAWLOG is set.
            // Inert in production; used to inspect the CLI's terminal-capability handshake.
            let raw_log = std::env::var("LIT_BRIDGE_RS_RAWLOG").ok();
            thread::spawn(move || {
                use std::io::Write as _;
                let mut log = raw_log.as_ref().and_then(|p| {
                    std::fs::OpenOptions::new().create(true).append(true).open(p).ok()
                });
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            // Track the CLI's win32-input-mode negotiation so input is
                            // encoded correctly (Windows interactive TUI).
                            if chunk.windows(8).any(|w| w == b"\x1b[?9001h") {
                                win32_r.store(true, Ordering::Relaxed);
                            }
                            if chunk.windows(8).any(|w| w == b"\x1b[?9001l") {
                                win32_r.store(false, Ordering::Relaxed);
                            }
                            if let Some(f) = log.as_mut() {
                                let _ = f.write_all(chunk);
                                let _ = f.flush();
                            }
                            if let Ok(mut g) = s.lock() {
                                g.process(chunk);
                            }
                            let _ = tee.send(chunk.to_vec()); // feed terminal-attach clients
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // Watch the JSONL transcript when we know the working dir (clean content).
        // Resolve CLAUDE_CONFIG_DIR exactly as Claude will: the create env overrides,
        // else the inherited process env (the child inherits it via CommandBuilder).
        let config_dir = env
            .get("CLAUDE_CONFIG_DIR")
            .cloned()
            .or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok());
        let jsonl = cwd.map(|wd| JsonlWatcher::new(cc_project_dir(wd, config_dir.as_deref())));

        let writer = pair.master.take_writer()?;
        Ok(Session {
            name,
            state: SessionState::Starting,
            last: String::new(),
            baseline_msgs: 0,
            observing: false,
            last_change: Instant::now(),
            turn_started: Instant::now(),
            sent: String::new(),
            pending_submit: None,
            last_submit_try: None,
            last_streamed: String::new(),
            prev_final: String::new(),
            last_thinking: None,
            model: None,
            jsonl,
            writer,
            win32,
            screen,
            output_tx,
            _master: pair.master,
            child,
        })
    }

    /// The Claude Code session id (the transcript filename stem) — for `--resume`.
    pub fn session_id(&self) -> Option<String> {
        self.jsonl.as_ref().and_then(|j| j.get_session_id())
    }

    /// Is the child CLI process still running?
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Terminate the child CLI process AND reap it. `kill()` alone only sends the
    /// signal; without a following `wait()` the exited child lingers as a `<defunct>`
    /// zombie until the bridge itself dies — the leak that accumulated dozens of dead
    /// claude procs per user. `wait()` reaps it immediately (SIGKILL exits fast).
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Subscribe to the live raw PTY output stream (for a terminal-attach client).
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    /// Write raw bytes straight to the PTY (terminal-attach keystrokes / slash commands).
    pub fn write_raw(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// The rendered visible screen as plain text — the `tmux capture-pane -p`
    /// analogue. Used by the parser/observer (which wants un-styled text).
    pub fn capture(&self) -> String {
        self.screen
            .lock()
            .map(|g| g.screen().contents())
            .unwrap_or_default()
    }

    /// Like [`capture`], but also returns the per-row classification (prose / verbatim /
    /// blank) from the SAME screen lock, so `row_kinds[i]` stays aligned with screen line
    /// `i` for the reflow path. Only called when reflow is enabled (the per-row cell scan
    /// isn't free), so plain `capture` stays the hot path.
    pub fn capture_with_kinds(&self) -> (String, Vec<crate::reflow::RowKind>) {
        self.screen
            .lock()
            .map(|g| {
                let screen = g.screen();
                let (rows, cols) = screen.size();
                let text = screen.contents();
                let kinds = (0..rows)
                    .map(|r| crate::reflow::classify_row(screen, r, cols))
                    .collect();
                (text, kinds)
            })
            .unwrap_or_default()
    }

    /// The rendered screen WITH styling (SGR color attributes + cursor positioning),
    /// as a terminal byte stream. Used to paint a freshly-attached terminal client so
    /// it shows color immediately instead of waiting for the CLI's next full redraw.
    pub fn capture_formatted(&self) -> Vec<u8> {
        self.screen
            .lock()
            .map(|g| g.screen().contents_formatted())
            .unwrap_or_default()
    }

    /// Write the message text (no submit). Submitting is a separate step so the
    /// trailing Enter doesn't race Claude's ingestion of the paste — a `\r` sent
    /// too eagerly on a longer message gets absorbed and the message is left
    /// unsubmitted in the input box. Mirrors tmux paste-buffer then send-keys Enter.
    pub fn send_text(&mut self, text: &str) -> Result<()> {
        if self.win32_active() {
            // Multi-line input via per-key records is unreliable under ConPTY:
            // a bare Enter submits, and Ctrl+J / Shift+Enter are racy (sometimes
            // insert, sometimes submit-per-line) — so a multi-line message gets
            // fragmented and the tail is left unsubmitted in the input box.
            //
            // Use BRACKETED PASTE instead. Claude enables it at startup
            // (ESC[?2004h); content wrapped in ESC[200~ … ESC[201~ is treated as
            // literal pasted text — newlines inserted, nothing submitted. Paste
            // is delivered as raw bracketed sequences even under win32-input-mode
            // (the mode changes keyboard-key encoding, not paste), so the raw
            // bytes are honored.
            //
            // Submit in the SAME atomic write (paste-end marker immediately
            // followed by an Enter key record), mirroring a real terminal's
            // paste-then-Enter. A separately-sent Enter (delayed, or from the poll
            // loop) raced the paste and didn't submit reliably under ConPTY.
            let mut out: Vec<u8> = Vec::new();
            out.extend_from_slice(b"\x1b[200~");
            for ch in text.chars() {
                if ch == '\r' {
                    continue; // normalize CRLF -> LF
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            out.extend_from_slice(b"\x1b[201~");
            out.extend_from_slice(key_w32("Enter").unwrap().as_bytes());
            self.writer.write_all(&out)?;
            self.writer.flush()?;
            return Ok(());
        }
        self.writer.write_all(text.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    /// Submit the current input line.
    pub fn send_enter(&mut self) -> Result<()> {
        if self.win32_active() {
            self.writer.write_all(key_w32("Enter").unwrap().as_bytes())?;
            self.writer.flush()?;
            return Ok(());
        }
        self.writer.write_all(b"\r")?;
        self.writer.flush()?;
        Ok(())
    }

    /// Mark the start of a new turn: reset the quiescence clocks and position the
    /// JSONL watcher at the transcript's EOF.
    pub fn begin_turn(&mut self) {
        let now = Instant::now();
        self.turn_started = now;
        self.last_change = now;
        self.last_thinking = None;
        if let Some(j) = &mut self.jsonl {
            j.begin_turn();
        }
    }

    /// True while the JSONL transcript shows the turn still open (authoritative).
    pub fn jsonl_turn_open(&self) -> bool {
        self.jsonl.as_ref().map(|j| j.turn_open()).unwrap_or(false)
    }

    /// Poll the JSONL transcript for new tool/text/completion events (clean content).
    pub fn poll_jsonl(&mut self) -> Vec<Value> {
        self.jsonl.as_mut().map(|j| j.poll()).unwrap_or_default()
    }

    /// Re-anchor the JSONL watcher to the transcript tail after a turn completes, so the
    /// organic (un-observed) path never re-reads the just-finished turn. No-op if the
    /// session has no watcher.
    pub fn prime_jsonl_to_eof(&mut self) {
        if let Some(j) = &mut self.jsonl {
            j.prime_to_eof();
        }
    }

    /// Send a named key (for dialogs / navigation).
    pub fn send_key(&mut self, key: &str) -> Result<()> {
        if self.win32_active() {
            if let Some(rec) = key_w32(key) {
                self.writer.write_all(rec.as_bytes())?;
                self.writer.flush()?;
                return Ok(());
            }
            // Not a named key. If it's a single printable char (e.g. a digit for
            // dialog quick-select), encode it as a win32 text record — the CLI
            // ignores raw bytes in win32-input-mode, so this is the only way it
            // registers under a headless ConPTY.
            if key.chars().count() == 1 {
                let ch = key.chars().next().unwrap();
                let rec = w32_record(0, 0, ch as u32, 0);
                self.writer.write_all(rec.as_bytes())?;
                self.writer.flush()?;
                return Ok(());
            }
        }
        let seq = match key {
            "Enter" => "\r",
            "Down" => "\x1b[B",
            "Up" => "\x1b[A",
            "Left" => "\x1b[D",
            "Right" => "\x1b[C",
            "Esc" => "\x1b",
            "Tab" => "\t",
            "Space" => " ",
            other => other,
        };
        self.writer.write_all(seq.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }
}
