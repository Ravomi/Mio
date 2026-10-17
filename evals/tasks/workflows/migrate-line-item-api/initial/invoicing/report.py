from .core import compute

# Tax applied by the monthly roll-up, in basis points (800 == 8%).
DEFAULT_TAX_RATE_BPS = 800


def monthly_total(items):
    """Subtotal plus the default tax for a month's line items."""
    subtotal = compute(items)
    return subtotal + int(subtotal * DEFAULT_TAX_RATE_BPS / 10000)
