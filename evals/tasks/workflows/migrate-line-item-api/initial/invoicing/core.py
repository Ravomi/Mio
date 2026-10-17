"""Line-item totals for customer invoices."""


def compute(items):
    """Return the line-item subtotal in integer cents.

    Rejects negative quantities so refunds cannot slip into a total.
    """
    subtotal = 0
    for item in items:
        quantity = item["quantity"]
        if quantity < 0:
            raise ValueError("quantity must be non-negative")
        subtotal += item["unit_cents"] * quantity
    return subtotal
