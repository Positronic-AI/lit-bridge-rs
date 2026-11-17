//! 2026-11-17 Harborview stage: a Team seat's fresh session paints a managed-
//! settings notice as a `●` row under the welcome banner. The parser read it as
//! a live response, so the startup paste gate never opened: the first message of
//! a new channel was held until the home gave up at 300 s, and so was every
//! resend. Screen below is the one the held session showed (attach-socket paint).
use lit_bridge_rs::parser::{select_parser, StartupAction, TuiParser};

const FRESH_TEAM_SEAT: &str = "\
 ▐▛███▛█   Claude Code v2.1.326
▝▜██████▀  Sonnet 5 · Claude Team
 ▝▝   ▝▝   ~/.lit/data/harborview/channels/v0-check

 Updated: 37 features, 358 fixes, 157 other changes
 code.claude.com/docs/en/changelog for details

● Your organization may apply global configuration to Claude Code. These settings are centrally managed by your IT administrator.

──────────────────────────────────────────────────
❯ Try \"fix lint errors\"
──────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

/// First turn of the same session, mid-reply: the banner is still on screen, but
/// a user line sits between it and the bullet, so the bullet is a real response.
const FIRST_TURN_REPLYING: &str = "\
 ▐▛███▛█   Claude Code v2.1.326
▝▜██████▀  Sonnet 5 · Claude Team
 ▝▝   ▝▝   ~/.lit/data/harborview/channels/v0-check

● Your organization may apply global configuration to Claude Code. These settings are centrally managed by your IT administrator.

❯ Prep me for the Riverbend call at two.

● Here is where the engagement stands. I am on Claude Code v2.1.326.

──────────────────────────────────────────────────
❯ 
──────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt
";

#[test]
fn a_notice_under_the_banner_is_not_a_response() {
    let p = select_parser("claude-code").unwrap();
    assert_eq!(p.detect_state(FRESH_TEAM_SEAT).as_str(), "idle",
        "a `●` row before any user message is a startup notice");
    assert_eq!(p.startup_action(FRESH_TEAM_SEAT), StartupAction::Prompt);
}

#[test]
fn a_reply_to_the_first_message_is_still_a_response() {
    let p = select_parser("claude-code").unwrap();
    assert_eq!(p.detect_state(FIRST_TURN_REPLYING).as_str(), "responding");
}
