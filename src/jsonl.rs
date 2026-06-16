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
        // the newest by mtime. Skip files whose last entry isn't user/assistant.
        for (_, p) in &cands {
            match last_entry_type(p).as_deref() {
                Some("assistant") | Some("user") | Some("human") => return Some(p.clone()),
                _ => {}
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
        self.emitted_tool_ids.clear();
        self.turn_text_parts.clear();
        self.open = false;
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
        self.pos += buf.len() as u64;
        Some(String::from_utf8_lossy(&buf).to_string())
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
            // No new data — did Claude Code roll to a new transcript?
            if let Some(newest) = self.find_active_jsonl() {
                if Some(&newest) != self.file.as_ref() {
                    self.file = Some(newest);
                    self.pos = 0;
                } else {
                    return Vec::new();
                }
            } else {
                return Vec::new();
            }
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
                        Some("end_turn") | Some("stop_sequence") | Some("max_tokens") => {
                            self.open = false;
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
        if completed {
            let content = self.turn_text_parts.join("\n\n");
            events.push(json!({"event": "turn_complete", "content": content}));
            self.turn_text_parts.clear();
        } else if parts_grew {
            // Cumulative clean text for live streaming (REPLACE).
            let text = self.turn_text_parts.join("\n\n");
            events.push(json!({"event": "replace", "text": text}));
        }

        events
    }
}

/// Type of the last conversation entry in a transcript (reads only the tail).
/// Used to distinguish real conversation files from title/summary sidecars.
fn last_entry_type(path: &Path) -> Option<String> {
    let mut fh = fs::File::open(path).ok()?;
    let len = fh.metadata().ok()?.len();
    let want = 16384u64;
    let start = len.saturating_sub(want);
    fh.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    fh.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            return v.get("type").and_then(|t| t.as_str()).map(|s| s.to_string());
        }
    }
    None
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
