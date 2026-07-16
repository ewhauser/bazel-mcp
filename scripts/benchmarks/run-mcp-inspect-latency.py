#!/usr/bin/env python3
"""Paired latency benchmark for a retained Bazel MCP inspection view."""

import argparse
import hashlib
import json
import pathlib
import platform
import statistics
import subprocess
import sys
import tempfile
import time


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


def call(process, request_id, name, arguments):
    send(
        process,
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        },
    )
    response = receive(process, request_id)
    if "error" in response or response.get("result", {}).get("isError"):
        raise RuntimeError(f"MCP call failed: {response}")
    return json.loads(response["result"]["content"][0]["text"])


def run_round(server, workspace, options, cache_parent):
    with tempfile.TemporaryDirectory(prefix="bazel-mcp-inspect-", dir=cache_parent) as root:
        process = subprocess.Popen(
            [
                str(server),
                "--allow-root",
                str(workspace),
                "--cache-root",
                root,
                "--log",
                "error",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=sys.stderr,
            text=True,
        )
        try:
            send(
                process,
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": {"name": "inspect-latency", "version": "1"},
                    },
                },
            )
            initialized = receive(process, 1)
            if "error" in initialized:
                raise RuntimeError(f"MCP initialize failed: {initialized}")
            send(process, {"jsonrpc": "2.0", "method": "notifications/initialized"})
            run_result = call(
                process,
                2,
                "bazel.run",
                {
                    "workspace": str(workspace),
                    "command": options.command,
                    "args": options.argument,
                    "timeout_seconds": options.timeout,
                },
            )
            inspect_arguments = {
                "invocation_id": run_result["invocation_id"],
                "view": options.view,
                "limit": options.limit,
            }
            request_id = 3
            for _ in range(options.warmup):
                call(process, request_id, "bazel.inspect", inspect_arguments)
                request_id += 1
            started = time.perf_counter_ns()
            for _ in range(options.calls):
                call(process, request_id, "bazel.inspect", inspect_arguments)
                request_id += 1
            return (time.perf_counter_ns() - started) / options.calls / 1_000
        finally:
            if process.stdin:
                process.stdin.close()
            if process.wait(timeout=10) != 0:
                raise RuntimeError("MCP server exited unsuccessfully")


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def binary_metadata(path, label):
    return {
        "label": label,
        "path": str(path),
        "sha256": sha256(path),
        "version": subprocess.check_output([str(path), "--version"], text=True).strip(),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline", type=pathlib.Path, required=True)
    parser.add_argument("--candidate", type=pathlib.Path, required=True)
    parser.add_argument("--baseline-label", default="baseline")
    parser.add_argument("--candidate-label", default="candidate")
    parser.add_argument("--workspace", type=pathlib.Path, required=True)
    parser.add_argument("--pairs", type=int, default=9)
    parser.add_argument("--calls", type=int, default=1_000)
    parser.add_argument("--warmup", type=int, default=100)
    parser.add_argument("--command", default="info")
    parser.add_argument("--argument", action="append")
    parser.add_argument("--view", default="summary")
    parser.add_argument("--limit", type=int, default=20)
    parser.add_argument("--timeout", type=int, default=60)
    parser.add_argument("--cache-parent", type=pathlib.Path)
    options = parser.parse_args()
    if options.argument is None:
        options.argument = ["release"]
    if options.pairs < 1 or options.calls < 1 or options.warmup < 0:
        parser.error("pairs and calls must be positive; warmup must be non-negative")
    baseline = options.baseline.resolve()
    candidate = options.candidate.resolve()
    workspace = options.workspace.resolve()
    cache_parent = options.cache_parent or pathlib.Path(tempfile.gettempdir())
    cache_parent.mkdir(parents=True, exist_ok=True)
    if workspace == cache_parent or workspace in cache_parent.parents:
        parser.error("cache-parent must be outside the benchmark workspace")

    samples = {"baseline": [], "candidate": []}
    paired = []
    for index in range(options.pairs):
        order = ["baseline", "candidate"]
        if index % 2:
            order.reverse()
        current = {}
        for name in order:
            server = baseline if name == "baseline" else candidate
            current[name] = run_round(server, workspace, options, cache_parent)
            samples[name].append(current[name])
        current["order"] = order
        current["improvement_percent"] = (
            (current["baseline"] - current["candidate"]) / current["baseline"] * 100
        )
        paired.append(current)

    baseline_median = statistics.median(samples["baseline"])
    candidate_median = statistics.median(samples["candidate"])
    report = {
        "schema_version": 1,
        "environment": {
            "platform": platform.platform(),
            "python": platform.python_version(),
        },
        "baseline": binary_metadata(baseline, options.baseline_label),
        "candidate": binary_metadata(candidate, options.candidate_label),
        "workload": {
            "workspace": str(workspace),
            "command": options.command,
            "arguments": options.argument,
            "view": options.view,
            "limit": options.limit,
            "pairs": options.pairs,
            "calls_per_round": options.calls,
            "warmup_calls": options.warmup,
            "server_startup_included": False,
            "bazel_run_included": False,
        },
        "baseline_samples_us_per_call": samples["baseline"],
        "candidate_samples_us_per_call": samples["candidate"],
        "paired_samples": paired,
        "baseline_median_us_per_call": baseline_median,
        "candidate_median_us_per_call": candidate_median,
        "median_ratio_improvement_percent": (
            (baseline_median - candidate_median) / baseline_median * 100
        ),
        "median_paired_improvement_percent": statistics.median(
            sample["improvement_percent"] for sample in paired
        ),
        "minimum_paired_improvement_percent": min(
            sample["improvement_percent"] for sample in paired
        ),
    }
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
