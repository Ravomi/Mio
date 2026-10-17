"""Domestic shipping fee calculator in cents."""


def shipping_fee_cents(order_cents, weight_grams, is_member=False):
    if order_cents < 0 or weight_grams <= 0:
        raise ValueError("invalid order")
    if is_member or order_cents >= 5000:
        return 0
    if weight_grams <= 1000:
        return 499
    return 499 + ((weight_grams - 1000 + 499) // 500) * 150
