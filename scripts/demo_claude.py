#!/usr/bin/env python3
"""Drive a real Claude session through lit-bridge-rs and print the response.
Start the daemon first:  ./target/debug/lit-bridge-rs --socket /tmp/lbrs.sock
Then:  python3 scripts/demo_claude.py /tmp/lbrs.sock "your message here"
Note: pass --model opus via args because this box's default (fable-5) is disabled.
"""
import socket, json, threading, time, sys
sock = sys.argv[1] if len(sys.argv) > 1 else "/tmp/lbrs.sock"
msg  = sys.argv[2] if len(sys.argv) > 2 else "Reply with exactly: hello there"
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(sock)
events=[]; lock=threading.Lock(); buf=b""
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
                    with lock: events.append(json.loads(ln))
        except socket.timeout: continue
        except OSError: break
threading.Thread(target=reader,daemon=True).start()
def send(o): s.sendall((json.dumps(o)+"\n").encode())
def drain():
    with lock: o=list(events); events.clear()
    return o
time.sleep(0.4)
send({"cmd":"create","session":"c","exe":"claude","args":["--model","opus"],"working_dir":"/tmp/lbrs-claude-test"})
time.sleep(10)
print(f">>> sending: {msg!r}")
send({"cmd":"send","session":"c","content":msg})
for _ in range(40):
    time.sleep(1)
    for e in drain():
        if e.get("event")=="complete":
            print("\n=== RESPONSE ===\n"+e.get("content","")); send({"cmd":"kill","session":"c"}); sys.exit(0)
print("(no completion in 40s)"); send({"cmd":"kill","session":"c"})
