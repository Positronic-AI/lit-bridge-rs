#!/usr/bin/env python3
"""Faithful replica of the platform's claude_interactive backend protocol use.
Replays the EXACT create/send command shapes and event-consumption logic from
lit-lib/.../backends/claude_interactive.py against lit-bridge-rs, and reconstructs
the response the way the real backend would (REPLACE / FINAL_REPLACE / chunk).

Proves protocol-layer drop-in compatibility WITHOUT touching the production socket.
Usage: python3 scripts/conformance.py <socket>
"""
import socket, json, threading, time, sys
sock = sys.argv[1] if len(sys.argv) > 1 else "/tmp/lbrs-conf.sock"
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(sock)
q=[]; lock=threading.Lock(); buf=b""
def reader():
    global buf; s.settimeout(1.0)
    while True:
        try:
            d=s.recv(65536)
            if not d: break
            buf+=d
            while b"\n" in buf:
                ln,buf=buf.split(b"\n",1)
                if ln.strip():
                    with lock: q.append(json.loads(ln))
        except socket.timeout: continue
        except OSError: break
threading.Thread(target=reader,daemon=True).start()
def send(o): s.sendall((json.dumps(o)+"\n").encode())
def get(timeout):
    end=time.time()+timeout
    while time.time()<end:
        with lock:
            if q: return q.pop(0)
        time.sleep(0.05)
    return None

SESSION="agenttest"
# --- exactly as backend: create ---
send({"cmd":"create","session":SESSION,"cli":"claude","parser":"claude-code",
      "args":["--model","opus"],"working_dir":"/tmp/lbrs-fresh2-1781560871",
      "channel_id":"big-think"})
# wait ready (drain stale)
ready=None
end=time.time()+30
while time.time()<end:
    e=get(end-time.time())
    if not e: break
    if e.get("event")=="ready": ready=e; break
    if e.get("event")=="error": print("CREATE ERROR:",e); sys.exit(1)
print("READY:", json.dumps(ready)[:120] if ready else "NONE")

# --- send ---
send({"cmd":"send","session":SESSION,"content":"In one short sentence, what is the lit platform?","channel_id":"big-think"})

# --- consume like the backend ---
final=None; streamed=""; markers=[]
end=time.time()+120
while time.time()<end:
    e=get(end-time.time())
    if not e: continue
    t=e.get("event")
    if t=="replace": streamed=e.get("text",""); markers.append("REPLACE")
    elif t=="chunk": streamed+=e.get("text",""); markers.append("chunk")
    elif t=="boundary": markers.append("BOUNDARY")
    elif t=="state": pass
    elif t in ("complete","paused"):
        final=e.get("content",""); markers.append("FINAL_REPLACE"); break
    elif t=="error": print("ERROR:",e.get("message")); break

print("event markers seen:", markers)
print("\n=== RECONSTRUCTED RESPONSE (what the backend would render) ===")
print(final if final is not None else streamed)
send({"cmd":"kill","session":SESSION,"store_resume":True}); time.sleep(0.3)
