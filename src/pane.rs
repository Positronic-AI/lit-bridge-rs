//! The CLI's diff panel. Claude Code (2.1.3xx) auto-opens a right-hand pane on a wide
//! terminal — "N files changed", a file list, a per-turn diff — split from the
//! conversation by a `│` border that runs from the top row down to the input box.
//! Our PTY is 200 columns, so every hands/home session gets one.
//!
//! The scrape joins full screen rows, so the panel's text landed beside the
//! response: "● Looking at the screenshot…   │ +sage 16:07:58 → …" was what the web
//! showed mid-stream (Ben's three screenshots, 2026-10-27 → 11-01). The settled
//! JSONL text was never affected; only the live frames were. Cut each row at the
//! border and the scrape sees the conversation pane alone.

use vt100::Screen;

/// Column of the diff-panel border, if a panel is open: a `│` in one column, in
/// the right half of the screen, running unbroken from row 0 over at least 60% of
/// the rows (the panel reaches from the top to the input box). A table in the
/// conversation would need a cell boundary in that column on the screen's first
/// row and thirty more below it to be mistaken for it.
pub fn border(screen: &Screen) -> Option<u16> {
    let (rows, cols) = screen.size();
    if rows < 10 {
        return None;
    }
    let bar = |r: u16, c: u16| screen.cell(r, c).map(|x| x.contents() == "│").unwrap_or(false);
    (cols / 2..cols).find(|&c| {
        if !bar(0, c) {
            return false;
        }
        let mut run = 0u16;
        while run < rows && bar(run, c) {
            run += 1;
        }
        usize::from(run) * 10 >= usize::from(rows) * 6
    })
}

/// The screen's text restricted to columns `0..width`, in the same shape as
/// `Screen::contents()` (one line per row, trailing blank rows dropped) so the
/// parser's row-index arithmetic is unchanged.
pub fn contents_left_of(screen: &Screen, width: u16) -> String {
    let mut out = String::new();
    for row in screen.rows(0, width) {
        out.push_str(&row);
        out.push('\n');
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_with(rows: u16, cols: u16, paint: impl Fn(&mut Vec<u8>)) -> vt100::Parser {
        let mut p = vt100::Parser::new(rows, cols, 0);
        let mut bytes = Vec::new();
        paint(&mut bytes);
        p.process(&bytes);
        p
    }

    fn at(b: &mut Vec<u8>, row: u16, col: u16, s: &str) {
        b.extend_from_slice(format!("\x1b[{};{}H{}", row + 1, col + 1, s).as_bytes());
    }

    #[test]
    fn finds_the_panel_border_and_cuts_rows_there() {
        let p = screen_with(50, 200, |b| {
            for r in 0..46u16 {
                at(b, r, 110, "│");
            }
            at(b, 1, 112, "6 files changed +18 -5");
            at(b, 13, 112, "  1 -# One runtime, three ways in");
            at(b, 19, 0, "● Looking at the screenshot to see the wrapping issue.");
            at(b, 20, 0, "  ⎿  Read 1 file");
            for r in [46u16, 48] {
                at(b, r, 0, &"─".repeat(200));
            }
        });
        let s = p.screen();
        assert_eq!(border(s), Some(110));
        let text = contents_left_of(s, 110);
        assert!(!text.contains("files changed"), "{text}");
        assert!(!text.contains('│'), "{text}");
        assert!(text.contains("● Looking at the screenshot to see the wrapping issue."));
        // Same row count as the full capture: the parser indexes rows into lines.
        assert_eq!(text.lines().count(), s.contents().lines().count());
    }

    #[test]
    fn no_panel_means_no_cut() {
        let p = screen_with(50, 200, |b| {
            at(b, 0, 0, "● A reply with a table");
            for r in 2..12u16 {
                at(b, r, 0, "│ a │ b │");
            }
        });
        assert_eq!(border(p.screen()), None);
    }

    #[test]
    fn a_table_that_does_not_start_at_the_top_is_not_a_panel() {
        let p = screen_with(50, 200, |b| {
            for r in 5..45u16 {
                at(b, r, 120, "│ cell");
            }
        });
        assert_eq!(border(p.screen()), None);
    }
}

#[cfg(test)]
mod corpus {
    //! Replays a recorded terminal paint (`LIT_PANE_PAINT=<file>`) — the attach
    //! socket's initial `capture_formatted` of a real session — and prints what
    //! the cut sees. Ignored unless the file is given.
    #[test]
    #[ignore]
    fn replay_recorded_paint() {
        let Ok(path) = std::env::var("LIT_PANE_PAINT") else { return };
        let bytes = std::fs::read(path).unwrap();
        let mut p = vt100::Parser::new(50, 200, 0);
        p.process(&bytes);
        let s = p.screen();
        let b = super::border(s);
        println!("border = {b:?}");
        let text = match b {
            Some(c) => super::contents_left_of(s, c),
            None => s.contents(),
        };
        let widest = text.lines().map(|l| l.chars().count()).max().unwrap_or(0);
        println!("rows = {}, widest row = {widest} chars", text.lines().count());
        for l in text.lines().skip(16).take(12) {
            println!("| {l}");
        }
        assert!(b.is_some());
    }
}
