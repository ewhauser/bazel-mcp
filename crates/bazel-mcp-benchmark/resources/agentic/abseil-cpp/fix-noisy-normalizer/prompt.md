The `//bazel_mcp_agentic_noisy:normalizer_test` regression emits a large
matrix of failures. Before editing any file, reproduce it with this target and
confirm that it fails. For a direct shell invocation, use
`--test_output=errors`; the Bazel MCP server owns test-output flags, so do not
pass `--test_output` through MCP.

`NormalizeKey` must remove leading and trailing ASCII whitespace and lowercase
the remaining key. Fix only the implementation source. Do not edit the BUILD
file, header, test source, or public function signature.

After the fix, rerun the same focused Bazel test with the same
adapter-appropriate output policy.
