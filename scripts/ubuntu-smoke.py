#!/usr/bin/env python3
"""Boots the Ubuntu server image (first boot, writes kept in memory), logs in, runs
`apt update` and powers off, printing how long each step took.

    scripts/ubuntu-smoke.py [path/to/ubuntu.img] [extra rust-v args...]
"""
import os
import re
import subprocess
import sys
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GUEST = os.path.join(ROOT, "third_party", "guest")
args = sys.argv[1:]
image = args.pop(0) if args and not args[0].startswith("-") else os.path.join(
    ROOT, "third_party", "ubuntu", "ubuntu-26.10-server-riscv64.img")
LOG = os.environ.get("SMOKE_LOG", "ubuntu-smoke.log")
PASSWORD = "rust-v-Smoke-1"

cmd = [os.path.join(ROOT, "target", "release", "rust-v"),
       "--bios", os.path.join(GUEST, "fw_dynamic.bin"),
       "--kernel", os.path.join(GUEST, "u-boot.bin"),
       "-m", "4G", "--disk", image + ",snapshot"] + args
proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
out = bytearray()
lock = threading.Lock()
log = open(LOG, "wb")


def reader():
    while True:
        b = proc.stdout.read1(65536)
        if not b:
            return
        with lock:
            out.extend(b)
        log.write(b)
        log.flush()


threading.Thread(target=reader, daemon=True).start()
start = time.monotonic()
pos = 0


def wait(pattern, timeout=600):
    """Waits for regex `pattern` in the console output after the last match."""
    global pos
    deadline = time.monotonic() + timeout
    rx = re.compile(pattern.encode())
    while time.monotonic() < deadline:
        with lock:
            m = rx.search(out, pos)
        if m:
            pos = m.end()
            return m
        if proc.poll() is not None:
            break
        time.sleep(0.1)
    sys.exit(f"timed out waiting for {pattern!r} (see {LOG})")


def send(text):
    proc.stdin.write(text.encode())
    proc.stdin.flush()


def mark(what):
    print(f"{time.monotonic() - start:7.1f}s  {what}", flush=True)


wait(r"U-Boot 20"); mark("U-Boot")
wait(r"Linux version"); mark("Linux")
wait(r"login: "); mark("login prompt")
# cloud-init sets the password a little after the first login prompt appears.
for attempt in range(10):
    time.sleep(2)
    send("ubuntu\n")
    wait(r"Password: ")
    send("ubuntu\n")
    m = wait(r"(Current password: |\$ |Login incorrect)")
    if b"incorrect" not in m.group(0):
        break
    wait(r"login: ")
    time.sleep(3)
if b"Current" in m.group(0):
    send("ubuntu\n"); wait(r"New password: ")
    send(PASSWORD + "\n"); wait(r"Retype new password: ")
    send(PASSWORD + "\n")
wait(r"\$ "); mark("logged in")
send("uname -a; nproc; free -m | head -2\n")
wait(r"\$ ")
# Extra commands to run after logging in, separated by newlines.
for extra in filter(None, os.environ.get("SMOKE_CMDS", "").split("\n")):
    send(extra + "; echo cmd-done-$((6*7))\n")
    wait(r"cmd-done-42", timeout=900)
send("sudo apt-get update; echo apt-exit=$?\n")
m = wait(r"(\[sudo\] password for ubuntu: |apt-exit=)")
if b"sudo" in m.group(0):
    send(PASSWORD + "\n")
m = wait(r"apt-exit=(\d+)", timeout=900)
mark(f"apt-get update finished (exit {m.group(1).decode()})")
send("sudo poweroff\n")
proc.wait(timeout=300)
mark("powered off")
