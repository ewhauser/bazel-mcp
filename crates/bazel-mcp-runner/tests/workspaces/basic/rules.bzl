def _write_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output, "ok\n")
    return [DefaultInfo(files = depset([output]))]

write_file = rule(implementation = _write_impl)

def _remote_cache_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [output],
        command = "printf 'remote cache fixture\\n' > $1",
        arguments = [output.path],
        mnemonic = "MatrixRemoteCache",
    )
    return [DefaultInfo(files = depset([output]))]

remote_cache_action = rule(implementation = _remote_cache_impl)

def _fail_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs = [output],
        command = "echo 'warning: repeated matrix warning' >&2; echo 'MATRIX_ACTION_ROOT_CAUSE' >&2; exit 1",
    )
    return [DefaultInfo(files = depset([output]))]

failing_action = rule(implementation = _fail_impl)

def _graph_node_impl(ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(output, ctx.label.name + "\n")
    transitive = [dep[DefaultInfo].files for dep in ctx.attr.deps]
    return [DefaultInfo(files = depset([output], transitive = transitive))]

graph_node = rule(
    implementation = _graph_node_impl,
    attrs = {"deps": attr.label_list()},
)

def large_graph(size):
    previous = None
    for index in range(size):
        name = "large_%d" % index
        graph_node(
            name = name,
            deps = [] if previous == None else [":" + previous],
        )
        previous = name

def _aspect_impl(target, ctx):
    output = ctx.actions.declare_file(ctx.label.name + ".matrix-aspect.txt")
    ctx.actions.write(output, "aspect " + str(ctx.label) + "\n")
    return [OutputGroupInfo(matrix_aspect = depset([output]))]

matrix_aspect = aspect(
    implementation = _aspect_impl,
    attr_aspects = ["deps"],
)

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
