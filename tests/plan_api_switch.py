"""Check Plan-to-API fallback: python3 -I tests/plan_api_switch.py target/debug/cc-proxy.

Runs a separate proxy with fake accounts and a local API. No real provider is contacted.
"""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid


binary = str(Path(sys.argv[1]).resolve())
binding_beta = "thinking-binding-controls-2026-08-01"
seen = []


class Api(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        seen.append((dict(self.headers), body))
        assert "metadata" not in body
        assert binding_beta in self.headers.get("anthropic-beta", "").split(",")
        assert body["thinking"]["block_binding"]["prefix_mismatch_behavior"] == "drop_block"
        assert body["messages"][1]["content"][0]["signature"] == "subscription-thinking"
        assert body["tools"][0]["name"] == "Read"
        throttle = body["messages"][-1]["content"] == "throttle"
        self.send_response(429 if throttle else 200)
        self.send_header("Content-Type", "application/json")
        if throttle:
            self.send_header("Retry-After", "10")
        self.end_headers()
        payload = (
            {"type": "error", "error": {"type": "rate_limit_error", "message": "API requests per minute exceeded. Try again later."}}
            if throttle
            else {"id": "msg_local", "type": "message", "role": "assistant", "model": body["model"], "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn", "usage": {"input_tokens": 4, "output_tokens": 1}}
        )
        self.wfile.write(json.dumps(payload).encode())


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


with tempfile.TemporaryDirectory(prefix="ccp-plan-api-") as tmp:
    root = Path(tmp)
    config = root / "config"
    config.mkdir()
    state = root / "state" / "cc-proxy"
    state.mkdir(parents=True)
    api = ThreadingHTTPServer(("127.0.0.1", 0), Api)
    threading.Thread(target=api.serve_forever, daemon=True).start()
    port = free_port()
    account = str(uuid.uuid4())
    native = config / "claude"
    (native / "accounts").mkdir(parents=True)
    (native / "accounts.json").write_text(json.dumps({"current": "plan", "accounts": [{"id": account, "name": "plan"}]}))
    # A future token and confirmed wait ensure the subscription is never called.
    (native / "accounts" / f"{account}.json").write_text(json.dumps({"access": "fake-plan-token", "refresh": "fake-refresh", "expires": 4102444800000}))
    (state / "rotation.json").write_text(json.dumps([{"provider": "claude", "account": account, "until_ms": int(time.time() * 1000) + 3600000}]))
    (config / "config.json").write_text(json.dumps({
        "port": port,
        "auth": {"autoRotate": True, "hourlyPing": False},
        "anthropic": {"apiKey": "fake-api-key", "baseUrl": f"http://127.0.0.1:{api.server_port}"},
        "route": {"on": True, "pool": [{"provider": "claude", "accountId": account, "enabled": True}, {"provider": "anthropic", "enabled": True}]},
    }))
    env = dict(PATH=os.environ["PATH"], HOME=tmp, USERPROFILE=tmp, CCP_CONFIG_DIR=str(config),
               CLAUDE_CONFIG_DIR=str(root / "cli"), XDG_STATE_HOME=str(root / "state"), NO_PROXY="127.0.0.1,localhost")
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    url = f"http://127.0.0.1:{port}"
    with (root / "server.log").open("wb") as log:
        proxy = subprocess.Popen([binary, "start", "--no-monitor"], env=env, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 30
            while True:
                assert proxy.poll() is None, (root / "server.log").read_text()
                try:
                    with opener.open(f"{url}/healthz", timeout=0.5):
                        break
                except OSError:
                    assert time.monotonic() < deadline, "Isolated proxy did not start"
                    time.sleep(0.05)
            body = {
                "model": "claude-sonnet-5-5", "stream": False, "thinking": {"type": "adaptive"}, "max_tokens": 32,
                "system": "You are Claude Code, Anthropic's official CLI for Claude.",
                "metadata": {"user_id": json.dumps({"session_id": "plan-api-check"})},
                "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
                "messages": [{"role": "user", "content": "hi"},
                             {"role": "assistant", "content": [{"type": "thinking", "thinking": "prior", "signature": "subscription-thinking"}, {"type": "text", "text": "ready"}]},
                             {"role": "user", "content": "throttle"}],
            }
            for prompt, expected in [("throttle", 429), ("throttle", 429), ("continue", 200)]:
                body["messages"][-1]["content"] = prompt
                request = urllib.request.Request(f"{url}/v1/messages", data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
                try:
                    response = opener.open(request, timeout=10)
                except urllib.error.HTTPError as error:
                    response = error
                with response:
                    payload = json.load(response)
                    assert response.status == expected, payload
                    assert response.headers.get("anthropic-ratelimit-unified-reset") is None, dict(response.headers)
                    if expected == 429:
                        assert response.headers["Retry-After"] == "10"
                        assert response.headers.get("x-should-retry") is None
                    else:
                        assert payload["content"][0]["text"] == "ok", payload
                waits = json.loads((state / "rotation.json").read_text())
                assert all(wait["provider"] != "anthropic" for wait in waits), waits
            assert len(seen) == 3, "A throttle rotated or hid an API call"
        finally:
            proxy.terminate()
            try:
                proxy.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proxy.kill()
                proxy.wait()
            api.shutdown()
            api.server_close()

print("PASS: Plan-to-API fallback preserves thinking controls; API throttles have no usage countdown or saved account wait; the next request succeeds")
