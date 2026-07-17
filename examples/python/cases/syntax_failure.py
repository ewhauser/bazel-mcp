def invoice_total(subtotal):
    options = {
        "currency": "USD",
    return subtotal + options["fee"]

invoice_total(40)
