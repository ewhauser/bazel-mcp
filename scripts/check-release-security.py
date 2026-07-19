#!/usr/bin/env python3
import pathlib
import re
import sys

root = pathlib.Path(__file__).resolve().parents[1]
errors = []
for workflow in (root / ".github/workflows").glob("*.yml"):
    source = workflow.read_text()
    if not re.search(r"(?m)^permissions: \{\}$", source):
        errors.append(f"{workflow.name}: workflow permissions must default to none")
    if "timeout-minutes:" not in source:
        errors.append(f"{workflow.name}: missing job timeout")
    for dangerous_trigger in ("pull_request_target", "workflow_run"):
        if re.search(rf"(?m)^\s*{dangerous_trigger}\s*:", source):
            errors.append(f"{workflow.name}: forbidden trigger: {dangerous_trigger}")
    for line in source.splitlines():
        if "uses:" in line:
            reference = line.split("uses:", 1)[1].split("#", 1)[0].strip()
            if not reference.startswith("./") and not re.search(
                r"@[0-9a-f]{40}$", reference
            ):
                errors.append(f"{workflow.name}: action is not pinned: {reference}")
    if "actions/checkout" in source and "persist-credentials: false" not in source:
        errors.append(f"{workflow.name}: checkout credentials are not disabled")

release_source = (root / ".github/workflows/release.yml").read_text()
if re.search(r"(?m)^\s+uses: actions/cache@|^\s+cache:\s*", release_source):
    errors.append("release.yml: release jobs must not restore mutable caches")
if "actions/attest@" not in release_source:
    errors.append("release.yml: release artifacts must receive build provenance")
if "install-cargo-dist.sh" not in release_source or "install-cargo-dist.ps1" not in release_source:
    errors.append("release.yml: cargo-dist must use checksum-verifying installers")

dependabot_source = (root / ".github/dependabot.yml").read_text()
if "cooldown:" not in dependabot_source or "default-days: 7" not in dependabot_source:
    errors.append("dependabot.yml: dependency updates must use a seven-day cooldown")
if errors:
    print("\n".join(errors), file=sys.stderr)
    raise SystemExit(1)
print("release workflow pinning and permissions passed")
