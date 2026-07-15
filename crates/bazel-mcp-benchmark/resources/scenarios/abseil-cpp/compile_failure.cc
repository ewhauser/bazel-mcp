#include "absl/strings/str_cat.h"

int main() {
  BAZEL_MCP_COMPILE_ROOT_CAUSE intentionally_missing_symbol = absl::StrCat("failure");
  return intentionally_missing_symbol;
}

