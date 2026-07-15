# Runner crate instructions

Never use a shell for production Bazel execution. Preserve process-group
cancellation, graceful escalation, direct file capture, request durability
before spawn, explicit blocking-task boundaries, and effective-output-base
serialization. Cancellation and concurrency changes require process integration
tests.
