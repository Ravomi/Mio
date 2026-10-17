"""Independent acceptance checks for the retrieve-wire-spec task.

Executed with the task workspace first on sys.path and the workspace as cwd. It
checks every normative rule from docs/wire-format.md with dedicated boundary
inputs, and verifies that the unrelated workspace files were left untouched.
"""
import hashlib
from pathlib import Path

from wire import parse, render

# Unrelated files must stay byte-for-byte identical. Line endings are normalised
# so that a checkout on another platform is not penalised.
UNCHANGED = {
    "docs/wire-format.md": "b067ecc8cdb1399c5bd27ff71a28e5a5bd889757ab6972f7fe22dfc6aebe213e",
    "docs/changelog.md": "4124bbeb3b8c3ae160e66a411f94da927e214d07f7414671e67a355a815a9bf6",
    "legacy/parse_legacy.py": "7edef3458abff89526c4cec71d79a40cb8e4cbbd47cbcce4b0a485a779b986de",
    "data/sample.vtx": "8b6200b0a03cab1c8176f9faba0674857f09d57a13cf32a305314376448b5e09",
}
for relative, expected in sorted(UNCHANGED.items()):
    digest = hashlib.sha256(Path(relative).read_bytes().replace(b"\r\n", b"\n")).hexdigest()
    assert digest == expected, "unrelated file was modified: " + relative

# RULE-1: blank and whitespace-only lines are skipped, including the trailing one.
assert parse("") == []
assert parse("\n") == []
assert parse("   \n\t\n") == []
assert parse("a=1\n") == [{"a": "1"}]
assert parse("a=1\n\n\n") == [{"a": "1"}]
assert parse("a=1\nb=2\n") == [{"a": "1"}, {"b": "2"}]

# RULE-2: keys are stored lower-cased; duplicates are rejected after folding.
assert parse("A=1|B=2\n") == [{"a": "1", "b": "2"}]
assert parse("Key=1\n") == [{"key": "1"}]

# RULE-3: backslash escapes for pipe and backslash, interpreted before splitting.
assert parse("v=a\\|b\n") == [{"v": "a|b"}]
assert parse("v=a\\\\b\n") == [{"v": "a\\b"}]
assert parse("v=a\\|b\\\\c\n") == [{"v": "a|b\\c"}]
assert parse("v=x\\|y|w=2\n") == [{"v": "x|y", "w": "2"}]
assert render([{"a": "1", "b": "x|y\\z"}]) == "a=1|b=x\\|y\\\\z\n"

# RULE-4: dangling and unknown escapes are malformed.
# RULE-5: pairs without "=" and empty keys are malformed; only the first "=" splits.
assert parse("v=a=b\n") == [{"v": "a=b"}]

# RULE-6: "seq" must be exactly four decimal digits, leading zeros preserved.
assert parse("seq=0042|c=9\n") == [{"seq": "0042", "c": "9"}]
assert parse("seq=0000\n") == [{"seq": "0000"}]

# RULE-7: canonical rendering and round-trips.
assert parse("Key=\n") == [{"key": ""}]
assert render([]) == ""
assert render([{"a": "1"}]) == "a=1\n"
assert render(parse("A=1|B=2\n")) == "a=1|b=2\n"
assert parse(render(parse("seq=0042|note=left\\|right\n"))) == [
    {"seq": "0042", "note": "left|right"}
]

for bad in [
    "a=1|a=2\n",          # RULE-2: duplicate key after lower-casing
    "a=1|A=2\n",          # RULE-2: duplicate key with different case
    "a=1|noequals\n",     # RULE-5: pair without "="
    "=1\n",               # RULE-5: empty key
    "v=trailing\\\n",     # RULE-4: dangling escape
    "v=bad\\q\n",         # RULE-4: unknown escape
    "seq=12\n",           # RULE-6: too short
    "seq=12345\n",        # RULE-6: too long
    "seq=12a4\n",         # RULE-6: non-digit
    "seq=----\n",         # RULE-6: non-digit
]:
    try:
        parse(bad)
    except ValueError:
        pass
    else:
        raise AssertionError("accepted malformed input: " + bad)
