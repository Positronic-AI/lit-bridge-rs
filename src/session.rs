//! A single managed CLI session: a child process under a PTY, plus an in-process
//! VT emulator capturing its rendered screen. This is the proven core from the
//! ConPTY spike, generalized. No tmux — the PTY lives in this process.

use std::collections::HashMap;
use std::io::{Read, Write};
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
    /// Last TUI-scraped response text emitted as a streaming `replace` (dedup).
    pub last_streamed: String,
    /// The CLI model this session launched with (from --model), for model-switch logic.
    pub model: Option<String>,
    /// Watches Claude Code's JSONL transcript for clean content + tool events.
    pub jsonl: Option<JsonlWatcher>,
    writer: Box<dyn Write + Send>,
    screen: Arc<Mutex<vt100::Parser>>,
    /// Live tee of the raw PTY output, for terminal-attach clients (the escape hatch).
    output_tx: broadcast::Sender<Vec<u8>>,
    // Kept alive for the lifetime of the session; dropping closes the PTY.
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Session {
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
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd)?;

        // Feed PTY output into the VT emulator on a blocking reader thread.
        let screen = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(512);
        let mut reader = pair.master.try_clone_reader()?;
        {
            let s = screen.clone();
            let tee = output_tx.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(mut g) = s.lock() {
                                g.process(&buf[..n]);
                            }
                            let _ = tee.send(buf[..n].to_vec()); // feed terminal-attach clients
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
            last_streamed: String::new(),
            model: None,
            jsonl,
            writer,
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

    /// Terminate the child CLI process.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
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

    /// The rendered visible screen — the `tmux capture-pane -p` analogue.
    pub fn capture(&self) -> String {
        self.screen
            .lock()
            .map(|g| g.screen().contents())
            .unwrap_or_default()
    }

    /// Write the message text (no submit). Submitting is a separate step so the
    /// trailing Enter doesn't race Claude's ingestion of the paste — a `\r` sent
    /// too eagerly on a longer message gets absorbed and the message is left
    /// unsubmitted in the input box. Mirrors tmux paste-buffer then send-keys Enter.
    pub fn send_text(&mut self, text: &str) -> Result<()> {
        self.writer.write_all(text.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    /// Submit the current input line.
    pub fn send_enter(&mut self) -> Result<()> {
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

    /// Send a named key (for dialogs / navigation).
    pub fn send_key(&mut self, key: &str) -> Result<()> {
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
