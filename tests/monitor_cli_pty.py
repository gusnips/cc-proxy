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
        # q detaches an attached monitor and leaves the proxy running.
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
    other = config / "ytest"
    (other / "accounts").mkdir(parents=True)
    other_account = {"id": str(uuid.uuid4()), "name": "main"}
    (other / "accounts.json").write_text(json.dumps({"current": "main", "accounts": [other_account]}))
    (other / "accounts" / f"{other_account['id']}.json").write_text(json.dumps({"apiKey": "fake-secret"}))
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
    pool = lambda: read()["route"].get("pool", [])
    url = f"http://127.0.0.1:{port}"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    def alive():
        try:
            with opener.open(f"{url}/healthz", timeout=0.2) as response:
                return response.status == 200
        except OSError:
            return False

    # A monitor opened while the proxy is off says so, and `s` starts it.
    saved_config = file.read_text()
    off = Monitor(env, "open")
    try:
        off.text("Proxy off. Press s to start it")
        off.text("s start proxy")
        off.keys(b"s")
        wait_for(alive, "The monitor did not start the proxy")
        off.text("The proxy is on")
        off.text("Ready for requests")
        settings = json.loads((root / "claude/settings.json").read_text())
        assert settings["env"]["ANTHROPIC_BASE_URL"].rstrip("/") == url
    finally:
        off.close()
    assert alive(), "Closing the monitor stopped the proxy"
    subprocess.run([binary, "stop"], env=env, check=True, capture_output=True)
    wait_for(lambda: not alive(), "The proxy did not stop")
    file.write_text(saved_config)

    with (root / "server.log").open("wb") as log:
        server = subprocess.Popen([binary, "start", "--no-monitor"], env=env, stdout=log, stderr=log)
        try:
            wait_for(alive, "Isolated proxy did not start")
            monitor = Monitor(env, "open")
            try:
                monitor.text("work")
                # Settings is the third tab. Auto-rotate is its first row; Right turns it on and saves at once.
                monitor.keys(b"3")
                monitor.text("auth.autoRotate")
                monitor.keys(b"\x1b[C")
                monitor.wait(lambda: read().get("auth", {}).get("autoRotate") is True, "Rotation did not save")
                monitor.keys(b"\x1b")
                monitor.text("Request order")
                # One flat list in the order requests try it: the saved accounts of ztest, then ytest's row (off).
                # Move up puts the second row first without changing the saved login.
                monitor.keys(b"\x1b[H" + b"\x1b[B" + b"\r\r")
                work_id = next(a["id"] for a in json.loads(index.read_text())["accounts"] if a["name"] == "work")
                monitor.wait(lambda: bool(pool()) and pool()[0].get("accountId") == work_id,
                             "The account did not move to the top")
                assert [row["provider"] for row in pool()] == ["ztest", "ztest", "ytest"], pool()
                assert pool()[0]["enabled"] is True
                # The first save writes the pool and drops the older keys.
                assert "provider" not in read()["route"] and "fallback" not in read()["route"], read()["route"]
                # Right opens the actions and Left comes back to the same row. Neither runs one.
                before = file.read_bytes(), index.read_bytes()
                monitor.keys(b"\x1b[C")
                monitor.text("Choose an action")
                monitor.keys(b"\x1b[C")
                monitor.keys(b"\x1b[D")
                # The menu is closed if Right opens it again (the screen only redraws what changed).
                monitor.keys(b"\x1b[C")
                monitor.text("Choose an action")
                monitor.keys(b"\x1b[D")
                assert (file.read_bytes(), index.read_bytes()) == before
                # Rename opens a field that owns every key (q included). Escape keeps the old name.
                monitor.keys(b"\x1b[C")
                monitor.text("Choose an action")
                monitor.keys(b"\x1b[B" * 3 + b"\r")
                monitor.text("New name:")
                monitor.keys(b"\x15q2")
                # The field owns q; it must not close the attached monitor.
                monitor.text("q")
                monitor.keys(b"\x1b")
                monitor.text("Details")
                assert (file.read_bytes(), index.read_bytes()) == before
                # Enter saves the new name. The saved choice and the login stay with the same id.
                monitor.keys(b"\x1b[C")
                monitor.text("Choose an action")
                monitor.keys(b"\x1b[B" * 3 + b"\r")
                monitor.text("New name:")
                monitor.keys(b"\x15Spare\r")
                monitor.wait(lambda: [a["name"] for a in json.loads(index.read_text())["accounts"]] == ["home", "spare"],
                             "The account was not renamed")
                saved = json.loads(index.read_text())
                assert saved["current"] == "home", saved
                assert saved["accounts"][1]["id"] == work_id, "Renaming changed the account id"
                assert (provider / "accounts" / f"{work_id}.json").exists()
                monitor.text("Renamed")
                monitor.text("spare")
                # Tab, Shift-Tab and 1-3 change tabs. Right on Activity opens details, not Settings.
                monitor.keys(b"\t")
                monitor.text("Recent requests")
                monitor.keys(b"\x1b[C")
                monitor.keys(b"\x1b[Z")
                monitor.text("Request order")
                # The other provider's row is off and last. Enable turns it on and keeps its place.
                monitor.keys(b"\x1b[H" + b"\x1b[B" * 2 + b"\r" + b"\x1b[B" + b"\r")
                monitor.wait(lambda: pool()[2]["enabled"] is True, "The row was not turned on")
                assert [row["provider"] for row in pool()] == ["ztest", "ztest", "ytest"], pool()
                # Move up changes where the row stands. The cursor stays on it.
                monitor.keys(b"\x1b[H" + b"\x1b[B" * 2 + b"\r\r")
                monitor.wait(lambda: [row["provider"] for row in pool()] == ["ztest", "ytest", "ztest"], "The row did not move up")
                # Neither "Use now" nor "Use provider" exists any more: the order and the on/off switch decide.
                monitor.keys(b"\r")
                monitor.text("Move down")
                assert b"Use now" not in monitor.output[monitor.mark:] and b"Use provider" not in monitor.output[monitor.mark:]
                monitor.keys(b"\x1b")
                monitor.text("Details")
                # A cancelled action must not save anything or exit the monitor.
                before = file.read_bytes(), index.read_bytes()
                monitor.keys(b"\x1b[F\r")
                monitor.text("Choose an action")
                monitor.keys(b"\x1b")
                monitor.text("Details")
                monitor.keys(b"2")
                monitor.text("Recent requests")
                assert (file.read_bytes(), index.read_bytes()) == before
                # Reuse hidden key entry, then return to the same action on failure.
                monitor.keys(b"1\x1b[H" + b"\r" + b"\x1b[B" * 2 + b"\r")
                monitor.text("API key:")
                monitor.keys(b"\r")
                monitor.text("Couldn't save the change")
                assert (file.read_bytes(), index.read_bytes()) == before
            finally:
                monitor.close()
            assert alive(), "Closing the monitor stopped the proxy"

            # Explicit URLs never read local account rows or write local configuration.
            before = file.read_bytes(), index.read_bytes()
            remote = Monitor(env, "open", "--url", url, width=80, height=24)
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
                demo.text("Request order")
                demo.keys(b"\x1b[F\r\r")
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

print("PASS: start from an off proxy, one flat request order (move, enable, rename), cancel, detach, remote read-only and safe demo")
