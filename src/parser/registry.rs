//! Parser registry — name → parser dispatch. The Rust analogue of
//! `lit-bridge/parsers/registry.py`.
//!
//! Contributors: register a new parser by adding a match arm here pointing at your
//! `impl TuiParser`. Names are the values the platform sends in a `create` command's
//! `parser` field (e.g. "claude-code").

use super::claude::v2_1::ClaudeV21Parser;
use super::TuiParser;

/// Resolve a parser name to an implementation. Returns `None` for unknown names.
pub fn select_parser(name: &str) -> Option<Box<dyn TuiParser>> {
    match name {
        "claude-code" | "claude" => Some(Box::new(ClaudeV21Parser::new())),
        // Add new parsers here, e.g.:
        //   "gemini"     => Some(Box::new(super::gemini::v1::GeminiParser::new())),
        //   "codex"      => Some(Box::new(super::codex::v1::CodexParser::new())),
        //   "claude-2.2" => Some(Box::new(super::claude::v2_2::ClaudeV22Parser::new())),
        _ => None,
    }
}
