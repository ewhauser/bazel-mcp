#include "bazel_mcp_agentic_noisy/normalizer.h"

#include <array>
#include <iostream>
#include <string>

struct Case {
  const char* input;
  const char* expected;
};

int main() {
  constexpr std::array<Case, 4> kCases = {{{
      "  Mixed-Key  ",
      "mixed-key",
  }, {
      "\tALPHA_BETA\n",
      "alpha_beta",
  }, {
      "\r  Release.Candidate\v",
      "release.candidate",
  }, {
      "\fProduction/West \t",
      "production/west",
  }}};
  int failures = 0;
  for (int round = 0; round < 128; ++round) {
    for (std::size_t index = 0; index < kCases.size(); ++index) {
      const std::string actual = NormalizeKey(kCases[index].input);
      if (actual == kCases[index].expected) continue;
      ++failures;
      std::cerr << "matrix_case=" << (round * kCases.size() + index)
                << " shard=" << round << " expected='" << kCases[index].expected
                << "' actual='" << actual
                << "' invariant=trim-leading-and-trailing-ascii-whitespace-then-lowercase\n";
    }
  }
  std::cerr << "normalizer_matrix_failures=" << failures << " total_cases=512\n";
  return failures == 0 ? 0 : 1;
}
