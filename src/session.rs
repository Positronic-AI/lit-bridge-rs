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
    /// A message held back because a modal dialog was on screen at dispatch time
    /// (e.g. the resume-into-full-context compact question). The poll loop pastes
    /// and submits it when the prompt returns to idle — the message is never
    /// typed into a dialog and never dropped.
    pub pending_text: Option<String>,
    /// Re-paste attempts for the current submit (the paste itself was
    /// swallowed — prompt still empty well after the write). Bounded.
    pub repastes: u8,
    /// Set once the CLI enables bracketed paste (ESC[?2004h) — the authoritative
    /// "input layer is live" beacon. Sending before this races the TUI's boot:
    /// the paste markers get eaten as literal text and the CLI receives a
    /// fragment (the 2026-07-30 "your message just says '2'" — the 2 of 200~).
    /// cmd_send holds messages until this is set; the poll loop delivers them.
    pub paste_ready: Arc<AtomicBool>,
    /// Spawn wall-clock — the paste gate's fallback ceiling measures from here.
    pub spawned_at: Instant,
    /// When the blocking dialog was first seen (for the held-too-long error).
    pub dialog_since: Option<Instant>,
    /// One held-too-long error per blockage.
    pub dialog_notified: bool,
    /// True once a structured `question` event was emitted for the current
    /// dialog (dedup; cleared when the dialog clears).
    pub question_emitted: bool,
    /// Set when an `answer` digit was keyed into a dialog: if the dialog is
    /// still up after a beat, the poll loop follows with Enter to confirm
    /// (pickers differ on whether a digit selects or select-and-confirms).
    pub pending_answer_enter: Option<Instant>,
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
    /// Total bytes the CLI has written to the PTY. "Did it repaint since I sent
    /// that keystroke?" is answered from THIS, never from the screen model —
    /// on Windows the model can sit blank while the CLI is alive and talking.
    out_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// `out_bytes` at the moment of our last send (paste / Enter / recovery).
    pub sent_at_bytes: u64,
    screen: Arc<Mutex<vt100::Parser>>,
    /// Live tee of the raw PTY output, for terminal-attach clients (the escape hatch).
    output_tx: broadcast::Sender<Vec<u8>>,
    // Kept alive for the lifetime of the session; dropping closes the PTY.
    _master: Box<dyn MasterPty + Send>,
    /// Enter presses issued for the current submit (retry loop).
    pub submit_tries: u8,
    /// When the CLI stopped reacting to our input (no output since a send).
    pub frozen_since: Option<Instant>,
    /// Echo-probe state for the recovery ladder: which dialect we last typed
    /// the probe character in, and when. `None` = no probe outstanding.
    pub echo_probe: Option<(bool, Instant)>,
    /// Dialect proven by echo during this submit (true = win32 records).
    pub proven_w32: Option<bool>,
    /// Probe characters typed so far this submit (each must be backspaced
    /// away before anything is submitted — they queue up during a freeze).
    pub probe_chars: u8,
    /// Last time a probe round started (re-probe cadence while frozen).
    pub last_probe_round: Option<Instant>,
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
        "Backspace" => (0x08, 0x0E, 0x08, 0),
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
    /// Bytes the CLI has emitted so far (see `out_bytes`).
    pub fn output_bytes(&self) -> u64 {
        self.out_bytes.load(Ordering::Relaxed)
    }

    /// Stamp "we just sent input" so the next tick can ask whether the CLI
    /// reacted (emitted anything) since.
    pub fn mark_sent(&mut self) {
        self.sent_at_bytes = self.output_bytes();
    }

    /// True if the CLI has written anything since the last `mark_sent`.
    pub fn reacted_since_send(&self) -> bool {
        self.output_bytes() != self.sent_at_bytes
    }

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
        let paste_ready = Arc::new(AtomicBool::new(false));
        let out_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
        {
            let s = screen.clone();
            let tee = output_tx.clone();
            let win32_r = win32.clone();
            let paste_r = paste_ready.clone();
            let out_r = out_bytes.clone();
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
                            out_r.fetch_add(n as u64, Ordering::Relaxed);
                            // Track the CLI's win32-input-mode negotiation so input is
                            // encoded correctly (Windows interactive TUI).
                            if chunk.windows(8).any(|w| w == b"\x1b[?9001h") {
                                win32_r.store(true, Ordering::Relaxed);
                            }
                            if chunk.windows(8).any(|w| w == b"\x1b[?9001l") {
                                win32_r.store(false, Ordering::Relaxed);
                            }
                            // Bracketed-paste enable = the CLI's input layer is live.
                            // (Chunk-boundary splits are possible but rare; the poll
                            // loop's age fallback covers a missed sighting.)
                            if chunk.windows(8).any(|w| w == b"\x1b[?2004h") {
                                paste_r.store(true, Ordering::Relaxed);
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
            pending_text: None,
            repastes: 0,
            paste_ready,
            spawned_at: Instant::now(),
            dialog_since: None,
            dialog_notified: false,
            question_emitted: false,
            pending_answer_enter: None,
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
            out_bytes,
            sent_at_bytes: 0,
            submit_tries: 0,
            frozen_since: None,
            echo_probe: None,
            proven_w32: None,
            probe_chars: 0,
            last_probe_round: None,
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
    /// Type one character in the given dialect (no Enter). Used by the echo
    /// probe: a live CLI repaints the prompt with the character — output from
    /// the CHILD, not ConPTY — so an echo proves both liveness and which input
    /// dialect the CLI is honoring right now.
    pub fn send_char(&mut self, ch: char, w32: bool) -> Result<()> {
        if w32 {
            self.writer.write_all(format!("\x1b[0;0;{};1;0;1_", ch as u32).as_bytes())?;
        } else {
            let mut b = [0u8; 4];
            self.writer.write_all(ch.encode_utf8(&mut b).as_bytes())?;
        }
        self.writer.flush()?;
        Ok(())
    }

    /// Paste + Enter in an explicit dialect (see send_text / send_text_w32).
    pub fn send_text_in(&mut self, text: &str, w32: bool) -> Result<()> {
        if w32 {
            self.send_text_w32(text)
        } else {
            // Raw bracketed paste regardless of the (unreliable) mode flag.
            let mut out: Vec<u8> = Vec::new();
            out.extend_from_slice(b"\x1b[200~");
            for ch in text.chars() {
                if ch == '\r' {
                    continue;
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            out.extend_from_slice(b"\x1b[201~");
            out.push(b'\r');
            self.writer.write_all(&out)?;
            self.writer.flush()?;
            Ok(())
        }
    }

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

    /// The paste encoded ENTIRELY as win32-input-mode key records — the
    /// bracketed-paste markers, every character, and Enter — the way Windows
    /// Terminal itself delivers a paste when that mode is on. Used by the
    /// recovery ladder as the OTHER dialect: on 2026-08-21 (CLI 2.1.239) a
    /// raw bracketed paste vanished without a trace into a CLI that was alive
    /// and answering a resize probe — the very "ignores raw bytes in
    /// win32-input-mode" case the comment above assumed paste was exempt from.
    /// Exactly one dialect lands in either mode, so alternating them can't
    /// double the message. Key-down records only (the up half is noise).
    pub fn send_text_w32(&mut self, text: &str) -> Result<()> {
        fn rec(out: &mut Vec<u8>, uc: u32) {
            out.extend_from_slice(format!("\x1b[0;0;{uc};1;0;1_").as_bytes());
        }
        let mut out: Vec<u8> = Vec::new();
        for ch in "\x1b[200~".chars() {
            rec(&mut out, ch as u32);
        }
        for ch in text.chars() {
            if ch == '\r' {
                continue;
            }
            rec(&mut out, ch as u32);
        }
        for ch in "\x1b[201~".chars() {
            rec(&mut out, ch as u32);
        }
        out.extend_from_slice(key_w32("Enter").unwrap().as_bytes());
        self.writer.write_all(&out)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Submit the current input line.
    /// Enter as a bare carriage return, whatever mode we THINK the CLI is in.
    /// The win32-input-mode flag is reconstructed from output chunks and can go
    /// stale (a `?9001l` split across a ConPTY read, or a CLI input re-init) —
    /// the 2026-08-21 wedge: chip in the prompt, 11 Enter records ignored, one
    /// raw CR submitted it. Harmless in true win32 mode (raw bytes are ignored).
    pub fn send_enter_raw(&mut self) -> Result<()> {
        self.writer.write_all(b"\r")?;
        self.writer.flush()?;
        Ok(())
    }

    /// Clear whatever sits in the prompt: `n` Backspaces, each in BOTH dialects
    /// (a win32 record + DEL) so it lands regardless of the stale-mode problem.
    /// A paste chip deletes as one unit, so a handful clears any leftovers.
    pub fn clear_prompt(&mut self, n: usize) -> Result<()> {
        let mut out = Vec::new();
        for _ in 0..n {
            out.extend_from_slice(key_w32("Backspace").unwrap().as_bytes());
            out.push(0x7f);
        }
        self.writer.write_all(&out)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Backspace `n` times in ONE dialect (the echo-proven one).
    pub fn backspace_in(&mut self, n: usize, w32: bool) -> Result<()> {
        let mut out = Vec::new();
        for _ in 0..n {
            if w32 {
                out.extend_from_slice(key_w32("Backspace").unwrap().as_bytes());
            } else {
                out.push(0x7f);
            }
        }
        self.writer.write_all(&out)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Enter in an explicit dialect.
    pub fn send_enter_in(&mut self, w32: bool) -> Result<()> {
        if w32 {
            self.writer.write_all(key_w32("Enter").unwrap().as_bytes())?;
        } else {
            self.writer.write_all(b"\r")?;
        }
        self.writer.flush()?;
        Ok(())
    }

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
