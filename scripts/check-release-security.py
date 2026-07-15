#!/usr/bin/env python3
import pathlib
import re
import sys

root = pathlib.Path(__file__).resolve().parents[1]
errors = []
for workflow in (root / ".github/workflows").glob("*.yml"):
    source = workflow.read_text()
    if "permissions:" not in source or "timeout-minutes:" not in source:
        errors.append(f"{workflow.name}: missing permissions or timeout")
    for line in source.splitlines():
        if "uses:" in line:
            reference = line.split("uses:", 1)[1].split("#", 1)[0].strip()
            if not re.search(r"@[0-9a-f]{40}$", reference):
                errors.append(f"{workflow.name}: action is not pinned: {reference}")
    if "actions/checkout" in source and "persist-credentials: false" not in source:
        errors.append(f"{workflow.name}: checkout credentials are not disabled")
if errors:
    print("\n".join(errors), file=sys.stderr)
    raise SystemExit(1)
print("release workflow pinning and permissions passed")
