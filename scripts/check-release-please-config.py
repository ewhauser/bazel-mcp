#!/usr/bin/env python3
import json
import pathlib

root = pathlib.Path(__file__).resolve().parents[1]
config = json.loads((root / ".release-please-config.json").read_text())
manifest = json.loads((root / ".release-please-manifest.json").read_text())
cargo = (root / "Cargo.toml").read_text()
module = (root / "MODULE.bazel").read_text()
server_build = (root / "crates/bazel-mcp-server/BUILD.bazel").read_text()
assert config["packages"]["."]["release-type"] == "bazel"
assert manifest["."] in cargo
assert manifest["."] in module
assert manifest["."] in server_build
print("release-please configuration is consistent")
