"""Check account-picker cancellation: python3 -I tests/account_cli_pty.py target/debug/cc-proxy."""

import fcntl
import json
import os
import struct
import termios
from pathlib import Path
import pty
import select
import signal
import sys
import tempfile
import time
import uuid

binary = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix="ccp-picker-") as tmp:
    root = Path(tmp)
    provider = root / "config/codex"
    (provider / "accounts").mkdir(parents=True)
    accounts = [{"id": str(uuid.uuid4()), "name": name} for name in ["home", "work"]]
    index = provider / "accounts.json"
    index.write_text(json.dumps({"current": "home", "accounts": accounts}))
    for account in accounts:
        (provider / "accounts" / f"{account['id']}.json").write_text(json.dumps({
            "access": "fake-secret", "refresh": "fake-refresh", "expires": 4102444800000
        }))
    before = index.read_bytes()
    env = dict(PATH="/usr/bin:/bin", HOME=tmp, USERPROFILE=tmp,
               CCP_CONFIG_DIR=str(root / "config"), CLAUDE_CONFIG_DIR=str(root / "claude"),
               XDG_STATE_HOME=str(root / "state"), TERM="xterm-256color")
    for args in [["login", "codex"], ["auth", "use", "codex"], ["logout", "codex"]]:
        for cancel in [b"\x1b", b"\x03"]:
            pid, fd = pty.fork()
            if pid == 0:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
                os.execve(binary, [binary] + args, env)
            exited = False
            output = b""
            try:
                deadline = time.monotonic() + 10
                while b"Esc cancel" not in output:
                    assert time.monotonic() < deadline, f"Picker never opened: {output!r}"
                    if select.select([fd], [], [], 0.1)[0]:
                        output += os.read(fd, 65536)
                os.write(fd, cancel)
                while time.monotonic() < deadline:
                    done, status = os.waitpid(pid, os.WNOHANG)
                    if done:
                        exited = True
                        assert os.waitstatus_to_exitcode(status) in (1, 2), status
                        break
                    if select.select([fd], [], [], 0.1)[0]:
                        try:
                            output += os.read(fd, 65536)
                        except OSError:
                            pass
                assert exited, f"Cancel opened another prompt instead of exiting: {output!r}"
                assert b"fake-secret" not in output
                assert index.read_bytes() == before
            finally:
                if not exited:
                    os.kill(pid, signal.SIGKILL)
                    os.waitpid(pid, 0)
                os.close(fd)
    print("PASS: Escape and Ctrl-C cancel login, account use and logout without changing saved accounts")
