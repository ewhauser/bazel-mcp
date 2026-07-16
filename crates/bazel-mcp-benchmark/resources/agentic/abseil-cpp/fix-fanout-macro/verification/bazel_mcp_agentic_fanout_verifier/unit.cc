#include <string>

#include "absl/strings/str_cat.h"

namespace {
const std::string kHiddenFanoutLabel = absl::StrCat("hidden-fanout-unit-", __LINE__);
}  // namespace
