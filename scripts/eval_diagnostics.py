#!/usr/bin/env python3
r"""Bounded, sanitized diagnostics for one evaluation task log.

Status: library only. ``scripts/agent_eval.py`` loads this module for the
opt-in ``--diagnostics-dir`` path and passes the per-task workspace root plus
the offline constraint allowlist. Importing this module has no side effects:
nothing auto-enables, and raw log lines are never persisted or copied into the
returned evidence.

What one scan retains (and nothing else):

* model completion-call counts and fixed finish-reason buckets;
* the real model-dispatch count and the pre-admission prepared count derived
  from ``run_observation`` events whose ``kind`` is ``model_dispatched`` or
  ``model_prepared``, plus the legacy request-side count that stays ``None``
  unless a structured ``model_request`` event exists (the engine emits no such
  event today). ``model_dispatched`` is the engine's own post-admission
  dispatch boundary: it is not a wire/HTTP request, not a provider
  acknowledgement and not a billing record, and a prepared observation that
  admission denied is counted as prepared, never as dispatched;
* workspace-relative ``read`` targets with an explicitly reported line or byte
  range, how often the exact same target and range were read again, and which
  fixed family supplied each entry: a structured ``read_observation`` that the
  engine emits only after a read actually succeeded, or a legacy
  ``tool_started`` attempt that carries no range at all;
* fixed-vocabulary builtin tool activity, counted only from the real CLI
  payloads: ``tool_started`` (request phase) and the ``Ok``/``Err``
  discriminant of ``tool_finished`` (result/error phases) carry the tool
  operation in ``data.operation``, and ``run_observation`` with
  ``kind=tool_dispatched`` is the unnamed post-admission dispatch total.
  Names come only from ``data.operation``; tool arguments, commands, patterns,
  targets, results, model text, call ids and paths are never read. A label
  outside the fixed vocabulary folds into ``other`` and the label itself is
  dropped, and a phase whose event family never appeared stays ``null``
  (unknown) instead of a fabricated zero;
* compaction prefix source-range counts (``source_messages`` from the
  half-open ``source_start``/``source_end`` prefix, plus ``source_bytes`` and
  ``summary_bytes``) kept explicitly distinct from the full-history
  before/after counters ``before_messages``/``after_messages``/``before_bytes``/
  ``after_bytes``, dropped-content counts and fixed compaction kinds;
* compaction constraint ids (which alone feed caller allowlist coverage) and
  preserved goal ids, which are counted as a bounded number and never treated
  as constraint coverage;
* coverage of the caller-supplied offline constraint ids, which stays
  ``unknown`` unless a structured event explicitly names a constraint id.

What it never retains: host absolute paths, paths outside the workspace, parent
traversal, symlink escapes, tool results or model text, argv or commands, raw
diagnostic messages, free-form descriptions, arbitrary identifiers (run ids and
effect ids are used only as transient dedup keys) and credentials.

Read ranges are never inferred. A structured ``read_observation`` may supply an
explicit ``offset``/``limit`` line range or ``byteOffset``/``byteLimit`` byte
range (the accepted spellings, and the only ones); a zero-length or empty
return is never expanded into a fabricated one-line range. A legacy
``tool_started`` record carries only ``operation`` and ``target``, so a read
seen through it always records ``range.kind == "unknown"`` with null bounds.
One read must never be counted twice: whenever the stream contains any
structured ``read_observation``, the whole stream is accounted from those
successful returns and every legacy ``tool_started`` read candidate is
suppressed regardless of event order (a structured observation that appears
early suppresses a legacy candidate that appears late, and the other way
round). A stream with no structured observation degrades to the legacy attempts
and marks their ranges ``unknown``. Numbers embedded in a path are never parsed
as a range, and no keyword matching is used to guess constraints.

Compaction reports are read in both spellings: the real
``CompactionLossReport`` nested under ``report`` uses ``snake_case`` fields,
while older test envelopes used ``camelCase``. A logical field is accepted from
whichever spelling is present; when both spellings are present with equivalent
values the field is accepted once, and when the two spellings contradict each
other the field is recorded as malformed/unknown and never guessed. The
original ``source_start``/``source_end``/``source_bytes``/``summary_bytes``
fields keep their replaced-prefix semantics; the newer
``before_messages``/``after_messages``/``before_bytes``/``after_bytes`` fields
describe the full history before and after compaction and are never derived
from the prefix range, so they stay ``null`` when the observation does not
report them.

Every bound is explicit in the ``limits`` block and every overflow is explicit
in the ``truncation`` block: event count, stored field length, serialized output
bytes, source bytes read and single-line bytes.

``collect`` returns the evidence dictionary; ``serialize`` renders it
canonically; ``write`` stores it with ``O_EXCL`` so an existing per-task
evidence file is never overwritten. See ``collect_and_write`` for both steps.
"""

from __future__ import annotations

import json
import os
import re

__all__ = [
    "DEFAULT_ENABLED",
    "DEFAULT_LIMITS",
    "EVIDENCE_KIND",
    "EVIDENCE_SCHEMA_VERSION",
    "EvidenceAlreadyExists",
    "Limits",
    "REQUIRES_INSTRUMENTATION",
    "collect",
    "collect_and_write",
    "serialize",
    "write",
]

# Diagnostics stay opt-in; the runner must ask for them explicitly.
DEFAULT_ENABLED = False

EVIDENCE_SCHEMA_VERSION = 1
EVIDENCE_KIND = "mio-eval-diagnostics"

# Required floors. The defaults meet or exceed every one of them.
MIN_EVENTS = 256
MIN_FIELD_CHARS = 256
MIN_OUTPUT_BYTES = 64 * 1024
MIN_SOURCE_BYTES = 16 * 1024 * 1024
MIN_LINE_BYTES = 1024 * 1024

DEFAULT_MAX_EVENTS = 2048
DEFAULT_MAX_FIELD_CHARS = MIN_FIELD_CHARS
DEFAULT_MAX_OUTPUT_BYTES = MIN_OUTPUT_BYTES
DEFAULT_MAX_SOURCE_BYTES = MIN_SOURCE_BYTES
DEFAULT_MAX_LINE_BYTES = MIN_LINE_BYTES

# Upper bound on caller-supplied constraint ids and on the ids harvested from a
# single structured evidence list, independent of the byte budget.
MAX_CONSTRAINTS = 256

READ_OPERATION = "read"
READ_OBSERVATION_TYPES = ("read_observation",)

# Fixed read-accounting families. ``read_observation`` is the structured record
# the engine emits only after a workspace read actually succeeded; the matching
# ``tool_started`` record is a legacy attempt with no range at all. The two
# family names, the two source labels and the two semantics labels are a closed
# vocabulary: the collector never emits a third spelling for either field.
READ_FAMILY_OBSERVATION = "read_observation"
READ_FAMILY_TOOL_STARTED = "tool_started"
READ_SOURCES = {
    READ_FAMILY_OBSERVATION: "read_observation",
    READ_FAMILY_TOOL_STARTED: "legacy_tool_started",
}
READ_SEMANTICS = {
    READ_FAMILY_OBSERVATION: "successful_read_return",
    READ_FAMILY_TOOL_STARTED: "legacy_read_attempt",
}
READ_SOURCE_UNKNOWN = "unknown"
READ_SEMANTICS_UNKNOWN = "unknown"
RUN_OBSERVATION_TYPE = "run_observation"
# The only run_observation kinds that describe a model request attempt. A
# `model_prepared` observation is the pre-admission correlation record and is
# never a dispatch; `model_dispatched` is the engine's post-admission dispatch
# boundary, not a wire request or a billing event.
MODEL_OBSERVATION_KINDS = ("model_dispatched", "model_prepared")
LINE_RANGE_KINDS = ("line", "byte")

# Fixed builtin tool-activity vocabulary. The live `--json` schema reports the
# tool operation in `data.operation` on `tool_started` / `tool_finished`; that is
# the only tool-name field this scan reads. `workspace` is the container tool's
# own canonical name and the current CLI never emits it as an operation label,
# so its bucket stays zero unless an event really reports it. Every other label
# (plugins, MCP, subagents, future tools, a secret-bearing string) folds into
# `other`, and the original label is never stored.
TOOL_ACTIVITY_NAMES = (
    "read",
    "write",
    "edit",
    "grep",
    "find",
    "ls",
    "shell",
    "bash",
    "workspace",
)
TOOL_ACTIVITY_OTHER = "other"
TOOL_ACTIVITY_BUCKETS = TOOL_ACTIVITY_NAMES + (TOOL_ACTIVITY_OTHER,)
# The run_observation kind that is the real post-admission tool dispatch. The
# observation carries no tool name in this schema, so it can only be a stream
# total and never a named count.
TOOL_DISPATCH_KIND = "tool_dispatched"

MIO_WIRE_FINISH_REASONS = ("stop", "length", "tool_calls", "content_filter")
FINISH_REASON_BUCKETS = MIO_WIRE_FINISH_REASONS + ("other", "unreported", "unrecognized")

COMPACTION_KINDS = ("request", "session", "unknown")

# Fixed, non-free-form rejection categories. The rejected path itself is never kept.
REJECTION_REASONS = (
    "not_a_string",
    "empty",
    "too_long",
    "control_chars",
    "absolute",
    "parent_traversal",
    "outside_workspace",
    "symlink_escape",
    "invalid",
)

CONSTRAINT_STATUSES = ("preserved", "dropped")

# A constraint id must look like an identifier, never like free text or a secret blob.
CONSTRAINT_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.:-]*$")

# Fixed, leak-free statement of what is still missing from the event stream.
REQUIRES_INSTRUMENTATION = (
    "model_dispatched: the real post-admission dispatch and stays null unless "
    "that observation is present; never a wire request or a billing record",
    "model_request: the legacy request event the engine never emits; its "
    "count stays null and is never summed with dispatched/prepared",
    "read_observation: consumed with explicit ranges and preferred "
    "stream-wide; the production CLI envelope still needs acceptance, else "
    "legacy tool_started ranges stay unknown",
    "constraint_observation: coverage needs explicitly named, allowlisted "
    "constraint ids from constraint_observation or compaction report lists; "
    "unknown until emitted",
    "context_compaction history: before/after_messages and before/after_bytes "
    "are full-history counts from explicit fields only, not tokens or a "
    "semantic loss; source_* keep replaced-prefix semantics",
    "context_compaction ids: preserved/dropped_constraint_ids feed coverage; "
    "preserved_goal_ids is counted only and never mixed into constraints",
)

_MODEL_DISPATCH_SEMANTICS = (
    "dispatched counts run_observation events with kind=model_dispatched: the "
    "engine handed a completion effect to its dispatch bus after admission. It "
    "is not a wire/HTTP request, not a provider acknowledgement and not a "
    "billing record. prepared counts kind=model_prepared observations, which "
    "are pre-admission only: a prepared attempt that admission denied is never "
    "counted as dispatched."
)

_MODEL_REQUEST_SEMANTICS = (
    "requests counts the legacy model_request event family. The engine emits "
    "no such event today; requests, prepared and dispatched are separate "
    "families and are never summed."
)

_REDACTION_NOTE = (
    "Only workspace-relative paths, explicitly reported ranges, counts, fixed "
    "categories, fixed builtin tool-operation buckets and caller-allowlisted "
    "constraint ids are retained. Tool "
    "results, model text, argv/commands, raw diagnostic messages, arbitrary "
    "identifiers (run ids and effect ids are used only as transient dedup "
    "keys) and credentials are dropped, not summarized."
)

_TOOL_ACTIVITY_NOTE = (
    "Observed CLI events, not executions, unique calls or billing. request = "
    "tool_started; result/error = tool_finished Ok/Err discriminant only; "
    "dispatch = run_observation kind=tool_dispatched after admission. Names "
    "are read only from data.operation; args, targets, results, commands, "
    "patterns, call ids and paths are never stored. Unknown/extension labels "
    "fold into other and the label is dropped. A phase whose event family was "
    "never seen is null (unknown), never a fabricated zero."
)

# Distinguishes non-JSON input from a parsed value; also the contradiction tag.
_NOT_JSON = object()
# Distinguishes an absent field from two contradicting explicit spellings.
_CONFLICT = object()


def _canonical(value):
    """A deterministic, order-insensitive key used only to compare two spellings.

    Scalars keep their JSON type (so ``1`` and ``true`` differ), and lists are
    compared as multisets because constraint-id order carries no meaning. The
    value itself is never stored: only the comparison is kept transiently.
    """
    try:
        if isinstance(value, list):
            return json.dumps(sorted(_canonical(item) for item in value), ensure_ascii=False)
        return json.dumps(value, ensure_ascii=False, sort_keys=True, default=repr)
    except (TypeError, ValueError):
        return repr(value)


def _dual_value(data, snake, camel):
    """Resolve one logical field that may appear in either of two spellings.

    Returns ``(value, conflict)``. A lone spelling wins; both spellings with
    equivalent values are accepted once; two contradicting spellings return
    ``(_CONFLICT, True)`` so the caller records the record as malformed instead
    of choosing one of the two values. An absent field returns ``(None, False)``.
    """
    has_snake = snake in data
    has_camel = camel in data
    if has_snake and has_camel:
        if _canonical(data[snake]) == _canonical(data[camel]):
            return data[snake], False
        return _CONFLICT, True
    if has_snake:
        return data[snake], False
    if has_camel:
        return data[camel], False
    return None, False


class EvidenceAlreadyExists(FileExistsError):
    """Raised instead of overwriting an existing per-task evidence file."""


def _positive_int(name, value):
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ValueError(f"{name} must be a positive integer")
    return value


class Limits:
    """Explicit scan and output bounds. Defaults satisfy the required floors."""

    __slots__ = (
        "max_events",
        "max_field_chars",
        "max_output_bytes",
        "max_source_bytes",
        "max_line_bytes",
    )

    def __init__(
        self,
        max_events=DEFAULT_MAX_EVENTS,
        max_field_chars=DEFAULT_MAX_FIELD_CHARS,
        max_output_bytes=DEFAULT_MAX_OUTPUT_BYTES,
        max_source_bytes=DEFAULT_MAX_SOURCE_BYTES,
        max_line_bytes=DEFAULT_MAX_LINE_BYTES,
    ):
        self.max_events = _positive_int("max_events", max_events)
        self.max_field_chars = _positive_int("max_field_chars", max_field_chars)
        self.max_output_bytes = _positive_int("max_output_bytes", max_output_bytes)
        self.max_source_bytes = _positive_int("max_source_bytes", max_source_bytes)
        self.max_line_bytes = _positive_int("max_line_bytes", max_line_bytes)

    def as_dict(self):
        return {
            "max_events": self.max_events,
            "max_field_chars": self.max_field_chars,
            "max_output_bytes": self.max_output_bytes,
            "max_source_bytes": self.max_source_bytes,
            "max_line_bytes": self.max_line_bytes,
        }


DEFAULT_LIMITS = Limits()


def _resolve_limits(limits):
    return DEFAULT_LIMITS if limits is None else limits


def _explicit_int(value, minimum):
    """Return an explicitly reported integer, or None. Never coerces or clamps."""
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        return None
    return value


def _decode_line(raw):
    """Decode one bounded log line as JSON; return the sentinel when it is not JSON."""
    try:
        return json.loads(raw)
    except (ValueError, UnicodeDecodeError, RecursionError):
        return _NOT_JSON


def _os_error_category(error):
    if isinstance(error, FileNotFoundError):
        return "missing"
    if isinstance(error, PermissionError):
        return "permission"
    return "io"


def _within(root, candidate):
    try:
        return os.path.commonpath([root, candidate]) == root
    except ValueError:
        return False


def _sanitize_path(raw, root, limits):
    """Return ``(relative_posix_path, None)`` or ``(None, rejection_reason)``.

    Rejects host absolute paths, parent traversal, control characters, paths
    over the field limit and anything whose resolved location leaves the
    workspace (including a symlink that points outside it). The offending raw
    value is never returned or stored, only a fixed reason category.
    """
    if not isinstance(raw, str):
        return None, "not_a_string"
    if not raw:
        return None, "empty"
    if len(raw) > limits.max_field_chars:
        return None, "too_long"
    for character in raw:
        code = ord(character)
        if code < 0x20 or code == 0x7F:
            return None, "control_chars"
    if (
        raw.startswith("/")
        or raw.startswith("\\")
        or raw.startswith("~")
        or re.match(r"^[A-Za-z]:", raw)
    ):
        return None, "absolute"
    parts = []
    for part in re.split(r"[\\/]+", raw):
        if part in ("", "."):
            continue
        if part == "..":
            return None, "parent_traversal"
        parts.append(part)
    if not parts:
        return None, "invalid"
    relative = "/".join(parts)
    if len(relative) > limits.max_field_chars:
        return None, "too_long"
    lexical = os.path.normpath(os.path.join(root, *parts))
    resolved = os.path.realpath(lexical)
    if not _within(root, resolved):
        return None, "symlink_escape" if resolved != lexical else "outside_workspace"
    return relative, None


def _extract_range(data):
    """Return ``(kind, start, end, length, ambiguous)`` from explicit fields only.

    ``start``/``end`` are inclusive and both are ``None`` when the event carries
    no complete, explicit range. A line range uses ``offset`` (1-based start)
    with ``limit`` (line count); a byte range uses ``byteOffset``/``byte_offset``
    with ``byteLimit``/``byte_limit``. Contradictory explicit ranges are reported
    as ``ambiguous`` rather than silently preferring one.
    """
    line = _line_range(data)
    byte = _byte_range(data)
    if line is not None and byte is not None:
        return "ambiguous", None, None, None, True
    if line is not None:
        return "line", line[0], line[1], line[2], False
    if byte is not None:
        return "byte", byte[0], byte[1], byte[2], False
    return "unknown", None, None, None, False


def _line_range(data):
    offset = _explicit_int(data.get("offset"), 1)
    limit = _explicit_int(data.get("limit"), 1)
    if offset is None or limit is None:
        return None
    return offset, offset + limit - 1, limit


def _byte_range(data):
    offset = data.get("byteOffset")
    if offset is None:
        offset = data.get("byte_offset")
    limit = data.get("byteLimit")
    if limit is None:
        limit = data.get("byte_limit")
    offset = _explicit_int(offset, 0)
    limit = _explicit_int(limit, 1)
    if offset is None or limit is None:
        return None
    return offset, offset + limit - 1, limit


def _finish_reason_bucket(value):
    if value is None:
        return "unreported"
    if isinstance(value, str) and value in MIO_WIRE_FINISH_REASONS:
        return value
    if isinstance(value, dict) and set(value) == {"other"} and isinstance(value["other"], str):
        return "other"
    return "unrecognized"


def _normalize_constraint_ids(values, limits):
    """Validate the caller allowlist; return ``(ids, truncated, fields_truncated)``."""
    ids = []
    seen = set()
    truncated = False
    fields_truncated = 0
    for value in values:
        if not isinstance(value, str):
            fields_truncated += 1
            continue
        candidate = value.strip()
        if (
            not candidate
            or len(candidate) > limits.max_field_chars
            or not CONSTRAINT_ID_RE.match(candidate)
        ):
            fields_truncated += 1
            continue
        if candidate in seen:
            continue
        if len(ids) >= MAX_CONSTRAINTS:
            truncated = True
            break
        seen.add(candidate)
        ids.append(candidate)
    return sorted(ids), truncated, fields_truncated


class _ReadFamily:
    """One bounded read-accounting family (structured observations or legacy attempts).

    ``seen`` counts every recognized read event of the family, including ones
    whose path was rejected, so the stream can still tell that the family was
    live. ``total`` counts only stored entries. Entries are keyed by
    ``(path, range.kind, start, end)`` so exactly repeated ranges become repeats
    instead of new entries.
    """

    __slots__ = ("seen", "total", "entries", "order", "rejected")

    def __init__(self):
        self.seen = 0
        self.total = 0
        self.entries = {}
        self.order = []
        self.rejected = {reason: 0 for reason in REJECTION_REASONS}


class _Scanner:
    """One bounded pass over one JSONL log; holds only sanitized aggregates."""

    def __init__(self, workspace_root, allowed_constraints, limits):
        self.limits = limits
        try:
            self.root = os.path.realpath(os.path.abspath(os.fspath(workspace_root)))
        except TypeError:
            raise ValueError("workspace_root must be a path-like value") from None
        self.allowed = set(allowed_constraints)

        self.opened = False
        self.unreadable = None
        self.events_scanned = 0
        self.events_recognized = 0
        self.events_truncated = False
        self.source_bytes = 0
        self.source_truncated = False
        self.oversized_lines = 0
        self.fields_truncated = 0
        self.ignored = 0
        self.malformed = 0

        self.completed = 0
        self.completion_keys = set()
        self.deduplicated = 0
        self.finish_reasons = {bucket: 0 for bucket in FINISH_REASON_BUCKETS}
        self.requests = 0
        self.requests_seen = False

        # run_observation model attempts. The two seen flags record whether the
        # corresponding observation family was instrumented: a zero dispatch
        # count is only reported when a prepared/dispatched observation really
        # exists, and the prepared count stays unknown unless a prepared
        # observation was seen (a dispatch-only stream never implies zero
        # prepared observations).
        self.model_prepared = 0
        self.model_dispatched = 0
        self.model_prepared_seen = False
        self.model_dispatched_seen = False
        self.prepared_duplicates = 0
        self.dispatched_duplicates = 0
        self.model_prepared_keys = set()
        self.model_dispatched_keys = set()

        # Fixed-vocabulary builtin tool activity. The two named families are
        # gated by the corresponding seen flag so a phase that was never
        # instrumented stays unknown instead of a fabricated zero; a real zero
        # is only reported once that family appeared. Only the operation label
        # and the Ok/Err discriminant are read -- never a target, argument,
        # result body, command, pattern, path or call id.
        self.tool_request = {bucket: 0 for bucket in TOOL_ACTIVITY_BUCKETS}
        self.tool_result = {bucket: 0 for bucket in TOOL_ACTIVITY_BUCKETS}
        self.tool_error = {bucket: 0 for bucket in TOOL_ACTIVITY_BUCKETS}
        self.tool_started_seen = False
        self.tool_finished_seen = False
        self.tool_dispatch = 0
        self.tool_dispatch_seen = False
        self.tool_dispatch_duplicates = 0
        self.tool_dispatch_keys = set()
        self.tool_activity_malformed = 0
        self.tool_activity_truncated = False

        # Reads are kept in two independent families so the stream-level choice
        # can be made after the whole scan: a structured read_observation that
        # appears at the end must still suppress legacy candidates seen earlier.
        # Each family is bounded by the same event cap as the scan itself.
        self.reads = {
            READ_FAMILY_OBSERVATION: _ReadFamily(),
            READ_FAMILY_TOOL_STARTED: _ReadFamily(),
        }
        self.read_observation_seen = False
        self.ambiguous_ranges = 0

        self.compactions = []
        self.conflicting_fields = 0
        self.goal_ids = 0

        self.constraint_events = 0
        self.constraint_evidence = 0
        self.preserved = set()
        self.dropped = set()
        self.non_allowlisted_ids = 0

    # -- scanning ---------------------------------------------------------

    def scan(self, path):
        try:
            handle = open(os.fspath(path), "rb")
        except OSError as error:
            self.unreadable = _os_error_category(error)
            return
        self.opened = True
        with handle:
            while self.events_scanned < self.limits.max_events:
                if self.source_bytes >= self.limits.max_source_bytes:
                    self.source_truncated = True
                    break
                remaining = self.limits.max_source_bytes - self.source_bytes
                raw = handle.readline(min(self.limits.max_line_bytes + 1, remaining))
                if not raw:
                    break
                self.source_bytes += len(raw)
                if self.source_bytes > self.limits.max_source_bytes:
                    self.source_truncated = True
                    break
                if len(raw) > self.limits.max_line_bytes:
                    # Oversized line: count it, then drain the remainder with
                    # bounded reads instead of buffering the whole line.
                    self.oversized_lines += 1
                    self.fields_truncated += 1
                    if not raw.endswith(b"\n"):
                        self._drain(handle)
                    if self.source_truncated:
                        break
                    continue
                self.events_scanned += 1
                self.handle_event(_decode_line(raw))
            if self.events_scanned >= self.limits.max_events:
                self.events_truncated = True

    def _drain(self, handle):
        while True:
            remaining = self.limits.max_source_bytes - self.source_bytes
            if remaining <= 0:
                self.source_truncated = True
                return
            chunk = handle.readline(min(self.limits.max_line_bytes + 1, remaining))
            if not chunk:
                return
            self.source_bytes += len(chunk)
            if chunk.endswith(b"\n"):
                return

    # -- event dispatch ---------------------------------------------------

    def handle_event(self, event):
        if event is _NOT_JSON or not isinstance(event, dict):
            self.ignored += 1
            return
        event_type = event.get("type")
        if event_type in ("tool_started", "tool_finished", "read_observation", "completion_call", "model_request", "run_observation", "context_compaction", "constraint_observation"):
            self.events_recognized += 1
        if event_type == "tool_started":
            self._handle_tool_started(event.get("data"))
        elif event_type == "tool_finished":
            self._handle_tool_finished(event.get("data"))
        elif event_type in READ_OBSERVATION_TYPES:
            self._handle_read_observation(event.get("data"))
        elif event_type == "completion_call":
            self._handle_completion_call(event.get("data"))
        elif event_type == "model_request":
            self._handle_model_request(event.get("data"))
        elif event_type == RUN_OBSERVATION_TYPE:
            self._handle_run_observation(event.get("data"))
        elif event_type == "context_compaction":
            self._handle_compaction(event.get("data"))
        elif event_type == "constraint_observation":
            self._handle_constraint_observation(event.get("data"))
        else:
            self.ignored += 1

    def _handle_tool_started(self, data):
        if not isinstance(data, dict):
            self.malformed += 1
            return
        # The tool body announced it is about to run. This is a real observed
        # event but it is neither the model's tool-call request nor an
        # admission record, so it is only ever labelled as `tool_started`.
        self.tool_started_seen = True
        self._record_tool_phase(data.get("operation"), "request")
        if data.get("operation") != READ_OPERATION:
            return
        self._record_read(data.get("target"), data, READ_FAMILY_TOOL_STARTED)

    def _handle_tool_finished(self, data):
        """Count one legacy tool result by its Ok/Err discriminant only.

        The result payload itself (which may contain file contents or a model
        reply) is never read: only which of the two serde keys, ``Ok`` or
        ``Err``, is present.
        """
        if not isinstance(data, dict):
            self.malformed += 1
            return
        self.tool_finished_seen = True
        result = data.get("result")
        phase = None
        if isinstance(result, dict):
            keys = set(result)
            if keys == {"Ok"}:
                phase = "result"
            elif keys == {"Err"}:
                phase = "error"
        if phase is None:
            self.tool_activity_malformed += 1
            return
        self._record_tool_phase(data.get("operation"), phase)

    def _record_tool_phase(self, raw_operation, phase):
        """Bump one fixed-vocabulary tool phase, folding unknown labels to `other`.

        Only the operation label is read. A missing, non-string or empty label
        is malformed; an oversized label is reported as truncated and folded to
        ``other``; any other unknown label is folded to ``other``. In every
        case the label string itself is dropped, never stored.
        """
        if not isinstance(raw_operation, str) or not raw_operation:
            self.tool_activity_malformed += 1
            return
        if len(raw_operation) > self.limits.max_field_chars:
            self.tool_activity_truncated = True
            bucket = TOOL_ACTIVITY_OTHER
        elif raw_operation in TOOL_ACTIVITY_NAMES:
            bucket = raw_operation
        else:
            bucket = TOOL_ACTIVITY_OTHER
        if phase == "request":
            self.tool_request[bucket] += 1
        elif phase == "result":
            self.tool_result[bucket] += 1
        else:
            self.tool_error[bucket] += 1

    def _record_tool_dispatch(self, data):
        """Count the unnamed post-admission tool dispatch observation.

        ``run_observation`` carries no tool name in this schema, so this stays
        a stream total and never a named count. The real ``runId``/``effectId``
        are used only as a transient dedup key, exactly like the model
        dispatch family; neither is stored.
        """
        self.tool_dispatch_seen = True
        run_id = data.get("runId")
        effect_id = _explicit_int(data.get("effectId"), 0)
        if (
            not isinstance(run_id, str)
            or not run_id
            or len(run_id) > self.limits.max_field_chars
            or effect_id is None
        ):
            self.tool_activity_malformed += 1
            return
        key = (run_id, effect_id)
        if key in self.tool_dispatch_keys:
            self.tool_dispatch_duplicates += 1
            return
        self.tool_dispatch_keys.add(key)
        self.tool_dispatch += 1

    def _handle_read_observation(self, data):
        """Record one structured, successful read observation.

        The family is marked observed as soon as a well-formed observation
        object arrives, even when its path is rejected, because the structured
        channel is then live for the stream and stale legacy candidates must be
        suppressed. A non-object payload is malformed and marks nothing.
        """
        if not isinstance(data, dict):
            self.malformed += 1
            return
        self.read_observation_seen = True
        self._record_read(data.get("path"), data, READ_FAMILY_OBSERVATION)

    def _record_read(self, raw_path, data, family_name):
        family = self.reads[family_name]
        family.seen += 1
        kind, start, end, length, ambiguous = _extract_range(data)
        if ambiguous:
            self.ambiguous_ranges += 1
        relative, reason = _sanitize_path(raw_path, self.root, self.limits)
        if relative is None:
            family.rejected[reason] += 1
            if reason == "too_long":
                self.fields_truncated += 1
            return
        key = (relative, kind, start, end)
        entry = family.entries.get(key)
        if entry is None:
            entry = {
                "path": relative,
                "origin": family_name,
                "semantics": READ_SEMANTICS[family_name],
                "range": {"kind": kind, "start": start, "end": end, "length": length},
                "count": 0,
                "repeats": 0,
            }
            family.entries[key] = entry
            family.order.append(key)
        entry["count"] += 1
        entry["repeats"] = entry["count"] - 1
        family.total += 1

    def _handle_completion_call(self, data):
        if not isinstance(data, dict):
            self.malformed += 1
            return
        run_id = data.get("runId")
        call_index = data.get("callIndex")
        if (
            not isinstance(run_id, str)
            or not run_id
            or len(run_id) > self.limits.max_field_chars
            or isinstance(call_index, bool)
            or not isinstance(call_index, int)
            or call_index < 0
        ):
            self.malformed += 1
            return
        key = (run_id, call_index)
        if key in self.completion_keys:
            self.deduplicated += 1
            return
        self.completion_keys.add(key)
        self.completed += 1
        self.finish_reasons[_finish_reason_bucket(data.get("finishReason"))] += 1

    def _handle_model_request(self, data):
        if not isinstance(data, dict):
            self.malformed += 1
            return
        self.requests_seen = True
        self.requests += 1

    def _handle_run_observation(self, data):
        """Count the real model prepared/dispatched run observations.

        Only ``kind`` values in :data:`MODEL_OBSERVATION_KINDS` are counted;
        run/tool lifecycle observations are tolerated and ignored. The real
        ``runId`` and ``effectId`` are used only as a transient dedup key so a
        repeated line for the same dispatch is not counted twice -- neither is
        stored nor reported. ``model_prepared`` is pre-admission and never
        marks a dispatch.
        """
        if not isinstance(data, dict):
            self.malformed += 1
            return
        kind = data.get("kind")
        if not isinstance(kind, str):
            self.malformed += 1
            return
        if kind == TOOL_DISPATCH_KIND:
            # The real post-admission tool dispatch, counted as its own
            # unnamed phase. It never touches the model prepared/dispatched
            # families below.
            self._record_tool_dispatch(data)
            return
        if kind not in MODEL_OBSERVATION_KINDS:
            # run_started / run_finished / tool_* and future kinds are valid
            # observations that this scan does not count.
            self.ignored += 1
            return
        run_id = data.get("runId")
        effect_id = _explicit_int(data.get("effectId"), 0)
        if (
            not isinstance(run_id, str)
            or not run_id
            or len(run_id) > self.limits.max_field_chars
            or effect_id is None
        ):
            self.malformed += 1
            return
        key = (run_id, effect_id)
        if kind == "model_dispatched":
            self.model_dispatched_seen = True
            if key in self.model_dispatched_keys:
                self.dispatched_duplicates += 1
                return
            self.model_dispatched_keys.add(key)
            self.model_dispatched += 1
        else:
            self.model_prepared_seen = True
            if key in self.model_prepared_keys:
                self.prepared_duplicates += 1
                return
            self.model_prepared_keys.add(key)
            self.model_prepared += 1

    def _handle_compaction(self, data):
        if not isinstance(data, dict):
            self.malformed += 1
            return
        report = data.get("report")
        if not isinstance(report, dict):
            self.malformed += 1
            return
        kind = data.get("kind")
        # Original replaced-prefix range. ``source_messages`` is the count of
        # messages inside the half-open prefix, never the full history length.
        source_start = self._report_int(report, "source_start", "sourceStart", 0)
        source_end = self._report_int(report, "source_end", "sourceEnd", 0)
        truncated_sections = self._report_list(report, "truncated_sections", "truncatedSections")
        preserved_constraint_ids = self._report_list(
            report, "preserved_constraint_ids", "preservedConstraintIds"
        )
        dropped_constraint_ids = self._report_list(
            report, "dropped_constraint_ids", "droppedConstraintIds"
        )
        preserved_goal_ids = self._report_list(
            report, "preserved_goal_ids", "preservedGoalIds"
        )
        self.compactions.append(
            {
                "kind": kind if kind in COMPACTION_KINDS else "unknown",
                # Prefix source range (original report semantics).
                "source_messages": (
                    source_end - source_start
                    if source_start is not None and source_end is not None and source_end >= source_start
                    else None
                ),
                "source_bytes": self._report_int(report, "source_bytes", "sourceBytes", 0),
                "summary_bytes": self._report_int(report, "summary_bytes", "summaryBytes", 0),
                "preserved_system_messages": self._report_int(
                    report, "preserved_system_messages", "preservedSystemMessages", 0
                ),
                "unrepresented_content_messages": self._report_int(
                    report,
                    "unrepresented_content_messages",
                    "unrepresentedContentMessages",
                    0,
                ),
                # Full-history counters. Only an explicit field is kept; they
                # are never derived from the prefix range, so an old report
                # leaves them unknown instead of passing the prefix off as the
                # whole history.
                "before_messages": self._report_int(
                    report, "before_messages", "beforeMessages", 0
                ),
                "after_messages": self._report_int(
                    report, "after_messages", "afterMessages", 0
                ),
                "before_bytes": self._report_int(report, "before_bytes", "beforeBytes", 0),
                "after_bytes": self._report_int(report, "after_bytes", "afterBytes", 0),
                "truncated_sections": (
                    len(truncated_sections[:MAX_CONSTRAINTS])
                    if truncated_sections is not None
                    else None
                ),
                "tool_call_ids_truncated": self._report_bool(
                    report, "tool_call_ids_truncated", "toolCallIdsTruncated"
                ),
                "extractive": _explicit_bool(report.get("extractive")),
                "pinned_items_preserved": self._report_bool(
                    report, "pinned_items_preserved", "pinnedItemsPreserved"
                ),
            }
        )
        if preserved_constraint_ids is not None:
            for value in preserved_constraint_ids[:MAX_CONSTRAINTS]:
                self._record_constraint(value, "preserved")
        if dropped_constraint_ids is not None:
            for value in dropped_constraint_ids[:MAX_CONSTRAINTS]:
                self._record_constraint(value, "dropped")
        # Goal ids are counted as a bounded number only: they never enter the
        # caller constraint allowlist and no goal id string is ever stored.
        if preserved_goal_ids is not None:
            for value in preserved_goal_ids[:MAX_CONSTRAINTS]:
                self._record_goal(value)

    def _report_field(self, report, snake, camel):
        """Resolve one report field in either spelling, flagging contradictions.

        Two contradicting explicit spellings make the record malformed so the
        field is treated as unknown; a value is never chosen between them.
        """
        value, conflict = _dual_value(report, snake, camel)
        if conflict:
            self.conflicting_fields += 1
            self.malformed += 1
            return _CONFLICT
        return value

    def _report_int(self, report, snake, camel, minimum):
        value = self._report_field(report, snake, camel)
        if value is _CONFLICT:
            return None
        return _explicit_int(value, minimum)

    def _report_bool(self, report, snake, camel):
        value = self._report_field(report, snake, camel)
        if value is _CONFLICT:
            return None
        return _explicit_bool(value)

    def _report_list(self, report, snake, camel):
        value = self._report_field(report, snake, camel)
        if value is _CONFLICT or not isinstance(value, list):
            return None
        return value

    def _record_goal(self, raw_id):
        """Count one bounded, identifier-shaped goal id without storing it.

        Goal ids are deliberately kept out of :attr:`allowed`, :attr:`preserved`
        and :attr:`dropped`: they are not constraint coverage.
        """
        if not isinstance(raw_id, str):
            self.fields_truncated += 1
            return
        candidate = raw_id.strip()
        if (
            not candidate
            or len(candidate) > self.limits.max_field_chars
            or not CONSTRAINT_ID_RE.match(candidate)
        ):
            self.fields_truncated += 1
            return
        self.goal_ids += 1

    def _handle_constraint_observation(self, data):
        if not isinstance(data, dict):
            self.malformed += 1
            return
        status = data.get("status")
        if status not in CONSTRAINT_STATUSES:
            self.malformed += 1
            return
        self.constraint_events += 1
        self._record_constraint(data.get("constraintId"), status)

    def _record_constraint(self, raw_id, status):
        if not isinstance(raw_id, str):
            self.fields_truncated += 1
            return
        candidate = raw_id.strip()
        if (
            not candidate
            or len(candidate) > self.limits.max_field_chars
            or not CONSTRAINT_ID_RE.match(candidate)
        ):
            self.fields_truncated += 1
            return
        if candidate not in self.allowed:
            self.non_allowlisted_ids += 1
            return
        if status == "preserved":
            self.preserved.add(candidate)
        else:
            self.dropped.add(candidate)
        self.constraint_evidence += 1


def _explicit_bool(value):
    return value if isinstance(value, bool) else None


def _sum_or_null(values):
    if not values:
        return None
    if all(isinstance(value, int) and not isinstance(value, bool) for value in values):
        return sum(values)
    return None


def _all_bool_or_null(values):
    if not values:
        return None
    observed = [value for value in values if isinstance(value, bool)]
    if len(observed) != len(values):
        return None
    return all(observed)


def _tool_phase_block(source, events, counts):
    """One named tool phase: fixed source label, observed flag, total and buckets.

    ``events`` is ``None`` when the phase family was never observed, and then
    ``by_name`` is ``None`` too rather than a map of fabricated zeros.
    """
    return {
        "source": source,
        "observed": events is not None,
        "events": events,
        "by_name": (
            {bucket: counts[bucket] for bucket in TOOL_ACTIVITY_BUCKETS}
            if events is not None
            else None
        ),
    }


def _build_payload(scan, allowed, limits):
    reliable = (
        scan.opened
        and scan.events_recognized > 0
        and not scan.events_truncated
        and not scan.source_truncated
        and scan.malformed == 0
        and scan.oversized_lines == 0
    )

    # One read is never counted twice: any structured read_observation makes the
    # whole stream prefer successful read returns, so legacy tool_started
    # candidates are suppressed no matter where they appeared. Without a
    # structured observation the legacy attempts are kept, degraded to unknown
    # ranges. The family choice itself is only reported when the scan is
    # reliable: a truncated scan may have hidden the other family entirely.
    structured_seen = scan.read_observation_seen
    legacy_family = scan.reads[READ_FAMILY_TOOL_STARTED]
    selected_family = (
        scan.reads[READ_FAMILY_OBSERVATION] if structured_seen else legacy_family
    )
    if not reliable:
        read_source = READ_SOURCE_UNKNOWN
        read_semantics = READ_SEMANTICS_UNKNOWN
    elif structured_seen:
        read_source = READ_SOURCES[READ_FAMILY_OBSERVATION]
        read_semantics = READ_SEMANTICS[READ_FAMILY_OBSERVATION]
    elif legacy_family.seen:
        read_source = READ_SOURCES[READ_FAMILY_TOOL_STARTED]
        read_semantics = READ_SEMANTICS[READ_FAMILY_TOOL_STARTED]
    else:
        read_source = READ_SOURCE_UNKNOWN
        read_semantics = READ_SEMANTICS_UNKNOWN

    files = [dict(selected_family.entries[key]) for key in selected_family.order]
    repeats = [entry["repeats"] for entry in files]
    legacy_attempts_suppressed = legacy_family.seen if structured_seen else 0

    compactions = scan.compactions
    kinds = {kind: 0 for kind in COMPACTION_KINDS}
    for record in compactions:
        kinds[record["kind"]] += 1

    preserved = set(scan.preserved)
    dropped = set(scan.dropped)
    conflicting = preserved & dropped
    allowed_set = set(allowed)

    # Tool activity is reported per phase. A phase whose event family was never
    # seen is null (unknown), because a scan that never saw the family cannot
    # assert zero; once the family was observed, a zero is a real observed
    # zero. The by-name map is present exactly when the phase count is.
    tool_request_events = (
        sum(scan.tool_request.values())
        if reliable and scan.tool_started_seen
        else None
    )
    tool_result_events = (
        sum(scan.tool_result.values())
        if reliable and scan.tool_finished_seen
        else None
    )
    tool_error_events = (
        sum(scan.tool_error.values())
        if reliable and scan.tool_finished_seen
        else None
    )
    tool_dispatch_events = (
        scan.tool_dispatch if reliable and scan.tool_dispatch_seen else None
    )

    return {
        "schema_version": EVIDENCE_SCHEMA_VERSION,
        "kind": EVIDENCE_KIND,
        "source": {
            "opened": scan.opened,
            "unreadable": scan.unreadable,
            "bytes_read": scan.source_bytes,
            "events_scanned": scan.events_scanned,
            "events_recognized": scan.events_recognized,
            "unsupported_records": scan.ignored,
            "malformed": scan.malformed,
        },
        "model_calls": {
            "completed": scan.completed if reliable else None,
            "prepared": (
                scan.model_prepared if reliable and scan.model_prepared_seen else None
            ),
            "dispatched": (
                scan.model_dispatched
                if reliable and (scan.model_dispatched_seen or scan.model_prepared_seen)
                else None
            ),
            "requests": (scan.requests if reliable and scan.requests_seen else None),
            "deduplicated": scan.deduplicated,
            "prepared_duplicates": scan.prepared_duplicates,
            "dispatched_duplicates": scan.dispatched_duplicates,
            "finish_reasons": dict(scan.finish_reasons),
            "dispatch_note": _MODEL_DISPATCH_SEMANTICS,
            "request_note": _MODEL_REQUEST_SEMANTICS,
        },
        "reads": {
            "total": selected_family.total if reliable else None,
            "distinct": len(files) if reliable else None,
            "repeated_reads": sum(repeats) if reliable else None,
            "max_repeats": max(repeats) if reliable and repeats else None,
            "source": read_source,
            "semantics": read_semantics,
            "legacy_attempts_suppressed": legacy_attempts_suppressed,
            "range_known_entries": sum(
                1 for entry in files if entry["range"]["kind"] in LINE_RANGE_KINDS
            ),
            "range_unknown_entries": sum(
                1 for entry in files if entry["range"]["kind"] == "unknown"
            ),
            "ambiguous_entries": sum(
                1 for entry in files if entry["range"]["kind"] == "ambiguous"
            ),
            "rejected_paths": dict(selected_family.rejected),
            "files": files,
        },
        "tool_activity": {
            "note": _TOOL_ACTIVITY_NOTE,
            "names": list(TOOL_ACTIVITY_BUCKETS),
            "request": _tool_phase_block(
                "tool_started", tool_request_events, scan.tool_request
            ),
            "dispatch": {
                "source": "run_observation:tool_dispatched",
                "observed": tool_dispatch_events is not None,
                "events": tool_dispatch_events,
                "by_name": None,
                "duplicates": scan.tool_dispatch_duplicates,
                "note": (
                    "tool_dispatched carries no tool name in this schema, so "
                    "only a stream total is reported and by_name stays null; "
                    "post-admission, not a wire request or billing record."
                ),
            },
            "result": _tool_phase_block(
                "tool_finished:ok", tool_result_events, scan.tool_result
            ),
            "error": _tool_phase_block(
                "tool_finished:err", tool_error_events, scan.tool_error
            ),
            "malformed": scan.tool_activity_malformed,
            "truncated": scan.tool_activity_truncated,
        },
        "compaction": {
            "events": len(compactions) if reliable else None,
            "kinds": kinds,
            # Replaced-prefix range (original report semantics). Never presented
            # as the full history length.
            "source_messages": (
                _sum_or_null([r["source_messages"] for r in compactions]) if reliable else None
            ),
            "source_bytes": (
                _sum_or_null([r["source_bytes"] for r in compactions]) if reliable else None
            ),
            "summary_bytes": (
                _sum_or_null([r["summary_bytes"] for r in compactions]) if reliable else None
            ),
            # Full-history before/after counters. Unknown (null) unless the
            # observation explicitly reports them for every record.
            "messages_before": (
                _sum_or_null([r["before_messages"] for r in compactions]) if reliable else None
            ),
            "messages_after": (
                _sum_or_null([r["after_messages"] for r in compactions]) if reliable else None
            ),
            "bytes_before": (
                _sum_or_null([r["before_bytes"] for r in compactions]) if reliable else None
            ),
            "bytes_after": (
                _sum_or_null([r["after_bytes"] for r in compactions]) if reliable else None
            ),
            "dropped_messages": (
                _sum_or_null(
                    [r["unrepresented_content_messages"] for r in compactions]
                )
                if reliable
                else None
            ),
            "dropped_bytes": None,
            "truncated_sections": (
                _sum_or_null([r["truncated_sections"] for r in compactions])
                if reliable
                else None
            ),
            "tool_call_ids_truncated": (
                _all_bool_or_null([r["tool_call_ids_truncated"] for r in compactions])
                if reliable
                else None
            ),
            "extractive": (
                _all_bool_or_null([r["extractive"] for r in compactions])
                if reliable
                else None
            ),
            "pinned_items_preserved": (
                _all_bool_or_null([r["pinned_items_preserved"] for r in compactions])
                if reliable
                else None
            ),
            "goal_ids": scan.goal_ids if reliable else None,
            "note": (
                "source_messages/source_bytes/summary_bytes are the replaced "
                "prefix; before/after counts are explicit-only and never "
                "prefix-derived, not tokens or a semantic loss; dropped_bytes "
                "null; goal_ids excluded from coverage."
            ),
        },
        "constraint_coverage": {
            "source": "structured_events" if scan.constraint_events else "unknown",
            "allowed": sorted(allowed_set),
            "preserved": sorted(preserved - conflicting),
            "dropped": sorted(dropped - conflicting),
            "conflicting": sorted(conflicting),
            "unknown": sorted(allowed_set - preserved - dropped),
            "non_allowlisted_ids": scan.non_allowlisted_ids,
            "evidence_ids": scan.constraint_evidence,
            "note": (
                "Coverage is unknown unless a structured event explicitly names a "
                "caller-allowlisted constraint id; keywords are never matched."
            ),
        },
        "limits": limits.as_dict(),
        "truncation": {
            "events_truncated": scan.events_truncated,
            "source_truncated": scan.source_truncated,
            "oversized_lines": scan.oversized_lines,
            "fields_truncated": scan.fields_truncated,
            "ambiguous_ranges": scan.ambiguous_ranges,
            "conflicting_fields": scan.conflicting_fields,
            "output_truncated": False,
            "dropped_records": 0,
            "allowed_truncated": False,
        },
        "requires_instrumentation": list(REQUIRES_INSTRUMENTATION),
        "redaction": {"note": _REDACTION_NOTE},
    }


def serialize(evidence):
    """Render evidence canonically as UTF-8 bytes (the exact bytes ``write`` stores)."""
    text = json.dumps(evidence, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    return (text + "\n").encode("utf-8")


def _serialized_size(payload):
    return len(serialize(payload))


def _drop_one(payload):
    """Remove one optional record for the byte budget; return False when none remain."""
    files = payload["reads"]["files"]
    if files:
        files.pop()
        return True
    coverage = payload["constraint_coverage"]
    if coverage["allowed"]:
        coverage["allowed"].pop()
        remaining = set(coverage["allowed"])
        for category in ("preserved", "dropped", "conflicting", "unknown"):
            coverage[category] = [value for value in coverage[category] if value in remaining]
        payload["truncation"]["allowed_truncated"] = True
        return True
    tool_activity = payload.get("tool_activity")
    if isinstance(tool_activity, dict):
        if any(
            isinstance(tool_activity.get(phase), dict)
            and tool_activity[phase].get("by_name") is not None
            for phase in ("request", "result", "error")
        ):
            for phase in ("request", "result", "error"):
                block = tool_activity.get(phase)
                if isinstance(block, dict):
                    block["by_name"] = None
            tool_activity["truncated"] = True
            return True
        payload["tool_activity"] = None
        return True
    return False


def _enforce_output_budget(payload, limits):
    """Shrink optional records until the canonical output fits the byte budget."""
    truncation = payload["truncation"]
    truncation["output_truncated"] = True
    truncation["dropped_records"] = 0
    dropped = 0
    while _serialized_size(payload) > limits.max_output_bytes and _drop_one(payload):
        dropped += 1
        truncation["dropped_records"] = dropped
    if dropped == 0:
        # "false" is one byte longer than "true"; re-check before settling.
        truncation["output_truncated"] = False
        if _serialized_size(payload) > limits.max_output_bytes:
            truncation["output_truncated"] = True
    if _serialized_size(payload) > limits.max_output_bytes:
        raise ValueError("diagnostic metadata exceeds the configured output byte bound")
    return payload


def collect(log_path, workspace_root, *, allowed_constraints=(), limits=None):
    """Scan one JSONL log and return bounded, sanitized evidence.

    ``workspace_root`` is required so host paths and symlink escapes can be
    rejected by resolved containment. ``allowed_constraints`` is the caller's
    explicit offline constraint id allowlist; ids outside it are counted but
    never stored. ``limits`` overrides the scan bounds (tests use small values);
    the defaults meet the required floors.

    A missing or unreadable log yields ``source.opened == False`` with the count
    fields ``None`` (unknown), never zero.
    """
    limits = _resolve_limits(limits)
    allowed, allowed_truncated, fields_truncated = _normalize_constraint_ids(
        allowed_constraints, limits
    )
    scan = _Scanner(workspace_root, allowed, limits)
    scan.fields_truncated += fields_truncated
    scan.scan(log_path)
    payload = _build_payload(scan, allowed, limits)
    payload["truncation"]["allowed_truncated"] = allowed_truncated
    return _enforce_output_budget(payload, limits)


def write(evidence, path, *, limits=None, create_parents=False):
    """Write evidence exclusively, refusing to overwrite. Returns bytes written.

    The parent directory is created only when ``create_parents`` is true. The
    file is created with ``O_EXCL`` (and ``O_NOFOLLOW`` where available) and
    owner-only permissions, so a pre-existing or symlinked target is refused.
    """
    limits = _resolve_limits(limits)
    data = serialize(evidence)
    if len(data) > limits.max_output_bytes:
        raise ValueError("evidence exceeds the output byte budget")
    target = os.fspath(path)
    parent = os.path.dirname(os.path.abspath(target))
    if create_parents:
        os.makedirs(parent, mode=0o700, exist_ok=True)
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(target, flags, 0o600)
    except FileExistsError as error:
        raise EvidenceAlreadyExists(f"{target}: evidence file already exists") from error
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(data)
    return len(data)


def collect_and_write(
    log_path,
    workspace_root,
    output_path,
    *,
    allowed_constraints=(),
    limits=None,
    create_parents=False,
):
    """Collect one log and store it exclusively; return the evidence dictionary."""
    evidence = collect(
        log_path, workspace_root, allowed_constraints=allowed_constraints, limits=limits
    )
    write(evidence, output_path, limits=limits, create_parents=create_parents)
    return evidence
