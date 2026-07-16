#!/usr/bin/env python3
import argparse
import atexit
import http.server
import json
import os
import pathlib
import subprocess
import sys
import threading


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


def call(process, request_id, workspace, command, arguments, timeout=60, startup_args=None):
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
                "startup_args": startup_args or [],
                "timeout_seconds": timeout,
            },
        },
    })
    response = receive(process, request_id)
    if "error" in response or response.get("result", {}).get("isError"):
        raise RuntimeError(f"MCP call failed: {response}")
    return json.loads(response["result"]["content"][0]["text"])


def inspect(process, request_id, invocation_id, view, limit=100, cursor=None):
    arguments = {"invocation_id": invocation_id, "view": view, "limit": limit}
    if cursor:
        arguments["cursor"] = cursor
    send(process, {
        "jsonrpc": "2.0", "id": request_id, "method": "tools/call",
        "params": {"name": "bazel.inspect", "arguments": arguments},
    })
    response = receive(process, request_id)
    if "error" in response or response.get("result", {}).get("isError"):
        raise RuntimeError(f"MCP inspect failed: {response}")
    return json.loads(response["result"]["content"][0]["text"])


class CacheHandler(http.server.BaseHTTPRequestHandler):
    blobs = {}
    gets = 0
    puts = 0

    def log_message(self, _format, *_args):
        return

    def do_PUT(self):
        length = int(self.headers.get("Content-Length", "0"))
        type(self).blobs[self.path] = self.rfile.read(length)
        type(self).puts += 1
        self.send_response(200)
        self.end_headers()

    def do_GET(self):
        type(self).gets += 1
        blob = type(self).blobs.get(self.path)
        if blob is None:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(blob)))
        self.end_headers()
        self.wfile.write(blob)


def start_remote_cache():
    CacheHandler.blobs = {}
    CacheHandler.gets = 0
    CacheHandler.puts = 0
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), CacheHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, f"http://127.0.0.1:{server.server_port}"


def shutdown_bazel(bazel, workspace, output_user_root, output_base=None):
    command = [str(bazel), f"--output_user_root={output_user_root}"]
    if output_base is not None:
        command.append(f"--output_base={output_base}")
    command.append("shutdown")
    try:
        subprocess.run(
            command, cwd=workspace, env=os.environ.copy(),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
            timeout=30,
        )
    except subprocess.TimeoutExpired:
        pass


def cleanup_bazel_servers(args):
    for output_base in [None, args.root / "remote-one", args.root / "remote-two"]:
        shutdown_bazel(args.bazel, args.workspace, args.root / "bazel", output_base)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--workspace", type=pathlib.Path, required=True)
    parser.add_argument("--server", type=pathlib.Path, required=True)
    parser.add_argument("--bazel", type=pathlib.Path, required=True)
    parser.add_argument("--root", type=pathlib.Path, required=True)
    parser.add_argument("--wrapper", action="store_true")
    parser.add_argument("--remote-executor", default=os.environ.get("BAZEL_MCP_REMOTE_EXECUTOR"))
    parser.add_argument("--bes-backend", default=os.environ.get("BAZEL_MCP_BES_BACKEND"))
    parser.add_argument("--remote-header", action="append", default=[])
    parser.add_argument("--bes-header", action="append", default=[])
    args = parser.parse_args()
    atexit.register(cleanup_bazel_servers, args)
    args.root.mkdir(parents=True, exist_ok=True)
    config = args.root / "config.toml"
    executable_config = "" if args.wrapper else (
        "bazel_executable = " + json.dumps(str(args.bazel)) + "\n"
    )
    config.write_text(
        "allowed_roots = [" + json.dumps(str(args.workspace)) + "]\n"
        + "cache_root = " + json.dumps(str(args.root / "store")) + "\n"
        + executable_config
        + "output_user_root = " + json.dumps(str(args.root / "bazel")) + "\n"
        + "environment_allowlist = [\"USE_BAZEL_VERSION\", \"BAZEL_MCP_WRAPPED_BAZEL\"]\n"
        + 'result_encoding = "text"\n'
        + "redaction_patterns = [\"(?i)(authorization|x-buildbuddy-api-key)=[^\\\\s]+\"]\n",
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
        ("cquery", "cquery", ["deps(//:large_319)"], 0, "succeeded", 60),
        ("aquery_aspect", "aquery", [
            "--include_aspects",
            "--aspects=//:rules.bzl%matrix_aspect",
            "--output_groups=matrix_aspect",
            "deps(//:large_319)",
        ], 0, "succeeded", 120),
        ("keep_going", "build", [
            "--keep_going", "//:keep_going_failure_one",
            "//:keep_going_failure_two", "//:keep_going_failure_three",
        ], 1, "failed", 60),
        ("external_repository_failure", "build", [
            "@matrix_broken_repo//:missing",
        ], 1, "failed", 60),
        ("timeout", "build", ["//:slow"], None, "timed_out", 1),
    ]
    for request_id, (name, command, command_args, exit_code, state, timeout) in enumerate(cases, 2):
        result = call(process, request_id, args.workspace, command, command_args, timeout)
        if result["state"] != state or result.get("exit_code") != exit_code:
            raise RuntimeError(f"{name} mismatch: {result}")
        print(f"{name}\t{result['state']}\t{result.get('exit_code')}")
    query_result = call(
        process, 100, args.workspace, "query", ["filter('^//:large_', //:*)"], 60
    )
    first_page = inspect(
        process, 101, query_result["invocation_id"], "query_results", limit=100
    )
    if first_page.get("total_count") != 320 or not first_page.get("next_cursor"):
        raise RuntimeError(f"large query was not paginated: {first_page}")
    second_page = inspect(
        process, 102, query_result["invocation_id"], "query_results",
        limit=100, cursor=first_page["next_cursor"],
    )
    if not second_page.get("items"):
        raise RuntimeError(f"large query second page was empty: {second_page}")
    print("large_query_pagination\tsucceeded\t0")

    cache_server, cache_url = start_remote_cache()
    try:
        remote_args = [
            "//:remote_cache_target", f"--remote_cache={cache_url}",
            "--disk_cache=",
            "--remote_upload_local_results=true", "--remote_timeout=10",
        ]
        first = call(
            process, 103, args.workspace, "build", remote_args, 120,
            startup_args=[f"--output_base={args.root / 'remote-one'}"],
        )
        second = call(
            process, 104, args.workspace, "build", remote_args, 120,
            startup_args=[f"--output_base={args.root / 'remote-two'}"],
        )
        if first["state"] != "succeeded" or second["state"] != "succeeded":
            raise RuntimeError(f"remote cache builds failed: {first} {second}")
        if CacheHandler.puts == 0 or CacheHandler.gets == 0:
            raise RuntimeError(
                "remote cache did not observe both uploads and reads "
                f"(puts={CacheHandler.puts}, gets={CacheHandler.gets})"
            )
        print("remote_cache\tsucceeded\t0")
    finally:
        cache_server.shutdown()
        cache_server.server_close()

    if args.remote_executor:
        remote_arguments = [
            "//:ok", f"--remote_executor={args.remote_executor}",
            "--spawn_strategy=remote", "--remote_timeout=60",
        ]
        remote_arguments.extend(f"--remote_header={value}" for value in args.remote_header)
        if args.bes_backend:
            remote_arguments.extend([
                f"--bes_backend={args.bes_backend}",
                "--bes_upload_mode=wait_for_upload_complete",
            ])
            remote_arguments.extend(f"--bes_header={value}" for value in args.bes_header)
        remote = call(
            process, 105, args.workspace, "build", remote_arguments, 300,
            startup_args=[f"--output_base={args.root / 'remote-execution'}"],
        )
        if remote["state"] != "succeeded":
            raise RuntimeError(f"remote execution failed: {remote}")
        case = "remote_execution_bes_coexistence" if args.bes_backend else "remote_execution"
        print(f"{case}\tsucceeded\t0")

    if args.bes_backend and not args.remote_executor:
        bes_arguments = [
            "//:ok", f"--bes_backend={args.bes_backend}",
            "--bes_upload_mode=wait_for_upload_complete",
        ]
        bes_arguments.extend(f"--bes_header={value}" for value in args.bes_header)
        bes = call(
            process, 106, args.workspace, "build", bes_arguments, 180,
            startup_args=[f"--output_base={args.root / 'bes'}"],
        )
        if bes["state"] != "succeeded":
            raise RuntimeError(f"BES coexistence failed: {bes}")
        print("bes_coexistence\tsucceeded\t0")
    process.stdin.close()
    if process.wait(timeout=10) != 0:
        raise RuntimeError("MCP server exited unsuccessfully")


if __name__ == "__main__":
    main()
