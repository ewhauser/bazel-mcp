#include <string>

#include "absl/strings/str_cat.h"

int main() {
  return absl::StrCat("bazel", "-mcp") == std::string("bazel-mcp") ? 0 : 1;
}

