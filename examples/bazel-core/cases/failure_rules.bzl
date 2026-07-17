"""Minimal actions used to exercise Bazel-owned action failure evidence."""

def _fixture_failure_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(
        outputs = [output],
        command = "echo '%s' >&2; exit 1" % ctx.attr.message,
        mnemonic = "ReducerFixtureFailure",
        progress_message = "Running reducer fixture %{label}",
    )
    return [DefaultInfo(files = depset([output]))]

fixture_failure = rule(
    implementation = _fixture_failure_impl,
    attrs = {
        "message": attr.string(mandatory = True),
    },
)

def _analysis_failure_impl(ctx):
    fail("BAZEL_MCP_ANALYSIS_ROOT_CAUSE for %s" % ctx.label)

analysis_failure = rule(implementation = _analysis_failure_impl)
