def _action_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(
        outputs = [output],
        command = ctx.attr.command,
        mnemonic = "FixtureAction",
        progress_message = "Running fixture action %{label}",
    )
    return [DefaultInfo(files = depset([output]))]

fixture_action = rule(
    implementation = _action_impl,
    attrs = {"command": attr.string(mandatory = True)},
)

def _test_impl(ctx):
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(executable, ctx.attr.script, is_executable = True)
    return [DefaultInfo(executable = executable)]

fixture_test = rule(
    implementation = _test_impl,
    test = True,
    attrs = {"script": attr.string(mandatory = True)},
)

def _library_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output, "fixture")
    return [DefaultInfo(files = depset([output]))]

fixture_library = rule(implementation = _library_impl)

def _consumer_impl(ctx):
    return [DefaultInfo(files = ctx.attr.dep[DefaultInfo].files)]

fixture_consumer = rule(
    implementation = _consumer_impl,
    attrs = {"dep": attr.label(mandatory = True)},
)
