---
title: Debug a failure
description: Use bounded summaries and targeted inspection to diagnose a Bazel failure without replaying the full log.
---

The efficient debugging path is **run once, inspect narrowly, then validate the
fix**.

## 1. Reproduce with `bazel.run`

Ask the agent to run the smallest target that reproduces the problem:

> Test `//services/payments:payments_test` and explain the first concrete failure.

Failed results prioritize root-cause diagnostics over aggregate action
failures. Equivalent fanout failures are deduplicated and include a repetition
count instead of repeating the same compiler message.

## 2. Use the invocation ID

Start with the diagnostic already present in the bounded result. Inspect more
evidence only when it would change the next action.

Useful follow-ups include:

- the failed test log for one label;
- the next page of diagnostics;
- the normalized log around a specific pattern;
- artifacts or coverage produced by the invocation;
- query results too large for the initial response.

The server normalizes, redacts, deduplicates, filters, and sequences captured
stdout and stderr. There is intentionally no raw stdout/stderr selector in the
MCP shape.

## 3. Fix and rerun the focused target

After editing, rerun the same focused target through `bazel.run`. Preserve the
original invocation ID if comparison evidence may still be useful; every run
receives its own ID.

## 4. Expand validation deliberately

Once the focused target passes, expand to the smallest meaningful package or
suite. Avoid retrieving logs from successful invocations unless a specific
artifact or behavior needs verification.

:::tip
Treat `bazel.inspect` as a query over retained evidence, not as “show me
everything.” A narrow view and filter usually makes the next model turn both
cheaper and more accurate.
:::
