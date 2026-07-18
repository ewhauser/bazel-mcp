#!/usr/bin/env python3
"""Enforce the dependency and semantic boundary of the reusable reducer crates."""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CRATES = ROOT / "crates"

EXPECTED_DEPENDENCIES = {
    "diagnostic-reducer-core": {"serde", "serde_json"},
    "diagnostic-reducer": {
        "diagnostic-reducer-core",
        "serde_json",
    },
    "diagnostic-reducer-cli": {
        "clap",
        "diagnostic-reducer",
        "serde_json",
    },
}

# These are provider concepts that belong in bazel-mcp-reducer. Matching is
# intentionally case-insensitive and limited to Rust source, so migration docs
# can explain the boundary without weakening the executable check.
FORBIDDEN_SOURCE_TERMS = (
    "bazel",
    "execroot",
    "runfiles",
    "starlark",
    "aspect",
    "strict dependencies",
    "no such package",
    "no such target",
)


def load_manifest(crate: str) -> dict[str, object]:
    path = CRATES / crate / "Cargo.toml"
    with path.open("rb") as handle:
        return tomllib.load(handle)


def dependency_errors() -> list[str]:
    errors: list[str] = []
    for crate, expected in EXPECTED_DEPENDENCIES.items():
        manifest = load_manifest(crate)
        actual = set(manifest.get("dependencies", {}))
        if actual != expected:
            errors.append(
                f"{crate}: runtime dependencies {sorted(actual)!r}; "
                f"expected {sorted(expected)!r}"
            )

        package = manifest.get("package", {})
        if not isinstance(package, dict) or not package.get("version"):
            errors.append(f"{crate}: package.version must be explicit")
        if package.get("publish") is False:
            errors.append(f"{crate}: package must remain publishable")
    return errors


def semantic_errors() -> list[str]:
    errors: list[str] = []
    for crate in EXPECTED_DEPENDENCIES:
        source_root = CRATES / crate / "src"
        for path in sorted(source_root.rglob("*.rs")):
            for line_number, line in enumerate(
                path.read_text(encoding="utf-8").splitlines(), start=1
            ):
                lowered = line.lower()
                for term in FORBIDDEN_SOURCE_TERMS:
                    if term in lowered:
                        relative = path.relative_to(ROOT)
                        errors.append(f"{relative}:{line_number}: forbidden term {term!r}")
    return errors


def main() -> int:
    errors = dependency_errors() + semantic_errors()
    if errors:
        print("diagnostic reducer extraction boundary failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1
    print("diagnostic reducer extraction boundary is intact")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
