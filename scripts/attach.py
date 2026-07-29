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

# Connect. A missing socket means this user/box has no rs bridge — defer to the
# fallback. But a socket FILE that exists yet refuses connection is a STALE orphan
# (a killed bridge that didn't unlink its socket). Don't fail silently on that: say
# so, and try the canonical /tmp path (the default when XDG_RUNTIME_DIR is unset)
# before giving up — that's exactly the case where a selector chose a dead socket in
# the shared runtime dir while the live bridge sits on /tmp.
def _connect(path):
    sk = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sk.connect(path)  # raises OSError if the file is missing OR has no live listener
    return sk

try:
    s = _connect(sock_path)
except OSError:
    s = None
    if os.path.exists(sock_path):
        import getpass
        alt = "/tmp/lit-bridge-rs-%s.sock.attach" % getpass.getuser()
        sys.stderr.write(
            "attach: %s exists but no live bridge (stale socket)%s\n"
            % (sock_path, ("; trying %s" % alt) if alt != sock_path else "")
        )
        if alt != sock_path:
            try:
                s = _connect(alt)
            except OSError:
                s = None
    if s is None:
        sys.exit(NOT_AVAILABLE)

selector = key if key.lstrip().startswith("{") else '{"session":"%s"}' % key


def _peek(sk):
    """First read after sending the selector: session screen, or a rejection."""
    r, _, _ = select.select([sk], [], [], 2.0)
    if not r:
        return b""
    return sk.recv(65536)


def _rejected(first):
    """True iff the daemon refused the attach. Match the exact rejection preamble —
    a bare substring test false-positives when the session's own SCREEN contains the
    words "no such session" (e.g. while debugging this very tooling)."""
    return first.startswith(b"\r\nlit-bridge-rs: no such session")


def _find_elsewhere(tried_path):
    """A live daemon may exist on another socket (XDG vs /tmp vs desktop) and hold
    the session. Ask each candidate with {"list":true} and return the first match."""
    import getpass
    import json
    user = getpass.getuser()
    cands = []
    xdg = os.environ.get("XDG_RUNTIME_DIR")
    if xdg:
        cands.append(os.path.join(xdg, "lit-bridge-rs-%s.sock.attach" % user))
    cands.append("/tmp/lit-bridge-rs-%s.sock.attach" % user)
    cands.append(os.path.expanduser(
        "~/.local/share/lit-desktop/run/lit-bridge-rs-%s.sock.attach" % user))
    plain = key if not key.lstrip().startswith("{") else None
    for path in cands:
        if path == tried_path or not os.path.exists(path):
            continue
        try:
            sk = _connect(path)
            sk.sendall(b'{"list":true}\n')
            r, _, _ = select.select([sk], [], [], 2.0)
            buf = sk.recv(65536) if r else b""
            sk.close()
            info = json.loads(buf.decode())
        except (OSError, ValueError):
            continue
        if plain and any(x.get("name") == plain for x in info.get("sessions", [])):
            return path
    return None


s.sendall(selector.encode() + b"\n")

# Peek before touching the terminal: a real session paints its screen immediately; a
# missing one gets "no such session" and a closed socket. Resolve which BEFORE raw mode so
# a fallback (tmux attach) inherits a clean terminal.
first = _peek(s)
if not first or _rejected(first):
    # This daemon is live but doesn't hold the session — another daemon might
    # (e.g. attach preferred the XDG socket while the session lives on /tmp).
    alt = _find_elsewhere(sock_path)
    if alt:
        sys.stderr.write("attach: session not on %s; found on %s\n" % (sock_path, alt))
        s.close()
        s = _connect(alt)
        s.sendall(selector.encode() + b"\n")
        first = _peek(s)
    if not first or _rejected(first):
        why = "bridge reports no such session" if first else "bridge closed the connection without data"
        print(f"attach: {why} for '{key}'", file=sys.stderr)
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
