#include "bazel_mcp_agentic_noisy/normalizer.h"

#include <string>

int main() {
  if (NormalizeKey("\v  Canary-42\r\n") != std::string("canary-42")) return 1;
  if (NormalizeKey("already-normal") != std::string("already-normal")) return 2;
  if (NormalizeKey("\t \r\n") != std::string()) return 3;
  return 0;
}
