#include "bazel_mcp_agentic/greeting.h"

#include "absl/strings/str_cat.h"

std::string Greeting(absl::string_view name) {
  return absl::StrCat("Hello, ", name.empty() ? "world" : name, "!");
}
