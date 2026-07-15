# BEP crate instructions

Keep protobuf fields wire-compatible with the pinned Bazel source documented in
`proto/PROVENANCE.md`. Generated Rust stays in `OUT_DIR`, never in Git. Every
parser change must preserve partial-stream recovery, unknown fields, frame-size
limits, and adversarial/fuzz coverage across supported Bazel majors.
