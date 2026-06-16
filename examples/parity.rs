//! Parity harness: run the ported Rust parser over a directory of capture files
//! and print `<filename>\t<state>\t<assistant_msgs>` per file. Compared against the
//! Python parser's output (scripts/classify_all.py) to prove byte-for-byte parity.
//!
//! Usage: cargo run --example parity -- <dir-of-*.txt>

use std::fs;
use std::path::PathBuf;

use lit_bridge_rs::parser::{select_parser, TuiParser};

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let parser = select_parser("claude-code").expect("claude-code parser");

    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "txt").unwrap_or(false))
        .collect();
    files.sort();

    for path in files {
        // Lossy decode to match Python's read_text(errors="replace") — some corpus
        // files have truncated tails (SSM 24KB output cap) with invalid UTF-8.
        let bytes = fs::read(&path).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let state = parser.detect_state(&text).as_str();
        let n = parser.count_assistant_messages(&text);
        let esc = |s: String| s.replace('\n', "\\n").replace('\t', " ");
        let nr = esc(parser.extract_new_response(0, &text));
        let rr = esc(parser.extract_raw_response(0, &text, None, 0));
        let name = path.file_name().unwrap().to_string_lossy();
        println!("{name}\t{state}\t{n}\t{nr}\t{rr}");
    }
}
