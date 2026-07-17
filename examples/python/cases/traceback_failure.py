def validate_currency(currency):
    if currency != "EUR":
        raise ValueError(f"invoice currency {currency} is unsupported")

validate_currency("USD")
