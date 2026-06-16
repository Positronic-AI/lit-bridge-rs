# lit-bridge-rs

A native, cross-platform daemon that manages interactive AI CLI sessions under a real
PTY and streams structured events to your application. The Rust successor to
[lit-bridge](https://github.com/Positronic-AI/lit-bridge) — **same JSON-lines protocol, no tmux.**

**One static binary.** ConPTY on Windows, `forkpty`/`openpty` on Unix. Runs the CLI in a
real terminal — not a pipe.

## What it does

lit-bridge-rs sits between your application and an AI CLI (like Claude Code). It:

1. Spawns CLI sessions under a real PTY (no tmux, no shell wrapper)
2. Sends messages by typing into the terminal
3. Renders the TUI in an in-process terminal emulator and watches it for state changes
   (thinking, responding, idle)
4. Tails the CLI's JSONL transcript for authoritative turn completion and tool events
5. Streams structured JSON events back to your application over a Unix socket

Your application speaks a simple JSON-lines protocol. It never touches the PTY, never
parses terminal output, never manages processes.

## Why a rewrite

The Python lit-bridge leans on **tmux** for three things: hosting the PTY, providing a
queryable rendered screen (`capture-pane`), and keeping sessions alive across daemon
restarts. tmux is also the only reason `claude-interactive` is **Unix-only**.

lit-bridge-rs replaces tmux with:

- **[`portable-pty`](https://crates.io/crates/portable-pty)** (WezTerm) — ConPTY + Unix PTY behind one API. This is what unblocks **Windows**.
- **[`vt100`](https://crates.io/crates/vt100)** — an in-process terminal emulator that gives a rendered screen grid, the `capture-pane` replacement.

Session-survival-across-restart is intentionally dropped — the binary *is* the long-lived
per-user daemon, and recovery after a restart is the CLI's own `--resume` (which the
bridge already drives). One fewer moving part.

> **Why not pipe mode?** AI CLIs render rich TUIs and manage their own context, tool
> permissions, and MCP servers. Using `-p`/`--output-format json` strips that away and can
> reclassify subscription usage as programmatic. lit-bridge-rs deliberately drives the real
> TUI, just like its predecessor.

## Quick start

```bash
# Build
cargo build --release

# Start the daemon (defaults to /tmp/lit-bridge-rs.sock)
./target/release/lit-bridge-rs --socket /tmp/lit-bridge-rs.sock

# In another terminal, connect and interact:
# (using socat for the demo — your app would use a proper socket client)

# Create a session
echo '{"cmd":"create","session":"demo","cli":"claude","args":["--model","sonnet"]}' \
  | socat - UNIX-CONNECT:/tmp/lit-bridge-rs.sock

# Send a message
echo '{"cmd":"send","session":"demo","content":"What is the capital of France?"}' \
  | socat - UNIX-CONNECT:/tmp/lit-bridge-rs.sock

# Events stream back as JSON lines:
# {"session":"demo","event":"state","from":"idle","to":"thinking"}
# {"session":"demo","event":"replace","text":"● The capital of France is Paris."}
# {"session":"demo","event":"complete","content":"● The capital of France is Paris."}
```

## Protocol

lit-bridge-rs speaks JSON-lines: one JSON object per line, newline-delimited, over a Unix
domain socket. On connect, the daemon sends a `monitor_ready` handshake, then flushes any
events buffered while no client was attached.

### Commands (client → lit-bridge-rs)

| Command | Purpose |
|---|---|
| `create` | Start (or reuse) a CLI session |
| `send` | Send a message and observe the response |
| `input` | Type text + Enter without observing (slash commands like `/model`, `/clear`) |
| `keystroke` | Send named keys (`Up`, `Down`, `Enter`, …) for dialogs/menus |
| `list` | List sessions and their states |
| `dump` | Diagnostic snapshot of a session (rendered screen + parser counts) |
| `kill` | Terminate a session (optionally stash its id for `--resume`) |
| `ping` | Health check → `{"event":"pong"}` |

**`create`**

```json
{
  "cmd": "create",
  "session": "my-session",
  "cli": "claude",
  "args": ["--model", "opus"],
  "working_dir": "/path/to/project",
  "env": {"ANTHROPIC_API_KEY": "sk-..."},
  "channel_id": "optional-channel-name"
}
```

| Field | Required | Description |
|---|---|---|
| `session` | yes | Unique session name. |
| `cli` | no | CLI binary name. Default `"claude"`. (`exe` is also accepted.) |
| `args` | no | Additional CLI arguments. |
| `working_dir` | no | Working directory for the CLI process. |
| `env` | no | Environment variables to set. |
| `channel_id` | no | Sub-session key — `session:channel_id` are tracked independently. |

Response: `{"session":"my-session","event":"ready","state":"starting","model":"opus","resumed":false}`,
or `{"session":"my-session","event":"ready","reused":true,"model":"opus"}` if a live session is reused.

**`send`** — types the message and begins observing. Events stream back as the CLI responds.

```json
{"cmd": "send", "session": "my-session", "content": "Explain quicksort"}
```

**`kill`** — pass `"store_resume": true` to stash the CLI session id so the next `create`
appends `--resume <id>` (used for model switches: kill + recreate, conversation intact).

```json
{"cmd": "kill", "session": "my-session", "store_resume": true}
```

### Events (lit-bridge-rs → client)

| Event | Meaning |
|---|---|
| `monitor_ready` | Handshake on client connect (`sessions`, `buffered` counts) |
| `ready` | A `create` finished startup (dialogs dismissed, prompt reached) |
| `state` | State changed (`from`/`to`) |
| `replace` | Full current response text (replace, don't append) |
| `complete` | Turn finished — `content` is the authoritative text from the JSONL transcript |
| `tool_use` | CLI invoked a tool |
| `tool_result` | A tool returned |
| `dialog_dismissed` | A startup dialog was auto-dismissed during `create` |
| `killed` | Session terminated |
| `error` | Something went wrong (`message`) |

States: `starting`, `idle`, `thinking`, `responding`, `dialog`, `dead`.

The `replace` event always carries the **complete** current response, so your UI replaces
rather than appends — this handles tool calls, retries, and edits cleanly. `complete`
arrives from the JSONL transcript (authoritative); a TUI-quiescence fallback closes the
turn only if the transcript doesn't, and is tuned to fail *slower*, never *shorter*.

## Completion detection

Two corroborating paths (see [`docs/.../02-completion-detection.md`](../docs/plans/lit-bridge-rs/02-completion-detection.md)):

1. **JSONL transcript (authoritative).** Tailing the CLI's `.jsonl` transcript yields clean
   turn text and tool events as soon as the turn ends — fast and exact.
2. **TUI quiescence (fallback).** If the transcript isn't holding the turn open (no pending
   tool), and the rendered screen has been still past a quiescence window with the prompt
   visible, close the turn. Hard-capped so a turn never hangs forever. This never overrides
   the JSONL path — truncation is unacceptable, latency is fine.

## Live terminal attach (escape hatch)

The daemon also listens on `<socket>.attach`. A terminal client connects, sends a one-line
session selector (`{"session":"…","channel_id":"…"}` or a bare key), and then gets a
**bidirectional raw byte pipe** to the PTY: live output out, keystrokes in. On attach the
current rendered screen is painted so the terminal isn't blank. This is how the platform's
xterm.js "open terminal" button works — for slash commands, dismissing odd dialogs, and
diagnostics when scraping falls short.

The PTY is a **fixed size** (200×50) because the parser's unwrap logic depends on a known
width. An attaching browser terminal is a *view* of that fixed grid, not a resize of it.

See [`scripts/attach.py`](scripts/attach.py) for a reference client.

## Architecture

```
            ┌─→ vt100 emulator → parser/observer ──→ JSON event protocol (socket)
PTY output ─┤
            └─→ raw bytes ─────────────────────────→ attached terminal client(s) (.attach socket)
```

- **`src/main.rs`** — the daemon: socket server, command dispatch, 300ms observer loop, attach handler.
- **`src/session.rs`** — one PTY session: spawn, capture (rendered screen), send text/keys, JSONL tailing, broadcast tee for attach.
- **`src/jsonl.rs`** — JSONL transcript watcher: tool events + authoritative turn completion.
- **`src/parser/`** — version-aware TUI parser, parity-locked to the Python `claude/v2_1` parser.

## Requirements

- Rust (stable) to build, or a prebuilt binary
- A supported CLI installed and on PATH (e.g. `claude` from `@anthropic-ai/claude-code`) — plus its runtime (Node)
- **No tmux.** Works on Linux, macOS, and Windows (native ConPTY — no WSL).

## Status

Dogfooding. macOS/Linux parity with lit-bridge is the current bar; Windows-native and
packaging are in progress. Versioned `0.0.x` until the parity and packaging gates are met.

## License

[Business Source License 1.1](LICENSE). Free for non-production use; production use requires
a commercial license (licensing@positronic.ai). Converts to Apache 2.0 four years after each
version's release. Contributions are covered by the [CLA](CLA.md).
