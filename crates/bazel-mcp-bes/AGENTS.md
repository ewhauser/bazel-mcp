# BES crate instructions

Keep the service wire-compatible with Bazel's `google.devtools.build.v1`
PublishBuildEvent API. Decode protocol messages with Buffa, bind only to the
loopback interface, acknowledge an event only after its raw BEP frame has been
written, and preserve strict frame, stream-byte, event-count, invocation-ID,
and sequence-number validation.
