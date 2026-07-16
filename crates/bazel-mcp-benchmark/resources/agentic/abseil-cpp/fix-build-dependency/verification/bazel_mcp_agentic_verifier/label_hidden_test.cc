#include "bazel_mcp_agentic/label.h"

#include <string>

int main() {
  return BuildLabel(-3) == std::string("build--3") ? 0 : 1;
}
