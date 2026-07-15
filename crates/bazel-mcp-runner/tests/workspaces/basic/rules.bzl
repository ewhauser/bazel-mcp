def _write_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output, "ok\n")
    return [DefaultInfo(files = depset([output]))]

write_file = rule(implementation = _write_impl)

def _fail_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [output],
        command = "echo 'warning: repeated matrix warning' >&2; echo 'MATRIX_ACTION_ROOT_CAUSE' >&2; exit 1",
    )
    return [DefaultInfo(files = depset([output]))]

failing_action = rule(implementation = _fail_impl)

def _slow_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [output],
        command = "sleep 10; echo done > $1",
        arguments = [output.path],
    )
    return [DefaultInfo(files = depset([output]))]

slow_action = rule(implementation = _slow_impl)

def _test_impl(ctx):
    executable = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        executable,
        "#!/usr/bin/env bash\necho '%s' >&2\nexit %d\n" % (ctx.attr.message, ctx.attr.exit_code),
        is_executable = True,
    )
    return [DefaultInfo(executable = executable)]

matrix_test = rule(
    implementation = _test_impl,
    test = True,
    attrs = {
        "exit_code": attr.int(default = 0),
        "message": attr.string(default = "ok"),
    },
)
