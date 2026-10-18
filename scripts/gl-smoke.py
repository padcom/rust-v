#!/usr/bin/env python3
"""Checks 3D acceleration (--gpu virgl) quickly, without a desktop: boots a copy of the Ubuntu
server image with mesa-utils and kmscube installed, prints the guest's OpenGL renderer and
runs kmscube on the console's DRM device (screenshot in gl-smoke-kmscube.png).

    scripts/gl-smoke.py [extra rust-v args...]

The copy (GL_IMAGE, default third_party/ubuntu/gl-test.img) is made from the pristine image
and set up on the first run; later runs keep their writes in memory. SMOKE_LOG names the
console log, SMOKE_SHOT the screenshot (gl-smoke.png); SMOKE_CMDS adds commands.
"""
import os
import shutil
import subprocess
import sys
import time

from guestlib import UBUNTU, Guest

PASSWORD = "rust-v-Smoke-1"
image = os.environ.get("GL_IMAGE", os.path.join(UBUNTU, "gl-test.img"))
log = os.environ.get("SMOKE_LOG", "gl-smoke.log")
shot = os.environ.get("SMOKE_SHOT", "gl-smoke.png")


def boot(disk, extra):
    g = Guest(["-m", "4G", "--disk", disk] + extra, log)
    g.wait(r"login: ", timeout=900); g.mark("login prompt")
    return g


def sudo(g, command, timeout=1800):
    g.run(f"echo {PASSWORD} | sudo -S sh -c '{command}'", timeout=timeout)


if not os.path.exists(image):
    pristine = os.path.join(UBUNTU, "ubuntu-26.10-server-riscv64-pristine.img")
    # A copy-on-write clone where the file system can (APFS, Btrfs, XFS).
    if subprocess.run(["cp", "-c", pristine, image], stderr=subprocess.DEVNULL).returncode != 0:
        shutil.copyfile(pristine, image)
    os.chmod(image, 0o644)
    g = boot(image, [])
    g.wait(r"Cloud-init v\. \S+ finished", timeout=900, anywhere=True)
    g.login("ubuntu", "ubuntu", PASSWORD); g.mark("first login")
    sudo(g, "apt-get update -q && DEBIAN_FRONTEND=noninteractive apt-get install -y -q mesa-utils kmscube")
    g.mark("installed mesa-utils and kmscube")
    g.send("sudo poweroff\n")
    g.proc.wait(timeout=300)

g = boot(image + ",snapshot", ["--display", "headless", "--screenshot", shot] + sys.argv[1:])
g.login("ubuntu", PASSWORD); g.mark("logged in")
sudo(g, 'eglinfo -B -p surfaceless 2>&1 | grep -E "OpenGL (core|ES) profile (renderer|version)"')
g.mark("eglinfo")
# 300 frames of kmscube (in the background; it stops at input on stdin, so it gets one that
# stays quiet), with a screenshot of it running; at most 100 s.
sudo(g, "(sleep 120 | timeout 100 kmscube -c 300 >/tmp/kmscube.txt 2>&1 &)")
time.sleep(6)
if os.path.exists(shot):
    shutil.copyfile(shot, os.path.splitext(shot)[0] + "-kmscube.png")
g.run("while pgrep -x kmscube >/dev/null; do sleep 1; done; tail -2 /tmp/kmscube.txt", timeout=300)
g.mark("kmscube")
for extra in filter(None, os.environ.get("SMOKE_CMDS", "").split("\n")):
    g.run(extra, timeout=1800)
    g.mark(f"ran: {extra[:60]}")
g.send("sudo poweroff\n")
g.proc.wait(timeout=300)
g.mark("powered off")
