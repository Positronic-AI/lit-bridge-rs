//! Re-flow: reconstruct logical text from claude's Ink-rendered TUI grid.
//!
//! Ink pre-wraps its output to the terminal width and positions it with explicit cursor
//! moves, discarding the terminal soft-wrap flag — so vt100 sees pre-broken rows with
//! `wrapped() == false` everywhere, and `contents()` gives us text with the 200-col
//! display wrapping baked in as hard newlines. Relaying that verbatim is the "not pretty"
//! bug: it word-wraps in funny places at any other render width.
//!
//! We reconstruct the logical paragraphs by *undoing* Ink's word-wrap, while leaving
//! verbatim regions (code, tables, diffs) exactly as displayed. Two facts make this work,
//! both established empirically (see `examples/wraptest.rs` and the captured corpus):
//!   1. Prose word-wraps at spaces, so a line was soft-wrapped iff the FIRST WORD of the
//!      next line could not have fit on it. That un-wrap test reconstructs paragraphs to
//!      the byte (verified against the JSONL clean text).
//!   2. Verbatim regions are detectable in the grid: code/diffs carry syntax-colored
//!      cells (prose is uniformly white), and tables render with box-drawing characters
//!      (and are NOT colored) — so we need BOTH signals.
//!
//! Verbatim rows are preserved as-displayed (a rare >200-col code line stays visually
//! wrapped); the JSONL clean text remains the authority for the settled turn, so any
//! mid-stream imperfection is corrected when the turn completes.

use vt100::{Color, Screen};

/// Classification of a rendered grid row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// No content — a paragraph separator.
    Blank,
    /// Reflowable natural-language text (uniformly white/default).
    Prose,
    /// Code / table / diff — must keep its real line breaks.
    Verbatim,
}

const WHITE: Color = Color::Rgb(255, 255, 255);

/// Box Drawing (U+2500–U+257F) + Block Elements (U+2580–U+259F): tables and boxes.
fn is_boxdraw(c: char) -> bool {
    ('\u{2500}'..='\u{259F}').contains(&c)
}

/// True if a foreground color indicates syntax highlighting (anything other than the
/// prose default of white / terminal-default).
fn is_syntax_color(fg: Color) -> bool {
    fg != Color::Default && fg != WHITE
}

/// Classify a grid row. Verbatim iff any content cell is syntax-colored (code/diff) or
/// contains a box-drawing char (tables/boxes); prose is uniformly white/default.
pub fn classify_row(screen: &Screen, row: u16, cols: u16) -> RowKind {
    let mut has_content = false;
    let mut verbatim = false;
    for c in 0..cols {
        if let Some(cell) = screen.cell(row, c) {
            if !cell.has_contents() {
                continue;
            }
            has_content = true;
            if is_syntax_color(cell.fgcolor()) || cell.contents().chars().any(is_boxdraw) {
                verbatim = true;
            }
        }
    }
    if !has_content {
        RowKind::Blank
    } else if verbatim {
        RowKind::Verbatim
    } else {
        RowKind::Prose
    }
}

fn first_word_len(s: &str) -> usize {
    s.trim_start()
        .split(' ')
        .next()
        .map(|w| w.chars().count())
        .unwrap_or(0)
}

fn flush(cur: &mut String, out: &mut Vec<String>) {
    if !cur.is_empty() {
        out.push(std::mem::take(cur));
    }
}

/// Reconstruct logical lines from classified, de-indented response rows (top-to-bottom).
///
/// `content_width` is the box content width Ink wrapped to — the terminal columns minus
/// the response indent. A prose line was soft-wrapped iff the next line's first word
/// could not have fit within `content_width`.
pub fn reflow(lines: &[(String, RowKind)], content_width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (text, kind) in lines {
        match kind {
            RowKind::Blank => {
                flush(&mut cur, &mut out);
                out.push(String::new());
            }
            RowKind::Verbatim => {
                flush(&mut cur, &mut out);
                out.push(text.clone()); // preserve exactly — never join code/tables
            }
            RowKind::Prose => {
                if cur.is_empty() {
                    cur = text.clone();
                } else {
                    let wrapped =
                        cur.chars().count() + 1 + first_word_len(text) > content_width;
                    if wrapped {
                        cur.push(' ');
                        cur.push_str(text.trim_start());
                    } else {
                        flush(&mut cur, &mut out);
                        cur = text.clone();
                    }
                }
            }
        }
    }
    flush(&mut cur, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::RowKind::*;
    use super::*;

    // content width for these synthetic tests
    const W: usize = 40;

    fn prose(s: &str) -> (String, RowKind) {
        (s.to_string(), Prose)
    }
    fn verb(s: &str) -> (String, RowKind) {
        (s.to_string(), Verbatim)
    }
    fn blank() -> (String, RowKind) {
        (String::new(), Blank)
    }

    #[test]
    fn joins_wrapped_prose() {
        // line 1 is full (37 chars); next word "wrapped" (7) can't fit in 40 → join.
        let lines = vec![
            prose("the quick brown fox jumped over the lazy"),
            prose("dog and then kept running"),
        ];
        let out = reflow(&lines, W);
        assert_eq!(out, vec!["the quick brown fox jumped over the lazy dog and then kept running"]);
    }

    #[test]
    fn keeps_intentional_short_break() {
        // "short" (5) leaves room; the next word "next" (4) WOULD have fit → real break.
        let lines = vec![prose("short"), prose("next line stays separate")];
        let out = reflow(&lines, W);
        assert_eq!(out, vec!["short", "next line stays separate"]);
    }

    #[test]
    fn preserves_verbatim_and_flushes_prose() {
        let lines = vec![
            prose("intro paragraph before the code block here"),
            prose("continuing across the wrap boundary nicely"),
            verb("def f(x):"),
            verb("    return x + 1"),
        ];
        let out = reflow(&lines, W);
        assert_eq!(
            out,
            vec![
                "intro paragraph before the code block here continuing across the wrap boundary nicely",
                "def f(x):",
                "    return x + 1",
            ]
        );
    }

    #[test]
    fn blank_separates_paragraphs() {
        let lines = vec![prose("para one"), blank(), prose("para two")];
        let out = reflow(&lines, W);
        assert_eq!(out, vec!["para one", "", "para two"]);
    }

    #[test]
    fn boxdraw_detected() {
        assert!(is_boxdraw('│'));
        assert!(is_boxdraw('─'));
        assert!(is_boxdraw('┼'));
        assert!(!is_boxdraw('a'));
        assert!(!is_boxdraw('|'));
    }
}
