#!/usr/bin/env python3
"""Enumerate sessions on every lit-bridge-rs daemon (diagnostics).

Usage:
  python3 scripts/sessions.py                 # probe the standard socket locations
  python3 scripts/sessions.py <attach-sock>…  # probe specific attach sockets

Talks to the ATTACH socket (`<socket>.attach`) with a {"list":true} selector —
the main control socket can't answer ad-hoc queries because the mux holds it
exclusively. Requires a daemon built on/after 2026-07-28; older daemons answer
"no such session ''", which is reported as such.
"""
import getpass
import json
import os
import select
import socket
import sys

user = getpass.getuser()

def candidates():
    xdg = os.environ.get("XDG_RUNTIME_DIR")
    paths = []
    if xdg:
        paths.append(os.path.join(xdg, f"lit-bridge-rs-{user}.sock.attach"))
    paths.append(f"/tmp/lit-bridge-rs-{user}.sock.attach")
    paths.append(os.path.expanduser(
        f"~/.local/share/lit-desktop/run/lit-bridge-rs-{user}.sock.attach"))
    return paths

def query(path):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(3)
    s.connect(path)
    s.sendall(b'{"list":true}\n')
    buf = b""
    while b"\n" not in buf:
        r, _, _ = select.select([s], [], [], 3.0)
        if not r:
            break
        d = s.recv(65536)
        if not d:
            break
        buf += d
    s.close()
    return buf

any_seen = False
for path in sys.argv[1:] or candidates():
    if not os.path.exists(path):
        print(f"{path}\n  (no socket)")
        continue
    any_seen = True
    try:
        buf = query(path)
    except OSError as e:
        print(f"{path}\n  ERROR: {e}")
        continue
    print(path)
    if b"no such session" in buf:
        print("  daemon is running an OLD binary (no list support) — needs restart")
        continue
    try:
        info = json.loads(buf.decode())
    except ValueError:
        print(f"  unparseable reply: {buf[:120]!r}")
        continue
    sess = info.get("sessions", [])
    print(f"  daemon pid {info.get('pid')}, {len(sess)} session(s)")
    for x in sess:
        print(f"    {x['name']:48s} state={x['state']:12s} model={x.get('model')}")

if not any_seen:
    print("no bridge sockets found")
    sys.exit(3)
