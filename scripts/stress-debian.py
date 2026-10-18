#!/usr/bin/env python3
"""Stress test for lost interrupts and stalled devices: boots a fresh Debian image
(`rust-v debian`), then keeps four direct-I/O disk readers, two fsync'ing writers and apt's
network traffic going, and reports a hang when the guest's disk counters stop moving for a
minute (or it reports a hung task). The console log, with device-state dumps every 15 s
(RV_DEBUG_DUMP), is HOME/console.log.

    scripts/stress-debian.py HOME MINUTES      # HOME: the RUST_V_HOME to download into

Exit status 0 without a hang, 1 on a timeout, 2 on a hang.
"""
import os, re, subprocess, sys, threading, time
h, minutes = sys.argv[1], float(sys.argv[2])
env = dict(os.environ, RUST_V_HOME=h, RV_DEBUG_DUMP="15")
p = subprocess.Popen([os.environ.get("RUST_V_BIN", "target/release/rust-v"), "debian", "--fresh", "--ssh-port", "0"], stdin=subprocess.PIPE,
                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env)
out = bytearray(); log = open(os.path.join(h, "console.log"), "wb"); t0 = time.monotonic()
def rd():
    while True:
        b = p.stdout.read1(65536)
        if not b: return
        out.extend(b); log.write(b); log.flush()
threading.Thread(target=rd, daemon=True).start()
def say(s): print(f"{time.monotonic()-t0:7.1f}s  {s}", flush=True)
def send(s): p.stdin.write(s.encode()); p.stdin.flush()
def wait(pat, timeout, start=0):
    rx = re.compile(pat.encode()); end = time.monotonic() + timeout
    while time.monotonic() < end:
        m = rx.search(out, start)
        if m: return m
        if b"blocked for more than" in out: say("HUNG (hung task)"); p.kill(); sys.exit(2)
        if p.poll() is not None: break
        time.sleep(0.2)
    say(f"TIMEOUT {pat}"); p.kill(); sys.exit(1)
m = wait(r"Press any key to proceed", 900); send("\n"); pos = m.end()
while True:
    m = wait(r"(empty to skip[^:]*: |password again[^:]*: |login: )", 300, pos); pos = m.end()
    if m.group(0) == b"login: ": break
    ctx = out[max(0, m.start()-80):m.end()]
    time.sleep(0.5); send("rust-v-Test-1\n" if b"password" in ctx else "\n")
time.sleep(2); send("root\n"); m = wait(r"Password: ", 60, pos); time.sleep(0.5); send("rust-v-Test-1\n")
wait(r"root@[^:]*:~# ", 120, m.end()); say("logged in")
send("for i in 1 2 3 4; do (while true; do dd if=/dev/vda of=/dev/null bs=64k count=500 skip=$((RANDOM*4)) iflag=direct 2>/dev/null; done) & done; "
     "for i in 1 2; do (while true; do dd if=/dev/zero of=/root/f$i bs=1M count=32 oflag=direct conv=fsync 2>/dev/null; done) & done; "
     "(while true; do apt-get update -qq >/dev/null 2>&1; done) & "
     "(while true; do echo ALIVE $(awk '$3==\"vda\"{print $4, $8}' /proc/diskstats); sleep 10; done) &\n")
say("stress started")
last, since, hung = None, time.monotonic(), False; pos = len(out)
while time.monotonic() - t0 < minutes * 60:
    time.sleep(5)
    if b"blocked for more than" in out: say("HUNG (hung task)"); hung = True; break
    alive = re.findall(rb"ALIVE (\d+) (\d+)", out[pos:])
    if alive:
        cur = alive[-1]
        if cur != last: last, since = cur, time.monotonic()
        elif time.monotonic() - since > 60: say(f"HUNG (disk counters stuck at {cur})"); hung = True; break
    if p.poll() is not None: say("exited"); break
else:
    say(f"no hang; disk reads/writes completed: {last}")
time.sleep(20)  # one more device dump
p.kill()
sys.exit(2 if hung else 0)
