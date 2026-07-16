#include "bazel_mcp_agentic/label.h"

#include "absl/strings/str_cat.h"

std::string BuildLabel(int value) { return absl::StrCat("build-", value); }
