"""Check monitor controls: python3 -I tests/monitor_cli_pty.py target/debug/cc-proxy.

Uses fake API-key accounts and a separate local proxy. No provider is contacted.
"""

import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import urllib.request
import uuid


binary = str(Path(sys.argv[1]).resolve())


def wait_for(check, description):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.05)
    raise AssertionError(description)


class Monitor:
    def __init__(self, env, *args, width=120, height=36):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
            os.execve(binary, [binary, *args], env)
        self.output = b""
        self.mark = 0
        self.exited = False

    def read(self):
        if select.select([self.fd], [], [], 0.05)[0]:
            try:
                self.output += os.read(self.fd, 65536)
            except OSError:
                pass

    def text(self, expected):
        def found():
            self.read()
            return expected.encode() in self.output[self.mark:]
        wait_for(found, f"Missing {expected!r}: {self.output[-12000:]!r}")

    def wait(self, check, description):
        def ready():
            self.read()
            return check()
        try:
            wait_for(ready, description)
        except AssertionError:
            raise AssertionError(f"{description}: {self.output[-12000:]!r}") from None

    def keys(self, keys):
        self.mark = len(self.output)
        os.write(self.fd, keys)

    def close(self):
        self.keys(b"q")
        def finished():
            self.read()
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.exited = True
                assert os.waitstatus_to_exitcode(status) == 0, self.output
            return self.exited
        try:
            wait_for(finished, "Monitor did not close")
            assert b"fake-secret" not in self.output, "A saved key was printed"
        finally:
            if not self.exited:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
            os.close(self.fd)


with tempfile.TemporaryDirectory(prefix="ccp-monitor-") as tmp:
    root = Path(tmp)
    config = root / "config"
    config.mkdir()
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    env = dict(PATH="/usr/bin:/bin", HOME=tmp, USERPROFILE=tmp, TERM="xterm-256color",
               CCP_CONFIG_DIR=str(config), CLAUDE_CONFIG_DIR=str(root / "claude"),
               XDG_STATE_HOME=str(root / "state"), XDG_CONFIG_HOME=str(root / "xdg"))
    provider = config / "ztest"
    (provider / "accounts").mkdir(parents=True)
    accounts = [{"id": str(uuid.uuid4()), "name": name} for name in ["home", "work"]]
    index = provider / "accounts.json"
    index.write_text(json.dumps({"current": "home", "accounts": accounts}))
    for account in accounts:
        (provider / "accounts" / f"{account['id']}.json").write_text(json.dumps({"apiKey": "fake-secret"}))
    file = config / "config.json"
    file.write_text(json.dumps({
        "port": port,
        "providers": {name: {"baseUrl": "http://127.0.0.1:9/v1", "models": ["tiny"], "apiKey": "fake-secret"}
                      for name in ["ytest", "ztest"]},
        "route": {"on": True, "provider": "ztest", "fallback": [],
                  "tiers": {name: {tier: "tiny" for tier in ["fable", "opus", "sonnet", "haiku"]}
                            for name in ["ytest", "ztest"]}},
    }))
    read = lambda: json.loads(file.read_text())
    url = f"http://127.0.0.1:{port}"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    def alive():
        try:
            with opener.open(f"{url}/healthz", timeout=0.2) as response:
                return response.status == 200
        except OSError:
            return False

    with (root / "server.log").open("wb") as log:
        server = subprocess.Popen([binary, "start", "--no-monitor"], env=env, stdout=log, stderr=log)
        try:
            wait_for(alive, "Isolated proxy did not start")
            monitor = Monitor(env, "monitor")
            try:
                monitor.text("work")
                # Home opens rotation. Enter first chooses an action, then runs it.
                monitor.keys(b"\x1b[H\r\r")
                monitor.wait(lambda: read().get("auth", {}).get("autoRotate") is True, "Rotation did not save")
                # End selects work, the last saved account. It must not switch provider.
                monitor.keys(b"\x1b[F\r\r")
                monitor.wait(lambda: json.loads(index.read_text())["current"] == "work", "Account did not switch")
                assert read()["route"]["provider"] == "ztest"
                # ytest is directly before ztest and its two accounts.
                monitor.keys(b"\x1b[F\x1b[A\x1b[A\x1b[A\r\x1b[B\r")
                monitor.wait(lambda: read()["route"]["fallback"] == ["ytest"], "Fallback did not save")
                # Choose that provider. The model choices and login already exist.
                monitor.keys(b"\r\r")
                monitor.wait(lambda: read()["route"]["provider"] == "ytest", "Provider did not switch")
                monitor.text("Claude Code uses ytest")
                assert read()["route"]["fallback"] == [], "Primary provider remained in its own fallback chain"
                settings = json.loads((root / "claude/settings.json").read_text())
                assert settings["env"]["ANTHROPIC_BASE_URL"].rstrip("/") == url
                # A cancelled action must not save anything or exit the monitor.
                before = file.read_bytes(), index.read_bytes()
                monitor.keys(b"\x1b[H\r")
                monitor.text("Choose an action")
                monitor.keys(b"\x1b")
                monitor.text("Details")
                monitor.keys(b"2")
                monitor.text("Recent requests")
                assert (file.read_bytes(), index.read_bytes()) == before
                # Reuse hidden key entry, then return to the same action on failure.
                monitor.keys(b"1\x1b[F\r\x1b[B\x1b[B\x1b[B\r")
                monitor.text("API key:")
                monitor.keys(b"\r")
                monitor.text("Couldn't save the change")
                assert (file.read_bytes(), index.read_bytes()) == before
            finally:
                monitor.close()
            assert alive(), "Closing the monitor stopped the proxy"

            # Explicit URLs never read local account rows or write local configuration.
            before = file.read_bytes(), index.read_bytes()
            remote = Monitor(env, "monitor", "--url", url, width=80, height=24)
            try:
                remote.text("Recent requests")
                remote.keys(b"1")
                remote.text("read-only")
                remote.keys(b"\r\r")
            finally:
                remote.close()
            assert (file.read_bytes(), index.read_bytes()) == before
            assert alive()

            # Demo actions work on simulated rows, never this computer's saved logins.
            demo = Monitor(env, "demo", width=40, height=16)
            try:
                demo.text("Providers and accounts")
                demo.keys(b"\x1b[H\r\r")
                demo.text("Demo only")
                demo.keys(b"2")
                demo.text("Recent requests")
            finally:
                demo.close()
            assert (file.read_bytes(), index.read_bytes()) == before
        finally:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()

print("PASS: account/provider selection, rotation, fallback, cancel, detach, remote read-only and safe demo")
