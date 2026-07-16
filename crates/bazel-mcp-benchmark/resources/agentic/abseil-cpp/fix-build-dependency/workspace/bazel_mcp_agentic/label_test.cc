#include "bazel_mcp_agentic/label.h"

#include <string>

int main() {
  return BuildLabel(17) == std::string("build-17") ? 0 : 1;
}
