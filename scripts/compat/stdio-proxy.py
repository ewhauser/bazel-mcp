#!/usr/bin/env python3
"""Transparent stdio proxy that records normalized MCP JSON lines."""

import argparse
import json
import subprocess
import sys
import threading


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--trace", required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    arguments = parser.parse_args()
    if not arguments.command:
        parser.error("a server command is required")

    child = subprocess.Popen(
        arguments.command,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    trace = open(arguments.trace, "a", encoding="utf-8")
    trace_lock = threading.Lock()

    def record(direction, line):
        try:
            message = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError):
            message = {"invalid": line.decode("utf-8", errors="replace").rstrip("\n")}
        with trace_lock:
            trace.write(json.dumps({"direction": direction, "message": message}, sort_keys=True))
            trace.write("\n")
            trace.flush()

    def client_to_server():
        try:
            for line in sys.stdin.buffer:
                record("client_to_server", line)
                child.stdin.write(line)
                child.stdin.flush()
        finally:
            child.stdin.close()

    def server_to_client():
        for line in child.stdout:
            record("server_to_client", line)
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()

    def server_stderr():
        for chunk in iter(lambda: child.stderr.read(8192), b""):
            sys.stderr.buffer.write(chunk)
            sys.stderr.buffer.flush()

    threads = [
        threading.Thread(target=client_to_server, daemon=True),
        threading.Thread(target=server_to_client, daemon=True),
        threading.Thread(target=server_stderr, daemon=True),
    ]
    for thread in threads:
        thread.start()
    status = child.wait()
    threads[1].join(timeout=2)
    threads[2].join(timeout=2)
    trace.close()
    raise SystemExit(status)


if __name__ == "__main__":
    main()
