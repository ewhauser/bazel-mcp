#include <stdexcept>

#include "gtest/gtest.h"

TEST(InvoiceTest, RejectsMissingCurrency) {
  throw std::runtime_error("invoice currency was not configured");
}
