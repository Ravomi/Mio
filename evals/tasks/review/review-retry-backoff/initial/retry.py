"""Bounded retry policy for HTTP status codes."""

RETRYABLE_STATUSES = frozenset({408, 429, 502, 503, 504})


def next_backoff_ms(attempt, status_code, max_attempts=3, base_ms=100):
    if status_code not in RETRYABLE_STATUSES:
        return None
    if attempt > max_attempts:
        return None
    return base_ms * (2 ** attempt)
