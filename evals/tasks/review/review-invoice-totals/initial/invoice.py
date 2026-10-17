"""Invoice line calculation with integer cents and non-stacking coupons."""


def compute_invoice(items, tax_bps, coupon=None, existing_discount_cents=0):
    subtotal = 0
    for item in items:
        line_total = round(item["unit_cents"] * item["qty"] * (1 - item.get("discount_rate", 0.0)))
        subtotal += line_total
    discount = existing_discount_cents
    if coupon is not None:
        discount += coupon["off_cents"]
    taxable = max(0, subtotal - discount)
    tax = (taxable * tax_bps) // 10_000
    return {"subtotal": subtotal, "discount": discount, "tax": tax, "total": taxable + tax}
