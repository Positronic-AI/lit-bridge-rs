// End-to-end reflow pipeline on a captured PTY stream, using the lib's reflow module.
// Isolates the response region (bullet / 2-space indent), classifies each row, and
// reflows — prose rejoined, verbatim (code/table) preserved.
// Usage: cargo run --example wraptest -- <capture.bin> <byte_cut>
use lit_bridge_rs::reflow::{classify_row, reflow, RowKind};
use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let cut: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let mut data = Vec::new();
    std::fs::File::open(path).unwrap().read_to_end(&mut data).unwrap();
    let data = &data[..data.len().min(cut)];

    let mut p = vt100::Parser::new(50, 200, 0);
    p.process(data);
    let screen = p.screen();
    let (rows, cols) = screen.size();

    // Isolate the response region (bullet `● ` or 2-space continuation) and de-indent.
    let mut lines: Vec<(String, RowKind)> = Vec::new();
    for r in 0..rows {
        let kind = classify_row(&screen, r, cols);
        let text = screen.contents_between(r, 0, r, cols);
        let t = text.trim_end().to_string();
        if let Some(rest) = t.strip_prefix("● ") {
            lines.push((rest.to_string(), kind));
        } else if let Some(rest) = t.strip_prefix("  ") {
            lines.push((rest.to_string(), kind));
        } else if t.is_empty() && !lines.is_empty() {
            lines.push((String::new(), RowKind::Blank));
        }
        // else: TUI chrome (tool calls, status, input box) — skipped
    }

    let logical = reflow(&lines, (cols as usize) - 2);
    println!("=== reflowed logical lines (kind tag shows verbatim preservation) ===");
    for (text, kind) in &lines {
        let _ = (text, kind);
    }
    for l in &logical {
        // re-annotate for display: verbatim lines are exact grid rows, prose are joined
        println!("[{:>3}] {}", l.chars().count(), l);
    }
}
