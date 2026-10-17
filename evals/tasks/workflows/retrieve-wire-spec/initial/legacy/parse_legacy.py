"""Legacy reader kept for compatibility with v0 exports.

This module is frozen. New code must use `wire.parse` instead, but the legacy
splitting behaviour is still relied on by downstream archives, so it must not be
deleted, renamed, or rewritten.
"""


def read_v1(text):
    """Return the non-empty physical lines of a v0 document."""
    return [line for line in text.splitlines() if line]
