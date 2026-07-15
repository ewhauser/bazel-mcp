#!/usr/bin/env python3
import argparse
import json
import os
import pathlib
import subprocess
import sys


def send(process, message):
    process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
    process.stdin.flush()


def receive(process, expected_id):
    while True:
        line = process.stdout.readline()
        if not line:
            raise RuntimeError("MCP server closed stdout")
        message = json.loads(line)
        if message.get("id") == expected_id:
            return message


def call(process, request_id, workspace, command, arguments, timeout=60):
    send(process, {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "tools/call",
        "params": {
            "name": "bazel.run",
            "arguments": {
                "workspace": str(workspace),
                "command": command,
                "args": arguments,
                "timeout_seconds": timeout,
            },
        },
    })
    response = receive(process, request_id)
    if "error" in response or response.get("result", {}).get("isError"):
        raise RuntimeError(f"MCP call failed: {response}")
    return json.loads(response["result"]["content"][0]["text"])


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--workspace", type=pathlib.Path, required=True)
    parser.add_argument("--server", type=pathlib.Path, required=True)
    parser.add_argument("--bazel", type=pathlib.Path, required=True)
    parser.add_argument("--root", type=pathlib.Path, required=True)
    args = parser.parse_args()
    args.root.mkdir(parents=True, exist_ok=True)
    config = args.root / "config.toml"
    config.write_text(
        "allowed_roots = [" + json.dumps(str(args.workspace)) + "]\n"
        "cache_root = " + json.dumps(str(args.root / "store")) + "\n"
        "bazel_executable = " + json.dumps(str(args.bazel)) + "\n"
        "output_user_root = " + json.dumps(str(args.root / "bazel")) + "\n"
        "environment_allowlist = [\"USE_BAZEL_VERSION\"]\n",
        encoding="utf-8",
    )
    process = subprocess.Popen(
        [str(args.server), "--config", str(config), "--log", "error"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=sys.stderr,
        text=True,
        env=os.environ.copy(),
    )
    send(process, {
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "matrix", "version": "1"},
        },
    })
    if "error" in receive(process, 1):
        raise RuntimeError("MCP initialize failed")
    send(process, {"jsonrpc": "2.0", "method": "notifications/initialized"})
    cases = [
        ("build_success", "build", ["//:ok"], 0, "succeeded", 60),
        ("loading_failure", "build", ["//:missing"], 1, "failed", 60),
        ("action_failure", "build", ["//:action_failure"], 1, "failed", 60),
        ("test_success", "test", ["//:test_success"], 0, "succeeded", 60),
        ("test_failure", "test", ["//:test_failure"], 3, "failed", 60),
        ("coverage", "coverage", ["//:test_success"], 0, "succeeded", 60),
        ("query", "query", ["//..."], 0, "succeeded", 60),
        ("timeout", "build", ["//:slow"], None, "timed_out", 1),
    ]
    for request_id, (name, command, command_args, exit_code, state, timeout) in enumerate(cases, 2):
        result = call(process, request_id, args.workspace, command, command_args, timeout)
        if result["state"] != state or result.get("exit_code") != exit_code:
            raise RuntimeError(f"{name} mismatch: {result}")
        print(f"{name}\t{result['state']}\t{result.get('exit_code')}")
    process.stdin.close()
    if process.wait(timeout=10) != 0:
        raise RuntimeError("MCP server exited unsuccessfully")


if __name__ == "__main__":
    main()
