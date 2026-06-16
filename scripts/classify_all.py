import sys, pathlib
sys.path.insert(0, "/opt/lit-platform/lit-bridge")
from parsers.registry import select_parser
p = select_parser("claude-code")
d = pathlib.Path(sys.argv[1])
def esc(s): return s.replace('\n','\\n').replace('\t',' ')
for f in sorted(d.glob("*.txt")):
    t = f.read_text(encoding="utf-8", errors="replace")
    nr = esc(p.extract_new_response(0, t))
    rr = esc(p.extract_raw_response(0, t))
    print(f"{f.name}\t{p.detect_state(t).value}\t{p.count_assistant_messages(t)}\t{nr}\t{rr}")
