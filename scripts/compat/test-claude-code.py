#!/usr/bin/env python3
"""Pinned Claude Code compatibility test using a loopback mock Messages API."""

import argparse
import http.server
import json
import os
import pathlib
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading


ROOT = pathlib.Path(__file__).resolve().parents[2]
LOCK = json.loads((ROOT / "scripts/compat/claude-code.lock").read_text())


def message_payload(block, stop_reason):
    return {
        "id": "msg_bazel_mcp_compat",
        "type": "message",
        "role": "assistant",
        "model": "claude-compat-mock",
        "content": [block],
        "stop_reason": stop_reason,
        "stop_sequence": None,
        "usage": {"input_tokens": 1, "output_tokens": 1},
    }


class MockMessagesHandler(http.server.BaseHTTPRequestHandler):
    workspace = ""
    requests = []

    def log_message(self, _format, *_arguments):
        return

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        type(self).requests.append({"path": self.path, "body": body})
        if "count_tokens" in self.path:
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"input_tokens":1}')
            return

        saw_tool_result = any(
            isinstance(content, list)
            and any(
                isinstance(block, dict) and block.get("type") == "tool_result"
                for block in content
            )
            for message in body.get("messages", [])
            for content in [message.get("content")]
        )
        if saw_tool_result:
            block = {"type": "text", "text": "Bazel result acknowledged."}
            stop_reason = "end_turn"
        else:
            tool = next(
                (
                    tool
                    for tool in body.get("tools", [])
                    if "bazel_run" in tool.get("name", "")
                    or "bazel.run" in tool.get("name", "")
                ),
                None,
            )
            if tool is None:
                block = {"type": "text", "text": "No Bazel MCP tool was available."}
                stop_reason = "end_turn"
            else:
                block = {
                    "type": "tool_use",
                    "id": "toolu_bazel_mcp_compat",
                    "name": tool["name"],
                    "input": {
                        "workspace": type(self).workspace,
                        "command": "build",
                        "args": ["//:compat"],
                    },
                }
                stop_reason = "tool_use"
        payload = message_payload(block, stop_reason)
        if body.get("stream"):
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.end_headers()
            start = dict(payload)
            start["content"] = []
            start["stop_reason"] = None
            events = [
                ("message_start", {"type": "message_start", "message": start}),
                (
                    "content_block_start",
                    {
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": (
                            {**block, "input": {}}
                            if block["type"] == "tool_use"
                            else {**block, "text": ""}
                        ),
                    },
                ),
            ]
            if block["type"] == "tool_use":
                events.append(
                    (
                        "content_block_delta",
                        {
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": json.dumps(block["input"]),
                            },
                        },
                    )
                )
            else:
                events.append(
                    (
                        "content_block_delta",
                        {
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {"type": "text_delta", "text": block["text"]},
                        },
                    )
                )
            events.extend(
                [
                    ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                    (
                        "message_delta",
                        {
                            "type": "message_delta",
                            "delta": {"stop_reason": stop_reason, "stop_sequence": None},
                            "usage": {"output_tokens": 1},
                        },
                    ),
                    ("message_stop", {"type": "message_stop"}),
                ]
            )
            for event, data in events:
                self.wfile.write(f"event: {event}\ndata: {json.dumps(data)}\n\n".encode())
                self.wfile.flush()
        else:
            encoded = json.dumps(payload).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)


class LoopbackServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


def verify_claude():
    executable = shutil.which("claude")
    if not executable:
        raise SystemExit(
            f"Claude Code {LOCK['version']} is required; install "
            f"{LOCK['package']}@{LOCK['version']}"
        )
    version = subprocess.run(
        [executable, "--version"], capture_output=True, text=True, check=True
    ).stdout
    if LOCK["version"] not in version:
        raise SystemExit(
            f"Claude Code version mismatch: expected {LOCK['version']}, observed {version.strip()}"
        )
    return executable


def trace_messages(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def run_case(claude, temporary, workspace, wrapper, policy, live=False):
    case = temporary / policy
    case.mkdir()
    trace = case / "stdio.jsonl"
    server_config = case / "server.toml"
    server_config.write_text(
        f"allowed_roots = [{json.dumps(str(workspace))}]\n"
        f"cache_root = {json.dumps(str(case / 'cache'))}\n"
        f"bazel_executable = {json.dumps(str(wrapper))}\n"
        f"mcp_execution_policy = {json.dumps(policy)}\n"
        "task_poll_interval_ms = 100\n"
    )
    mcp_config = case / "mcp.json"
    mcp_config.write_text(
        json.dumps(
            {
                "mcpServers": {
                    "bazel": {
                        "type": "stdio",
                        "command": sys.executable,
                        "args": [
                            str(ROOT / "scripts/compat/stdio-proxy.py"),
                            "--trace",
                            str(trace),
                            str(ROOT / "target/debug/bazel-mcp"),
                            "--config",
                            str(server_config),
                            "--log",
                            "error",
                        ],
                    }
                }
            }
        )
    )
    env = os.environ.copy()
    mock = None
    thread = None
    if live:
        if not env.get("ANTHROPIC_API_KEY"):
            raise SystemExit("ANTHROPIC_API_KEY is required for test-claude-code-live")
    else:
        MockMessagesHandler.workspace = str(workspace)
        MockMessagesHandler.requests = []
        mock = LoopbackServer(("127.0.0.1", 0), MockMessagesHandler)
        thread = threading.Thread(target=mock.serve_forever, daemon=True)
        thread.start()
        env.update({
            "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{mock.server_port}",
            "ANTHROPIC_API_KEY": "dummy-compatibility-key",
        })
    env.update({
        "CLAUDE_CONFIG_DIR": str(case / "claude-config"),
        "DISABLE_TELEMETRY": "1",
        "DISABLE_ERROR_REPORTING": "1",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    })
    command = [
        claude,
        "--bare",
        "--print",
        "--strict-mcp-config",
        "--mcp-config",
        str(mcp_config),
        "--no-session-persistence",
        "--allowedTools",
        "mcp__bazel__*",
    ]
    if live:
        command.extend(["--max-budget-usd", "0.10"])
    try:
        completed = subprocess.run(
            command,
            cwd=workspace,
            env=env,
            input="Build //:compat with the Bazel MCP server and report the result.\n",
            capture_output=True,
            text=True,
            timeout=60,
        )
    finally:
        if mock is not None:
            mock.shutdown()
            mock.server_close()
            thread.join(timeout=2)
    if completed.returncode != 0:
        raise AssertionError(
            f"Claude Code {policy} case failed ({completed.returncode}):\n"
            f"stdout={completed.stdout}\nstderr={completed.stderr}"
        )
    messages = trace_messages(trace)
    client = [
        item["message"]
        for item in messages
        if item["direction"] == "client_to_server"
    ]
    server = [
        item["message"]
        for item in messages
        if item["direction"] == "server_to_client"
    ]
    initialized = next(message for message in client if message.get("method") == "initialize")
    assert initialized["params"]["protocolVersion"] == "2025-11-25"
    list_request = next(message for message in client if message.get("method") == "tools/list")
    listed = next(message for message in server if message.get("id") == list_request["id"])
    assert [tool["name"] for tool in listed["result"]["tools"]] == [
        "bazel.cancel",
        "bazel.inspect",
        "bazel.run",
    ]
    call = next(message for message in client if message.get("method") == "tools/call")
    response = next(message for message in server if message.get("id") == call["id"])
    run_tool = next(tool for tool in listed["result"]["tools"] if tool["name"] == "bazel.run")
    assert "task" not in call["params"], call["params"]
    if policy == "sync_only":
        assert "execution" not in next(
            tool for tool in listed["result"]["tools"] if tool["name"] == "bazel.run"
        )
        assert response["result"]["isError"] is False
    elif policy == "auto":
        assert run_tool["execution"]["taskSupport"] == "optional"
        assert response["result"]["isError"] is False
    else:
        assert run_tool["execution"]["taskSupport"] == "required"
        assert response["error"]["code"] == -32601
    assert not any(
        message.get("method", "").startswith("tasks/") for message in client
    )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--live", action="store_true")
    arguments = parser.parse_args()
    claude = verify_claude()
    server = ROOT / "target/debug/bazel-mcp"
    if not server.is_file():
        raise SystemExit("build target/debug/bazel-mcp before running compatibility")
    with tempfile.TemporaryDirectory() as name:
        temporary = pathlib.Path(name)
        workspace = temporary / "workspace"
        workspace.mkdir()
        (workspace / "MODULE.bazel").write_text("module(name='claude_compat')\n")
        launches = temporary / "launches.jsonl"
        wrapper = temporary / "fake-bazel.py"
        wrapper.write_text(
            "#!/usr/bin/env python3\n"
            "import json, pathlib, sys, time\n"
            "if sys.argv[1:] == ['--version']:\n"
            "    print('bazel 9.1.0')\n"
            "    raise SystemExit(0)\n"
            f"path = pathlib.Path({str(launches)!r})\n"
            "with path.open('a') as output:\n"
            "    output.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "time.sleep(0.4)\n"
        )
        wrapper.chmod(0o700)
        for policy in ("sync_only", "auto", "tasks_required"):
            before = len(launches.read_text().splitlines()) if launches.exists() else 0
            run_case(claude, temporary, workspace, wrapper, policy, arguments.live)
            after = len(launches.read_text().splitlines()) if launches.exists() else 0
            expected = before if policy == "tasks_required" else before + 1
            assert after == expected
    kind = "live" if arguments.live else "credential-free"
    print(
        f"Claude Code {LOCK['version']} {kind} synchronous fallback and task-policy compatibility passed"
    )


if __name__ == "__main__":
    main()
