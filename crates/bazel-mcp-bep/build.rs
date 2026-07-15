use std::{env, ffi::OsString, path::PathBuf, process::Command};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    let descriptor = out_dir.join("bazel-mcp-bep-descriptor.bin");
    generate_descriptor(protoc.into_os_string(), &descriptor)?;

    buffa_build::Config::new()
        .files(&["build_event_stream_subset.proto"])
        .descriptor_set(descriptor)
        .generate_views(true)
        .preserve_unknown_fields(false)
        .compile()?;
    println!("cargo:rerun-if-changed=proto/build_event_stream_subset.proto");
    println!("cargo:rerun-if-changed=proto/PROVENANCE.md");
    Ok(())
}

fn generate_descriptor(
    protoc: OsString,
    descriptor: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(protoc)
        .arg("--include_imports")
        .arg("--include_source_info")
        .arg(format!("--descriptor_set_out={}", descriptor.display()))
        .arg("--proto_path=proto")
        .arg("proto/build_event_stream_subset.proto")
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "vendored protoc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}
