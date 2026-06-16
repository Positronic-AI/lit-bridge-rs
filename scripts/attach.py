#!/usr/bin/env python3
"""Raw terminal attach to a lit-bridge-rs session (the escape hatch).

Usage:
  python3 scripts/attach.py <attach-socket> <session-key>

  e.g.  python3 scripts/attach.py \
            "$XDG_RUNTIME_DIR/lit-bridge-rs-ben.sock.attach" \
            "lit-ben-claude-rs:big-think"

Gives an interactive raw terminal wired straight to the session's PTY: you see the live
output and your keystrokes go through (slash commands, dismiss a stuck dialog, debug when
the scraper falls short). Press Ctrl-\\ to detach.

The session key is  lit-<user>-<agent_id>[:<channel_id>]  (the daemon's session name).
The attach socket is the main socket path with `.attach` appended.

Exit status:
  0  attached and detached cleanly
  3  no rs bridge for this session (socket missing, or the daemon has no such session) —
     the caller can fall back to `tmux attach`. The terminal is left untouched in this case
     so the fallback renders cleanly.
"""
import socket
import sys
import select
import tty
import termios
import os

NOT_AVAILABLE = 3

if len(sys.argv) < 3:
    print(__doc__)
    sys.exit(1)

sock_path, key = sys.argv[1], sys.argv[2]

# Connect. A missing socket means this user/box has no rs bridge — defer to the fallback.
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect(sock_path)
except OSError:
    sys.exit(NOT_AVAILABLE)

selector = key if key.lstrip().startswith("{") else '{"session":"%s"}' % key
s.sendall(selector.encode() + b"\n")

# Peek before touching the terminal: a real session paints its screen immediately; a
# missing one gets "no such session" and a closed socket. Resolve which BEFORE raw mode so
# a fallback (tmux attach) inherits a clean terminal.
first = b""
r, _, _ = select.select([s], [], [], 2.0)
if r:
    first = s.recv(65536)
    if not first or b"no such session" in first:
        sys.exit(NOT_AVAILABLE)

old = termios.tcgetattr(0)
tty.setraw(0)
try:
    if first:
        os.write(1, first)
    while True:
        r, _, _ = select.select([0, s], [], [])
        if 0 in r:
            d = os.read(0, 1024)
            if not d or d == b"\x1c":  # Ctrl-\ detaches
                break
            s.sendall(d)
        if s in r:
            d = s.recv(65536)
            if not d:
                break
            os.write(1, d)
finally:
    termios.tcsetattr(0, termios.TCSADRAIN, old)
    sys.stdout.write("\r\n[detached]\r\n")
