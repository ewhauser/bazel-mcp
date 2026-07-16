"""Example custom compiler diagnostic reducer for bazel-mcp."""

API_VERSION = 1
NAME = "custom-compiler"
PRIORITY = 100
MODE = "augment"
COMMANDS = ["build", "test"]
TARGET_LABELS = ["//custom/..."]
TARGET_KINDS = ["custom_library rule", "custom_binary rule"]
ACTION_TYPES = ["CustomCompile"]

def reduce(ctx):
    diagnostics = regex_diagnostics(
        ctx["stderr"],
        r"(?m)^(?P<path>[^:\n]+):(?P<line>[0-9]+):(?P<column>[0-9]+): error: (?P<message>.+)$",
        category = "compilation",
        max_matches = 100,
    )
    if not diagnostics:
        return None
    return patch(
        diagnostics,
        headline = "Custom compiler failed",
    )
