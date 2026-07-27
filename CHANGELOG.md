# Changelog

All notable changes to lit-bridge-rs are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.2] — 2026-07-27

Everything since the initial preview: Windows-native maturity, live-stream parity
with the terminal, structured dialog relay, and reflow foundations.

### Added
- **Dialog relay**: modal questions (CLI system dialogs, resume/compact pickers) are
  detected, parsed into structured `question` events (digit-continuity option
  grouping), and answered back over the pipe as digit presses — held messages
  dispatch when the dialog clears, and Enter is never blindly pressed into a picker.
- **Compaction visibility**: the spinner + progress bar (`Compacting conversation…
  ▰▰▱▱ 17%`) relay as `thinking` events during the held-message window, and the
  dispatch gate is now `Idle && no active spinner` — never paste into a busy TUI.
- **Live spinner parity**: pre-body think-gap and mid-turn tool-gap spinner verbs
  stream to the client verbatim (web shows the same shimmer as the terminal).
- **Per-turn token usage** captured from the JSONL and forwarded on `complete`.
- **Organic turns**: un-observed (background) turns relay to the channel store.
- **Reflow** (opt-in, `LIT_BRIDGE_RS_REFLOW=<channel>`): un-wrap the Ink-rendered
  TUI grid into logical prose, scoped to a single channel for evaluation.
- **TCP-loopback transport** for native Windows (where Unix sockets aren't available):
  `--port` flag, control channel on N and raw-PTY attach on N+1, boxed async read/write
  halves behind a `Listener` enum (`#[cfg(unix)]` Unix arm + always-present TCP arm).
- **win32-input-mode** key encoding: the interactive Claude TUI requests
  `ESC[?9001h` and ignores legacy VT keystrokes, so input is encoded as win32 input
  records (down+up; `ENHANCED_KEY` on arrows; codepoints for text) whenever the mode is
  active. Enables driving the TUI (dialogs, message submission) under Windows ConPTY.
- Diagnostics module (`src/diag.rs`) emitting `RX`/`EMIT`/`TURN` traces when
  `LIT_BRIDGE_RS_EVENTLOG` is set, for observing the turn-completion path.

### Fixed
- Reliable turn completion: frame JSONL reads on newline boundaries so the final
  `end_turn` entry is never eaten by a partial read (was causing permanent stuck-open
  hangs on thinking→text straddles); defer `turn_complete` when an `end_turn` arrives
  with empty parts, awaiting the following text block, instead of firing prematurely.
- Windows submission hardening: bracketed paste for multi-line input, Ctrl+J
  newlines, submit-with-verification via the poll loop (Enter retried until the
  turn starts, never into a dialog), correct project-dir slug, bypass-permissions
  dialog dismissed via digit quick-select.
- Session hygiene: unlink sockets on exit + self-heal attach past a stale socket;
  reap killed children and idle sessions; self-rotate the diagnostic event log at
  64 MiB; re-anchor dormant-channel rolls by pin-time snapshot; mid-turn session
  roll + durable stale-frame gate (no more previous-response flash); don't freeze
  the live stream when the response bullet scrolls offscreen.

## [0.0.1] — 2026-06-16

Initial public preview. Native, cross-platform replacement for the Python + tmux
lit-bridge, speaking the identical JSON-lines protocol so it's a drop-in behind the
`claude-interactive` backend.

### Added
- PTY session host via `portable-pty` (ConPTY on Windows, `forkpty`/`openpty` on Unix) — no tmux.
- In-process terminal emulation via `vt100` (the `tmux capture-pane` replacement).
- JSON-lines command/event protocol over a Unix socket: `create`, `send`, `input`,
  `keystroke`, `list`, `dump`, `kill`, `ping`; `monitor_ready`/`ready`/`state`/`replace`/
  `complete`/`tool_use`/`tool_result`/`dialog_dismissed`/`killed`/`error` events.
- Authoritative JSONL-transcript completion detection, with a slower-never-shorter TUI
  quiescence fallback.
- Version-aware TUI parser, parity-locked to the Python `claude/v2_1` parser.
- Live PTY attach escape hatch on `<socket>.attach` — bidirectional raw byte pipe for the
  xterm.js "open terminal" button, with an initial screen paint. Reference client in
  `scripts/attach.py`.
- `--resume` support: stash a session id on `kill` (`store_resume`) to continue a
  conversation across a model-switch recreate.
- Single-client-at-a-time socket server with event buffering across disconnects.

### Known limitations
- No out-of-process session survival across daemon restarts (by design — `--resume` recovers).
- Packaging/distribution (platform binaries via the wheel) not yet wired up.
