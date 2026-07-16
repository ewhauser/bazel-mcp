export const prerender = true;

const base = 'https://ewhauser.github.io/bazel-mcp';

export function GET() {
  const body = `# bazel-mcp

> A local MCP server that runs Bazel for coding agents, returns bounded actionable results, and retains complete invocation evidence for narrow follow-up inspection.

bazel-mcp exposes exactly three tools: bazel.run, bazel.inspect, and bazel.cancel. It never invokes Bazel through a shell. Raw evidence stays local, while summaries, durable metadata, and telemetry are redacted.

## Start here

- [Get started](${base}/getting-started/): Install the server, connect an MCP client, and run the first build.
- [How it works](${base}/concepts/how-it-works/): Evidence lifecycle, response budgets, encodings, and the security boundary.
- [Tool reference](${base}/tools/): The complete public MCP tool surface.
- [Configuration](${base}/reference/generated/configuration/): Authoritative settings and CLI options.

## Guides

- [Debug a failure](${base}/guides/debugging-failures/): Root-cause-first debugging with targeted inspection.
- [Long-running builds](${base}/guides/long-running-builds/): Negotiated MCP task execution and recovery.
- [Architecture](${base}/project/architecture/): Runtime flow and design specifications.

## Evidence

- [Benchmarks](${base}/reference/generated/benchmarks/): Reproducible token and visible-byte measurements.
- [Security](${base}/reference/generated/security/): Vulnerability reporting and local execution threat model.
- [Full documentation context](${base}/llms-full.txt): Concatenated core project documentation.
`;

  return new Response(body, {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
}
