#!/usr/bin/env python3
"""Checks clipboard sharing: boots the Ubuntu desktop live ISO (whose spice-vdagent is the
guest's agent), copies text on the host and reads it in the guest, then the other way round.

    scripts/clipboard-smoke.py [extra rust-v args...]

It uses the host clipboard, so run it where that is a throwaway one: on Linux under Xvfb
(packaging/virgl-test has xvfb and xclip). CLIP_COPY and CLIP_PASTE name the host commands
(default xclip); SMOKE_LOG names the console log.
"""
import os
import shlex
import subprocess
import sys
import time

from guestlib import boot_desktop

copy = shlex.split(os.environ.get("CLIP_COPY", "xclip -selection clipboard"))
paste = shlex.split(os.environ.get("CLIP_PASTE", "xclip -o -selection clipboard"))

g = boot_desktop(["--display", "headless", "--no-sound"] + sys.argv[1:],
                 os.environ.get("SMOKE_LOG", "clipboard-smoke.log"))
print(g.output("ls /dev/virtio-ports; pgrep -a spice-vdagent"), flush=True)
# The guest side goes through GTK on the session's X server, as spice-vdagent does.
g.run("export DISPLAY=:0 XAUTHORITY=$(ls /run/user/1000/.mutter-Xwaylandauth.* | head -1)")
gtk = 'import gi; gi.require_version("Gtk", "3.0"); from gi.repository import Gtk, Gdk, GLib; '
gtk += "c = Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD); "
failed = False

host_text = f"from the host {time.time():.0f}"
subprocess.run(copy, input=host_text.encode(), check=True)
time.sleep(3)
got = g.output(f"python3 -c '{gtk}print(c.wait_for_text())'").strip()
print(f"host -> guest: {got!r}")
failed |= got != host_text

guest_text = f"from the guest {time.time():.0f}"
g.run(f"(python3 -c '{gtk}c.set_text(\"{guest_text}\", -1); "
      "GLib.timeout_add_seconds(30, Gtk.main_quit); Gtk.main()' &)")
got = ""
for _ in range(20):
    time.sleep(1)
    got = subprocess.run(paste, capture_output=True).stdout.decode()
    if got == guest_text:
        break
print(f"guest -> host: {got!r}")
failed |= got != guest_text

g.send("sudo poweroff\n")
g.proc.wait(timeout=300)
g.mark("powered off")
sys.exit("clipboard sharing failed" if failed else 0)
