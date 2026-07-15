# Security policy

Security fixes are supported for the latest release. Please report a
vulnerability privately through GitHub Security Advisories rather than a public
issue.

`bazel-mcp` executes Bazel with the invoking user's permissions. Allowed roots,
command policy, reserved flags, a filtered child environment, response budgets,
and regex redaction reduce risk but do not make untrusted repositories safe.
Use a sandbox or isolated account for untrusted source. Invocation logs and BEP
files may contain source paths and compiler output; they are stored with private
permissions, retained locally, and should be treated as sensitive.
