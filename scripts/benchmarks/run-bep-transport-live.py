#!/usr/bin/env python3
"""Benchmark warm Bazel invocations through tail, FIFO, and BES MCP servers."""

import argparse
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile


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


class McpServer:
    def __init__(self, binary, config):
        self.process = subprocess.Popen(
            [str(binary), "--config", str(config), "--log", "error"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=sys.stderr,
            text=True,
        )
        send(
            self.process,
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "bep-transport-benchmark", "version": "1"},
                },
            },
        )
        response = receive(self.process, 1)
        if "error" in response:
            raise RuntimeError(f"MCP initialize failed: {response}")
        send(self.process, {"jsonrpc": "2.0", "method": "notifications/initialized"})
        self.request_id = 2

    def build(self, workspace, target, timeout, build_args):
        request_id = self.request_id
        self.request_id += 1
        send(
            self.process,
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {
                    "name": "bazel.run",
                    "arguments": {
                        "workspace": str(workspace),
                        "command": "build",
                        "args": [*build_args, target],
                        "timeout_seconds": timeout,
                    },
                },
            },
        )
        response = receive(self.process, request_id)
        if "error" in response or response.get("result", {}).get("isError"):
            raise RuntimeError(f"MCP build failed: {response}")
        result = json.loads(response["result"]["content"][0]["text"])
        if result["state"] != "succeeded":
            raise RuntimeError(f"Bazel build did not succeed: {result}")
        return float(result["duration_ms"])

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)


def write_config(path, workspace, store, output_user_root, bazel, transport):
    path.write_text(
        "allowed_roots = [" + json.dumps(str(workspace)) + "]\n"
        + "cache_root = " + json.dumps(str(store)) + "\n"
        + "bazel_executable = " + json.dumps(str(bazel)) + "\n"
        + "output_user_root = " + json.dumps(str(output_user_root)) + "\n"
        + "bep_transport = " + json.dumps(transport) + "\n"
        + 'result_encoding = "text"\n',
        encoding="utf-8",
    )


def metrics(samples):
    ordered = sorted(samples)
    median = percentile(ordered, 0.50)
    return {
        "median_ms": median,
        "p95_ms": percentile(ordered, 0.95),
        "mean_ms": sum(ordered) / len(ordered),
        "samples_ms": ordered,
    }


def percentile(ordered, quantile):
    return ordered[round((len(ordered) - 1) * quantile)]


def resolve_executable(value):
    if value:
        result = pathlib.Path(value).expanduser().resolve()
    else:
        candidate = shutil.which("bazelisk") or shutil.which("bazel")
        if not candidate:
            raise RuntimeError("could not find bazelisk or bazel on PATH")
        result = pathlib.Path(candidate).resolve()
    if not result.is_file():
        raise RuntimeError(f"executable does not exist: {result}")
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--workspace", type=pathlib.Path, default=pathlib.Path.cwd())
    parser.add_argument("--server", type=pathlib.Path, default="target/debug/bazel-mcp")
    parser.add_argument("--bazel")
    parser.add_argument("--target", default="//:bazel-mcp")
    parser.add_argument("--build-arg", action="append", default=[])
    parser.add_argument("--samples", type=int, default=9)
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--timeout", type=int, default=300)
    parser.add_argument("--root", type=pathlib.Path)
    args = parser.parse_args()
    if args.samples < 1 or args.warmups < 1:
        parser.error("--samples and --warmups must be positive")

    workspace = args.workspace.resolve()
    server_binary = resolve_executable(args.server)
    bazel = resolve_executable(args.bazel)
    temporary = None
    if args.root:
        root = args.root.resolve()
        root.mkdir(parents=True, exist_ok=True)
    else:
        temporary = tempfile.TemporaryDirectory(prefix="bazel-mcp-bep-benchmark-")
        root = pathlib.Path(temporary.name)
    output_user_root = root / "bazel"
    transports = ["tail", "fifo", "bes"]
    servers = {}
    for transport in transports:
        config = root / f"{transport}.toml"
        write_config(
            config,
            workspace,
            root / f"{transport}-store",
            output_user_root,
            bazel,
            transport,
        )
        servers[transport] = McpServer(server_binary, config)
    try:
        for _ in range(args.warmups):
            for transport in transports:
                servers[transport].build(
                    workspace, args.target, args.timeout, args.build_arg
                )
        samples = {transport: [] for transport in transports}
        for sample in range(args.samples):
            offset = sample % len(transports)
            order = transports[offset:] + transports[:offset]
            for transport in order:
                samples[transport].append(
                    servers[transport].build(
                        workspace, args.target, args.timeout, args.build_arg
                    )
                )
    finally:
        for server in servers.values():
            server.close()
        if temporary:
            temporary.cleanup()

    results = {transport: metrics(samples[transport]) for transport in transports}
    tail = results["tail"]
    fifo = results["fifo"]
    bes = results["bes"]
    print(
        json.dumps(
            {
                "schema_version": 2,
                "workspace": str(workspace),
                "target": args.target,
                "build_args": args.build_arg,
                "samples": args.samples,
                "warmups": args.warmups,
                "tail": tail,
                "fifo": fifo,
                "bes": bes,
                "fifo_over_tail_median_ratio": fifo["median_ms"] / tail["median_ms"],
                "fifo_median_delta_ms": fifo["median_ms"] - tail["median_ms"],
                "bes_over_tail_median_ratio": bes["median_ms"] / tail["median_ms"],
                "bes_median_delta_ms": bes["median_ms"] - tail["median_ms"],
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
