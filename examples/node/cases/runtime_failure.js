function invoiceTotal(invoice) {
  return invoice.lines.reduce((total, line) => total + line.amount, 0);
}

invoiceTotal(undefined);
