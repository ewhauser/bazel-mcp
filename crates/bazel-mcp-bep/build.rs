fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc);
    config.enum_attribute(
        "bazel_mcp.bep.BuildEvent.payload",
        "#[allow(clippy::large_enum_variant)]",
    );
    config.compile_protos(&["proto/build_event_stream_subset.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/build_event_stream_subset.proto");
    println!("cargo:rerun-if-changed=proto/PROVENANCE.md");
    Ok(())
}
