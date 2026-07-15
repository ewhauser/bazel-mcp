# BEP protobuf provenance

`build_event_stream_subset.proto` is a wire-compatible subset of Bazel's
`build_event_stream.proto` at Bazel 9.1.0. Field numbers and types are derived
from:

https://github.com/bazelbuild/bazel/blob/9.1.0/src/main/java/com/google/devtools/build/lib/buildeventstream/proto/build_event_stream.proto

Bazel is licensed under Apache-2.0. The subset intentionally omits fields the
MVP does not inspect; protobuf unknown-field semantics preserve forward
compatibility. Update this file and cross-version fixtures whenever the pinned
source changes.

