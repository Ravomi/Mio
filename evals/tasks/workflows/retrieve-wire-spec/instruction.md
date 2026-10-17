The `wire` format is specified by `docs/wire-format.md`, a long working draft with
normative rules scattered across the middle and the end of the document, plus a
normative summary in the final section. Read that document and make `wire.py`
match it exactly.

- Implement `parse(text)` and `render(records)` per the normative rules. The rules
  are independent of each other, so satisfy all of them, not just the first one
  you find. The document is the only source of truth: this instruction does not
  restate the rules.
- `acceptance.py` is not part of your workspace and cannot be edited; it checks
  each normative rule with dedicated boundary inputs and requires the unrelated
  files below to stay byte-for-byte identical.
- Do not modify `docs/wire-format.md`, `docs/changelog.md`, `legacy/parse_legacy.py`,
  or `data/sample.vtx`. Only `wire.py` should change.
- Use only the Python standard library.

This task measures single-session retrieval from a long file: locating several
independent constraints inside one document and applying them to code. It is not
a test of conversation context compression or of anything that persists across
turns. The full spec is present on disk in this workspace every time.
