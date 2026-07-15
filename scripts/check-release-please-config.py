#!/usr/bin/env python3
import json
import pathlib

root = pathlib.Path(__file__).resolve().parents[1]
config = json.loads((root / ".release-please-config.json").read_text())
manifest = json.loads((root / ".release-please-manifest.json").read_text())
cargo = (root / "Cargo.toml").read_text()
assert config["packages"]["."]["release-type"] == "rust"
assert manifest["."] in cargo
print("release-please configuration is consistent")
