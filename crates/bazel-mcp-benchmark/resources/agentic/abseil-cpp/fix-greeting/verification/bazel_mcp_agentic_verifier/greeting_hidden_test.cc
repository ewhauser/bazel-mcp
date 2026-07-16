#include "bazel_mcp_agentic/greeting.h"

#include <string>

int main() {
  if (Greeting("  Codex\t") != std::string("Hello, Codex!")) return 1;
  if (Greeting("\r\n \t") != std::string("Hello, world!")) return 2;
  return 0;
}
