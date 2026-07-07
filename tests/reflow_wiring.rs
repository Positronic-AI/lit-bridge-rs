//! Wiring test for `extract_raw_response_reflowed` on the parity fixtures. The text
//! fixtures carry no color, so we pass all-Prose row_kinds — this exercises the prose
//! un-wrap + structural-marker preservation (bullets/⎿/✻) without the verbatim path
//! (covered separately by the color-capture golden test).

use lit_bridge_rs::parser::{select_parser, TuiParser};
use lit_bridge_rs::reflow::RowKind;
use std::path::Path;

fn reflowed(file: &str) -> String {
    let parser = select_parser("claude-code").expect("parser");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let bytes = std::fs::read(dir.join(file)).expect("fixture present");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let n = text.split('\n').count();
    let kinds = vec![RowKind::Prose; n];
    parser.extract_raw_response_reflowed(0, &text, None, 0, &kinds, 80)
}

#[test]
fn responding_prose_is_rejoined_markers_kept() {
    let out = reflowed("claude_2.1.x_responding.txt");
    // wrapped prose is one logical line now (no "reading the\n  current structure")
    assert!(
        out.contains("Let me start by reading the current structure."),
        "wrapped prose must rejoin; got:\n{out}"
    );
    // structural markers preserved
    assert!(out.contains("● Reading parsers/claude.py"), "bullet preserved");
    assert!(out.lines().any(|l| l.trim_start().starts_with("⎿ 202 lines")), "⎿ result preserved");
    // the second paragraph's wrap also joined
    assert!(
        out.contains("with version-specific parsers and a registry for version matching."),
        "second paragraph must rejoin; got:\n{out}"
    );
}

#[test]
fn tool_use_list_items_stay_separate() {
    let out = reflowed("claude_2.1.x_tool_use.txt");
    // A wrapped list item rejoins its OWN continuation...
    assert!(
        out.contains("`ManagedSession` wraps a tmux session with its parser and observation state"),
        "list item 1 continuation must rejoin; got:\n{out}"
    );
    // ...but list items must NOT merge into each other (the last_len fix).
    assert!(
        out.lines().any(|l| l.trim_start().starts_with("2. **Event loop**")),
        "list item 2 must remain its own line; got:\n{out}"
    );
    assert!(
        out.lines().any(|l| l.trim_start().starts_with("5. **Idle reaping**")),
        "list item 5 must remain its own line; got:\n{out}"
    );
    // completion marker preserved
    assert!(out.lines().any(|l| l.starts_with("✻ 5.1s")), "✻ completion preserved");
}
