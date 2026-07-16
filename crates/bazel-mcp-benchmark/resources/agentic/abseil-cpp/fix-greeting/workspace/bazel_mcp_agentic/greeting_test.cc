#include "bazel_mcp_agentic/greeting.h"

#include <string>

int main() {
  if (Greeting("Bazel") != std::string("Hello, Bazel!")) return 1;
  if (Greeting("") != std::string("Hello, world!")) return 2;
  return 0;
}
