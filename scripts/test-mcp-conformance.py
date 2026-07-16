#!/usr/bin/env python3
"""Credential-free stdio conformance for negotiated Bazel MCP execution."""

import json
import pathlib
import subprocess
import tempfile
import threading
import time
import uuid


TASKS_EXTENSION = "io.modelcontextprotocol/tasks"


class Session:
    def __init__(self, server, config):
        self.process = subprocess.Popen(
            [str(server), "--config", str(config), "--log", "bazel_mcp=info"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.next_id = 1
        self.messages = []
        self.stderr_lines = []
        self.stderr_reader = threading.Thread(target=self._drain_stderr, daemon=True)
        self.stderr_reader.start()

    def _drain_stderr(self):
        for line in self.process.stderr:
            self.stderr_lines.append(line)

    def stderr(self):
        self.stderr_reader.join(timeout=1)
        return "".join(self.stderr_lines)

    def request(self, method, params=None):
        request_id = self.next_id
        self.next_id += 1
        message = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        self.process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.process.stdin.flush()
        while True:
            line = self.process.stdout.readline()
            if not line:
                raise AssertionError(f"server closed stdout: {self.stderr()}")
            response = json.loads(line)
            self.messages.append(response)
            if response.get("id") == request_id:
                return response

    def notify(self, method, params=None):
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self.process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def cancel_request(self, method, params, delay=0.1):
        request_id = self.next_id
        self.next_id += 1
        message = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }
        self.process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.process.stdin.flush()
        time.sleep(delay)
        self.notify(
            "notifications/cancelled",
            {"requestId": request_id, "reason": "conformance cancellation"},
        )
        return request_id

    def initialize(self, version):
        response = self.request(
            "initialize",
            {
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": {"name": "bazel-mcp-conformance", "version": "1"},
            },
        )
        assert "error" not in response, response
        assert response["result"]["protocolVersion"] == version
        assert response["result"]["serverInfo"]["name"] == "bazel-mcp"
        self.notify("notifications/initialized")
        return response["result"]

    def close(self):
        self.process.stdin.close()
        assert self.process.wait(timeout=15) == 0
        stderr = self.stderr()
        assert "CONFORMANCE_SECRET" not in stderr
        for message in self.messages:
            json.dumps(message)


def extension_meta():
    return {
        "io.modelcontextprotocol/clientCapabilities": {
            "extensions": {TASKS_EXTENSION: {}}
        }
    }


def run_arguments(workspace, target="//:slow", command="build"):
    return {"workspace": str(workspace), "command": command, "args": [target]}


def tool_json(result):
    if result.get("structuredContent") is not None:
        return result["structuredContent"]
    return json.loads(result["content"][0]["text"])


def listed_tools(session):
    response = session.request("tools/list", {})
    assert "error" not in response, response
    tools = response["result"]["tools"]
    assert [tool["name"] for tool in tools] == [
        "bazel.cancel",
        "bazel.inspect",
        "bazel.run",
    ]
    for tool in tools:
        assert tool["description"]
        assert tool["inputSchema"]["type"] == "object"
    return tools


def without_execution(tools):
    return [{key: value for key, value in tool.items() if key != "execution"} for tool in tools]


def poll_extension(session, task_id, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        response = session.request(
            "tasks/get", {"taskId": task_id, "_meta": extension_meta()}
        )
        assert "error" not in response, response
        result = response["result"]
        assert result["resultType"] == "complete"
        if result["status"] != "working":
            return result
        time.sleep(0.05)
    raise AssertionError(f"task {task_id} did not become terminal")


def write_config(path, workspace, cache, wrapper, policy):
    path.write_text(
        f"allowed_roots = [{json.dumps(str(workspace))}]\n"
        f"cache_root = {json.dumps(str(cache))}\n"
        f"bazel_executable = {json.dumps(str(wrapper))}\n"
        f"mcp_execution_policy = {json.dumps(policy)}\n"
        "task_ttl_seconds = 3600\n"
        "task_poll_interval_ms = 100\n"
        "progress_initial_seconds = 1\n"
        "progress_interval_seconds = 60\n"
        "cancellation_interrupt_grace_seconds = 1\n"
        "cancellation_terminate_grace_seconds = 1\n"
    )


def main():
    root = pathlib.Path.cwd()
    server = root / "target/debug/bazel-mcp"
    assert server.is_file(), "build target/debug/bazel-mcp before conformance"
    with tempfile.TemporaryDirectory() as temporary_name:
        temporary = pathlib.Path(temporary_name)
        workspace = temporary / "workspace"
        workspace.mkdir()
        (workspace / "MODULE.bazel").write_text("module(name='conformance')\n")
        launches = temporary / "launches"
        wrapper = temporary / "fake-bazel"
        wrapper.write_text(
            "#!/bin/sh\n"
            "if [ \"${1:-}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\n"
            f"echo invocation >> {json.dumps(str(launches))}\n"
            f"trap 'echo cancelled >> {json.dumps(str(launches))}; exit 130' INT TERM\n"
            "delay=1.2\nstatus=0\ncrash_parent=0\n"
            "for arg in \"$@\"; do\n"
            "  [ \"$arg\" = //:instant ] && delay=0\n"
            "  [ \"$arg\" = //:cancel ] && delay=5\n"
            "  [ \"$arg\" = //:fail ] && status=7\n"
            "  [ \"$arg\" = //:crash-server ] && delay=0.4 && crash_parent=1\n"
            "done\n"
            "sleep \"$delay\"\n"
            "[ \"$crash_parent\" -eq 1 ] && kill -9 \"$PPID\"\n"
            "exit \"$status\"\n"
        )
        wrapper.chmod(0o700)

        def session(name, policy="auto"):
            config = temporary / f"{name}.toml"
            write_config(
                config, workspace, temporary / f"cache-{name}", wrapper, policy
            )
            return Session(server, config)

        invalid_config = temporary / "invalid-policy.toml"
        write_config(
            invalid_config,
            workspace,
            temporary / "cache-invalid-policy",
            wrapper,
            "detached",
        )
        invalid_startup = subprocess.run(
            [str(server), "--config", str(invalid_config)],
            input="",
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        assert invalid_startup.returncode != 0
        assert invalid_startup.stdout == ""
        assert invalid_startup.stderr

        # Earlier clients remain synchronous and keep bounded progress behavior.
        sync = session("sync")
        initialized = sync.initialize("2025-06-18")
        assert "tasks" not in initialized["capabilities"]
        sync_tools = listed_tools(sync)
        completed = sync.request(
            "tools/call",
            {
                "name": "bazel.run",
                "_meta": {"progressToken": "conformance-progress"},
                "arguments": run_arguments(workspace),
            },
        )
        assert completed["result"]["isError"] is False
        assert tool_json(completed["result"])["state"] == "succeeded"
        assert any(
            message.get("method") == "notifications/progress"
            for message in sync.messages
        )
        failed = sync.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, "//:fail"),
            },
        )
        assert failed["result"]["isError"] is False
        assert tool_json(failed["result"])["state"] == "failed"
        sync.cancel_request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, "//:cancel"),
            },
        )
        assert "error" not in sync.request("ping", {})
        cancellation_deadline = time.monotonic() + 5
        while "cancelled" not in launches.read_text():
            assert time.monotonic() < cancellation_deadline
            time.sleep(0.05)
        sync.close()

        # Legacy 2025-11-25 task creation, polling, listing, result, and cancellation.
        legacy = session("legacy")
        initialized = legacy.initialize("2025-11-25")
        assert initialized["capabilities"]["tasks"]["requests"]["tools"]["call"] == {}
        tools = listed_tools(legacy)
        assert without_execution(tools) == sync_tools
        run_tool = next(tool for tool in tools if tool["name"] == "bazel.run")
        assert run_tool["execution"]["taskSupport"] == "optional"
        started = time.monotonic()
        created = legacy.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace),
                "task": {"ttl": 1},
            },
        )
        assert time.monotonic() - started < 0.5
        task = created["result"]["task"]
        task_id = task["taskId"]
        uuid.UUID(task_id)
        assert task["status"] == "working"
        assert task["ttl"] >= 3_600_000
        assert created["result"]["_meta"]["io.modelcontextprotocol/related-task"]["taskId"] == task_id
        current = legacy.request("tasks/get", {"taskId": task_id})
        assert current["result"]["taskId"] == task_id
        listed = legacy.request("tasks/list", {})
        assert any(item["taskId"] == task_id for item in listed["result"]["tasks"])
        payload = legacy.request("tasks/result", {"taskId": task_id})
        assert payload["result"]["isError"] is False
        legacy_logical = tool_json(payload["result"])
        assert legacy_logical["state"] == "succeeded"
        assert payload["result"]["_meta"]["io.modelcontextprotocol/related-task"]["taskId"] == task_id

        failed_id = legacy.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, "//:fail"),
                "task": {},
            },
        )["result"]["task"]["taskId"]
        failed_payload = legacy.request("tasks/result", {"taskId": failed_id})
        assert failed_payload["result"]["isError"] is False
        assert tool_json(failed_payload["result"])["state"] == "failed"
        assert legacy.request("tasks/get", {"taskId": failed_id})["result"]["status"] == "completed"

        before = launches.read_text().count("invocation")
        denied = legacy.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, command="clean"),
                "task": {},
            },
        )
        assert denied["result"]["isError"] is True
        assert launches.read_text().count("invocation") == before
        augmented_inspect = legacy.request(
            "tools/call",
            {
                "name": "bazel.inspect",
                "arguments": {"invocation_id": task_id, "view": "summary"},
                "task": {},
            },
        )
        assert augmented_inspect["error"]["code"] == -32601
        assert legacy.request("tasks/update", {"taskId": task_id})["error"]["code"] == -32601
        assert legacy.request("tasks/get", {"taskId": str(uuid.uuid4())})["error"]["code"] == -32602

        pagination_ids = []
        for _ in range(101):
            pagination_ids.append(
                legacy.request(
                    "tools/call",
                    {
                        "name": "bazel.run",
                        "arguments": run_arguments(workspace, "//:instant"),
                        "task": {},
                    },
                )["result"]["task"]["taskId"]
            )
        first_page = legacy.request("tasks/list", {})["result"]
        assert len(first_page["tasks"]) == 100
        assert first_page.get("nextCursor")
        second_page = legacy.request(
            "tasks/list", {"cursor": first_page["nextCursor"]}
        )["result"]
        first_ids = {task["taskId"] for task in first_page["tasks"]}
        second_ids = {task["taskId"] for task in second_page["tasks"]}
        assert first_ids.isdisjoint(second_ids)
        assert set(pagination_ids).issubset(first_ids | second_ids)

        cancelled = legacy.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, "//:cancel"),
                "task": {},
            },
        )["result"]["task"]["taskId"]
        cancel_result = legacy.request("tasks/cancel", {"taskId": cancelled})
        assert cancel_result["result"]["status"] == "cancelled"
        assert legacy.request("tasks/cancel", {"taskId": cancelled})["error"]["code"] == -32602
        cancelled_payload = legacy.request("tasks/result", {"taskId": cancelled})
        assert tool_json(cancelled_payload["result"])["state"] == "cancelled"
        legacy.close()

        # Final SEP-2663 extension: discovery, per-request capability, inline result.
        extension = session("extension")
        discovered = extension.request("server/discover", {})
        assert TASKS_EXTENSION in discovered["result"]["capabilities"]["extensions"]
        initialized = extension.initialize("2026-06-30")
        assert "tasks" not in initialized["capabilities"]
        assert TASKS_EXTENSION in initialized["capabilities"]["extensions"]
        extension_tools = listed_tools(extension)
        assert extension_tools == sync_tools
        started = time.monotonic()
        created = extension.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace),
                "_meta": extension_meta(),
            },
        )
        assert time.monotonic() - started < 0.5
        task = created["result"]
        assert task["resultType"] == "task"
        extension_id = task["taskId"]
        immediate = extension.request(
            "tasks/get", {"taskId": extension_id, "_meta": extension_meta()}
        )
        assert immediate["result"]["resultType"] == "complete"
        assert immediate["result"]["status"] == "working"
        updated = extension.request(
            "tasks/update",
            {"taskId": extension_id, "inputResponses": {}, "_meta": extension_meta()},
        )
        assert updated["result"] == {"resultType": "complete"}
        assert extension.request(
            "tasks/result", {"taskId": extension_id, "_meta": extension_meta()}
        )["error"]["code"] == -32601
        assert extension.request(
            "tasks/list", {"_meta": extension_meta()}
        )["error"]["code"] == -32601
        terminal = poll_extension(extension, extension_id)
        assert terminal["status"] == "completed"
        extension_logical = tool_json(terminal["result"])
        assert extension_logical["state"] == "succeeded"
        for field in ("state", "command", "exit_code"):
            assert extension_logical[field] == legacy_logical[field], (
                field,
                extension_logical[field],
                legacy_logical[field],
            )

        failed_id = extension.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, "//:fail"),
                "_meta": extension_meta(),
            },
        )["result"]["taskId"]
        failed_task = poll_extension(extension, failed_id)
        assert failed_task["status"] == "completed"
        assert failed_task["result"]["isError"] is False
        assert tool_json(failed_task["result"])["state"] == "failed"
        assert extension.request(
            "tasks/get", {"taskId": str(uuid.uuid4()), "_meta": extension_meta()}
        )["error"]["code"] == -32602

        before = launches.read_text().count("invocation")
        denied = extension.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, command="clean"),
                "_meta": extension_meta(),
            },
        )
        assert denied["result"]["isError"] is True
        assert launches.read_text().count("invocation") == before

        # A non-declaring modern request falls back to an ordinary result.
        fallback = extension.request(
            "tools/call",
            {"name": "bazel.run", "arguments": run_arguments(workspace, "//:instant")},
        )
        assert fallback["result"]["isError"] is False

        cancelled = extension.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, "//:cancel"),
                "_meta": extension_meta(),
            },
        )["result"]["taskId"]
        ack = extension.request(
            "tasks/cancel", {"taskId": cancelled, "_meta": extension_meta()}
        )
        assert ack["result"] == {"resultType": "complete"}
        assert poll_extension(extension, cancelled)["status"] == "cancelled"
        extension.close()

        # Policy matrix: sync_only ignores task capability; tasks_required rejects early.
        sync_only = session("sync-only", "sync_only")
        sync_only.initialize("2025-11-25")
        assert "execution" not in next(
            tool for tool in listed_tools(sync_only) if tool["name"] == "bazel.run"
        )
        ordinary = sync_only.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, "//:instant"),
                "task": {},
            },
        )
        assert ordinary["result"]["isError"] is False
        sync_only.close()

        required_legacy = session("required-legacy", "tasks_required")
        required_legacy.initialize("2025-11-25")
        before = launches.read_text().count("invocation")
        missing = required_legacy.request(
            "tools/call",
            {"name": "bazel.run", "arguments": run_arguments(workspace, "//:instant")},
        )
        assert missing["error"]["code"] == -32601
        assert launches.read_text().count("invocation") == before
        required_legacy.close()

        required_extension = session("required-extension", "tasks_required")
        required_extension.initialize("2026-06-30")
        before = launches.read_text().count("invocation")
        missing = required_extension.request(
            "tools/call",
            {"name": "bazel.run", "arguments": run_arguments(workspace, "//:instant")},
        )
        assert missing["error"]["code"] == -32003
        assert TASKS_EXTENSION in missing["error"]["data"]["requiredCapabilities"]["extensions"]
        assert launches.read_text().count("invocation") == before
        required_extension.close()

        expiry_config = temporary / "expiry.toml"
        write_config(
            expiry_config,
            workspace,
            temporary / "cache-expiry",
            wrapper,
            "auto",
        )
        expiry_config.write_text(
            expiry_config.read_text().replace(
                "task_ttl_seconds = 3600", "task_ttl_seconds = 1"
            )
        )
        expiring = Session(server, expiry_config)
        expiring.initialize("2025-11-25")
        expiring_id = expiring.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, "//:instant"),
                "task": {},
            },
        )["result"]["task"]["taskId"]
        expiring.request("tasks/result", {"taskId": expiring_id})
        time.sleep(1.1)
        assert expiring.request("tasks/get", {"taskId": expiring_id})["error"]["code"] == -32602
        expiring.close()

        # A process crash never reruns Bazel; durable handles expose interruption.
        restart_config = temporary / "restart-legacy.toml"
        write_config(
            restart_config,
            workspace,
            temporary / "cache-restart-legacy",
            wrapper,
            "auto",
        )
        crashing = Session(server, restart_config)
        crashing.initialize("2025-11-25")
        restart_id = crashing.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, "//:crash-server"),
                "task": {},
            },
        )["result"]["task"]["taskId"]
        assert crashing.process.wait(timeout=5) != 0
        write_config(
            restart_config,
            workspace,
            temporary / "cache-restart-legacy",
            wrapper,
            "sync_only",
        )
        recovered = Session(server, restart_config)
        recovered.initialize("2025-11-25")
        recovered_task = recovered.request("tasks/get", {"taskId": restart_id})
        assert recovered_task["result"]["status"] == "completed"
        recovered_result = recovered.request("tasks/result", {"taskId": restart_id})
        assert tool_json(recovered_result["result"])["state"] == "interrupted"
        recovered.close()

        mismatched = Session(server, restart_config)
        mismatched.initialize("2026-06-30")
        assert mismatched.request(
            "tasks/get", {"taskId": restart_id, "_meta": extension_meta()}
        )["error"]["code"] == -32602
        mismatched.close()

        extension_restart_config = temporary / "restart-extension.toml"
        write_config(
            extension_restart_config,
            workspace,
            temporary / "cache-restart-extension",
            wrapper,
            "auto",
        )
        crashing = Session(server, extension_restart_config)
        crashing.initialize("2026-06-30")
        extension_restart_id = crashing.request(
            "tools/call",
            {
                "name": "bazel.run",
                "arguments": run_arguments(workspace, "//:crash-server"),
                "_meta": extension_meta(),
            },
        )["result"]["taskId"]
        assert crashing.process.wait(timeout=5) != 0
        recovered = Session(server, extension_restart_config)
        recovered.initialize("2026-06-30")
        recovered_task = recovered.request(
            "tasks/get", {"taskId": extension_restart_id, "_meta": extension_meta()}
        )["result"]
        assert recovered_task["status"] == "completed"
        assert tool_json(recovered_task["result"])["state"] == "interrupted"
        recovered.close()

    print("negotiated synchronous, legacy task, extension task, and policy conformance passed")


if __name__ == "__main__":
    main()
