# Changelog

All notable changes to lit-bridge-rs are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
