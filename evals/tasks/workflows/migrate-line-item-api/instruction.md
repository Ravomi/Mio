Migrate the invoicing package to the new line-item API, then keep every caller consistent.

Required target API in `invoicing/core.py`:

- `compute_total(items, *, tax_rate_bps=0)` returns the invoice total in integer cents.
- `items` is an iterable of mappings with `unit_cents` and `quantity`. The old rules stand: totals are integer cents, an empty item list is `0`, and a negative `quantity` raises `ValueError`.
- `tax_rate_bps` is keyword-only. It is a tax rate in basis points and must satisfy `0 <= tax_rate_bps <= 10000`; anything else raises `ValueError`. The tax added on top of the subtotal is rounded half-up to the nearest cent (an exact half cent rounds up).
- The deprecated `compute` function must be gone: no attribute named `compute` may remain on `invoicing/core.py`.

Update the callers so the package is internally consistent: `invoicing/report.py` must keep exporting `DEFAULT_TAX_RATE_BPS = 800` and `monthly_total(items)` must now delegate to the migrated API with that default rate. `app.py` must keep printing the monthly total as a single integer line for a JSON list of items.

Use only the Python standard library. Do not change the module names or the public names `monthly_total` and `DEFAULT_TAX_RATE_BPS`.
