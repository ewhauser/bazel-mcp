---
title: Get started
description: Install bazel-mcp, connect an MCP client, and run the first Bazel build.
---

`bazel-mcp` is a local MCP server. It runs beside your coding agent, invokes the
Bazel already configured for your workspace, and returns a bounded result.

## Requirements

- macOS, Linux, or Windows x86_64 (preview)
- Bazel 8 or 9, Bazelisk, or an executable workspace-local `tools/bazel`
- An MCP-compatible client

## Install

The shortest path on macOS or Linux with Homebrew is:

```sh
brew install ewhauser/tap/bazel-mcp
```

You can also download a prebuilt archive from the
[latest GitHub release](https://github.com/ewhauser/bazel-mcp/releases/latest).
The release includes shell and PowerShell installers.

## Connect your client

Register the installed binary as a local stdio MCP server. MCP clients use
different settings files, but the server entry has this shape:

```json
{
  "mcpServers": {
    "bazel": {
      "command": "bazel-mcp"
    }
  }
}
```

Restart the client after changing its MCP configuration.

:::note
Standard output is reserved for MCP protocol frames. Server diagnostics are
written to standard error, so the default stdio transport remains safe.
:::

## Run the first build

Open a Bazel workspace in your coding agent and ask:

> Build `//app:server`.

The agent calls `bazel.run` with the workspace, command, and argument array. A
successful result is limited to 2 KiB and a failed result to 8 KiB. If more
evidence exists, the result includes an invocation ID the agent can pass to
`bazel.inspect`.

Try these next:

- “Run the tests under `//services/...` and explain any failures.”
- “Which targets depend on `//lib:core`?”
- “Show the failed test log from the last invocation.”

## Optional configuration

No configuration file is required. The defaults can use any Bazel workspace
available to the current user and retain invocation evidence in a local cache.

For shared environments, workspace restrictions, custom redaction, retention,
or task settings, continue to the [configuration reference](../reference/generated/configuration/).
