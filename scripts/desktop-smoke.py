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

from guestlib import boot_desktop

shot = os.environ.get("SMOKE_SHOT", "desktop.png")
g = boot_desktop(["--display", "headless", "--screenshot", shot] + sys.argv[1:],
                 os.environ.get("SMOKE_LOG", "desktop-smoke.log"))
for extra in filter(None, os.environ.get("SMOKE_CMDS", "").split("\n")):
    g.run(extra, timeout=1800)
    g.mark(f"ran: {extra[:60]}")
# Keep the session's last screen (the shutdown messages replace it).
if os.path.exists(shot):
    shutil.copyfile(shot, os.path.splitext(shot)[0] + "-session.png")
g.send("sudo poweroff\n")
g.proc.wait(timeout=300)
g.mark("powered off")
