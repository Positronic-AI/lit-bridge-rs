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
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const RESULT_MAX: usize = 5000;

/// Derive Claude Code's project dir from a working directory (and optional config dir).
///
/// Claude's slug is every non-alphanumeric char of the absolute path mapped to `-`.
/// On Unix the leading `/` becomes a leading `-` (e.g. `/opt/x` -> `-opt-x`). On
/// Windows the path starts with a drive letter, so there is NO leading dash
/// (e.g. `C:\Users\ben\x` -> `C--Users-ben-x`). Do not trim a leading separator or
/// re-prepend `-` — that produced a spurious leading dash on Windows
/// (`-C--Users-...`), so the JSONL transcript was never found and `complete` never
/// fired even though Claude had responded.
pub fn cc_project_dir(working_dir: &str, config_dir: Option<&str>) -> PathBuf {
    let slug: String = working_dir
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
    base.join("projects").join(slug)
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
    /// Snapshot of every `.jsonl` that existed in `project_dir` at `begin_turn`.
    /// A mid-turn session roll is recognised by ABSENCE from this set (the file
    /// was created after the pin), which is filesystem- and libc-independent —
    /// unlike birth time (`created()`), which is silently unreadable on some
    /// builds/mounts (static-musl, NFS/NAS) and there disables re-anchoring,
    /// stranding the turn on the dead pinned file (the dormant-channel dark turn).
    existing_at_pin: HashSet<PathBuf>,
    /// Wall-clock moment `begin_turn` pinned the active transcript. A conversation
    /// transcript whose BIRTH time is after this was created during the turn — i.e.
    /// a mid-turn session roll (a `[resumed]` session spins up a fresh transcript and
    /// writes the response there, leaving the pinned file silent). Such a file cannot
    /// hold stale history for this turn, so it is the one cross-file switch `poll`
    /// may safely make. `None` until the first `begin_turn`.
    pin_time: Option<SystemTime>,
    /// Accumulated Claude token usage for the CURRENT turn, summed across every
    /// assistant entry that carries a `message.usage` block (a turn with tool calls
    /// writes several, each billing its own input+output — summing is billing-correct).
    /// Attached to the `turn_complete` event so per-turn token counts ride the same
    /// rails as the response. Reset at `begin_turn` and at a new user-turn boundary.
    turn_in_tokens: u64,
    turn_out_tokens: u64,
    turn_cache_read_tokens: u64,
    turn_cache_creation_tokens: u64,
    turn_usage_seen: bool,
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
            pin_time: None,
            existing_at_pin: HashSet::new(),
            turn_in_tokens: 0,
            turn_out_tokens: 0,
            turn_cache_read_tokens: 0,
            turn_cache_creation_tokens: 0,
            turn_usage_seen: false,
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

    /// Set of all `.jsonl` paths in `dir` right now — captured at `begin_turn` so a
    /// roll target is recognised by absence (created after the pin). Only the dir
    /// listing is needed, so this works regardless of build target or mount type.
    fn snapshot_jsonl(dir: &Path) -> HashSet<PathBuf> {
        let mut set = HashSet::new();
        if let Ok(rd) = fs::read_dir(dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                    set.insert(p);
                }
            }
        }
        set
    }

    /// Detect a mid-turn session roll: the response is being written to a transcript
    /// that did not exist when this turn was pinned. Returns the roll target, or
    /// `None` if there isn't one.
    ///
    /// The discriminator is ABSENCE from the pin-time snapshot, not birth time.
    /// `poll`'s anti-flap guard exists to stop re-reading a *pre-existing* newer file
    /// from offset 0 and replaying its historical `end_turn` (the channel-turns-2+
    /// replay bug). A file that did NOT exist at pin time can't carry that hazard — it
    /// holds only this turn — so switching to it is the one safe cross-file move.
    /// We deliberately do NOT use `created()`: birth time is silently unreadable on
    /// static-musl builds and NFS/NAS mounts, and there it stranded the turn on the
    /// dead pinned file (the dormant-channel dark turn). Set-absence needs only the
    /// directory listing, which always works.
    fn find_rolled_transcript(&self) -> Option<PathBuf> {
        let pin_time = self.pin_time?;
        let cur = self.file.as_ref();
        let mut best: Option<(SystemTime, PathBuf)> = None;
        // DIAGNOSTIC (temp): record siblings written THIS turn that we declined to
        // re-anchor onto, so a dark turn shows exactly why the roll target was passed
        // over. Gated on mtime>pin below, so idle polls and stale corpses stay quiet.
        let ms = |t: SystemTime| t.duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let short = |p: &Path| p.file_stem().map(|s| s.to_string_lossy().chars().take(8).collect::<String>()).unwrap_or_default();
        let mut rejects: Vec<String> = Vec::new();
        for entry in fs::read_dir(&self.project_dir).ok()?.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                if Some(&p) == cur {
                    continue;
                }
                let md = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                // Only files touched since the pin are turn-relevant; skip corpses quietly.
                let mtime_after = md.modified().map(|t| t > pin_time).unwrap_or(false);
                if !mtime_after {
                    continue;
                }
                // A file ABSENT from the pin-time snapshot was created during this turn
                // — the one safe re-anchor target. No `created()` call, so no born_err.
                let is_new = !self.existing_at_pin.contains(&p);
                let conv = is_conversation_transcript(&p);
                if is_new && conv {
                    // Order by mtime (always readable) to pick the freshest roll target.
                    let key = md.modified().unwrap_or(pin_time);
                    if best.as_ref().map(|(t, _)| key > *t).unwrap_or(true) {
                        best = Some((key, p));
                    }
                } else {
                    rejects.push(format!(
                        "{}:is_new={} conv={} mtime_ms={}",
                        short(&p), is_new, conv,
                        md.modified().ok().map(ms).unwrap_or(0)
                    ));
                }
            }
        }
        if best.is_none() && !rejects.is_empty() {
            crate::diag::log(
                "RANCHOR_MISS",
                &format!(
                    "pin_ms={} cur={} rejects=[{}]",
                    ms(pin_time),
                    cur.map(|f| short(f)).unwrap_or_else(|| "<none>".into()),
                    rejects.join(", ")
                ),
            );
        }
        best.map(|(_, p)| p)
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
        // Stamp the pin moment so poll() can recognise a transcript born mid-turn
        // (a session roll) and distinguish it from a pre-existing newer file.
        self.pin_time = Some(SystemTime::now());
        // Snapshot the transcripts that exist RIGHT NOW. A mid-turn roll writes the
        // response to a brand-new file; `find_rolled_transcript` spots it by its
        // absence from this set — no dependence on `created()` birth time, which is
        // unreadable on static-musl / NAS and there leaves the turn dark.
        self.existing_at_pin = Self::snapshot_jsonl(&self.project_dir);
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
        self.reset_turn_usage();
    }

    /// Zero the per-turn token accumulator. Called at both turn-reset points
    /// (`begin_turn` and a new user-turn boundary) so usage never bleeds across turns.
    fn reset_turn_usage(&mut self) {
        self.turn_in_tokens = 0;
        self.turn_out_tokens = 0;
        self.turn_cache_read_tokens = 0;
        self.turn_cache_creation_tokens = 0;
        self.turn_usage_seen = false;
    }

    /// True while the JSONL transcript shows the turn still in progress. The TUI
    /// completion fallback must be blocked while this holds.
    pub fn turn_open(&self) -> bool {
        self.open
    }

    /// Advance to EOF of the active transcript and drop any in-flight turn state,
    /// WITHOUT re-pinning (`pin_time`/`existing_at_pin` are left as-is — those are the
    /// observed-turn roll machinery). Called the instant a turn completes so the
    /// organic watcher starts clean at the tail: it must NOT re-read the just-finished
    /// turn's trailing JSONL (e.g. a late `turn_complete` written after the TUI fallback
    /// already closed the turn — that would double-emit the same response as organic).
    pub fn prime_to_eof(&mut self) {
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
        self.pending_complete = false;
        self.reset_turn_usage();
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
            // ever written (channel turns 2+ silently lost).
            //
            // The ONE exception: a genuine mid-turn session roll. A `[resumed]`
            // session spins up a BRAND-NEW transcript and writes the response there,
            // leaving the pinned file silent forever — the turn goes dark and only
            // closes via the slow TUI quiescence fallback (stuck blinking cursor).
            // `find_rolled_transcript` returns a file ONLY if it was born after this
            // turn's pin, which means it cannot replay historical entries — so it is
            // safe to re-anchor onto it from offset 0 and read the real response.
            if let Some(rolled) = self.find_rolled_transcript() {
                crate::diag::log(
                    "ROLL",
                    &format!(
                        "from={} to={}",
                        self.file
                            .as_ref()
                            .map(|f| f.display().to_string())
                            .unwrap_or_else(|| "<none>".into()),
                        rolled.display()
                    ),
                );
                self.file = Some(rolled);
                self.pos = 0;
                // Fall through: read the rolled file from the start this same poll.
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
                    // Accumulate Claude's per-message token usage. Additive to the
                    // turn total; missing/non-numeric fields contribute 0. Independent
                    // of the completion gate below — purely records what the entry bills.
                    if let Some(u) = msg.get("usage") {
                        let get = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                        let (i, o, cr, cc) = (
                            get("input_tokens"),
                            get("output_tokens"),
                            get("cache_read_input_tokens"),
                            get("cache_creation_input_tokens"),
                        );
                        if i + o + cr + cc > 0 {
                            self.turn_in_tokens += i;
                            self.turn_out_tokens += o;
                            self.turn_cache_read_tokens += cr;
                            self.turn_cache_creation_tokens += cc;
                            self.turn_usage_seen = true;
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
                        self.reset_turn_usage();
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
            let mut ev = json!({"event": "turn_complete", "content": content});
            if self.turn_usage_seen {
                ev["usage"] = json!({
                    "input_tokens": self.turn_in_tokens,
                    "output_tokens": self.turn_out_tokens,
                    "cache_read_tokens": self.turn_cache_read_tokens,
                    "cache_creation_tokens": self.turn_cache_creation_tokens,
                });
            }
            events.push(ev);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_slug_unix_and_windows() {
        // Unix: leading `/` becomes a leading `-`.
        assert_eq!(
            cc_project_dir("/opt/lit-platform", Some("/cfg")).file_name().unwrap(),
            "-opt-lit-platform"
        );
        // Windows: drive-letter path → no leading dash (matches Claude's own slug).
        assert_eq!(
            cc_project_dir(r"C:\Users\ben\lbrs-e2e2", Some("/cfg")).file_name().unwrap(),
            "C--Users-ben-lbrs-e2e2"
        );
    }
}
