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

## Supply-chain verification

Release workflows use commit-pinned GitHub Actions, least-privilege job tokens,
checksum-verified tool bootstrapping, cache-free release builds, and OIDC-backed
Sigstore attestations. Release archives include SHA-256 checksums and a locked
CycloneDX SBOM.

After downloading a release asset, verify its provenance and integrity with:

```console
gh attestation verify <asset> --repo ewhauser/bazel-mcp
shasum -a 256 --check <checksums-file>
```
