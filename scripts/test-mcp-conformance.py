#!/usr/bin/env python3
"""Stdio checks complementing official MCP conformance 0.1.15.

The upstream server runner currently accepts an HTTP URL, while bazel-mcp is a
stdio-only local server. These checks exercise the same initialize/tools
contracts at the actual production boundary.
"""
import json
import pathlib
import subprocess
import sys
import tempfile


def send(process, value):
    process.stdin.write(json.dumps(value, separators=(",", ":")) + "\n")
    process.stdin.flush()


def receive(process, request_id, notifications=None):
    while True:
        line = process.stdout.readline()
        if not line:
            raise AssertionError("server closed stdout")
        message = json.loads(line)
        if message.get("id") == request_id:
            return message
        if notifications is not None:
            notifications.append(message)


def main():
    root = pathlib.Path.cwd()
    server = root / "target/debug/bazel-mcp"
    with tempfile.TemporaryDirectory() as temporary:
        temporary = pathlib.Path(temporary)
        workspace = temporary / "workspace"
        workspace.mkdir()
        (workspace / "MODULE.bazel").write_text("module(name='conformance')\n")
        wrapper = workspace / "tools" / "bazel"
        wrapper.parent.mkdir()
        wrapper.write_text("#!/bin/sh\nsleep 2\nexit 0\n")
        wrapper.chmod(0o700)
        config = temporary / "config.toml"
        config.write_text(
            f'allowed_roots = [{json.dumps(str(workspace))}]\n'
            f'cache_root = {json.dumps(str(temporary / "cache"))}\n'
            'progress_initial_seconds = 1\n'
            'progress_interval_seconds = 60\n'
        )
        process = subprocess.Popen(
            [str(server), "--config", str(config), "--log", "error"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr,
            text=True,
        )
        send(process, {
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "conformance", "version": "1"}},
        })
        initialized = receive(process, 1)
        assert "error" not in initialized
        assert initialized["result"]["serverInfo"]["name"] == "bazel-mcp"
        send(process, {"jsonrpc": "2.0", "method": "notifications/initialized"})
        send(process, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        listed = receive(process, 2)
        tools = listed["result"]["tools"]
        assert [tool["name"] for tool in tools] == ["bazel.cancel", "bazel.inspect", "bazel.run"]
        for tool in tools:
            assert tool["description"]
            assert tool["inputSchema"]["type"] == "object"
        progress = []
        send(process, {
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "bazel.run",
                       "_meta": {"progressToken": "conformance-progress"},
                       "arguments": {"workspace": str(workspace),
                                     "command": "build", "args": ["//..."]}},
        })
        completed = receive(process, 3, progress)
        assert completed["result"]["isError"] is False
        assert any(
            message.get("method") == "notifications/progress"
            and message.get("params", {}).get("progressToken") == "conformance-progress"
            for message in progress
        )
        send(process, {
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "bazel.run", "arguments": {
                "workspace": str(workspace), "command": "clean", "args": []}},
        })
        denied = receive(process, 4)
        assert denied["result"]["isError"] is True
        process.stdin.close()
        assert process.wait(timeout=10) == 0
    print("stdio MCP initialize, tools, progress, schema, and error semantics passed")


if __name__ == "__main__":
    main()
