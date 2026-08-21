//! Startup-dialog decisions are read off the screen, never off a substring.
//! Regression corpus for the 2026-08-20 incident: a lingering "Yes, I accept"
//! above a LIVE prompt must never produce a keystroke.

use lit_bridge_rs::parser::{select_parser, StartupAction};
use std::path::Path;

fn parser() -> Box<dyn lit_bridge_rs::parser::TuiParser> {
    select_parser("claude-code").expect("claude-code parser")
}

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    String::from_utf8(std::fs::read(p).expect("fixture present")).unwrap()
}

const BYPASS: &str = "\
╭──────────────────────────────────────────────────────────────╮
│ WARNING: Claude Code running in Bypass Permissions mode      │
│                                                              │
│   In Bypass Permissions mode, Claude Code will not ask for   │
│   your approval before running potentially dangerous         │
│   commands.                                                  │
│                                                              │
│   By proceeding, you accept all responsibility for actions   │
│   taken while running in Bypass Permissions mode.            │
│                                                              │
│   ❯ 1. No, exit                                              │
│     2. Yes, I accept                                         │
│                                                              │
│   Enter to confirm · Esc to exit                             │
╰──────────────────────────────────────────────────────────────╯";

#[test]
fn bypass_dialog_active_quick_selects_accept_by_label() {
    let a = parser().startup_action(BYPASS);
    assert_eq!(
        a,
        StartupAction::Answer { keys: vec!["2".into()], dialog: "bypass-permissions".into() }
    );
}

#[test]
fn bypass_dialog_accept_already_highlighted_confirms_with_enter() {
    let cap = BYPASS
        .replace("❯ 1. No, exit     ", "  1. No, exit     ")
        .replace("  2. Yes, I accept", "❯ 2. Yes, I accept");
    let a = parser().startup_action(&cap);
    assert_eq!(
        a,
        StartupAction::Answer { keys: vec!["Enter".into()], dialog: "bypass-permissions".into() }
    );
}

#[test]
fn bypass_renumbered_still_picks_by_label() {
    // A CLI update swaps the options: "1. Yes, I accept / 2. No, exit".
    let cap = BYPASS
        .replace("❯ 1. No, exit     ", "❯ 1. Yes, I accept")
        .replace("  2. Yes, I accept", "  2. No, exit     ");
    let a = parser().startup_action(&cap);
    assert_eq!(
        a,
        StartupAction::Answer { keys: vec!["Enter".into()], dialog: "bypass-permissions".into() }
    );
}

#[test]
fn lingering_dialog_text_above_live_prompt_is_prompt_not_answer() {
    // The 2026-08-20 shape: the dismissed dialog is still on screen (ConPTY
    // repaint lag / retained buffer) but the prompt box is back at the bottom.
    let cap = format!(
        "{}\n\n✻ Welcome back!\n\n─────────────────────────────────────────────────────\n\n❯ \n",
        BYPASS
    );
    assert_eq!(parser().startup_action(&cap), StartupAction::Prompt);
}

#[test]
fn trust_dialog_fixture_confirms_highlighted_trust_option() {
    let cap = fixture("claude_2.1.x_dialog.txt");
    assert_eq!(
        parser().startup_action(&cap),
        StartupAction::Answer { keys: vec!["Enter".into()], dialog: "workspace-trust".into() }
    );
}

#[test]
fn trust_dialog_with_distrust_highlighted_moves_to_trust_by_digit() {
    let cap = fixture("claude_2.1.x_dialog.txt")
        .replace("❯ 1. I trust this folder", "  1. I trust this folder")
        .replace("  2. I don't trust", "❯ 2. I don't trust");
    assert_eq!(
        parser().startup_action(&cap),
        StartupAction::Answer { keys: vec!["1".into()], dialog: "workspace-trust".into() }
    );
}

#[test]
fn idle_prompt_is_prompt() {
    assert_eq!(parser().startup_action(&fixture("claude_2.1.x_idle.txt")), StartupAction::Prompt);
}

#[test]
fn unknown_picker_is_never_answered() {
    let cap = "\
  Which flavour?

  ❯ 1. Vanilla
    2. Chocolate

  Enter to confirm · Esc to cancel";
    assert_eq!(parser().startup_action(cap), StartupAction::Unknown);
}

#[test]
fn blank_screen_is_booting() {
    assert_eq!(parser().startup_action("   \n  \n"), StartupAction::Booting);
}

#[test]
fn responding_session_is_not_a_dialog() {
    assert_eq!(
        parser().startup_action(&fixture("claude_2.1.x_responding.txt")),
        StartupAction::Booting
    );
}
