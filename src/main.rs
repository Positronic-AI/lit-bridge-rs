//! lit-bridge-rs — native CLI session multiplexer.
//!
//! Speaks the lit-bridge JSON-lines protocol (see docs/plans/lit-bridge-rs/01-wire-contract.md)
//! over a Unix socket. Manages interactive AI CLI sessions under a real PTY — no tmux.
//!
//! MVP scope: create / send / keystroke / list / kill / ping, a 300ms observer that
//! emits state transitions + naive chunks, single-client-at-a-time with event buffering
//! across disconnects. NOT yet ported: full v2_1 parser, JSONL completion detection,
//! the --resume reaping, and the raw xterm.js attach channel. Those are the next milestones.

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::LocalSet;

/// Boxed half-streams so the same session code drives either transport: a Unix
/// domain socket (Linux — local + SSH/socat) or a TCP loopback socket (Windows,
/// where Unix sockets and tmux never existed). The runtime is single-threaded
/// (`current_thread` + LocalSet), so these trait objects need not be `Send`.
type BoxRead = Box<dyn AsyncRead + Unpin>;
type BoxWrite = Box<dyn AsyncWrite + Unpin>;

/// A bound control/attach socket. The Unix arm is compiled out entirely on
/// Windows (tokio's `UnixListener` is `#[cfg(unix)]`), leaving TCP as the only
/// transport there.
enum Listener {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    Tcp(TcpListener),
}

impl Listener {
    /// Accept one connection and hand back boxed read/write halves.
    async fn accept_split(&self) -> std::io::Result<(BoxRead, BoxWrite)> {
        match self {
            #[cfg(unix)]
            Listener::Unix(l) => {
                let (s, _) = l.accept().await?;
                let (r, w) = s.into_split();
                Ok((Box::new(r), Box::new(w)))
            }
            Listener::Tcp(l) => {
                let (s, _) = l.accept().await?;
                let _ = s.set_nodelay(true);
                let (r, w) = s.into_split();
                Ok((Box::new(r), Box::new(w)))
            }
        }
    }
}

use lit_bridge_rs::parser::{select_parser, SessionState, TuiParser};
use lit_bridge_rs::session::Session;

const ROWS: u16 = 50;
const COLS: u16 = 200;
const BUF_MAX: usize = 500;

// Completion-detection timing for the TUI FALLBACK only. JSONL `end_turn` is the
// authoritative, fast completion path; these gate the fallback so it fails *slower*,
// never *shorter* (see docs/plans/lit-bridge-rs/02-completion-detection.md).
const QUIESCENCE: Duration = Duration::from_secs(8); // screen must be still this long
const MIN_TURN: Duration = Duration::from_secs(2); // never complete a turn faster than this
const MAX_TURN: Duration = Duration::from_secs(600); // hard cap so we never hang forever

struct Monitor {
    sessions: HashMap<String, Session>,
    client: Option<BoxWrite>,
    buffer: VecDeque<String>,
    parser: Box<dyn TuiParser>,
    /// session key → stashed Claude session id, for `--resume` on the next create
    /// (model-switch kill+recreate, or reaped-session reuse).
    reaped: HashMap<String, String>,
}

impl Monitor {
    fn new() -> Self {
        Monitor {
            sessions: HashMap::new(),
            client: None,
            buffer: VecDeque::new(),
            parser: select_parser("claude-code").expect("claude-code parser registered"),
            reaped: HashMap::new(),
        }
    }

    async fn emit(&mut self, v: Value) {
        // Diagnostic capture (off unless LIT_BRIDGE_RS_EVENTLOG is set): record the
        // session + event type, and for content-bearing events the length, so a lost
        // or repeated message can be reconstructed without re-running blind.
        {
            let sess = v.get("session").and_then(|x| x.as_str()).unwrap_or("-");
            let ev = v.get("event").and_then(|x| x.as_str()).unwrap_or("-");
            let detail = v
                .get("content")
                .or_else(|| v.get("text"))
                .and_then(|x| x.as_str())
                .map(|s| format!(" len={}", s.chars().count()))
                .unwrap_or_default();
            lit_bridge_rs::diag::log("EMIT", &format!("{sess} {ev}{detail}"));
        }
        let mut line = v.to_string();
        line.push('\n');
        if let Some(w) = &mut self.client {
            if w.write_all(line.as_bytes()).await.is_err() {
                self.client = None;
                self.push_buf(line);
            }
        } else {
            self.push_buf(line);
        }
    }

    fn push_buf(&mut self, line: String) {
        if self.buffer.len() >= BUF_MAX {
            self.buffer.pop_front();
        }
        self.buffer.push_back(line);
    }

    async fn attach_client(&mut self, mut w: BoxWrite) -> Result<()> {
        // Handshake: the Connector expects monitor_ready first.
        let ready = json!({
            "event": "monitor_ready",
            "sessions": self.sessions.len(),
            "buffered": self.buffer.len()
        })
        .to_string()
            + "\n";
        w.write_all(ready.as_bytes()).await?;
        // Flush events buffered while disconnected.
        while let Some(l) = self.buffer.pop_front() {
            if w.write_all(l.as_bytes()).await.is_err() {
                break;
            }
        }
        self.client = Some(w);
        Ok(())
    }

    fn detach_client(&mut self) {
        self.client = None;
    }

    fn session_key(cmd: &Value) -> String {
        let name = cmd.get("session").and_then(|v| v.as_str()).unwrap_or("");
        match cmd.get("channel_id").and_then(|v| v.as_str()) {
            Some(c) => format!("{name}:{c}"),
            None => name.to_string(),
        }
    }

    async fn handle_line(&mut self, mon: &Rc<Mutex<Monitor>>, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let cmd: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                self.emit(json!({"event": "error", "message": format!("invalid JSON: {e}")}))
                    .await;
                return;
            }
        };
        match cmd.get("cmd").and_then(|v| v.as_str()).unwrap_or("") {
            "ping" => self.emit(json!({"event": "pong"})).await,
            "create" => self.cmd_create(mon, &cmd).await,
            "send" => self.cmd_send(mon, &cmd).await,
            "input" => self.cmd_input(&cmd).await,
            "keystroke" => self.cmd_keystroke(&cmd).await,
            "list" => self.cmd_list().await,
            "dump" => self.cmd_dump(&cmd).await,
            "kill" => self.cmd_kill(&cmd).await,
            other => {
                self.emit(json!({"event": "error", "message": format!("unknown command: {other}")}))
                    .await
            }
        }
    }

    async fn cmd_create(&mut self, mon: &Rc<Mutex<Monitor>>, cmd: &Value) {
        let key = Self::session_key(cmd);
        // Reuse an existing live session; report its model for the backend's
        // model-switch decision (kill+resume if it differs).
        if let Some(s) = self.sessions.get(&key) {
            let model = s.model.clone();
            self.emit(json!({"session": key, "event": "ready", "reused": true, "model": model}))
                .await;
            return;
        }

        let exe = cmd
            .get("exe")
            .or_else(|| cmd.get("cli"))
            .and_then(|v| v.as_str())
            .unwrap_or("claude")
            .to_string();
        let base_args: Vec<String> = cmd
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let cwd: Option<String> = cmd.get("working_dir").and_then(|v| v.as_str()).map(String::from);
        let mut env = HashMap::new();
        if let Some(o) = cmd.get("env").and_then(|v| v.as_object()) {
            for (k, v) in o {
                if let Some(s) = v.as_str() {
                    env.insert(k.clone(), s.to_string());
                }
            }
        }
        let model = model_from_args(&base_args);

        // Resume a previously-stashed session id (model-switch or reaped reuse).
        let resume_id = self.reaped.remove(&key);
        let mut args = base_args.clone();
        if let Some(id) = &resume_id {
            args.push("--resume".to_string());
            args.push(id.clone());
        }

        if let Err(e) = self.spawn_into(&key, &exe, &args, cwd.as_deref(), &env, model.clone()) {
            self.emit(json!({"session": key, "event": "error", "message": format!("spawn failed: {e}")}))
                .await;
            return;
        }

        // Finish startup (resume-retry, dialog dismissal, ready) in a BACKGROUND task with
        // granular short locks. The daemon must never hold the global lock across these
        // waits, or one slow create wedges every session (the lit-platform bug).
        tokio::task::spawn_local(finalize_create(
            mon.clone(),
            key,
            exe,
            base_args,
            cwd,
            env,
            model,
            resume_id.is_some(),
        ));
    }

    /// Spawn a session and insert it under `key`, stamping its model.
    fn spawn_into(
        &mut self,
        key: &str,
        exe: &str,
        args: &[String],
        cwd: Option<&str>,
        env: &HashMap<String, String>,
        model: Option<String>,
    ) -> Result<()> {
        let mut s = Session::spawn(key.to_string(), exe, args, cwd, env, ROWS, COLS)?;
        s.model = model;
        self.sessions.insert(key.to_string(), s);
        Ok(())
    }

    async fn cmd_send(&mut self, mon: &Rc<Mutex<Monitor>>, cmd: &Value) {
        let key = Self::session_key(cmd);
        let content = cmd.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
        // Baseline the assistant-message count before sending, for completion detection.
        let baseline = match self.sessions.get(&key) {
            Some(s) => self.parser.count_assistant_messages(&s.capture()),
            None => {
                self.emit(json!({"session": key, "event": "error", "message": "session not found"}))
                    .await;
                return;
            }
        };
        let outcome = match self.sessions.get_mut(&key) {
            Some(s) => {
                s.baseline_msgs = baseline;
                s.observing = true;
                s.last_streamed.clear();
                s.sent = content.clone();
                s.begin_turn(); // reset quiescence clocks + position the JSONL watcher
                match s.send_text(&content) {
                    Ok(()) => {
                        let old = s.state;
                        s.state = SessionState::Thinking;
                        Ok(old)
                    }
                    Err(e) => {
                        s.observing = false;
                        Err(format!("send failed: {e}"))
                    }
                }
            }
            None => Err("session not found".to_string()),
        };
        let sent_ok = outcome.is_ok();
        match outcome {
            Ok(old) => {
                self.emit(json!({"session": key, "event": "state", "from": old.as_str(), "to": "thinking"}))
                    .await
            }
            Err(msg) => self.emit(json!({"session": key, "event": "error", "message": msg})).await,
        }
        // Submission is handled atomically inside send_text (bracketed paste
        // immediately followed by the Enter record), so no separate/poll-loop
        // Enter is armed here — that raced the paste and was unreliable.
        let _ = sent_ok;
    }

    /// Send text + Enter WITHOUT starting an observed turn — for slash commands
    /// (/model, /clear, …) and the escape hatch. No completion is expected.
    async fn cmd_input(&mut self, cmd: &Value) {
        let key = Self::session_key(cmd);
        let content = cmd.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let ok = if let Some(s) = self.sessions.get_mut(&key) {
            s.write_raw(content.as_bytes());
            s.write_raw(b"\r");
            true
        } else {
            false
        };
        if ok {
            self.emit(json!({"session": key, "event": "input_sent"})).await;
        } else {
            self.emit(json!({"session": key, "event": "error", "message": "session not found"}))
                .await;
        }
    }

    async fn cmd_keystroke(&mut self, cmd: &Value) {
        let key = Self::session_key(cmd);
        let keys: Vec<String> = cmd
            .get("keys")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if let Some(s) = self.sessions.get_mut(&key) {
            for k in &keys {
                let _ = s.send_key(k);
            }
            self.emit(json!({"session": key, "event": "keystroke_sent", "keys": keys}))
                .await;
        } else {
            self.emit(json!({"session": key, "event": "error", "message": "session not found"}))
                .await;
        }
    }

    async fn cmd_dump(&mut self, cmd: &Value) {
        let key = Self::session_key(cmd);
        let info = self.sessions.get(&key).map(|s| {
            let cap = s.capture();
            (
                cap.clone(),
                self.parser.detect_state(&cap).as_str(),
                self.parser.count_assistant_messages(&cap),
                s.baseline_msgs,
                s.observing,
            )
        });
        match info {
            Some((cap, state, count, baseline, observing)) => {
                self.emit(json!({
                    "session": key, "event": "dump",
                    "state": state, "count": count, "baseline": baseline,
                    "observing": observing, "capture": cap
                }))
                .await
            }
            None => {
                self.emit(json!({"session": key, "event": "error", "message": "session not found"}))
                    .await
            }
        }
    }

    async fn cmd_list(&mut self) {
        let list: Vec<Value> = self
            .sessions
            .values()
            .map(|s| json!({"name": s.name.clone(), "state": s.state.as_str()}))
            .collect();
        self.emit(json!({"event": "sessions", "sessions": list})).await;
    }

    async fn cmd_kill(&mut self, cmd: &Value) {
        let key = Self::session_key(cmd);
        let store_resume = cmd.get("store_resume").and_then(|v| v.as_bool()).unwrap_or(false);
        match self.sessions.remove(&key) {
            Some(mut s) => {
                // Stash the Claude session id so the next create resumes the conversation
                // (used for model switches: kill + recreate with --resume).
                if store_resume {
                    if let Some(id) = s.session_id() {
                        self.reaped.insert(key.clone(), id);
                    }
                }
                s.kill();
                self.emit(json!({"session": key, "event": "killed"})).await;
            }
            None => {
                self.emit(json!({"session": key, "event": "error", "message": "session not found"}))
                    .await;
            }
        }
    }

    /// 300ms observer: detect state transitions and emit naive chunks.
    /// (Real completion detection — JSONL end_turn, ✻ debounce, quiescence — is the
    /// observer.py port, a later milestone.)
    async fn poll(&mut self) {
        let mut events: Vec<Value> = Vec::new();
        for s in self.sessions.values_mut() {
            let cap = s.capture();
            // Reset the quiescence clock whenever the screen changes.
            if cap != s.last {
                s.last_change = Instant::now();
            }
            let new_state = self.parser.detect_state(&cap);
            if new_state != s.state {
                events.push(json!({
                    "session": s.name.clone(),
                    "event": "state",
                    "from": s.state.as_str(),
                    "to": new_state.as_str()
                }));
                s.state = new_state;
            }
            // Submit-with-verification. The bracketed paste lands the full message
            // in the prompt within a few hundred ms; then press Enter (retry,
            // spaced) until the turn actually starts. Do NOT gate the Enter on
            // detected state: the parser misclassifies a prompt holding a big
            // pasted message as non-idle, which blocked the Enter entirely; and
            // the blinking cursor means the screen never "settles". Stop once a new
            // assistant message appears (the turn really started) or the prompt
            // leaves idle after we've pressed Enter.
            if let Some(pasted_at) = s.pending_submit {
                let turn_started =
                    self.parser.count_assistant_messages(&cap) > s.baseline_msgs;
                if turn_started
                    || (new_state != SessionState::Idle && s.last_submit_try.is_some())
                {
                    s.pending_submit = None; // submitted — turn under way
                } else if pasted_at.elapsed() > Duration::from_secs(12) {
                    s.pending_submit = None; // give up
                } else if pasted_at.elapsed() > Duration::from_millis(700)
                    && s
                        .last_submit_try
                        .map_or(true, |t| t.elapsed() > Duration::from_millis(900))
                {
                    let _ = s.send_enter();
                    s.last_submit_try = Some(Instant::now());
                }
            }
            if s.observing {
                // AUTHORITATIVE completion path: the JSONL transcript. `turn_complete`
                // carries clean text; tool_use/tool_result/replace stream through.
                for ev in s.poll_jsonl() {
                    match ev.get("event").and_then(|v| v.as_str()) {
                        Some("turn_complete") => {
                            let content = ev.get("content").cloned().unwrap_or_else(|| json!(""));
                            events.push(json!({
                                "session": s.name.clone(), "event": "complete", "content": content
                            }));
                            s.observing = false;
                        }
                        _ => {
                            let mut e = ev;
                            e["session"] = json!(s.name.clone());
                            events.push(e);
                        }
                    }
                }
                // Stream the response text live from the TUI scrape (the JSONL
                // transcript only lands at turn-end). Emit `replace` when the scraped
                // text changes; the clean JSONL `complete` swaps in at the end. Skip
                // updates that would drop the response bullet (avoids flicker).
                //
                // While a dialog/question picker is up (e.g. AskUserQuestion), the scrape
                // is picker chrome, not response text — streaming it would clobber the
                // structured tool_use (the interactive question) the frontend renders from
                // JSONL. So suppress the scrape in Dialog state and let the question show.
                if s.observing && new_state != SessionState::Dialog {
                    let resp =
                        self.parser
                            .extract_raw_response(s.baseline_msgs, &cap, Some(&s.sent), 0);
                    if !resp.is_empty() && resp != s.last_streamed {
                        let had = s.last_streamed.lines().any(|l| l.trim_start().starts_with('●'));
                        let has = resp.lines().any(|l| l.trim_start().starts_with('●'));
                        if !(had && !has) {
                            events.push(json!({
                                "session": s.name.clone(), "event": "replace", "text": resp.clone()
                            }));
                            s.last_streamed = resp;
                        }
                    }
                }
            }
            // TUI FALLBACK — corroborates, never overrides. It may close a turn ONLY
            // when the JSONL is not holding it open (no pending tool_use), and only after
            // SUSTAINED quiescence — never on a transient ✻. This is the carve-out from
            // docs/plans/lit-bridge-rs/02-completion-detection.md: fail slower, never
            // shorter; truncation is unacceptable, latency is fine.
            if s.observing && !s.jsonl_turn_open() {
                let now = Instant::now();
                let elapsed = now.duration_since(s.turn_started);
                let quiescent = now.duration_since(s.last_change) >= QUIESCENCE;
                let tui_done =
                    new_state == SessionState::Idle && self.parser.turn_complete(&cap);
                let complete = (elapsed >= MIN_TURN && quiescent && tui_done)
                    || elapsed >= MAX_TURN;
                if complete {
                    let resp =
                        self.parser
                            .extract_raw_response(s.baseline_msgs, &cap, Some(&s.sent), 0);
                    events.push(json!({
                        "session": s.name.clone(), "event": "complete", "content": resp
                    }));
                    s.observing = false;
                }
            }
            s.last = cap;
        }
        for e in events {
            self.emit(e).await;
        }
    }
}

/// Background finalize for a freshly-spawned session: resume-retry, startup-dialog
/// dismissal, then emit `ready`. Uses only SHORT lock acquisitions and sleeps with the
/// lock RELEASED, so it never blocks the command loop or the observer.
async fn finalize_create(
    mon: Rc<Mutex<Monitor>>,
    key: String,
    exe: String,
    base_args: Vec<String>,
    cwd: Option<String>,
    env: HashMap<String, String>,
    model: Option<String>,
    mut resumed: bool,
) {
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // If --resume failed (the session died immediately), retry fresh.
    if resumed {
        let dead = {
            let mut m = mon.lock().await;
            m.sessions.get_mut(&key).map(|s| !s.is_alive()).unwrap_or(true)
        };
        if dead {
            {
                mon.lock().await.sessions.remove(&key);
            }
            {
                let mut m = mon.lock().await;
                if let Err(e) = m.spawn_into(&key, &exe, &base_args, cwd.as_deref(), &env, model.clone())
                {
                    m.emit(json!({"session": key, "event": "error", "message": format!("spawn failed: {e}")}))
                        .await;
                    return;
                }
            }
            resumed = false;
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    }

    // Auto-dismiss startup dialogs so the session reaches an interactive prompt.
    //
    // Dialogs (workspace-trust, bypass-permissions, theme) can render SECONDS after
    // spawn — especially under ConPTY on Windows, where claude boots slowly. The old
    // loop bailed on the first dialog-free frame, so on a slow boot it gave up before
    // the trust dialog ever appeared and the session wedged on it. Instead, keep
    // watching until the prompt is genuinely interactive (idle) or we hit a ceiling,
    // dismissing any dialog we encounter along the way.
    let dialog_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if Instant::now() > dialog_deadline {
            break;
        }
        let cap = match mon.lock().await.sessions.get(&key).map(|s| s.capture()) {
            Some(c) => c,
            None => break,
        };
        // Bind to a local so the lock guard from the scrutinee is released here —
        // an `if let mon.lock()...` would hold it across the block and deadlock the
        // re-lock below (the async Mutex is not reentrant).
        let dialog = mon.lock().await.parser.is_startup_dialog(&cap);
        if let Some((_, keys, name)) = dialog {
            {
                let mut m = mon.lock().await;
                if let Some(s) = m.sessions.get_mut(&key) {
                    for k in &keys {
                        let _ = s.send_key(k);
                    }
                }
            }
            mon.lock()
                .await
                .emit(json!({"session": key, "event": "dialog_dismissed", "dialog": name}))
                .await;
            tokio::time::sleep(Duration::from_millis(1000)).await;
            continue;
        }
        // No dialog visible. If the prompt is interactive we're done; otherwise the
        // CLI may still be booting (or a dialog is about to appear) — wait and re-check.
        let state = mon.lock().await.parser.detect_state(&cap);
        if state == SessionState::Idle {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    mon.lock()
        .await
        .emit(json!({
            "session": key, "event": "ready", "state": "starting",
            "model": model, "resumed": resumed
        }))
        .await;
}

/// Bridge a raw terminal-attach connection to a session's PTY. Protocol: the client
/// sends ONE selector line (`{"session":"…","channel_id":"…"}` or a bare key), then it's
/// a bidirectional raw byte pipe — live PTY output out, keystrokes in. On attach we paint
/// the current rendered screen so the terminal isn't blank.
async fn handle_attach(mon: Rc<Mutex<Monitor>>, mut rh: BoxRead, mut wh: BoxWrite) {
    // Read the one-line session selector.
    let mut sel = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match rh.read(&mut b).await {
            Ok(0) => return,
            Ok(_) => {
                if b[0] == b'\n' {
                    break;
                }
                sel.push(b[0]);
                if sel.len() > 4096 {
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let sel_str = String::from_utf8_lossy(&sel);
    let key = match serde_json::from_str::<Value>(sel_str.trim()) {
        Ok(v) => {
            let name = v.get("session").and_then(|x| x.as_str()).unwrap_or("");
            match v.get("channel_id").and_then(|x| x.as_str()) {
                Some(c) => format!("{name}:{c}"),
                None => name.to_string(),
            }
        }
        Err(_) => sel_str.trim().to_string(),
    };

    // Subscribe to live output + grab the current screen for the initial paint.
    // Use the *formatted* capture so color/styling render immediately (it carries its
    // own SGR + cursor positioning, so no newline translation is needed).
    let (mut rx, paint) = {
        let m = mon.lock().await;
        match m.sessions.get(&key) {
            Some(s) => (s.subscribe(), s.capture_formatted()),
            None => {
                let _ = wh
                    .write_all(format!("\r\nlit-bridge-rs: no such session '{key}'\r\n").as_bytes())
                    .await;
                return;
            }
        }
    };
    let _ = wh.write_all(b"\x1b[2J\x1b[H").await; // clear + home
    let _ = wh.write_all(&paint).await;

    // Forward live PTY output → terminal client.
    let out = tokio::task::spawn_local(async move {
        loop {
            match rx.recv().await {
                Ok(chunk) => {
                    if wh.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    // Forward terminal keystrokes → PTY.
    let mut buf = [0u8; 1024];
    loop {
        match rh.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let mut m = mon.lock().await;
                match m.sessions.get_mut(&key) {
                    Some(s) => s.write_raw(&buf[..n]),
                    None => break,
                }
            }
            Err(_) => break,
        }
    }
    out.abort();
}

fn model_from_args(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--model" {
            return it.next().cloned();
        }
    }
    None
}

// Only referenced by the Unix-socket bind path; unused when compiled for Windows.
#[cfg_attr(not(unix), allow(dead_code))]
fn arg(flag: &str, default: &str) -> String {
    let a: Vec<String> = std::env::args().collect();
    if let Some(i) = a.iter().position(|x| x == flag) {
        if let Some(v) = a.get(i + 1) {
            return v.clone();
        }
    }
    default.to_string()
}

fn arg_opt(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == flag)
        .and_then(|i| a.get(i + 1).cloned())
}

/// Bind the control listener and the optional raw-PTY attach listener.
///
/// `--port N` selects TCP loopback (control on `127.0.0.1:N`, attach on `N+1`) —
/// the cross-platform path, and the only one available on Windows. Otherwise we
/// fall back to a Unix socket at `--socket` (attach at `<socket>.attach`), which
/// is what the Linux local + SSH/socat path expects.
async fn bind_listeners() -> Result<(Listener, Option<Listener>)> {
    if let Some(port) = arg_opt("--port") {
        let port: u16 = port
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid --port: {port}"))?;
        let ctrl = TcpListener::bind(("127.0.0.1", port)).await?;
        eprintln!("lit-bridge-rs listening on tcp://127.0.0.1:{port}");
        let attach = TcpListener::bind(("127.0.0.1", port + 1)).await.ok();
        if attach.is_some() {
            eprintln!("lit-bridge-rs attach socket on tcp://127.0.0.1:{}", port + 1);
        }
        return Ok((Listener::Tcp(ctrl), attach.map(Listener::Tcp)));
    }

    #[cfg(unix)]
    {
        let socket = arg("--socket", "/tmp/lit-bridge-rs.sock");
        let _ = std::fs::remove_file(&socket);
        let ctrl = tokio::net::UnixListener::bind(&socket)?;
        eprintln!("lit-bridge-rs listening on {socket}");

        let attach_path = format!("{socket}.attach");
        let _ = std::fs::remove_file(&attach_path);
        let attach = tokio::net::UnixListener::bind(&attach_path).ok();
        if attach.is_some() {
            eprintln!("lit-bridge-rs attach socket on {attach_path}");
        }
        Ok((Listener::Unix(ctrl), attach.map(Listener::Unix)))
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("--port <N> is required on this platform (no Unix socket support)")
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Control listener + optional raw-PTY attach listener (the escape hatch: a
    // terminal client connects, sends a one-line session selector, then gets a
    // live bidirectional byte pipe to the PTY — for slash commands, dismissing
    // odd dialogs, and diagnostics when scraping falls short).
    let (listener, attach_listener) = bind_listeners().await?;

    let local = LocalSet::new();
    local
        .run_until(async move {
            let mon = Rc::new(Mutex::new(Monitor::new()));

            // Observer task — runs whether or not a client is attached.
            let m2 = mon.clone();
            tokio::task::spawn_local(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(300));
                loop {
                    tick.tick().await;
                    m2.lock().await.poll().await;
                }
            });

            // Attach-socket accept loop: each connection is a raw PTY terminal session.
            if let Some(al) = attach_listener {
                let mon_a = mon.clone();
                tokio::task::spawn_local(async move {
                    loop {
                        if let Ok((rh, wh)) = al.accept_split().await {
                            tokio::task::spawn_local(handle_attach(mon_a.clone(), rh, wh));
                        }
                    }
                });
            }

            // One client at a time; reconnect-friendly (events buffer while detached).
            loop {
                let (rh, wh) = listener.accept_split().await?;
                mon.lock().await.attach_client(wh).await?;
                let mut lines = BufReader::new(rh).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    mon.lock().await.handle_line(&mon, &l).await;
                }
                mon.lock().await.detach_client();
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        })
        .await
}
