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
