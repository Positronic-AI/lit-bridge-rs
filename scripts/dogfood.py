#!/usr/bin/env python3
"""Persistent-session dogfood for lit-bridge-rs — self-managing.

Launches its OWN lit-bridge-rs daemon as a subprocess, keeps ONE session alive
across multiple turns (so Claude writes a normal transcript and the JSONL
clean-content + completion-detection paths engage), then tears the daemon down.

Runs the completion-detection acceptance test from
docs/plans/lit-bridge-rs/02-completion-detection.md.

Usage: python3 scripts/dogfood.py [working_dir]
"""
import socket, json, threading, time, sys, os, subprocess, functools, signal, re, glob

print = functools.partial(print, flush=True)

REPO = "/opt/lit-platform/lit-bridge-rs"
BIN = f"{REPO}/target/debug/lit-bridge-rs"
SOCK = "/tmp/lbrs-dogfood.sock"
CWD = sys.argv[1] if len(sys.argv) > 1 else REPO

# Where Claude writes this cwd's transcript (config dir inherited from our env).
_cfg = os.environ.get("CLAUDE_CONFIG_DIR", os.path.expanduser("~/.claude"))
_slug = re.sub(r"[^a-zA-Z0-9]", "-", CWD.lstrip("/"))
PROJ = f"{_cfg}/projects/-{_slug}"


def show_transcripts(tag):
    files = sorted(glob.glob(PROJ + "/*.jsonl"), key=os.path.getmtime, reverse=True)
    desc = ", ".join(f"{os.path.basename(f)[:8]}={os.path.getsize(f)}b" for f in files[:6])
    print(f"  [transcripts @ {tag}] dir-exists={os.path.isdir(PROJ)} {len(files)} files: {desc}")

try:
    os.unlink(SOCK)
except FileNotFoundError:
    pass

# --- launch the daemon ---
errf = open("/tmp/lbrs-dogfood.err", "w")
daemon = subprocess.Popen([BIN, "--socket", SOCK], stdout=errf, stderr=errf,
                          start_new_session=True)
print(f"daemon pid {daemon.pid}, waiting for socket {SOCK} ...")

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(50):
    try:
        s.connect(SOCK)
        break
    except (FileNotFoundError, ConnectionRefusedError):
        time.sleep(0.2)
else:
    print("FATAL: daemon socket never came up")
    daemon.terminate()
    sys.exit(1)
print("connected.")

q = []
lock = threading.Lock()
buf = b""


def reader():
    global buf
    s.settimeout(1.0)
    while True:
        try:
            d = s.recv(65536)
            if not d:
                break
            buf += d
            while b"\n" in buf:
                ln, buf = buf.split(b"\n", 1)
                if ln.strip():
                    with lock:
                        q.append(json.loads(ln))
        except socket.timeout:
            continue
        except OSError:
            break


threading.Thread(target=reader, daemon=True).start()


def send(o):
    s.sendall((json.dumps(o) + "\n").encode())


def drain():
    with lock:
        o = list(q)
        q.clear()
    return o


def do_turn(label, content, timeout=180):
    print(f"\n===== TURN: {label} =====")
    print(f"  > {content[:90]}")
    send({"cmd": "send", "session": "df", "content": content})
    t0 = time.time()
    final = None
    streamed = ""
    tools = []
    while time.time() - t0 < timeout:
        time.sleep(0.5)
        for e in drain():
            et = e.get("event")
            if et == "replace":
                streamed = e.get("text", "")
            elif et == "chunk":
                streamed += e.get("text", "")
            elif et == "tool_use":
                tools.append(e.get("name", "?"))
                print(f"    [tool_use] {e.get('name')}  ({time.time()-t0:.1f}s)")
            elif et == "tool_result":
                print(f"    [tool_result] {len(str(e.get('content','')))} chars")
            elif et in ("complete", "paused"):
                final = e.get("content", "")
                break
            elif et == "error":
                print(f"    [error] {e.get('message')}")
                final = "<error>"
                break
        if final is not None:
            break
    dt = time.time() - t0
    body = final if final is not None else streamed
    clean = "●" not in (body or "") and "✻" not in (body or "")
    print(f"  -> completed in {dt:.1f}s | tools={tools} | clean(no chrome)={clean} | {len(body or '')} chars")
    print("  RESPONSE:")
    print("    " + (body or "<none>").replace("\n", "\n    ")[:1500])
    return body, tools


try:
    time.sleep(0.4)
    drain()
    print(f"create session in {CWD}")
    send({"cmd": "create", "session": "df", "cli": "claude",
          "args": ["--model", "opus", "--dangerously-skip-permissions"], "working_dir": CWD})
    time.sleep(10)
    for e in drain():
        if e.get("event") in ("ready", "error", "dialog_dismissed"):
            print(" ", json.dumps(e)[:100])
    show_transcripts("after-create")

    do_turn("clean-content check", "Reply with exactly: PONG", timeout=60)
    show_transcripts("after-turn1")

    do_turn(
        "completion-detection acceptance (preamble -> tools -> answer)",
        "Before answering, use your Read tool to read these files one at a time, "
        "thinking briefly between each: src/jsonl.rs, then src/session.rs, then "
        "src/parser/mod.rs. Then give a 3-sentence summary of how they fit together.",
        timeout=180,
    )
    show_transcripts("after-turn2")

    send({"cmd": "kill", "session": "df"})
    time.sleep(1)
finally:
    print("\n--- tearing down daemon ---")
    try:
        s.close()
    except OSError:
        pass
    daemon.send_signal(signal.SIGTERM)
    try:
        daemon.wait(timeout=5)
    except subprocess.TimeoutExpired:
        daemon.kill()
    errf.close()
    print("done.")
