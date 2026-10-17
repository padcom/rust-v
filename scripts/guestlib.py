"""Drives rust-v through its serial console, expect-style, for the smoke scripts."""
import os
import re
import subprocess
import sys
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GUEST = os.path.join(ROOT, "third_party", "guest")
UBUNTU = os.path.join(ROOT, "third_party", "ubuntu")


class Guest:
    def __init__(self, args, log):
        binary = os.environ.get("RUST_V_BIN", os.path.join(ROOT, "target", "release", "rust-v"))
        cmd = [binary,
               "--bios", os.path.join(GUEST, "fw_dynamic.bin"),
               "--kernel", os.path.join(GUEST, "u-boot.bin")] + args
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.STDOUT)
        self.out = bytearray()
        self.lock = threading.Lock()
        self.log_path = log
        self.log = open(log, "wb")
        self.pos = 0
        self.start = time.monotonic()
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        while True:
            b = self.proc.stdout.read1(65536)
            if not b:
                return
            with self.lock:
                self.out.extend(b)
            self.log.write(b)
            self.log.flush()

    def wait(self, pattern, timeout=600, anywhere=False):
        """Waits for regex `pattern` in the console output after the last match (or, with
        `anywhere`, at any point so far)."""
        deadline = time.monotonic() + timeout
        rx = re.compile(pattern.encode())
        while time.monotonic() < deadline:
            with self.lock:
                m = rx.search(self.out, 0 if anywhere else self.pos)
            if m:
                self.pos = max(self.pos, m.end())
                return m
            if self.proc.poll() is not None:
                break
            time.sleep(0.1)
        self.proc.kill()
        sys.exit(f"timed out waiting for {pattern!r} (see {self.log_path})")

    def send(self, text, delay=0.0):
        """Types `text`; with `delay`, one character at a time (for GRUB's slow input)."""
        data = text.encode()
        if delay:
            for i in range(len(data)):
                self.proc.stdin.write(data[i:i + 1])
                self.proc.stdin.flush()
                time.sleep(delay)
        else:
            self.proc.stdin.write(data)
            self.proc.stdin.flush()

    def run(self, command, timeout=900):
        """Runs a shell command at a prompt and waits for it to finish."""
        self.send(command + "; echo cmd-done-$((6*7))\n")
        self.wait(r"cmd-done-42", timeout=timeout)

    def mark(self, what):
        print(f"{time.monotonic() - self.start:7.1f}s  {what}", flush=True)

    def login(self, user, password, new_password=None):
        """Logs in on the serial console. Retries while the account is still being set up."""
        for _ in range(10):
            time.sleep(2)
            self.send(user + "\n")
            m = self.wait(r"(Password: |\$ )")
            if b"$" in m.group(0):
                return
            self.send(password + "\n")
            m = self.wait(r"(Current password: |\$ |Login incorrect)")
            if b"incorrect" not in m.group(0):
                break
            self.wait(r"login: ")
            time.sleep(3)
        if b"$" in m.group(0):
            return
        if b"Current" in m.group(0):
            self.send(password + "\n"); self.wait(r"New password: ")
            self.send(new_password + "\n"); self.wait(r"Retype new password: ")
            self.send(new_password + "\n")
        self.wait(r"\$ ")
