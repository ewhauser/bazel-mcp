# Starlark reducer examples

This dependency-free Bzlmod workspace exercises Bazel's real Starlark parser,
loader, macro evaluator, and rule-analysis evaluator. Each broken target lives
in its own package so one loading failure cannot hide the other scenarios.
`//:success` remains buildable; the reducer harness invokes the broken packages
explicitly through MCP.
