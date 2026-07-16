#include "bazel_mcp_agentic_noisy/normalizer.h"

#include "absl/strings/ascii.h"

std::string NormalizeKey(absl::string_view input) {
  return absl::AsciiStrToLower(input);
}
