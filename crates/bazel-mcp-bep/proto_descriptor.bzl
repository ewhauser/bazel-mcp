"""Rule for generating a protobuf descriptor set with the exec-platform protoc."""

def _proto_descriptor_impl(ctx):
    src = ctx.file.src
    descriptor = ctx.actions.declare_file(ctx.label.name + ".bin")
    args = ctx.actions.args()
    args.add("--include_imports")
    args.add("--include_source_info")
    args.add(descriptor, format = "--descriptor_set_out=%s")
    args.add(src.dirname, format = "--proto_path=%s")
    args.add(src)

    ctx.actions.run(
        executable = ctx.executable._protoc,
        arguments = [args],
        inputs = [src],
        outputs = [descriptor],
        mnemonic = "ProtoDescriptor",
        progress_message = "Generating protobuf descriptor %{output}",
    )

    return DefaultInfo(files = depset([descriptor]))

proto_descriptor = rule(
    implementation = _proto_descriptor_impl,
    attrs = {
        "src": attr.label(
            allow_single_file = [".proto"],
            mandatory = True,
        ),
        "_protoc": attr.label(
            cfg = "exec",
            default = "@com_google_protobuf//:protoc",
            executable = True,
        ),
    },
)
