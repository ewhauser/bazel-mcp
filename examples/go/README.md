# Go reducer examples

This standalone Bzlmod workspace pins `rules_go` and a hermetic Go 1.23.4
toolchain. `//:success` demonstrates normal use. Targets under `//cases` are
intentionally broken and tagged `manual` so the reducer harness can exercise a
real compiler and real `go test` output without breaking wildcard builds.

Run these cases through `reducer-cases`; the harness is responsible for calling
`bazel.run` and retaining sanitized MCP evidence.
