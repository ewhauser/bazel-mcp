The `//bazel_mcp_agentic:label_test` target fails because its Bazel dependency
metadata is incomplete. The C++ implementation and its tests are correct.

Fix only the Bazel metadata needed for the target to compile and pass. Run the
relevant Bazel test before finishing.
