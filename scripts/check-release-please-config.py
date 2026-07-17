#!/usr/bin/env python3
import json
import pathlib
import tomllib

root = pathlib.Path(__file__).resolve().parents[1]
config = json.loads((root / ".release-please-config.json").read_text())
manifest = json.loads((root / ".release-please-manifest.json").read_text())
cargo = (root / "Cargo.toml").read_text()
cargo_data = tomllib.loads(cargo)
cargo_lock = tomllib.loads((root / "Cargo.lock").read_text())
module = (root / "MODULE.bazel").read_text()
server_build = (root / "crates/bazel-mcp-server/BUILD.bazel").read_text()
package_config = config["packages"]["."]
version = manifest["."]

assert package_config["release-type"] == "bazel"
assert package_config["bump-minor-pre-major"] is True
assert cargo_data["workspace"]["package"]["version"] == version
assert version in module
assert version in server_build

workspace_packages = {
    tomllib.loads(path.read_text())["package"]["name"]
    for path in (root / "crates").glob("*/Cargo.toml")
    if tomllib.loads(path.read_text())["package"].get("version") == {"workspace": True}
}
lock_versions = {
    package["name"]: package["version"]
    for package in cargo_lock["package"]
    if package["name"] in workspace_packages
}
assert lock_versions == dict.fromkeys(workspace_packages, version)

lock_updater = next(
    updater
    for updater in package_config["extra-files"]
    if updater["path"] == "Cargo.lock"
)
for package in workspace_packages:
    assert f'@.name.value=="{package}"' in lock_updater["jsonpath"]
print("release-please configuration is consistent")
