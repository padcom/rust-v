#!/usr/bin/env python3
"""End-to-end check of the MCP server (`--mcp`) with Debian: boots a fresh image
(`rust-v debian --fresh`) with a headless display and the MCP server on HTTP, then, through
MCP only, answers Debian's first-boot questions and logs in on the console, runs commands
with console_run, installs the QEMU guest agent and uses guest_exec and the file tools, and
types on the display's console and checks it on a screenshot.

    scripts/mcp-smoke.py HOME [PORT]      # HOME: the RUST_V_HOME to download into

Exit status 0 when everything worked. The emulator's output is in HOME/mcp-smoke.log.
"""
import json, os, re, subprocess, sys, time, urllib.request

home = sys.argv[1]
port = int(sys.argv[2]) if len(sys.argv) > 2 else 7731
url = f"http://127.0.0.1:{port}/mcp"
password = "rust-v-Test-1"
env = dict(os.environ, RUST_V_HOME=home)
os.makedirs(home, exist_ok=True)
log = open(os.path.join(home, "mcp-smoke.log"), "wb")
p = subprocess.Popen(
    [os.environ.get("RUST_V_BIN", "target/release/rust-v"), "debian", "--fresh", "--ssh-port", "0",
     "--", "--display", "headless", "--mcp", str(port)],
    stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, env=env)
t0 = time.monotonic()
ids = 0


def say(s):
    print(f"{time.monotonic() - t0:7.1f}s  {s}", flush=True)


def fail(s):
    say(f"FAIL {s}")
    p.kill()
    sys.exit(1)


def rpc(method, params=None, notify=False):
    global ids
    msg = {"jsonrpc": "2.0", "method": method, "params": params or {}}
    if not notify:
        ids += 1
        msg["id"] = ids
    req = urllib.request.Request(url, json.dumps(msg).encode(),
                                 {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"})
    with urllib.request.urlopen(req, timeout=900) as r:
        body = r.read()
        return json.loads(body) if body else r.status


def tool(name, **args):
    r = rpc("tools/call", {"name": name, "arguments": args})
    if "error" in r:
        fail(f"{name}: {r['error']}")
    return r["result"]


def text(result):
    return "\n".join(c["text"] for c in result["content"] if c["type"] == "text")


def ok(name, **args):
    r = tool(name, **args)
    if r["isError"]:
        fail(f"{name}: {text(r)}")
    return text(r)


# The download takes a while on the first run; the server is up once the machine starts.
while True:
    try:
        init = rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                  "clientInfo": {"name": "mcp-smoke", "version": "1"}})
        break
    except OSError:
        if p.poll() is not None:
            fail("rust-v exited")
        time.sleep(1)
assert init["result"]["protocolVersion"] == "2025-06-18", init
assert rpc("notifications/initialized", notify=True) == 202
names = [t["name"] for t in rpc("tools/list")["result"]["tools"]]
say(f"initialized; tools: {' '.join(names)}")
for n in ["console_run", "guest_exec", "screenshot", "type_text", "mouse_click"]:
    assert n in names, n

# First boot, through console_read and console_write only.
seen = ""
def until(pattern, timeout):
    global seen
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        out = ok("console_read", wait_seconds=10)
        if out != "(no new output)":
            seen += out
        m = re.search(pattern, seen)
        if m:
            before = seen[max(0, m.start() - 120):m.start()]
            seen = seen[m.end():]
            return m, before
    fail(f"no {pattern!r} on the console; last output:\n{seen[-2000:]}")

until(r"Press any key to proceed", 900)
ok("console_write", text="\n")
while True:
    m, before = until(r"(empty to skip[^:]*: |password again[^:]*: |login: )", 300)
    if m.group(0) == "login: ":
        break
    time.sleep(0.5)
    ok("console_write", text=password if "password" in before + m.group(0) else "", enter=True)
time.sleep(2)
ok("console_write", text="root", enter=True)
until(r"Password: ", 60)
time.sleep(0.5)
ok("console_write", text=password, enter=True)
until(r"root@[^:]*:~# ", 120)
say("logged in through console_write")

out = ok("console_run", command="uname -m; echo $((6*7))")
if not re.search(r"exit status 0\nriscv64\n42", out):
    fail(f"console_run: {out!r}")
out = ok("console_run", command="ls /nonexistent")
if not out.startswith("exit status 2"):
    fail(f"console_run status: {out!r}")
say("console_run works")

r = tool("guest_exec", command="true")
if not r["isError"] or "not running" not in text(r):
    fail(f"guest_exec without an agent: {text(r)}")
out = ok("console_run", command="DEBIAN_FRONTEND=noninteractive apt-get update -qq && apt-get install -y -qq qemu-guest-agent >/dev/null 2>&1; systemctl start qemu-guest-agent; systemctl is-active qemu-guest-agent", timeout_seconds=900)
if "active" not in out:
    fail(f"installing the guest agent: {out}")
say("guest agent installed")
for _ in range(30):
    if "guest agent: running" in ok("machine_status"):
        break
    time.sleep(1)
else:
    fail("machine_status never reported the agent")

out = ok("guest_exec", command="id -un; cat; echo err >&2; exit 3", input="from stdin\n")
if out != "exit status 3\n--- stdout\nroot\nfrom stdin\n\n--- stderr\nerr\n\n":
    fail(f"guest_exec: {out!r}")
content = "line one\nünïcödé\n" + "x" * 3_000_000
ok("guest_write_file", path="/root/mcp.txt", content=content)
ok("guest_write_file", path="/root/mcp.txt", content="tail\n", append=True)
if ok("guest_read_file", path="/root/mcp.txt") != content + "tail\n":
    fail("guest file round trip")
if str(len((content + "tail\n").encode())) not in ok("guest_exec", command="wc -c < /root/mcp.txt"):
    fail("file size in the guest")
say("guest_exec and files work")

# The display shows tty1's login prompt: log in there with the keyboard.
shot = tool("screenshot", scale=0.5)
img = next(c for c in shot["content"] if c["type"] == "image")
png = __import__("base64").b64decode(img["data"])
assert png[:8] == b"\x89PNG\r\n\x1a\n", png[:8]
open(os.path.join(home, "mcp-screen.png"), "wb").write(png)
ok("type_text", text="root\n")
time.sleep(2)
ok("type_text", text=password + "\n")
time.sleep(4)
ok("type_text", text="echo MCP-KEYS-$((40+2)) > /root/keys.txt\n")
ok("press_keys", keys="ctrl+l")
time.sleep(3)
if "MCP-KEYS-42" not in ok("guest_exec", command="cat /root/keys.txt"):
    fail("typing on the display's console")
ok("mouse_click", x=10, y=10)
ok("mouse_scroll", x=10, y=10, amount=-2)
say(f"screenshot ({len(png)} bytes PNG, {text(shot)}), keyboard and mouse work")

try:
    tool("guest_exec", command="poweroff", timeout_seconds=5)
except OSError:
    pass  # the machine may be gone before it answers
try:
    p.wait(120)
except subprocess.TimeoutExpired:
    fail("no power-off")
say("PASS")
