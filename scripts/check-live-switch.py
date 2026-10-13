#!/usr/bin/env python3
"""Check live-switch history with the installed Claude CLI and local mock APIs.

Run after `cargo build`: python3 -I scripts/check-live-switch.py
To reproduce the old bug: pass an old proxy binary and --expect-broken.
No real credentials, settings or upstream services are used.
"""

import argparse
import gzip
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


MARKER = "original_task_marker_739164"
TOOL_RESULT = "tool_result_marker_285630"
MODEL = "claude-opus-5-5"


def events_bytes(events):
    return "".join(
        f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events
    ).encode()


def anthropic_reply(number, block):
    tool = block["type"] == "tool_use"
    start = {**block, "input": {}} if tool else {"type": "text", "text": ""}
    delta = ({"type": "input_json_delta", "partial_json": json.dumps(block["input"])}
             if tool else {"type": "text_delta", "text": block["text"]})
    return events_bytes([
        {"type": "message_start", "message": {
            "id": f"msg_seed_{number}", "type": "message", "role": "assistant",
            "model": MODEL, "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 1000, "output_tokens": 0},
        }},
        {"type": "content_block_start", "index": 0, "content_block": start},
        {"type": "content_block_delta", "index": 0, "delta": delta},
        {"type": "content_block_stop", "index": 0},
        {"type": "message_delta", "delta": {
            "stop_reason": "tool_use" if block["type"] == "tool_use" else "end_turn",
            "stop_sequence": None,
        }, "usage": {"output_tokens": 10}},
        {"type": "message_stop"},
    ])


def codex_reply(number):
    item = {"type": "message", "id": f"out_{number}", "role": "assistant",
            "content": [{"type": "output_text", "text": "Mock reply.", "annotations": []}]}
    return events_bytes([
        {"type": "response.created", "response": {"id": f"resp_{number}", "output": []}},
        {"type": "response.output_item.added", "output_index": 0,
         "item": {**item, "content": []}},
        {"type": "response.output_text.delta", "output_index": 0,
         "content_index": 0, "delta": "Mock reply."},
        {"type": "response.output_item.done", "output_index": 0, "item": item},
        {"type": "response.completed", "response": {
            "id": f"resp_{number}", "status": "completed", "output": [item],
            "usage": {"input_tokens": 1000, "output_tokens": 10, "total_tokens": 1010},
        }},
    ])


def check(proxy_binary, claude_binary, expect_broken):
    with tempfile.TemporaryDirectory(prefix="ccp-live-switch-") as temp:
        root = Path(temp)
        config = root / "proxy"
        (config / "codex").mkdir(parents=True)
        claude_config = root / "claude"
        claude_config.mkdir()
        settings = claude_config / "settings.json"
        marker_file = root / "marker.txt"
        marker_file.write_text(TOOL_RESULT)
        (config / "codex/auth.json").write_text(json.dumps({
            "access": "test-access", "refresh": "test-refresh",
            "expires": 4102444800000, "account_id": "acct_test",
        }))
        http = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        requests = []
        upstream = []
        seed_requests = []
        failures = queue.Queue()

        def write_settings(url):
            pending = settings.with_suffix(".tmp")
            pending.write_text(json.dumps({"env": {
                "ANTHROPIC_BASE_URL": url, "CLAUDE_CODE_OAUTH_TOKEN": "unused",
            }}))
            pending.replace(settings)

        class Mock(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, status, body, content_type="application/json"):
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                self.reply(200, b'{"data": []}')

            def do_POST(self):
                try:
                    raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                    if self.headers.get("Content-Encoding") == "gzip":
                        raw = gzip.decompress(raw)
                    body = json.loads(raw)
                    if self.path.startswith("/anthropic/v1/messages"):
                        seed_requests.append(body)
                        number = len(seed_requests)
                        if number == 1:
                            block = {"type": "text", "text": "Saved the original task."}
                        else:
                            # Switch while this process is awaiting its mock API reply.
                            # Its next request contains the result of this Read call.
                            write_settings(base + "/proxy")
                            time.sleep(2)
                            block = {"type": "tool_use", "id": "toolu_before_switch",
                                     "name": "Read", "input": {"file_path": str(marker_file)}}
                        self.reply(200, anthropic_reply(number, block), "text/event-stream")
                    elif self.path.startswith("/proxy/v1/messages"):
                        headers = {k: v for k, v in self.headers.items()
                                   if k.lower() not in ("host", "content-length", "content-encoding")}
                        req = urllib.request.Request(
                            proxy_url + self.path.removeprefix("/proxy"), data=raw, headers=headers)
                        try:
                            response = http.open(req, timeout=30)
                        except urllib.error.HTTPError as error:
                            response = error
                        with response:
                            result = response.read()
                            requests.append((body, response.status, result))
                            self.reply(response.status, result, response.headers.get("Content-Type"))
                    elif self.path.startswith("/codex"):
                        upstream.append(body)
                        self.reply(200, codex_reply(len(upstream)), "text/event-stream")
                    else:
                        raise AssertionError(f"Unexpected mock path: {self.path}")
                except Exception as error:
                    failures.put(error)
                    self.reply(500, b'{"error":{"type":"api_error","message":"Mock failed"}}')

        mock = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
        base = f"http://127.0.0.1:{mock.server_port}"
        threading.Thread(target=mock.serve_forever, daemon=True).start()
        # Reserve an unused local port for this test's proxy, never the live port.
        with ThreadingHTTPServer(("127.0.0.1", 0), Mock) as reserved:
            port = reserved.server_port
        proxy_url = f"http://127.0.0.1:{port}"
        (config / "config.json").write_text(json.dumps({
            "route": {"on": True, "provider": "codex"},
            "codex": {"model": "gpt-5.5", "transport": "http",
                      "baseUrl": base + "/codex/responses"},
        }))
        write_settings(base + "/anthropic")
        env = {k: os.environ[k] for k in ("PATH", "LANG", "TMPDIR") if k in os.environ}
        env.update({
            "HOME": str(root), "CLAUDE_CONFIG_DIR": str(claude_config),
            "CCP_CONFIG_DIR": str(config), "XDG_STATE_HOME": str(root / "state"),
            "XDG_CONFIG_HOME": str(root / "xdg"), "NO_PROXY": "127.0.0.1,localhost",
            "CLAUDE_CODE_TETHER_LIVE": "1", "DISABLE_AUTOUPDATER": "1",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1",
        })
        proxy = None
        claude = None
        try:
            with (root / "proxy.log").open("w+") as proxy_log, (root / "claude.log").open("w+") as claude_log:
                proxy = subprocess.Popen(
                    [str(proxy_binary), "start", "--no-monitor", "--port", str(port)],
                    cwd=root, env=env, stdout=proxy_log, stderr=subprocess.STDOUT)
                deadline = time.monotonic() + 30
                while True:
                    try:
                        with http.open(proxy_url + "/healthz", timeout=1):
                            break
                    except OSError:
                        assert proxy.poll() is None, "Test proxy exited before listening"
                        assert time.monotonic() < deadline, "Test proxy did not start"
                        time.sleep(0.1)
                claude = subprocess.Popen([
                    str(claude_binary), "-p", "--input-format", "stream-json",
                    "--output-format", "stream-json", "--verbose", "--model", MODEL,
                    "--tools", "Read", "--permission-mode", "bypassPermissions",
                    "--system-prompt", "This is a local conversation-history test.",
                ], cwd=root, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                    stderr=claude_log, text=True, bufsize=1)
                output = queue.Queue()

                def read_output():
                    for line in claude.stdout:
                        output.put(json.loads(line))
                    output.put(None)

                threading.Thread(target=read_output, daemon=True).start()
                sessions = set()

                def turn(text):
                    claude.stdin.write(json.dumps({"type": "user", "message": {
                        "role": "user", "content": text,
                    }}) + "\n")
                    claude.stdin.flush()
                    deadline = time.monotonic() + 45
                    while True:
                        event = output.get(timeout=max(0.1, deadline - time.monotonic()))
                        assert event is not None, "Claude exited before completing the test turn"
                        if event.get("session_id"):
                            sessions.add(event["session_id"])
                        if event.get("type") == "result":
                            assert not event.get("is_error"), event
                            return

                try:
                    turn(f"Remember the original task marker: {MARKER}.")
                    turn(f"Read {marker_file}, then continue the original task.")
                    assert any(b.get("thread", {}).get("type") == "continue" for b, _, _ in requests), (
                        "No partial continuation reached the proxy after switching")
                    if not failures.empty():
                        raise failures.get()
                    if expect_broken:
                        assert upstream and any(MARKER not in json.dumps(b["input"]) for b in upstream)
                        print("Reproduced: the old proxy forwarded a continuation without the original task.")
                        return
                    rejected = [json.loads(result) for _, status, result in requests if status == 400]
                    assert any(r["error"]["details"]["error_code"] == "thread_unsupported_request"
                               for r in rejected), "No structured replay rejection"
                    turn("Continue again without forgetting the original task.")
                    assert len(upstream) >= 2, "The follow-up turn did not reach the mock Codex API"
                    for body in upstream:
                        text = json.dumps(body["input"])
                        assert MARKER in text, "The original user task was lost"
                        assert TOOL_RESULT in text, "The tool result was lost"
                        calls = {item["call_id"] for item in body["input"]
                                 if item["type"] == "function_call"}
                        results = {item["call_id"] for item in body["input"]
                                   if item["type"] == "function_call_output"}
                        assert "toolu_before_switch" in calls & results, "The tool pair was lost"
                        assert "local conversation-history test" in body.get("instructions", ""), (
                            "The original system prompt was lost")
                        assert any(tool.get("name") == "Read" for tool in body.get("tools", [])), (
                            "The original tool definition was lost")
                    assert len(sessions) == 1 and claude.poll() is None, "The session was restarted"
                    if not failures.empty():
                        raise failures.get()
                    print("Passed: the same Claude process resent full history and kept it on the next turn.")
                except Exception:
                    claude_log.flush()
                    print("Request shapes:", [(b.get("thread"), len(b.get("messages", [])), status)
                                               for b, status, _ in requests])
                    print("Mock upstream requests:", len(upstream))
                    for body in upstream[:1]:
                        print("Synthetic tool exchange:", [item for item in body["input"]
                              if item.get("type") in ("function_call", "function_call_output")])
                    print("Claude stderr:", (root / "claude.log").read_text()[-4000:])
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
            mock.shutdown()
            mock.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("proxy", nargs="?", default="target/debug/cc-proxy")
    parser.add_argument("--expect-broken", action="store_true")
    args = parser.parse_args()
    claude = shutil.which("claude")
    if not claude:
        parser.error("Install Claude Code before running this check.")
    check(Path(args.proxy).resolve(), Path(claude).resolve(), args.expect_broken)
