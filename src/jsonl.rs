//! JSONL transcript watcher — Rust port of `lit-bridge/jsonl_watcher.py`.
//!
//! Tails the transcript Claude Code writes for each session
//! (`<config>/projects/<slug>/<session-id>.jsonl`) to get CLEAN response text and
//! structured tool_use/tool_result events — the source of truth the TUI scrape can't
//! match. This is why responses come out without `●`/`✻` chrome.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

const RESULT_MAX: usize = 5000;

/// Derive Claude Code's project dir from a working directory (and optional config dir).
/// Mirrors `cc_project_dir` in the Python watcher exactly.
pub fn cc_project_dir(working_dir: &str, config_dir: Option<&str>) -> PathBuf {
    let slug: String = working_dir
        .trim_start_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let base = match config_dir {
        Some(c) => PathBuf::from(c),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".claude")
        }
    };
    base.join("projects").join(format!("-{slug}"))
}

pub struct JsonlWatcher {
    project_dir: PathBuf,
    file: Option<PathBuf>,
    pos: u64,
    emitted_tool_ids: HashSet<String>,
    turn_text_parts: Vec<String>,
    /// True while the transcript shows the turn in progress (last assistant
    /// `stop_reason == tool_use`, or a tool_result is pending) and not yet
    /// `end_turn`. While this is true, NO TUI heuristic may close the turn —
    /// the JSONL transcript is authoritative for turn boundaries.
    open: bool,
    /// Set when a terminal `stop_reason` (`end_turn` etc.) was seen but NO text
    /// had been accumulated yet. With extended thinking on, Claude Code splits one
    /// assistant message across entries: the `thinking` block lands first carrying
    /// `end_turn`, then the `text` block (also `end_turn`) in a later write. If a
    /// poll boundary falls between them, completing on the thinking entry would fire
    /// a turn_complete with EMPTY content — the frontend never finalizes (blinking
    /// cursor) and the bridge stops observing, dropping the real text. So we defer:
    /// hold the turn open and complete once the text actually arrives.
    pending_complete: bool,
}

impl JsonlWatcher {
    pub fn new(project_dir: PathBuf) -> Self {
        let mut w = JsonlWatcher {
            project_dir,
            file: None,
            pos: 0,
            emitted_tool_ids: HashSet::new(),
            turn_text_parts: Vec::new(),
            open: false,
            pending_complete: false,
        };
        // Start at EOF of the active transcript so we only see NEW entries.
        if let Some(f) = w.find_active_jsonl() {
            if let Ok(meta) = fs::metadata(&f) {
                w.pos = meta.len();
                w.file = Some(f);
            }
        }
        w
    }

    fn find_active_jsonl(&self) -> Option<PathBuf> {
        let dir = &self.project_dir;
        if !dir.is_dir() {
            return None;
        }
        let mut cands: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                if let Ok(m) = entry.metadata().and_then(|m| m.modified()) {
                    cands.push((m, p));
                }
            }
        }
        cands.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
        // Prefer the newest transcript that's an actual conversation — Claude writes
        // the auto-title to a SEPARATE ephemeral transcript, and that sidecar can be
        // the newest by mtime. Discriminate on whether the file CONTAINS conversation
        // anywhere in its tail, NOT on whether its last entry is conversation: a live
        // session routinely ends on a metadata marker (`permission-mode`, `system`,
        // `file-history-snapshot`) that belongs to the session but isn't the response.
        // The old last-entry allowlist rejected every live file (they all trail a
        // `permission-mode` stamp under --dangerously-skip-permissions) and latched the
        // newest STALE transcript whose tail happened to be a clean `user` entry.
        for (_, p) in &cands {
            if is_conversation_transcript(p) {
                return Some(p.clone());
            }
        }
        cands.into_iter().next().map(|(_, p)| p) // fallback: newest overall
    }

    /// Call when a new send starts — find/reset the active transcript at its current EOF.
    pub fn begin_turn(&mut self) {
        self.file = self.find_active_jsonl();
        self.pos = self
            .file
            .as_ref()
            .and_then(|f| fs::metadata(f).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        // Record exactly which transcript got pinned (and at what offset) so a turn
        // whose JSONL completion never fires can be traced to a stale/wrong-file pin
        // vs a mid-turn session roll — without this, the watcher is a black box.
        crate::diag::log(
            "PIN",
            &format!(
                "dir={} file={} pos={}",
                self.project_dir.display(),
                self.file.as_ref().map(|f| f.display().to_string()).unwrap_or_else(|| "<none>".into()),
                self.pos
            ),
        );
        self.emitted_tool_ids.clear();
        self.turn_text_parts.clear();
        self.open = false;
        self.pending_complete = false;
    }

    /// True while the JSONL transcript shows the turn still in progress. The TUI
    /// completion fallback must be blocked while this holds.
    pub fn turn_open(&self) -> bool {
        self.open
    }

    pub fn get_session_id(&self) -> Option<String> {
        let f = self.file.clone().or_else(|| self.find_active_jsonl())?;
        f.file_stem().map(|s| s.to_string_lossy().to_string())
    }

    fn read_new(&mut self) -> Option<String> {
        let f = self.file.clone()?;
        let mut fh = fs::File::open(&f).ok()?;
        fh.seek(SeekFrom::Start(self.pos)).ok()?;
        let mut buf = Vec::new();
        fh.read_to_end(&mut buf).ok()?;
        // Frame on newlines: Claude appends each entry as a line, but a poll can land
        // mid-write of a large final entry (the `text` block carrying `end_turn`). If we
        // consumed the partial bytes, the line would fail to parse AND `pos` would skip
        // past it — losing the entry, so `turn_complete` never fires and the gate hangs
        // the turn forever. Only advance `pos` to the last complete line; re-read the
        // unterminated tail next poll once it's fully written.
        let consume = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => return None, // no complete line yet — leave pos before the partial
        };
        self.pos += consume as u64;
        Some(String::from_utf8_lossy(&buf[..consume]).to_string())
    }

    /// Read new transcript entries and return events ready to emit:
    /// `tool_use`, `tool_result`, `replace` (cumulative clean text), `turn_complete`.
    pub fn poll(&mut self) -> Vec<Value> {
        // Ensure we have an active file.
        if self.file.as_ref().map(|f| !f.exists()).unwrap_or(true) {
            self.file = self.find_active_jsonl();
            self.pos = 0;
            if self.file.is_none() {
                return Vec::new();
            }
        }
        let size = self
            .file
            .as_ref()
            .and_then(|f| fs::metadata(f).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        if size <= self.pos {
            // No new data yet. CRITICAL: do NOT flap to a different "newest"
            // transcript and re-read it from offset 0. Transcript selection is
            // unstable — title sidecars and trailing non-conversational entries
            // (`file-history-snapshot`, `permission-mode`) can momentarily look
            // newest — so switching mid-turn re-read the whole file from the start,
            // replaying historical tool_results and a STALE `end_turn`. That fired a
            // spurious turn_complete and ended the turn before the real response was
            // ever written (channel turns 2+ silently lost). The file is pinned by
            // begin_turn for the whole turn; a genuine new-session roll re-anchors
            // through begin_turn on the next send, and the TUI quiescence fallback
            // still closes a turn if a transcript ever rolls mid-flight — slower,
            // never shorter, never replayed.
            return Vec::new();
        }

        let new_data = match self.read_new() {
            Some(d) => d,
            None => return Vec::new(),
        };

        let mut events = Vec::new();
        let mut parts_grew = false;
        let mut completed = false;

        for line in new_data.trim().split('\n') {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Capture the transcript line as the watcher sees it: type + stop_reason.
            // This is what reveals an end_turn / trailing-text straddle across polls.
            {
                let ty = entry.get("type").and_then(|v| v.as_str()).unwrap_or("?");
                let stop = entry
                    .get("message")
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                crate::diag::log("RX", &format!("type={ty} stop={stop}"));
            }
            match entry.get("type").and_then(|v| v.as_str()) {
                Some("assistant") => {
                    let msg = entry.get("message").cloned().unwrap_or(Value::Null);
                    if let Some(blocks) = msg.get("content").and_then(|v| v.as_array()) {
                        for block in blocks {
                            match block.get("type").and_then(|v| v.as_str()) {
                                Some("tool_use") => {
                                    let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                    if self.emitted_tool_ids.insert(id.to_string()) {
                                        let name = block
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        let input =
                                            block.get("input").cloned().unwrap_or(json!({}));
                                        events.push(json!({
                                            "event": "tool_use",
                                            "tool_use_id": id,
                                            "name": name,
                                            "input": input,
                                        }));
                                        self.turn_text_parts.push(format!(
                                            "\u{02}TOOLJSON{}\u{03}",
                                            json!({"name": name, "input": input})
                                        ));
                                        parts_grew = true;
                                    }
                                }
                                Some("text") => {
                                    let text = block
                                        .get("text")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .trim()
                                        .to_string();
                                    if !text.is_empty() {
                                        self.turn_text_parts.push(text);
                                        parts_grew = true;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    match msg.get("stop_reason").and_then(|v| v.as_str()) {
                        // Terminal stop reasons: the turn is over. DEFER emitting
                        // turn_complete to the end of the poll — Claude frequently writes
                        // an EMPTY assistant message carrying `end_turn` first, then the
                        // final text in the next message. Emitting here would drop that text.
                        // We do NOT set `open=false` here: the completion gate below decides
                        // whether we actually have content to complete with, or must hold the
                        // turn open for the text block still being written (thinking straddle).
                        Some("end_turn") | Some("stop_sequence") | Some("max_tokens") => {
                            completed = true;
                        }
                        // The model is about to call a tool — turn is provably open.
                        Some("tool_use") => {
                            self.open = true;
                        }
                        _ => {}
                    }
                }
                Some("user") => {
                    let msg = entry.get("message").cloned().unwrap_or(Value::Null);
                    // A genuine user message (string content, or a `text` block) marks a
                    // NEW turn boundary. Follow-up channel messages are injected into the
                    // live session WITHOUT a fresh `send`/begin_turn, so without resetting
                    // here the watcher accumulates every turn since session start into one
                    // ever-growing response (the "same message over and over" bug). Tool
                    // results are also `user` entries but must NOT reset — they're mid-turn.
                    let is_user_turn = match msg.get("content") {
                        Some(Value::String(s)) => !s.trim().is_empty(),
                        Some(Value::Array(blocks)) => blocks
                            .iter()
                            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("text")),
                        _ => false,
                    };
                    if is_user_turn {
                        self.turn_text_parts.clear();
                        self.emitted_tool_ids.clear();
                        self.open = true;
                        self.pending_complete = false;
                    }
                    if let Some(blocks) = msg.get("content").and_then(|v| v.as_array()) {
                        for block in blocks {
                            if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                                let id = block
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let content = flatten_tool_result(block.get("content"));
                                let content = truncate_chars(&content, RESULT_MAX);
                                events.push(json!({
                                    "event": "tool_result",
                                    "tool_use_id": id,
                                    "content": content,
                                }));
                                let safe = content.replace('\u{02}', "").replace('\u{03}', "");
                                self.turn_text_parts
                                    .push(format!("\u{02}RESULT\u{03}{safe}\u{02}/RESULT\u{03}"));
                                parts_grew = true;
                                self.open = true; // mid-tool: turn is open
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Emit completion at end-of-poll so any trailing text written after the
        // `end_turn` marker (in this same batch) is included in the content.
        //
        // A terminal stop_reason this poll, OR one carried over from a previous poll
        // that had no content yet (`pending_complete`), both mean "the turn wants to
        // close". But we ONLY actually close when we have content to close with — a
        // terminal stop on an empty turn is the thinking-block straddle (the `text`
        // block, also `end_turn`, is still being written). Closing empty would fire a
        // turn_complete with "" → frontend never finalizes (blinking cursor) and the
        // bridge stops observing, losing the real text. So in that case we hold the
        // turn OPEN and wait for the next poll to bring the text.
        let want_complete = completed || self.pending_complete;
        if want_complete && !self.turn_text_parts.is_empty() {
            let content = self.turn_text_parts.join("\n\n");
            crate::diag::log(
                "TURN",
                &format!(
                    "complete parts={} len={}",
                    self.turn_text_parts.len(),
                    content.chars().count()
                ),
            );
            self.open = false;
            self.pending_complete = false;
            events.push(json!({"event": "turn_complete", "content": content}));
            self.turn_text_parts.clear();
        } else if want_complete {
            // Terminal stop seen but nothing to emit yet — defer. Keep the turn open
            // so neither the TUI fallback nor a stale state can close it before the
            // text block (carrying its own end_turn) lands in a subsequent poll.
            self.open = true;
            self.pending_complete = true;
            crate::diag::log("TURN", "defer: end_turn with empty parts, awaiting text block");
        } else if parts_grew {
            // Cumulative clean text for live streaming (REPLACE).
            let text = self.turn_text_parts.join("\n\n");
            events.push(json!({"event": "replace", "text": text}));
        }

        events
    }
}

/// True if a transcript holds real conversation (a `user`/`assistant`/`human` entry)
/// anywhere in its tail window — as opposed to a title/summary sidecar that holds only
/// `summary` entries. We scan the tail rather than the single last line because a live
/// session routinely ends on a metadata marker (`permission-mode`, `system`,
/// `file-history-snapshot`) that belongs to the session but isn't itself the response.
/// 64KB covers many turns of trailing metadata; a real conversation has a user/assistant
/// entry well within it, a sidecar never does.
fn is_conversation_transcript(path: &Path) -> bool {
    let mut fh = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let len = fh.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(65536);
    if fh.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    if fh.read_to_end(&mut buf).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            match v.get("type").and_then(|t| t.as_str()) {
                Some("assistant") | Some("user") | Some("human") => return true,
                _ => {}
            }
        }
    }
    false
}

fn flatten_tool_result(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for c in arr {
                if let Some(s) = c.as_str() {
                    parts.push(s.to_string());
                } else if c.get("type").and_then(|v| v.as_str()) == Some("text") {
                    parts.push(c.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string());
                }
            }
            parts.join("\n")
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    } else {
        s.to_string()
    }
}

// Allow Path comparison helper used above without extra imports.
#[allow(dead_code)]
fn _path_marker(_: &Path) {}
