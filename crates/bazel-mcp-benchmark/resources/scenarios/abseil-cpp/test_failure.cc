#include <iostream>

#include "absl/strings/str_cat.h"

int main() {
  for (int i = 0; i < 2000; ++i) {
    std::cerr << "warning: duplicated test setup warning\n";
  }
  std::cerr << absl::StrCat("BAZEL_MCP_TEST_ROOT_CAUSE: expected 3, received 4") << "\n";
  return 1;
}
