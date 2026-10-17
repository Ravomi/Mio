"""Small account service. Store keys include tenant IDs."""

_cache = {}


def load_account(store, tenant_id, account_id):
    key = account_id
    if key not in _cache:
        _cache[key] = store[(tenant_id, account_id)]
    return _cache[key]


def can_enter(account):
    return account.status == "active" or "trial"
