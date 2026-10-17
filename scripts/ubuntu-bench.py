#!/usr/bin/env python3
"""Boots the Ubuntu server image, runs scripts/bench-guest.sh inside it and prints the times.

    scripts/ubuntu-bench.py [extra rust-v args...]
"""
import os
import re
import sys

from guestlib import ROOT, UBUNTU, Guest

image = os.path.join(UBUNTU, "ubuntu-26.10-server-riscv64-pristine.img")
g = Guest(["-m", "4G", "--disk", image + ",snapshot", "--stats"] + sys.argv[1:],
          os.environ.get("SMOKE_LOG", "ubuntu-bench.log"))
g.wait(r"login: ")
# cloud-init sets the password after the first prompt appears; failed attempts lock the
# account for a while, so wait until it is done.
g.wait(r"Cloud-init v\. \S+ finished", timeout=900, anywhere=True)
g.mark("login prompt")
g.login("ubuntu", "ubuntu", "rust-v-Bench-1")
script = open(os.path.join(ROOT, "scripts", "bench-guest.sh")).read()
# Type the script into a file through a heredoc, then run it.
g.send("cat > /tmp/bench.sh <<'RUSTV_EOF'\n" + script + "RUSTV_EOF\n")
g.run("bash /tmp/bench.sh 2>&1 | tee /tmp/bench.out", timeout=1800)
with g.lock:
    text = g.out.decode(errors="replace")
for name, secs in re.findall(r"bench (\w+) (\d+\.\d+)", text):
    print(f"{name:8} {float(secs):8.2f} s")
g.send("sudo poweroff\n")
m = g.wait(r"(\[sudo\] password for ubuntu: |reboot: Power down)")
if b"sudo" in m.group(0):
    g.send("rust-v-Bench-1\n")
g.proc.wait(timeout=300)
with g.lock:
    stats = re.findall(r"rust-v: .*", g.out.decode(errors="replace"))
print("\n".join(stats))
