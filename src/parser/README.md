# Parsers

A parser turns a **captured, VT-rendered screen** of a CLI's TUI into structured state
(idle / thinking / responding, the assistant's response text, tool calls, dialogs).
lit-bridge-rs scrapes the rendered terminal rather than using a CLI's pipe/JSON mode,
because pipe mode reclassifies subscription usage as programmatic. Parsers are where all
TUI-specific knowledge lives.

## Layout

```
parser/
  mod.rs            base types + the `TuiParser` trait
  registry.rs       select_parser(name) -> Box<dyn TuiParser>
  claude/
    mod.rs
    v2_1.rs         ClaudeV21Parser  (validated vs Claude Code v2.1.x)
```

This mirrors `lit-bridge/parsers/` on the Python side, which remains the reference spec.

## Add a parser

1. **Create the module.** For a new CLI: `parser/<cli>/mod.rs` + `parser/<cli>/v1.rs`.
   For a new version of an existing CLI: add `parser/claude/v2_2.rs` — never edit a
   validated version in place.
2. **Implement `TuiParser`** (see `mod.rs`) for your struct. Keep it pure and
   deterministic — no I/O. Use the existing `ClaudeV21Parser` as a worked example.
3. **Register a name** in `registry.rs` — the name is what the platform sends in a
   `create` command's `parser` field (e.g. `"claude-code"`, `"gemini"`).

## Validate

Every parser must be validated against a capture corpus, not just eyeballed.

- Record real captures for each state (idle, thinking, responding, tool_use, dialog).
- For ports, diff your parser's output against the reference (Python) parser over the
  same captures — see `scripts/classify_all.py` (reference) and `examples/parity.rs`
  (Rust), and lock the result in a test like `tests/parity.rs`.

The bar: identical `detect_state`, message counts, and extracted response strings across
the corpus.
