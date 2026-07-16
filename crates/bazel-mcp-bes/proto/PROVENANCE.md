# BES protobuf provenance

`publish_build_event_subset.proto` is a wire-compatible subset of the Google
Build Event Service API used by Bazel 8 and 9. Field numbers and types are
derived from:

- https://github.com/googleapis/googleapis/blob/master/google/devtools/build/v1/publish_build_event.proto
- https://github.com/googleapis/googleapis/blob/master/google/devtools/build/v1/build_events.proto

The upstream files are licensed under Apache-2.0. The subset intentionally
ignores lifecycle contents and fields unused by the local capture service.
Unknown fields are ignored for forward compatibility.
