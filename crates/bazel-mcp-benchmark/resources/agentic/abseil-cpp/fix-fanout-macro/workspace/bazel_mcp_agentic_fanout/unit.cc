#include <string>

#include "absl/strings/str_cat.h"

namespace {
const std::string kFanoutLabel = absl::StrCat("fanout-unit-", __LINE__);
}  // namespace
