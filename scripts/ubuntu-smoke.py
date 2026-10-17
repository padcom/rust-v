#!/usr/bin/env python3
"""Boots the Ubuntu server image (first boot, writes kept in memory), logs in, runs
`apt update` and powers off, printing how long each step took.

    scripts/ubuntu-smoke.py [path/to/ubuntu.img] [extra rust-v args...]

SMOKE_LOG names the console log; SMOKE_CMDS holds extra commands to run after logging in,
one per line.
"""
import os
import sys

from guestlib import UBUNTU, Guest

args = sys.argv[1:]
image = args.pop(0) if args and not args[0].startswith("-") else os.path.join(
    UBUNTU, "ubuntu-26.10-server-riscv64-pristine.img")
PASSWORD = "rust-v-Smoke-1"

g = Guest(["-m", "4G", "--disk", image + ",snapshot"] + args, os.environ.get("SMOKE_LOG", "ubuntu-smoke.log"))
g.wait(r"U-Boot 20"); g.mark("U-Boot")
g.wait(r"Linux version"); g.mark("Linux")
g.wait(r"login: "); g.mark("login prompt")
# cloud-init sets the password after the first prompt appears; failed attempts lock the
# account for a while, so wait until it is done.
g.wait(r"Cloud-init v\. \S+ finished", timeout=900, anywhere=True)
g.login("ubuntu", "ubuntu", PASSWORD); g.mark("logged in")
g.run("uname -a; nproc; free -m | head -2")
for extra in filter(None, os.environ.get("SMOKE_CMDS", "").split("\n")):
    g.run(extra)
g.send("sudo apt-get update; echo apt-exit=$?\n")
m = g.wait(r"(\[sudo\] password for ubuntu: |apt-exit=)")
if b"sudo" in m.group(0):
    g.send(PASSWORD + "\n")
m = g.wait(r"apt-exit=(\d+)", timeout=900)
g.mark(f"apt-get update finished (exit {m.group(1).decode()})")
g.send("sudo poweroff\n")
g.proc.wait(timeout=300)
g.mark("powered off")
