#include "gtest/gtest.h"

int CalculateInvoiceTotal(int subtotal, int service_fee) {
  return subtotal + service_fee;
}

TEST(InvoiceTest, IncludesServiceFee) {
  EXPECT_EQ(CalculateInvoiceTotal(40, 2), 41)
      << "invoice total should include the service fee";
}
