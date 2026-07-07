//! Golden regression for the re-flow pipeline on a REAL captured PTY stream (raw bytes
//! preserve the syntax-color + box-draw info that plain-text fixtures lose).
//!
//! The fixture `reflow_mixed_code_table.bin` is a captured turn containing a wrapping
//! prose paragraph, a Python code block (with a wrapped string literal), and a markdown
//! table. The invariants below lock in the three behaviors we care about:
//!   - prose is rejoined into one logical line,
//!   - code is preserved verbatim (never joined — even the wrapped string),
//!   - the box-drawn table is preserved verbatim.

use std::path::Path;

use lit_bridge_rs::reflow::{classify_row, reflow, RowKind};

/// Replay bytes through vt100, isolate the response region (bullet / 2-space indent),
/// classify each row, and reflow. Mirrors the production seam except for isolation,
/// which in production is the parser's existing response-region extraction.
fn reflow_capture(bytes: &[u8], cut: usize) -> Vec<String> {
    let mut p = vt100::Parser::new(50, 200, 0);
    p.process(&bytes[..bytes.len().min(cut)]);
    let screen = p.screen();
    let (rows, cols) = screen.size();

    let mut lines: Vec<(String, RowKind)> = Vec::new();
    for r in 0..rows {
        let kind = classify_row(&screen, r, cols);
        let t = screen.contents_between(r, 0, r, cols).trim_end().to_string();
        if let Some(rest) = t.strip_prefix("● ") {
            lines.push((rest.to_string(), kind));
        } else if let Some(rest) = t.strip_prefix("  ") {
            lines.push((rest.to_string(), kind));
        } else if t.is_empty() && !lines.is_empty() {
            lines.push((String::new(), RowKind::Blank));
        }
    }
    reflow(&lines, cols as usize - 2)
}

#[test]
fn mixed_code_table_reflow_invariants() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let bytes = std::fs::read(dir.join("reflow_mixed_code_table.bin")).expect("fixture present");
    let out = reflow_capture(&bytes, 46000);
    let joined = out.join("\n");

    // 1. Table preserved verbatim — the box-drawn header row survives intact.
    assert!(
        out.iter().any(|l| l.contains("│ Column A │ Column B │ Column C │")),
        "table header row must be preserved verbatim, got:\n{joined}"
    );
    // 2. Table rule rows (box-draw) preserved, not merged into prose.
    assert!(
        out.iter().any(|l| l.starts_with("┌") && l.contains("┬")),
        "table top rule must be preserved verbatim"
    );

    // 3. Code preserved verbatim: the wrapped string literal is NOT joined with a space
    //    into one line — its display wrap is kept (a verbatim row stays its own line).
    assert!(
        out.iter().any(|l| l.starts_with("message = \"this is a deliberately")),
        "code line must be present and start-anchored (not merged into prose)"
    );
    assert!(
        !out.iter().any(|l| l.contains("mid word when it exceeds the width\" url =")),
        "verbatim code rows must not be joined together"
    );

    // 4. Prose still reflows: the intro sentence is one logical line (no mid-word breaks
    //    like 'struc\ntural').
    assert!(
        out.iter().any(|l| l.contains("its rules and pipes are structural — must stay verbatim")),
        "prose line must be reflowed into one logical line, got:\n{joined}"
    );
}
