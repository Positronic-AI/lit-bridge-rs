//! Characterization/regression net for `extract_raw_response`, which had NO test
//! coverage. Freezes its current output on the parity fixtures so the upcoming
//! `locate_response` refactor (extracting the region-isolation logic to share it with
//! the reflow path) is provably behavior-preserving. These are the CURRENT outputs —
//! they intentionally include the 200-col display wrapping (e.g. "reading the\n  current
//! structure"), which is precisely what the reflow path will later un-wrap.

use lit_bridge_rs::parser::{select_parser, TuiParser};
use std::path::Path;

fn extract(file: &str) -> String {
    let parser = select_parser("claude-code").expect("parser");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let bytes = std::fs::read(dir.join(file)).expect("fixture present");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    parser.extract_raw_response(0, &text, None, 0)
}

#[test]
fn responding_fixture_unchanged() {
    assert_eq!(
        extract("claude_2.1.x_responding.txt"),
        "● I'll refactor the parser into versioned modules. Let me start by reading the\n  current structure.\n\n● Reading parsers/claude.py\n\n  ⎿ 202 lines\n\n● I'll create the new directory structure with version-specific parsers and a\n  registry for version matching."
    );
}

#[test]
fn tool_use_fixture_unchanged() {
    assert_eq!(
        extract("claude_2.1.x_tool_use.txt"),
        "● Reading lit-monitor/monitor.py\n\n  ⎿ 980 lines (ctrl+e to expand)\n\n● The monitor.py file is the core daemon for lit-monitor. Here are the key\n  components:\n\n  1. **Session management** — `ManagedSession` wraps a tmux session with its\n     parser and observation state\n  2. **Event loop** — polls tmux capture-pane at 300ms intervals\n  3. **State machine** — detects IDLE/THINKING/RESPONDING/DIALOG transitions\n  4. **Message extraction** — parses user/assistant messages from TUI output\n  5. **Idle reaping** — kills sessions idle >10min, stores for `--resume`\n\n✻ 5.1s (24.7k↓ tokens, 1.2k↑ tokens)"
    );
}

#[test]
fn idle_fixture_unchanged() {
    assert_eq!(
        extract("claude_2.1.x_idle.txt"),
        "● This is the LIT Platform, a workspace for AI-powered development. The project\n  includes several components:\n\n  - **lit-lib** — Python package with ML engine and agentic layer\n  - **lit-server** — Angular frontend\n  - **lit-monitor** — Terminal session management daemon\n  - **scripts/** — Deployment and utility scripts\n\n  The platform connects AI CLI tools (like Claude Code) to a web-based workspace\n  with channels, persistence, and collaboration features.\n\n✻ 3.2s (12.3k↓ tokens, 847↑ tokens)"
    );
}

#[test]
fn thinking_fixture_unchanged() {
    assert_eq!(
        extract("claude_2.1.x_thinking.txt"),
        "● This is the LIT Platform, a workspace for AI-powered development.\n\n✻ 3.2s (12.3k↓ tokens, 847↑ tokens)"
    );
}
