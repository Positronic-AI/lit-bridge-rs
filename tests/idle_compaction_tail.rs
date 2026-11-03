//! 2026-11-03 #lit-platform: a hands CLI resumed after Claude Code's *idle
//! compaction* paints the compaction notice as the last `●` row with no `✻ …`
//! completion marker after it. If the parser reads that screen as a live
//! response, the startup paste gate never opens and every message is held.
use lit_bridge_rs::parser::{select_parser, StartupAction, TuiParser};

const RESUMED_AFTER_TURN: &str = "\
❯ Are we done with the windows vm for now?
● Good. Nothing on my side depended on it.
✻ Crunched for 37s · done Monday, Nov 2, 3:30 PM
──────────────────────────────────────────────────
❯ 
──────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

const RESUMED_AFTER_IDLE_COMPACTION: &str = "\
❯ Are we done with the windows vm for now?
● Good. Nothing on my side depended on it.
✻ Crunched for 37s · done Monday, Nov 2, 3:30 PM
● Compacted while idle, before the prompt cache expired · turn off with \"idleCompaction\": false in settings
──────────────────────────────────────────────────
❯ 
──────────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

#[test]
fn resumed_screen_after_a_finished_turn_is_idle() {
    let p = select_parser("claude-code").unwrap();
    assert_eq!(p.detect_state(RESUMED_AFTER_TURN).as_str(), "idle");
    assert_eq!(p.startup_action(RESUMED_AFTER_TURN), StartupAction::Prompt);
}

#[test]
fn resumed_screen_after_an_idle_compaction_is_idle() {
    let p = select_parser("claude-code").unwrap();
    assert_eq!(p.detect_state(RESUMED_AFTER_IDLE_COMPACTION).as_str(), "idle",
        "the compaction notice is not a live response");
    assert_eq!(p.startup_action(RESUMED_AFTER_IDLE_COMPACTION), StartupAction::Prompt);
}
