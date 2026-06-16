//! Claude Code parsers, versioned by TUI release.
//!
//! When Anthropic changes the Claude Code TUI enough to break parsing, add a new
//! `v2_2.rs` (etc.) alongside `v2_1.rs` and register it — never edit a validated
//! version in place. Each version is validated against its own capture corpus.

pub mod v2_1;
