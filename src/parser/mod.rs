//! Parser layer — a versioned, registry-dispatched set of TUI parsers.
//!
//! Structure (mirrors `lit-bridge/parsers/` and is built for open-source contributors):
//!
//! ```text
//! parser/
//!   mod.rs            base types + the `TuiParser` trait (this file)
//!   registry.rs       select_parser(name) -> Box<dyn TuiParser>
//!   claude/
//!     mod.rs
//!     v2_1.rs         ClaudeV21Parser (validated against Claude Code v2.1.x)
//! ```
//!
//! To add a parser for another CLI (gemini, codex, …) or a new Claude TUI version:
//!   1. create `parser/<cli>/<version>.rs` with a struct implementing [`TuiParser`],
//!   2. register a name for it in [`registry::select_parser`].
//!
//! See `src/parser/README.md` for the contributor guide and validation workflow.

pub mod claude;
pub mod registry;

pub use registry::select_parser;

use crate::reflow::{first_word_len, RowKind};

/// The lifecycle state of a CLI session as read from its rendered screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionState {
    Starting,
    Idle,
    Thinking,
    Responding,
    Dialog,
    Dead,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Starting => "starting",
            SessionState::Idle => "idle",
            SessionState::Thinking => "thinking",
            SessionState::Responding => "responding",
            SessionState::Dialog => "dialog",
            SessionState::Dead => "dead",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TuiMessage {
    pub role: &'static str, // "user" | "assistant"
    pub content: String,
    pub duration: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TuiContentBlock {
    pub typ: &'static str, // "text" | "tool_call" | "tool_output"
    pub content: String,
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TuiState {
    pub state: SessionState,
    pub messages: Vec<TuiMessage>,
    pub version: Option<String>,
    pub errors: Vec<String>,
}

/// The contract every CLI TUI parser implements. A parser turns a captured,
/// VT-rendered screen (the `tmux capture-pane` analogue) into structured state.
///
/// Implementations should be pure (no I/O) and deterministic, and must be
/// validated against the reference corpus — see `src/parser/README.md`.
pub trait TuiParser {
    /// Coarse lifecycle state of the session from the current screen.
    fn detect_state(&self, capture: &str) -> SessionState;

    /// Number of assistant-message bullets visible (the completion baseline).
    fn count_assistant_messages(&self, capture: &str) -> usize;

    /// All user/assistant messages parsed from the screen.
    fn extract_messages(&self, capture: &str) -> Vec<TuiMessage>;

    /// Structured content blocks (text / tool_call / tool_output).
    fn extract_content_blocks(&self, capture: &str) -> Vec<TuiContentBlock>;

    /// Assistant message content added since `baseline_count` bullets.
    fn extract_new_response(&self, baseline_count: usize, capture: &str) -> String;

    /// Faithful raw response capture (first new bullet → closing completion marker),
    /// with several fallback strategies for scrollback/landmark loss.
    fn extract_raw_response(
        &self,
        baseline_count: usize,
        capture: &str,
        sent_content: Option<&str>,
        baseline_completions: usize,
    ) -> String;

    /// Isolate the response region: the split screen lines plus the `[start, end)` row
    /// range of the current turn's response. Shared by `extract_raw_response` (which
    /// joins the range) and the reflow path (which re-flows it) so both use identical
    /// isolation logic.
    fn locate_response<'a>(
        &self,
        baseline_count: usize,
        capture: &'a str,
        sent_content: Option<&str>,
        baseline_completions: usize,
    ) -> (Vec<&'a str>, Option<usize>, usize);

    /// Like [`extract_raw_response`], but re-flows the located region: prose soft-wraps
    /// are rejoined into logical lines while structural markers (bullets `●`, tool
    /// results `⎿`, completions `✻`) and verbatim rows (code/tables, per `row_kinds`) are
    /// preserved. `row_kinds[i]` classifies grid row `i` (== screen line `i`, since Ink
    /// never triggers a soft-wrap); it must cover the located region or we fall back to
    /// the plain join. The un-wrap test is width-agnostic: the wrap width is taken as the
    /// widest line in the region (wrapped lines reach the terminal width). This is a
    /// default method shared by all parsers — it depends only on `locate_response`.
    fn extract_raw_response_reflowed(
        &self,
        baseline_count: usize,
        capture: &str,
        sent_content: Option<&str>,
        baseline_completions: usize,
        row_kinds: &[RowKind],
        content_width: usize,
    ) -> String {
        let (lines, start_idx, end_idx) =
            self.locate_response(baseline_count, capture, sent_content, baseline_completions);
        let si = match start_idx {
            Some(s) => s,
            None => return String::new(),
        };
        // Alignment guard: without kinds covering the region, fall back to the join.
        if row_kinds.len() < end_idx {
            return lines[si..end_idx].join("\n").trim_end().to_string();
        }
        // `content_width` is the terminal width the content was rendered at (the live
        // bridge PTY is a fixed 200). A prose display line was soft-wrapped iff the next
        // line's first word could not have fit within it.

        let mut out: Vec<String> = Vec::new();
        let mut cur: Option<String> = None; // open prose logical line (keeps its ●/indent)
        let mut last_len = 0usize; // char length of the LAST display line folded into cur
        for i in si..end_idx {
            let line = lines[i].trim_end();
            let t = line.trim_start();
            let is_blank = t.is_empty();
            let is_marker = t.starts_with('⎿') || t.starts_with('✻');
            let is_bullet = t.starts_with('●');
            let is_verbatim = row_kinds[i] == RowKind::Verbatim;

            if is_blank {
                if let Some(c) = cur.take() {
                    out.push(c);
                }
                out.push(String::new());
            } else if is_verbatim || is_marker {
                if let Some(c) = cur.take() {
                    out.push(c);
                }
                out.push(line.to_string()); // preserve exactly
            } else if is_bullet {
                if let Some(c) = cur.take() {
                    out.push(c);
                }
                cur = Some(line.to_string()); // new logical line, keep the "● " prefix
                last_len = line.chars().count();
            } else {
                // a "  " prose continuation — rejoin iff the PREVIOUS display line was
                // full (its first word couldn't have fit). Test the last display line's
                // length, NOT the accumulated logical line (which spans multiple).
                let wrapped =
                    cur.is_some() && last_len + 1 + first_word_len(t) > content_width;
                if wrapped {
                    let c = cur.as_mut().unwrap();
                    c.push(' ');
                    c.push_str(t);
                } else {
                    if let Some(c) = cur.take() {
                        out.push(c);
                    }
                    cur = Some(line.to_string());
                }
                last_len = line.chars().count();
            }
        }
        if let Some(c) = cur.take() {
            out.push(c);
        }
        out.join("\n").trim_end().to_string()
    }

    /// The active spinner/status line shown during a think-gap — e.g.
    /// `✽ Thinking… (esc to interrupt · 3s · ↑ 1.2k tokens)` — returned raw so the
    /// web can render the SAME shimmer as the terminal. `None` when no in-progress
    /// spinner is on screen (i.e. the response has started or the turn is done).
    fn extract_spinner_line(&self, capture: &str) -> Option<String>;

    /// True when the in-flight turn has produced a completion marker after the prompt.
    fn turn_complete(&self, capture: &str) -> bool;

    /// If a startup dialog is present, return (detect-substring, dismiss-keys, name).
    fn is_startup_dialog(&self, capture: &str) -> Option<(String, Vec<String>, String)>;

    /// Full structured parse (state + messages + version + errors).
    fn parse(&self, capture: &str) -> TuiState;
}
