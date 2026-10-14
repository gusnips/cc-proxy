#!/usr/bin/env python3
"""Check live model labels with the installed Claude CLI and local mock APIs.

Run after `cargo build`: python3 -I scripts/check-model-labels.py
No real credentials, settings or upstream services are used.
"""

import argparse
import fcntl
import gzip
import importlib.util
import json
import os
from pathlib import Path
import pty
import queue
import re
import select
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


# Reuse the mock wire replies from the conversation-history check.
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "history_check", Path(__file__).with_name("check-live-switch.py"))
history = importlib.util.module_from_spec(spec)
spec.loader.exec_module(history)


def atomic_json(path, value):
    pending = path.with_suffix(".tmp")
    pending.write_text(json.dumps(value))
    pending.replace(path)


def plain(text):
    text = re.sub(r"\x1b\][^\x07]*(?:\x07|\x1b\\)", "", text)
    return re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", text)


def check(proxy_binary, claude_binary):
    with tempfile.TemporaryDirectory(prefix="ccp-model-labels-") as temp:
        root = Path(temp).resolve()
        config = root / "proxy"
        claude_config = root / "claude"
        claude_config.mkdir()
        settings = claude_config / "settings.json"
        status = root / "status.json"
        upstream = []
        seed = []
        failures = queue.Queue()
        http = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        class Mock(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, body, content_type="text/event-stream"):
                self.send_response(200)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                self.reply(b'{"data": []}', "application/json")

            def do_POST(self):
                try:
                    raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                    if self.headers.get("Content-Encoding") == "gzip":
                        raw = gzip.decompress(raw)
                    body = json.loads(raw)
                    if self.path.startswith("/anthropic/v1/messages"):
                        seed.append(body)
                        self.reply(history.anthropic_reply(len(seed), {
                            "type": "text", "text": "Mock reply.",
                        }))
                    elif self.path.startswith("/codex"):
                        upstream.append(("codex", body))
                        # Claude's status command runs on completion, not settings reload.
                        # Let its file watcher see the new exact-ID row first.
                        time.sleep(2)
                        self.reply(history.codex_reply(len(upstream)))
                    elif self.path == "/kimi/chat/completions":
                        upstream.append(("kimi", body))
                        time.sleep(2)
                        chunks = [
                            {"choices": [{"index": 0, "delta": {
                                "role": "assistant", "content": "Mock reply.",
                            }, "finish_reason": None}]},
                            {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                             "usage": {"prompt_tokens": 1000, "completion_tokens": 10}},
                        ]
                        self.reply(("".join(f"data: {json.dumps(chunk)}\n\n"
                                          for chunk in chunks) + "data: [DONE]\n\n").encode())
                    else:
                        raise AssertionError(f"Unexpected mock path: {self.path}")
                except Exception as error:
                    failures.put(error)
                    self.send_error(500, "Mock request failed")

        mock = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
        threading.Thread(target=mock.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{mock.server_port}"
        with ThreadingHTTPServer(("127.0.0.1", 0), Mock) as reserved:
            port = reserved.server_port
        proxy_url = f"http://127.0.0.1:{port}"
        for provider in ("codex", "kimi"):
            (config / provider).mkdir(parents=True)
            atomic_json(config / provider / "auth.json", {
                "access": "test-access", "refresh": "test-refresh",
                "expires": 4102444800000, "account_id": "acct_test",
            })
        atomic_json(config / "models_cache.json", {"models": []})
        atomic_json(config / "config.json", {
            "route": {"on": False, "provider": "codex", "tiers": {
                "codex": {tier: "gpt-6.1-sol" for tier in ("fable", "opus", "sonnet", "haiku")},
                "kimi": {tier: "kimi-k3" for tier in ("fable", "opus", "sonnet", "haiku")},
            }},
            "codex": {"transport": "http", "baseUrl": base + "/codex/responses"},
            "kimi": {"baseUrl": base + "/kimi"},
        })
        status_script = root / "statusline.py"
        status_script.write_text(
            "import json, pathlib, sys\n"
            "value = json.load(sys.stdin)\n"
            f"path = pathlib.Path({str(status)!r})\n"
            "pending = path.with_suffix('.tmp')\n"
            "pending.write_text(json.dumps(value))\n"
            "pending.replace(path)\n"
            "print('')\n")
        original_picker = {"replaceBuiltInOptions": False, "options": [{
            "model": "custom-test-model", "label": "My own model",
        }]}
        atomic_json(settings, {
            "env": {"ANTHROPIC_BASE_URL": base + "/anthropic",
                    "CLAUDE_CODE_OAUTH_TOKEN": "test-original-oauth-token"},
            "modelPicker": original_picker,
            "statusLine": {"type": "command", "command":
                           f"{shlex.quote(sys.executable)} -I {shlex.quote(str(status_script))}"},
        })
        atomic_json(claude_config / ".claude.json", {
            "hasCompletedOnboarding": True, "theme": "dark",
            "projects": {str(root): {"hasTrustDialogAccepted": True}},
        })
        env = {key: os.environ[key] for key in ("PATH", "LANG", "TMPDIR") if key in os.environ}
        env.update({
            "HOME": str(root), "CLAUDE_CONFIG_DIR": str(claude_config),
            "CCP_CONFIG_DIR": str(config), "CODEX_HOME": str(config), "PORT": str(port),
            "XDG_STATE_HOME": str(root / "state"), "XDG_CONFIG_HOME": str(root / "xdg"),
            "TERM": "xterm-256color", "NO_PROXY": "127.0.0.1,localhost",
            "DISABLE_AUTOUPDATER": "1", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1",
        })
        proxy = None
        claude = None
        master = None
        captured = bytearray()

        def command(*args):
            result = subprocess.run([str(proxy_binary), *args], cwd=root, env=env,
                                    capture_output=True, text=True, timeout=30)
            assert result.returncode == 0, result.stdout + result.stderr

        def read_for(seconds):
            deadline = time.monotonic() + seconds
            chunk = bytearray()
            while time.monotonic() < deadline:
                ready, _, _ = select.select([master], [], [], max(0, min(0.2, deadline - time.monotonic())))
                if ready:
                    try:
                        data = os.read(master, 65536)
                    except OSError:
                        break
                    if not data:
                        break
                    chunk.extend(data)
                    captured.extend(data)
            assert claude.poll() is None, "The native Claude process exited"
            return plain(chunk.decode(errors="replace"))

        def wait_status(label):
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                read_for(0.3)
                if status.exists():
                    value = json.loads(status.read_text())
                    if value["model"]["display_name"] == label:
                        return value
            raise AssertionError(f"Claude's status line did not show {label}")

        def turn(expected_provider, expected_model):
            before = len(upstream) if expected_provider else len(seed)
            os.write(master, b"Reply with one short sentence.\r")
            deadline = time.monotonic() + 30
            text = ""
            while time.monotonic() < deadline:
                text += read_for(0.3)
                requests = upstream if expected_provider else seed
                if len(requests) > before and "Mockreply." in "".join(text.split()):
                    break
            else:
                raise AssertionError("The native Claude turn did not finish")
            read_for(1)
            if expected_provider:
                provider, body = upstream[-1]
                assert provider == expected_provider and body["model"] == expected_model, (provider, body)
            else:
                assert seed[-1]["model"] == expected_model, seed[-1]
            if not failures.empty():
                raise failures.get()

        def assert_profile(value, baseline):
            assert value["session_id"] == baseline["session_id"], "The session ID changed"
            assert value["model"]["id"] == baseline["model"]["id"], "The native request model changed"
            assert value["context_window"]["context_window_size"] == baseline["context_window"]["context_window_size"], (
                "The native context window changed")

        def picker(label):
            os.write(master, b"/model\r")
            text = read_for(2)
            compact = "".join(text.split())
            assert label in compact and "Myownmodel" in compact, text
            assert "effort" in compact.lower(), "The native effort control disappeared"
            os.write(master, b"\x1b")
            read_for(0.5)

        try:
            with (root / "proxy.log").open("w+") as proxy_log:
                proxy = subprocess.Popen(
                    [str(proxy_binary), "start", "--no-monitor", "--port", str(port)],
                    cwd=root, env=env, stdout=proxy_log, stderr=subprocess.STDOUT)
                deadline = time.monotonic() + 30
                while True:
                    try:
                        with http.open(proxy_url + "/healthz", timeout=1):
                            break
                    except OSError:
                        assert proxy.poll() is None and time.monotonic() < deadline, "The isolated proxy did not start"
                        time.sleep(0.1)
                master, slave = pty.openpty()
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
                claude = subprocess.Popen([
                    str(claude_binary), "--model", history.MODEL, "--tools", "",
                    "--system-prompt", "A local model-label test.",
                ], cwd=root, env=env, stdin=slave, stdout=slave, stderr=slave)
                os.close(slave)
                initial = read_for(6)
                if "trustthisfolder" in "".join(initial.split()).lower():
                    os.write(master, b"\x1b[B\r")
                    read_for(2)
                turn(None, history.MODEL)
                baseline = json.loads(status.read_text())
                native_label = baseline["model"]["display_name"]

                for args, provider, model in [
                    (("use", "codex"), "codex", "gpt-6.1-sol"),
                    (("models", "map", "opus", "gpt-6.2-sol-fast"), "codex", "gpt-6.2-sol"),
                    (("models", "map", "opus", "--reset"), "codex", "gpt-6.1-sol"),
                    (("use", "kimi"), "kimi", "k3"),
                ]:
                    command(*args)
                    read_for(2)
                    turn(provider, model)
                    value = wait_status(f"{provider}/{model}")
                    assert_profile(value, baseline)
                    if args == ("use", "codex"):
                        # The sandbox status line prints nothing, so only the
                        # native header can show this label before /model opens.
                        screen = "".join(plain(captured.decode(errors="replace")).split())
                        assert f"{provider}/{model}" in screen, "The native header kept its built-in name"
                    picker(f"{provider}/{model}")

                command("use", "claude")
                read_for(2)
                turn(None, history.MODEL)
                assert_profile(wait_status(native_label), baseline)
                assert json.loads(settings.read_text())["modelPicker"] == original_picker, "The original picker was not restored"

                command("use", "codex")
                read_for(2)
                turn("codex", "gpt-6.1-sol")
                wait_status("codex/gpt-6.1-sol")
                edited = json.loads(settings.read_text())
                for option in edited["modelPicker"]["options"]:
                    if option["model"] == history.MODEL:
                        option["label"] = "My label"
                atomic_json(settings, edited)
                read_for(2)
                command("models", "map", "opus", "gpt-6.2-sol")
                turn("codex", "gpt-6.2-sol")
                assert_profile(wait_status("My label"), baseline)
                command("use", "claude")
                assert json.loads(settings.read_text())["modelPicker"] == edited["modelPicker"], "A manual picker edit was overwritten"
                assert claude.poll() is None and proxy.poll() is None, "A test process was restarted"
                print("Passed: live header, picker and status-line labels follow provider/map/reset changes.")
                print("The native model ID, session, context window and effort control stay intact. Off restores the picker; manual edits survive.")
        except Exception:
            print("Native output:", plain(captured.decode(errors="replace"))[-8000:])
            current = json.loads(settings.read_text())
            print("Picker:", current.get("modelPicker"))
            print("Picker ownership:", current.get("cc-proxy", {}).get("model_picker"))
            print("Upstream models:", [(provider, body.get("model")) for provider, body in upstream])
            if status.exists():
                print("Native model:", json.loads(status.read_text())["model"])
            request_log = root / "state/cc-proxy/proxy.log"
            if request_log.exists():
                print("Synthetic request log:", request_log.read_text()[-5000:])
            if (root / "proxy.log").exists():
                print("Isolated proxy log:", (root / "proxy.log").read_text()[-4000:])
            raise
        finally:
            for process in (claude, proxy):
                if process is not None and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            if master is not None:
                os.close(master)
            mock.shutdown()
            mock.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("proxy", nargs="?", default="target/debug/cc-proxy")
    args = parser.parse_args()
    claude = shutil.which("claude")
    if not claude:
        parser.error("Install Claude Code before running this check.")
    check(Path(args.proxy).resolve(), Path(claude).resolve())
