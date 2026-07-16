#ifndef BAZEL_MCP_AGENTIC_GREETING_H_
#define BAZEL_MCP_AGENTIC_GREETING_H_

#include <string>

#include "absl/strings/string_view.h"

std::string Greeting(absl::string_view name);

#endif  // BAZEL_MCP_AGENTIC_GREETING_H_
