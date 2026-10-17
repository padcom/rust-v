#!/usr/bin/env python3
"""Boots the Ubuntu desktop live ISO with a serial console, waits for GNOME to come up,
logs in on the serial console and runs SMOKE_CMDS (one per line), printing timings.
The display is headless; screenshots go to SMOKE_SHOT (default desktop.png), and the
session's last one to desktop-session.png.

    scripts/desktop-smoke.py [extra rust-v args...]
"""
import os
import shutil
import sys

from guestlib import UBUNTU, Guest

iso = os.path.join(UBUNTU, "ubuntu-26.10-desktop-riscv64.iso")
shot = os.environ.get("SMOKE_SHOT", "desktop.png")
args = ["-m", "8G", "--disk", iso + ",ro", "--display", "headless", "--screenshot", shot] + sys.argv[1:]
g = Guest(args, os.environ.get("SMOKE_LOG", "desktop-smoke.log"))
g.wait(r"Try or Install Ubuntu"); g.mark("GRUB")
# Boot the menu entry's kernel from GRUB's command line, with a serial console added.
g.send("c")
g.wait(r"grub> ")
g.send("linux /casper/vmlinux console=tty0 console=ttyS0 sysctl.kernel.watchdog_thresh=60 --- splash\n", delay=0.01)
g.wait(r"grub> ")
g.send("initrd /casper/initrd\n", delay=0.01)
g.wait(r"grub> ", timeout=900)
g.send("boot\n", delay=0.01)
g.wait(r"Linux version"); g.mark("Linux")
g.wait(r"login: ", timeout=1800); g.mark("serial login prompt")
g.login("ubuntu", "")
g.mark("logged in")
# Wait for the desktop session: GNOME Shell running for the live user.
g.run("until pgrep -u ubuntu -x gnome-shell >/dev/null; do sleep 2; done", timeout=1800)
g.mark("gnome-shell running")
for extra in filter(None, os.environ.get("SMOKE_CMDS", "").split("\n")):
    g.run(extra, timeout=1800)
    g.mark(f"ran: {extra[:60]}")
# Keep the session's last screen (the shutdown messages replace it).
if os.path.exists(shot):
    shutil.copyfile(shot, os.path.splitext(shot)[0] + "-session.png")
g.send("sudo poweroff\n")
g.proc.wait(timeout=300)
g.mark("powered off")
