#!/usr/bin/env python3
r"""Run small coding tasks against an agent and verify the resulting files.

This is a self-acceptance probe set, not a public benchmark. It measures whether
one fixed task set is solved under one declared model/budget profile. By default it
reads nothing from the agent's output; with --event-format mio-jsonl it parses Mio's
own completion_call JSONL events and with --event-format pi-jsonl it parses Pi's
authoritative message_end assistant events for real token usage, while cost is never
inferred. Its score is not comparable to leaderboard numbers. See evals/README.md for
caveats.

Example (uses the model credentials already configured in the environment):
  python3 scripts/agent_eval.py --label mio --output target/evals/mio.json
  python3 scripts/agent_eval.py --config evals/configs/mio-local.json
  python3 scripts/agent_eval.py --config evals/configs/pi-example.json \
    --label pi --timeout 240 --output target/evals/pi.json

Each agent runs in a fresh task directory. Commands are argv templates, never
shell scripts. A command may include {cwd}, {prompt} and {repo}; {cwd} is the task
directory, {prompt} the task instruction, and {repo} the absolute repository root
(REPO_ROOT). Absent {cwd}, its working directory is still the task directory. Use
environment variables for credentials: reports never contain the process
environment, and argv templates are redacted before they are written out.

By default the child agent inherits this runner's environment and every task
directory is deleted when the run ends. Two opt-in flags change that without
touching the default behaviour:

  --isolate-agent-env   clear the child's MIO_* / PI_* override variables and point
                        Pi's agent home and session store at a fresh private
                        temporary directory, while keeping credentials that arrive
                        through other environment variables (for example
                        DEEPSEEK_API_KEY) and keeping PI_DELEGATE_ACTIVE so Pi's
                        delegation wrapper still refuses a nested run;
  --artifacts-dir DIR   keep each task's workspace, agent log and check log under
                        DIR with 0700/0600 permissions. Files are copied up to a
                        fixed byte limit and the report records every truncation.
                        These artifacts are raw agent output, so they can contain
                        secrets even though the report itself never does.

A third opt-in stays completely separate from raw artifacts:

  --diagnostics-dir DIR  write one sanitized, bounded diagnostics evidence file per
                        task under DIR before the temporary logs are deleted, and
                        refuse to overwrite an existing evidence file (the task is
                        skipped before the agent starts so no duplicate side effect
                        is produced). Only pairs with --event-format mio-jsonl; other
                        formats stay explicitly unsupported. Enabling diagnostics
                        never enables --artifacts-dir and never persists raw agent
                        output. `--diagnostics-constraint ID` (repeatable) supplies
                        the optional offline constraint allowlist.
"""

import argparse
import datetime
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import shlex
import signal
import stat
import subprocess
import sys
import tempfile
import time

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TASKS_DIR = REPO_ROOT / "evals" / "tasks" / "coding"
DEFAULT_AGENT_COMMAND = (
    "target/debug/mio --cwd {cwd} --shared-limits-dir {cwd}/../coordination "
    "--no-session --no-context --no-mcp -p {prompt}"
)
DEFAULT_LABEL = "mio"
DEFAULT_TIMEOUT_SECONDS = 180.0
DEFAULT_BUDGET_VERSION = "mio-eval-budget-v1"
DEFAULT_TASK_SET_VERSION = 1
REPORT_SCHEMA_VERSION = 2
REPORT_VERSION = "mio-eval-report-v1"
REPORT_TYPE = "self-acceptance-probe"

TASK_DEFAULTS = {
    "instruction": "instruction.md",
    "initial": "initial",
    "acceptance": "acceptance.py",
    "acceptance_timeout_seconds": 10,
}

CONFIG_SCHEMA_VERSION = 1
CONFIG_KEYS = (
    "schema_version",
    "label",
    "agent_command",
    "timeout_seconds",
    "output",
    "tasks_dir",
    "tasks",
    "event_format",
    "isolate_agent_env",
    "artifacts_dir",
    "diagnostics_dir",
    "diagnostics_constraints",
    "budget",
)

OVERRIDABLE = (
    "label",
    "agent_command",
    "timeout_seconds",
    "output",
    "tasks_dir",
    "tasks",
    "event_format",
    "isolate_agent_env",
    "artifacts_dir",
    "diagnostics_dir",
    "diagnostics_constraints",
)

DEFAULT_SETTINGS = {
    "label": DEFAULT_LABEL,
    "agent_command": DEFAULT_AGENT_COMMAND,
    "timeout_seconds": DEFAULT_TIMEOUT_SECONDS,
    "output": None,
    "tasks_dir": DEFAULT_TASKS_DIR,
    "tasks": None,
    "event_format": None,
    "isolate_agent_env": False,
    "artifacts_dir": None,
    "diagnostics_dir": None,
    "diagnostics_constraints": (),
}

DEFAULT_BUDGET = {
    "version": DEFAULT_BUDGET_VERSION,
    "model": None,
    "thinking": None,
    "max_model_calls": None,
    "max_total_tokens": None,
}

# External event formats the runner knows how to read. Without one the runner never
# looks at agent output, so usage stays unknown.
#
# mio-jsonl: Mio's own `completion_call` lines. Every counter is read from mio-core's
# `Usage`, where `cached_input_tokens` and `cache_creation_input_tokens` are already
# included in `input_tokens`.
#
# pi-jsonl: Pi's `--mode json` stream. Only the authoritative `message_end` assistant
# event is counted. Pi's `Usage.input` is the *uncached* prompt portion: the reference
# provider code (pi-ai `api/openai-completions.js`, used by the DeepSeek provider)
# computes `input = max(0, prompt_tokens - cacheRead - cacheWrite)` and
# `totalTokens = input + output + cacheRead + cacheWrite`. Mio's `input_tokens` is the
# inclusive prompt total, so Pi's `input` is remapped to `input + cacheRead + cacheWrite`
# only when all three are reported, and that identity is cross-checked against
# `totalTokens` before a call is accepted. Pi's catalog-priced `usage.cost` is an
# estimate, never actual payment, so it is discarded and `cost_usd` stays null.
MIO_JSONL = "mio-jsonl"
PI_JSONL = "pi-jsonl"
EVENT_FORMATS = (MIO_JSONL, PI_JSONL)

# Every token counter mio-core's Usage can report, keyed exactly as mio-core
# serializes it. `Usage` keeps snake_case field names even inside the camelCase
# CompletionCallObservation, so these are the literal JSONL keys.
USAGE_COUNTERS = (
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "cached_input_tokens",
    "cache_creation_input_tokens",
    "tool_use_prompt_tokens",
    "reasoning_tokens",
)

# Counters Pi can actually report. Pi's `Usage` has no tool-use prompt counter in any
# provider mapping, so it is excluded from Pi's coverage label while staying null.
PI_COVERAGE_COUNTERS = tuple(
    name for name in USAGE_COUNTERS if name != "tool_use_prompt_tokens"
)

# Metrics this probe never claims, whatever the event format: cost is not derived
# from token counts, and the two claims are not measured at all.
ALWAYS_UNAVAILABLE_METRICS = (
    "cost_usd",
    "quality_leadership",
    "cross_vendor_comparison",
)

# Bounded scan limits so an oversized or hostile log cannot exhaust memory.
MAX_EVENT_LOG_BYTES = 8 * 1024 * 1024
MAX_EVENT_LOG_LINES = 200_000
MAX_EVENT_LOG_LINE_BYTES = 262_144
MAX_EVENT_CALLS = 50_000

EVENT_STAT_KEYS = ("scanned", "accepted", "duplicates", "ignored", "malformed")

# --- Opt-in agent isolation -------------------------------------------------
# Environment variable prefixes removed from the child when --isolate-agent-env
# is set: every Mio model/runtime override (MIO_*) and every Pi session, provider,
# model and marker variable (PI_*). Everything else - provider API keys such as
# DEEPSEEK_API_KEY, proxy settings, PATH - is inherited unchanged, because
# credentials are supposed to come from the environment rather than the report.
ISOLATED_ENV_PREFIXES = ("MIO_", "PI_")
# Deliberately NOT cleared, whatever the prefix rule says. Pi's delegation wrapper
# refuses to run when this marker is already set, and that guard is load-bearing:
# the runner must be started from a non-delegated environment rather than nesting
# delegation, so isolation must never quietly delete the marker to make a nested
# run succeed.
ISOLATED_ENV_KEEP = ("PI_DELEGATE_ACTIVE",)
# Pi's documented relocations for its agent home and session store. They point at
# fresh empty directories inside the run's private temporary root, so the measured
# run cannot read the user's ~/.pi/agent auth.json, models.json or settings.json,
# and cannot reuse a real session. HOME itself is deliberately left alone: the
# official Pi entry point resolves ~/.local/bin/pi-run through HOME.
PI_AGENT_DIR_ENV = "PI_CODING_AGENT_DIR"
PI_SESSION_DIR_ENV = "PI_CODING_AGENT_SESSION_DIR"

# --- Opt-in artifact retention ----------------------------------------------
# Retention has per-file, per-workspace and entry-count bounds. These bounds do
# not constrain the original agent process's disk writes.
MAX_ARTIFACT_LOG_BYTES = 4 * 1024 * 1024
MAX_ARTIFACT_FILE_BYTES = 4 * 1024 * 1024
MAX_ARTIFACT_WORKSPACE_BYTES = 32 * 1024 * 1024
MAX_ARTIFACT_FILES = 1024
MAX_ARTIFACT_SCAN_ENTRIES = 10_000

# --- Opt-in bounded diagnostics ---------------------------------------------
# scripts/eval_diagnostics.py is a standalone, side-effect-free collector that is
# loaded by path because the runner is a plain script, not an importable package.
# Importing it neither enables nor disables anything: collection stays opt-in.
def _load_diagnostics_module():
    path = Path(__file__).resolve().parent / "eval_diagnostics.py"
    spec = importlib.util.spec_from_file_location("mio_eval_diagnostics", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


eval_diagnostics = _load_diagnostics_module()

# Per-task diagnostics statuses. Every one of them is explicit in the report so a
# missing source, a bound overrun, an unsupported event format or a collector fault
# can never be presented as a complete scan.
DIAGNOSTICS_STATUSES = ("collected", "truncated", "unknown", "unsupported", "error")
DIAGNOSTICS_COVERAGE_KEYS = (
    "model_calls_completed",
    "model_calls_prepared",
    "model_calls_dispatched",
    "read_entries",
    "repeated_reads",
    "range_unknown_reads",
    "tool_request_events",
    "compaction_events",
    "constraint_evidence_ids",
)
DIAGNOSTICS_TRUNCATION_KEYS = (
    "events_truncated",
    "source_truncated",
    "oversized_lines",
    "fields_truncated",
    "ambiguous_ranges",
    "output_truncated",
    "dropped_records",
    "allowed_truncated",
)
# Deterministic, leak-free reasons for the formats diagnostics cannot describe.
DIAGNOSTICS_UNSUPPORTED_REASONS = {PI_JSONL: "pi_jsonl_out_of_scope"}
DIAGNOSTICS_UNSUPPORTED_DEFAULT = "event_format_not_selected"

# Pi finish reasons that end one assistant response. `error` and `aborted` are kept
# explicit so a failed or cancelled model call is reported, not silently dropped.
PI_STOP_REASONS = ("stop", "toolUse", "length", "error", "aborted", "deferred")

# Mio serializes mio-core's normalized `FinishReason` with snake_case names, and its
# provider-specific variant as the externally tagged `{"other": "<wire spelling>"}`.
# A missing reason is a real outcome: the provider reported nothing, which is not the
# same fact as a provider that reported `stop`. This vocabulary is NOT Pi's: only
# stop/length and (tool_calls, toolUse) line up, so the runner keeps the two under
# different keys instead of pretending they share one scale.
MIO_WIRE_FINISH_REASONS = ("stop", "length", "tool_calls", "content_filter")
MIO_FINISH_REASON_UNREPORTED = "unreported"
MIO_FINISH_REASONS = MIO_WIRE_FINISH_REASONS + ("other", MIO_FINISH_REASON_UNREPORTED)


def empty_event_stats():
    """Counters describing what a bounded event-log scan classified."""
    stats = {key: 0 for key in EVENT_STAT_KEYS}
    stats["truncated"] = False
    return stats


# A usage block with every counter unreported. Missing data stays null, never 0.
UNKNOWN_USAGE = {
    **{name: None for name in USAGE_COUNTERS},
    "cost_usd": None,
    "source": "unknown",
    "coverage": "unknown",
    "model_calls": None,
    "events": empty_event_stats(),
}

LIMITS = (
    "Small self-acceptance probes. By default the runner does not read model usage, "
    "so token, cache and cost fields stay null. With --event-format mio-jsonl the runner "
    "parses Mio's own completion_call JSONL events, and with --event-format pi-jsonl it "
    "parses Pi's message_end assistant events; both sum a counter only when every "
    "observed call reported it, keep unreported counters null, and never infer cost. "
    "Scores are meaningful only for the same task set, model and budget profile, and "
    "are not a public-benchmark claim. Agent logs are written under a temporary "
    "directory, parsed in place and discarded to avoid persisting model output or "
    "credentials. --isolate-agent-env and --artifacts-dir are explicit opt-ins: the "
    "first clears the child's MIO_*/PI_* variables (keeping PI_DELEGATE_ACTIVE) and "
    "gives Pi an empty temporary agent home, and the second retains raw per-task "
    "workspaces and logs with owner-only permissions and bounded size. Both are off "
    "by default, so a plain run behaves exactly as before. Retained artifacts are raw "
    "agent output and are not redacted; the report never copies their contents."
)

# A secret-looking flag: redact its value, whether inline or as the next argv item.
SECRET_HINT = re.compile(
    r"(?i)(api[_-]?key|access[_-]?key|secret|token|password|passwd|credential|authorization|bearer)"
)
# A long opaque blob (base64/hex-ish) that is unlikely to be a path or prompt.
LONG_OPAQUE = re.compile(r"^[A-Za-z0-9_\-]{32,}$")


def read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


def collect_initial_files(directory):
    """Return {relative posix path: text} for every file under an initial/ tree."""
    files = {}
    for path in sorted(directory.rglob("*")):
        if path.is_file():
            files[path.relative_to(directory).as_posix()] = path.read_text(encoding="utf-8")
    return files


def load_task_directory(directory):
    """Load one on-disk task directory and return its normalized description."""
    directory = Path(directory)
    metadata_path = directory / "task.json"
    if not metadata_path.is_file():
        raise ValueError(f"{directory}: missing task.json")
    try:
        metadata = read_json(metadata_path)
    except json.JSONDecodeError as error:
        raise ValueError(f"{metadata_path}: invalid JSON: {error}") from error
    if not isinstance(metadata, dict):
        raise ValueError(f"{metadata_path}: task.json must be a JSON object")

    task_id = metadata.get("id")
    if not isinstance(task_id, str) or not task_id:
        raise ValueError(f"{metadata_path}: 'id' must be a non-empty string")
    task_set = metadata.get("task_set", "mio-python-acceptance-v1")
    if not isinstance(task_set, str) or not task_set:
        raise ValueError(f"{metadata_path}: 'task_set' must be a non-empty string")
    task_set_version = metadata.get("task_set_version", DEFAULT_TASK_SET_VERSION)
    if not isinstance(task_set_version, int) or isinstance(task_set_version, bool):
        raise ValueError(f"{metadata_path}: 'task_set_version' must be an integer")

    layout = {}
    for key, default in TASK_DEFAULTS.items():
        layout[key] = metadata.get(key, default)

    instruction_path = directory / layout["instruction"]
    if not instruction_path.is_file():
        raise ValueError(f"{metadata_path}: missing instruction file {layout['instruction']!r}")
    prompt = instruction_path.read_text(encoding="utf-8").strip()
    if not prompt:
        raise ValueError(f"{instruction_path}: instruction must not be empty")

    initial_directory = directory / layout["initial"]
    if not initial_directory.is_dir():
        raise ValueError(f"{metadata_path}: missing initial file directory {layout['initial']!r}")
    files = collect_initial_files(initial_directory)
    if not files:
        raise ValueError(f"{initial_directory}: no initial files found")

    acceptance_path = directory / layout["acceptance"]
    if not acceptance_path.is_file():
        raise ValueError(f"{metadata_path}: missing acceptance script {layout['acceptance']!r}")

    acceptance_timeout = layout["acceptance_timeout_seconds"]
    if not isinstance(acceptance_timeout, (int, float)) or acceptance_timeout <= 0:
        raise ValueError(f"{metadata_path}: 'acceptance_timeout_seconds' must be positive")

    return {
        "id": task_id,
        "task_set": task_set,
        "task_set_version": task_set_version,
        "title": metadata.get("title", task_id),
        "language": metadata.get("language"),
        "covers": list(metadata.get("covers", [])),
        "directory": directory,
        "prompt": prompt,
        "files": files,
        "acceptance": acceptance_path,
        "acceptance_timeout_seconds": float(acceptance_timeout),
    }


def load_tasks(tasks_dir):
    """Load and validate every task directory under tasks_dir, sorted by id."""
    tasks_dir = Path(tasks_dir)
    if not tasks_dir.is_dir():
        raise ValueError(f"{tasks_dir}: tasks directory does not exist")
    tasks = [
        load_task_directory(child)
        for child in sorted(tasks_dir.iterdir())
        if child.is_dir() and (child / "task.json").is_file()
    ]
    if not tasks:
        raise ValueError(f"{tasks_dir}: no task directories with task.json found")

    seen = {}
    for task in tasks:
        if task["id"] in seen:
            raise ValueError(f"duplicate task id {task['id']!r} in {tasks_dir}")
        seen[task["id"]] = task["directory"]
    sets = {(task["task_set"], task["task_set_version"]) for task in tasks}
    if len(sets) != 1:
        raise ValueError(f"{tasks_dir}: task_set/task_set_version must be identical across tasks")
    return sorted(tasks, key=lambda task: task["id"])


def load_config(path):
    """Load a runner config; relative paths resolve against the config directory."""
    path = Path(path)
    try:
        config = read_json(path)
    except json.JSONDecodeError as error:
        raise ValueError(f"{path}: invalid JSON: {error}") from error
    if not isinstance(config, dict):
        raise ValueError(f"{path}: config must be a JSON object")
    unknown = sorted(set(config) - set(CONFIG_KEYS))
    if unknown:
        raise ValueError(f"{path}: unknown config keys: {', '.join(unknown)}")
    version = config.get("schema_version")
    if version != CONFIG_SCHEMA_VERSION:
        raise ValueError(f"{path}: schema_version must be {CONFIG_SCHEMA_VERSION}")

    base = path.resolve().parent
    for key in ("tasks_dir", "output", "artifacts_dir", "diagnostics_dir"):
        value = config.get(key)
        if value is None:
            continue
        if not isinstance(value, str) or not value:
            raise ValueError(f"{path}: {key} must be a non-empty string")
        candidate = Path(value)
        config[key] = candidate if candidate.is_absolute() else base / candidate

    tasks = config.get("tasks")
    if tasks is not None and (
        not isinstance(tasks, list) or not all(isinstance(item, str) and item for item in tasks)
    ):
        raise ValueError(f"{path}: 'tasks' must be a list of task ids")
    timeout = config.get("timeout_seconds")
    if timeout is not None and (not isinstance(timeout, (int, float)) or timeout <= 0):
        raise ValueError(f"{path}: 'timeout_seconds' must be positive")
    event_format = config.get("event_format")
    if event_format is not None and event_format not in EVENT_FORMATS:
        raise ValueError(
            f"{path}: 'event_format' must be null or one of: {', '.join(EVENT_FORMATS)}"
        )
    isolate = config.get("isolate_agent_env")
    if isolate is not None and not isinstance(isolate, bool):
        raise ValueError(f"{path}: 'isolate_agent_env' must be a boolean")
    constraints = config.get("diagnostics_constraints")
    if constraints is not None:
        if not isinstance(constraints, list):
            raise ValueError(
                f"{path}: 'diagnostics_constraints' must be a list of identifiers"
            )
        try:
            normalize_diagnostics_constraints(constraints)
        except ValueError as error:
            # Rejections never echo the offending value, so a secret-looking
            # identifier is not copied into a config error message.
            raise ValueError(f"{path}: 'diagnostics_constraints': {error}") from error
    budget = config.get("budget")
    if budget is not None:
        if not isinstance(budget, dict):
            raise ValueError(f"{path}: 'budget' must be an object")
        if not isinstance(budget.get("version"), str) or not budget["version"]:
            raise ValueError(f"{path}: 'budget.version' must be a non-empty string")
        temperature = budget.get("temperature")
        if temperature is not None and (
            isinstance(temperature, bool)
            or not isinstance(temperature, (int, float))
            or not math.isfinite(temperature)
            or not 0.0 <= temperature <= 2.0
        ):
            raise ValueError(
                f"{path}: 'budget.temperature' must be a finite number between 0 and 2"
            )
        output_cap = budget.get("max_output_tokens")
        if output_cap is not None and (
            isinstance(output_cap, bool)
            or not isinstance(output_cap, int)
            or output_cap <= 0
        ):
            raise ValueError(f"{path}: 'budget.max_output_tokens' must be a positive integer")
    return config


def resolve_settings(config, cli):
    """Merge defaults, config and explicit CLI values; CLI wins."""
    settings = dict(DEFAULT_SETTINGS)
    for key in OVERRIDABLE:
        value = config.get(key)
        if value is not None:
            settings[key] = value
    for key in OVERRIDABLE:
        value = getattr(cli, key, None)
        if value is not None:
            settings[key] = value
    if settings["output"] is not None:
        settings["output"] = Path(settings["output"])
    if settings["artifacts_dir"] is not None:
        settings["artifacts_dir"] = Path(settings["artifacts_dir"])
    if settings["diagnostics_dir"] is not None:
        settings["diagnostics_dir"] = Path(settings["diagnostics_dir"])
    settings["tasks_dir"] = Path(settings["tasks_dir"])
    return settings


def resolve_budget(config):
    """Return the declared model/budget profile recorded in the report."""
    declared = config.get("budget")
    budget = dict(DEFAULT_BUDGET)
    if declared:
        budget.update(declared)
    for key in DEFAULT_BUDGET:
        budget.setdefault(key, None)
    # The runner records this profile but cannot enforce or measure it.
    budget["enforced_by_runner"] = False
    return budget


# Pi has no `--temperature` flag, so the comparison config pins sampling through an
# explicit test extension loaded with `-e`. Any adapter that declares a fixed
# temperature or output cap must either pass the matching flag or name this
# extension; a declaration with no mechanism is refused instead of silently ignored.
PI_FIXED_SAMPLING_EXTENSION = "pi-fixed-sampling.ts"


def _flag_number(template, flag):
    """The numeric value of `--flag` in an argv template, or None when absent."""
    for index, part in enumerate(template):
        if part == flag:
            if index + 1 >= len(template):
                raise ValueError(f"{flag} in the agent command has no value")
            raw = template[index + 1]
        elif part.startswith(f"{flag}="):
            raw = part.split("=", 1)[1]
        else:
            continue
        try:
            return float(raw)
        except ValueError as error:
            raise ValueError(f"{flag} expects a number, got {raw!r}") from error
    return None


def verify_declared_sampling(budget, template):
    """Refuse a declared sampling profile the agent command never applies.

    A config that declares `budget.temperature` / `budget.max_output_tokens` is
    claiming a fixed sampling profile. If the resolved command neither passes the
    matching flag with that value nor loads the explicit Pi fixed-sampling test
    extension, the declaration would be silently ignored and the run would not be
    comparable; that is an error, not a warning.
    """
    declared_temperature = budget.get("temperature")
    declared_output = budget.get("max_output_tokens")
    if declared_temperature is None and declared_output is None:
        return
    uses_extension = any(PI_FIXED_SAMPLING_EXTENSION in part for part in template)
    problems = []
    if declared_temperature is not None:
        actual = _flag_number(template, "--temperature")
        if actual is None and not uses_extension:
            problems.append(
                "declares budget.temperature but the agent command neither passes "
                f"--temperature nor loads {PI_FIXED_SAMPLING_EXTENSION}"
            )
        elif actual is not None and actual != float(declared_temperature):
            problems.append(
                f"budget.temperature {declared_temperature!r} does not match "
                f"--temperature {actual!r}"
            )
    if declared_output is not None:
        actual = _flag_number(template, "--max-output-tokens")
        if actual is None and not uses_extension:
            problems.append(
                "declares budget.max_output_tokens but the agent command neither passes "
                f"--max-output-tokens nor loads {PI_FIXED_SAMPLING_EXTENSION}"
            )
        elif actual is not None and actual != float(declared_output):
            problems.append(
                f"budget.max_output_tokens {declared_output!r} does not match "
                f"--max-output-tokens {actual!r}"
            )
    if problems:
        raise ValueError("; ".join(problems))


def redact_command(template):
    """Redact secret-looking argv values from a template before persisting it."""
    redacted = []
    secret_next = False
    for part in template:
        if secret_next:
            redacted.append("[redacted]")
            secret_next = False
            continue
        if SECRET_HINT.search(part):
            if "=" in part:
                name = part.split("=", 1)[0]
                redacted.append(f"{name}=[redacted]")
                continue
            redacted.append(part)
            secret_next = True
            continue
        redacted.append("[redacted]" if LONG_OPAQUE.match(part) else part)
    return redacted


def unknown_usage(event_format=None, events=None):
    """A usage block with every counter unreported, for the selected format.

    `model_calls` is `None` here: with no observed completion events the number of
    model calls is unknown, not zero. The raw scan count stays in `events["accepted"]`.
    Pi runs also carry `stop_reasons`, kept `None` when nothing was observed so an
    unobserved cancellation is never reported as zero aborts.
    """
    usage = {name: None for name in USAGE_COUNTERS}
    source = event_format if event_format in EVENT_FORMATS else "unknown"
    usage.update({
        "cost_usd": None,
        "source": source,
        "coverage": "unknown",
        "model_calls": None,
        "events": dict(events) if events is not None else empty_event_stats(),
    })
    if event_format == PI_JSONL:
        usage["stop_reasons"] = None
    elif event_format == MIO_JSONL:
        usage["finish_reasons"] = None
    return usage


def counter_value(value):
    """A provider-reported count; anything else is unreported, never zero."""
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        return None
    return value


def decode_event_line(raw):
    """Decode one log line as JSON; return None when it is not a JSON value."""
    try:
        return json.loads(raw)
    except (json.JSONDecodeError, UnicodeDecodeError, RecursionError):
        return None


def normalize_finish_reason(value):
    """Map one mio serialized `FinishReason` onto a normalized bucket.

    mio-core serializes the normalized variants as snake_case strings and a
    provider-specific reason as the externally tagged `{"other": "<wire spelling>"}`.
    An absent value means the provider reported no reason at all, which is bucketed
    as `unreported` rather than smoothed into `stop`. Anything else is a value this
    runner cannot classify, so it returns `None` and the caller marks the call
    malformed instead of inventing an alignment with Pi.
    """
    if value is None:
        return MIO_FINISH_REASON_UNREPORTED
    if isinstance(value, str) and value in MIO_WIRE_FINISH_REASONS:
        return value
    if isinstance(value, dict) and set(value) == {"other"} and isinstance(value["other"], str):
        return "other"
    return None


def normalize_completion_call(event):
    """Return ((runId, callIndex), counters, finish_reason) for a completion_call.

    The finish-reason bucket can never be `None` on a successful normalization:
    `None` is reserved for a reason this runner cannot classify, so a caller that
    only recorded usage and ignored the third element stays compatible while a
    caller that tracks reasons treats an unknown spelling as malformed.
    """
    payload = event.get("data")
    if not isinstance(payload, dict):
        return None
    run_id = payload.get("runId")
    call_index = payload.get("callIndex")
    if not isinstance(run_id, str) or not run_id:
        return None
    if isinstance(call_index, bool) or not isinstance(call_index, int) or call_index < 0:
        return None
    reason = normalize_finish_reason(payload.get("finishReason"))
    if reason is None:
        return None
    usage = payload.get("usage")
    if not isinstance(usage, dict):
        usage = {}
    counters = {name: counter_value(usage.get(name)) for name in USAGE_COUNTERS}
    return (run_id, call_index), counters, reason


def bounded_log_events(path, stats):
    """Yield decoded lines while enforcing shared byte, line and line-size caps."""
    with Path(path).open("rb") as handle:
        consumed = 0
        while stats["scanned"] < MAX_EVENT_LOG_LINES and consumed < MAX_EVENT_LOG_BYTES:
            raw = handle.readline(min(MAX_EVENT_LOG_LINE_BYTES + 1, MAX_EVENT_LOG_BYTES - consumed))
            if not raw:
                return
            consumed += len(raw)
            stats["scanned"] += 1
            if not raw.endswith(b"\n") and (
                len(raw) > MAX_EVENT_LOG_LINE_BYTES or consumed >= MAX_EVENT_LOG_BYTES
            ):
                stats["truncated"] = True
                while consumed < MAX_EVENT_LOG_BYTES:
                    rest = handle.readline(min(MAX_EVENT_LOG_LINE_BYTES + 1, MAX_EVENT_LOG_BYTES - consumed))
                    consumed += len(rest)
                    if not rest or rest.endswith(b"\n"):
                        break
                continue
            yield decode_event_line(raw)
        stats["truncated"] = True


def parse_event_log(path):
    """Bounded, line-by-line scan of one agent log for completion_call events.

    Only `{"type": "completion_call", "data": {...}}` lines emitted by Mio's
    `--json` mode are read; every other line is ignored, malformed
    completion_call events are ignored, and a repeated `(runId, callIndex)` pair
    is counted once. Line contents are dropped as soon as they have been
    classified, so no raw model output survives the scan.

    Returns `(calls, finish_reasons, stats)`: deduplicated per-call counter blocks,
    per-bucket `FinishReason` counts, and a count of what the bounded scan saw. Every
    accepted call contributes exactly one bucket, so the counts sum to the number of
    accepted calls; a reason this runner cannot classify makes the whole event
    malformed instead of being bucketed as `stop`.
    """
    calls = []
    finish_reasons = {reason: 0 for reason in MIO_FINISH_REASONS}
    stats = empty_event_stats()
    seen = set()
    for event in bounded_log_events(path, stats):
        if not isinstance(event, dict) or event.get("type") != "completion_call":
            stats["ignored"] += 1
            continue
        call = normalize_completion_call(event)
        if call is None:
            stats["malformed"] += 1
            continue
        key, counters, reason = call
        if key in seen:
            stats["duplicates"] += 1
            continue
        seen.add(key)
        calls.append(counters)
        finish_reasons[reason] += 1
        stats["accepted"] += 1
        if stats["accepted"] >= MAX_EVENT_CALLS:
            stats["truncated"] = True
            break
    return calls, finish_reasons, stats


def pi_usage_counters(usage):
    """Map one Pi `Usage` object onto mio-core's counter names.

    Pi and Mio disagree on what "input" means. Pi reports the uncached prompt
    portion (`input`) plus separate `cacheRead`/`cacheWrite`, so its inclusive prompt
    total is `input + cacheRead + cacheWrite`; Mio's `input_tokens` already includes
    both cache counters. The remap therefore adds the two cache counters only when all
    three values are present, and only after cross-checking Pi's own identity
    `totalTokens == input + output + cacheRead + cacheWrite`.

    Returns `None` when Pi's numbers are self-inconsistent, so a contradictory record
    is dropped instead of being summed. Absent keys stay `None` (never fabricated as
    zero); Pi always serializes `Usage` with non-optional numeric fields, so a provider
    that omits a counter appears as Pi's own `0` and cannot be distinguished here.
    Pi's catalog-priced `cost` is an estimate, not payment, and is never read.
    """
    raw_input = counter_value(usage.get("input"))
    output = counter_value(usage.get("output"))
    cache_read = counter_value(usage.get("cacheRead"))
    cache_write = counter_value(usage.get("cacheWrite"))
    total = counter_value(usage.get("totalTokens"))
    reasoning = counter_value(usage.get("reasoning"))
    prompt_total = (
        raw_input + cache_read + cache_write
        if None not in (raw_input, cache_read, cache_write)
        else None
    )
    if prompt_total is not None and output is not None and total is not None:
        if prompt_total + output != total:
            return None
    return {
        "input_tokens": prompt_total,
        "output_tokens": output,
        "total_tokens": total,
        "cached_input_tokens": cache_read,
        "cache_creation_input_tokens": cache_write,
        "tool_use_prompt_tokens": None,
        "reasoning_tokens": reasoning,
    }


def pi_call_identity(message):
    """A stable dedup key for one Pi assistant message, or None when unusable."""
    response_id = message.get("responseId")
    if isinstance(response_id, str) and response_id:
        return ("responseId", response_id)
    timestamp = message.get("timestamp")
    if isinstance(timestamp, bool) or not isinstance(timestamp, int):
        return None
    return ("timestamp", str(message.get("provider")), str(message.get("model")), timestamp)


def normalize_pi_message_end(event):
    """Return ((identity), counters, stop_reason) for a Pi message_end, or None.

    Only `{"type": "message_end", "message": {role: "assistant", ...}}` records are
    countable; Pi documents this as the authoritative completed message. Assistant
    `message_update` deltas are intentionally not counted because their usage is a
    partial cumulative snapshot and summing deltas would double-count.
    """
    message = event.get("message")
    if not isinstance(message, dict) or message.get("role") != "assistant":
        return None
    stop_reason = message.get("stopReason")
    if not isinstance(stop_reason, str) or stop_reason not in PI_STOP_REASONS:
        return None
    key = pi_call_identity(message)
    if key is None:
        return None
    usage = message.get("usage")
    counters = pi_usage_counters(usage if isinstance(usage, dict) else {})
    if counters is None:
        return None
    return key, counters, stop_reason


def parse_pi_event_log(path):
    """Bounded, line-by-line scan of one Pi `--mode json` log for message_end usage.

    Non-JSON lines (Pi writes diagnostics to stderr, merged into the same log),
    non-`message_end` events, and non-assistant messages are ignored. A deduplicated
    `message_end` assistant event with a usable identity and self-consistent usage is
    accepted; an inconsistent or identity-less assistant completion is malformed.
    Repeated `(responseId)` / `(provider, model, timestamp)` keys count once. Line
    contents are dropped as soon as they are classified, so no raw model text is kept.

    Returns `(calls, stop_reasons, stats)`. `stop_reasons` counts accepted calls per
    terminal finish reason so error and cancellation are explicit rather than dropped.
    """
    calls = []
    stop_reasons = {reason: 0 for reason in PI_STOP_REASONS}
    stats = empty_event_stats()
    seen = set()
    for event in bounded_log_events(path, stats):
        if not isinstance(event, dict) or event.get("type") != "message_end":
            stats["ignored"] += 1
            continue
        normalized = normalize_pi_message_end(event)
        if normalized is None:
            message = event.get("message")
            if isinstance(message, dict) and message.get("role") == "assistant":
                stats["malformed"] += 1
            else:
                stats["ignored"] += 1
            continue
        key, counters, stop_reason = normalized
        if key in seen:
            stats["duplicates"] += 1
            continue
        seen.add(key)
        calls.append(counters)
        stop_reasons[stop_reason] += 1
        stats["accepted"] += 1
        if stats["accepted"] >= MAX_EVENT_CALLS:
            stats["truncated"] = True
            break
    return calls, stop_reasons, stats


def sum_counters(blocks):
    """Sum each counter only when every block reported it; otherwise null."""
    totals = {}
    for name in USAGE_COUNTERS:
        values = [block.get(name) for block in blocks]
        totals[name] = sum(values) if all(value is not None for value in values) else None
    return totals


def coverage_label(totals, reported, counters=USAGE_COUNTERS):
    """unknown / partial / complete for a block of counters.

    `counters` selects the counters that the selected event format can actually
    report; Pi's structural lack of a tool-use prompt counter must not keep every Pi
    call forever "partial".
    """
    if not reported:
        return "unknown"
    return "complete" if all(totals.get(name) is not None for name in counters) else "partial"


def coverage_counters(source):
    """Counters the selected format can report, for coverage labeling."""
    return PI_COVERAGE_COUNTERS if source == PI_JSONL else USAGE_COUNTERS


def merge_event_stats(stats_blocks):
    """Add up the per-task scan counters into one block."""
    merged = empty_event_stats()
    for stats in stats_blocks:
        if not isinstance(stats, dict):
            continue
        for key in EVENT_STAT_KEYS:
            merged[key] += stats.get(key, 0) or 0
        merged["truncated"] = merged["truncated"] or bool(stats.get("truncated"))
    return merged


def usage_from_calls(calls, stats=None, stop_reasons=None, source=MIO_JSONL, finish_reasons=None):
    """Aggregate one task's deduplicated calls into a usage block.

    `stop_reasons` carries Pi's native terminal vocabulary and `finish_reasons`
    carries Mio's normalized one. They are kept under separate keys and are never
    converted into each other: only stop/length and (tool_calls, toolUse) line up,
    and reporting a shared scale would invent agreement that does not exist.
    """
    if not calls:
        return unknown_usage(source, stats)
    if stats and (stats.get("truncated") or stats.get("malformed")):
        # A partial scan cannot establish a full-run total. Keep only the
        # observed event count in stats, not a misleading billable total.
        usage = unknown_usage(source, stats)
        usage["coverage"] = "partial"
        return usage
    totals = sum_counters(calls)
    reported = sum(1 for call in calls if any(value is not None for value in call.values()))
    usage = dict(totals)
    usage.update({
        "cost_usd": None,
        "source": source,
        "coverage": coverage_label(totals, reported, coverage_counters(source)),
        "model_calls": len(calls),
        "events": dict(stats) if stats is not None else empty_event_stats(),
    })
    if source == PI_JSONL:
        usage["stop_reasons"] = (
            {reason: stop_reasons.get(reason, 0) for reason in PI_STOP_REASONS}
            if stop_reasons is not None else None
        )
    elif source == MIO_JSONL:
        usage["finish_reasons"] = (
            {reason: finish_reasons.get(reason, 0) for reason in MIO_FINISH_REASONS}
            if finish_reasons is not None else None
        )
    return usage


def sum_task_usage(usages, note=None):
    """Sum per-task usage blocks, keeping a counter null unless every task reported it."""
    usages = list(usages)
    if not usages:
        usage = unknown_usage()
    else:
        totals = sum_counters(usages)
        reported = sum(1 for block in usages if block.get("coverage") != "unknown")
        observed_sources = {block.get("source") for block in usages if block.get("source") in EVENT_FORMATS}
        source = observed_sources.pop() if len(observed_sources) == 1 else "unknown"
        call_counts = [block.get("model_calls") for block in usages]
        usage = dict(totals)
        usage.update({
            "cost_usd": None,
            "source": source,
            "coverage": coverage_label(totals, reported, coverage_counters(source)),
            "model_calls": (
                sum(call_counts) if all(count is not None for count in call_counts) else None
            ),
            "events": merge_event_stats(block.get("events") for block in usages),
        })
        stop_blocks = [block.get("stop_reasons") for block in usages]
        if any(block is not None for block in stop_blocks):
            if all(isinstance(block, dict) for block in stop_blocks):
                merged = {reason: 0 for reason in PI_STOP_REASONS}
                for block in stop_blocks:
                    for reason in PI_STOP_REASONS:
                        merged[reason] += block.get(reason, 0) or 0
                usage["stop_reasons"] = merged
            else:
                usage["stop_reasons"] = None
        finish_blocks = [block.get("finish_reasons") for block in usages]
        if any(block is not None for block in finish_blocks):
            if all(isinstance(block, dict) for block in finish_blocks):
                merged = {reason: 0 for reason in MIO_FINISH_REASONS}
                for block in finish_blocks:
                    for reason in MIO_FINISH_REASONS:
                        merged[reason] += block.get(reason, 0) or 0
                usage["finish_reasons"] = merged
            else:
                usage["finish_reasons"] = None
    if note is not None:
        usage["note"] = note
    return usage


def collect_usage(event_format, log_path):
    """Read real usage from one task's temporary agent log, if opted in."""
    if event_format == MIO_JSONL:
        try:
            calls, finish_reasons, stats = parse_event_log(log_path)
        except OSError:
            return unknown_usage(MIO_JSONL)
        return usage_from_calls(
            calls, stats, finish_reasons=finish_reasons, source=MIO_JSONL
        )
        return usage_from_calls(calls, stats)
    if event_format == PI_JSONL:
        try:
            calls, stop_reasons, stats = parse_pi_event_log(log_path)
        except OSError:
            return unknown_usage(PI_JSONL)
        return usage_from_calls(calls, stats, stop_reasons=stop_reasons, source=PI_JSONL)
    return unknown_usage()


def unavailable_metrics(event_format):
    """Metrics the current run does not collect; token counters need the opt-in."""
    metrics = list(ALWAYS_UNAVAILABLE_METRICS)
    if event_format == MIO_JSONL:
        return metrics
    if event_format == PI_JSONL:
        # Pi's Usage has no tool-use prompt counter in any provider mapping.
        return ["tool_use_prompt_tokens"] + metrics
    return list(USAGE_COUNTERS) + metrics


def unknown_timing():
    return {
        "source": "unknown", "coverage": "unknown",
        "model": {"calls": None, "errors": None, "duration_seconds_sum": None},
        "tool": {"calls": None, "errors": None, "duration_seconds_sum": None},
        "context_seconds": None, "queue_seconds": None,
        "limits": "Observed dispatch-to-outcome spans only; concurrent spans overlap. "
                  "Sums are not wall time. Context preparation, preceding admission waits "
                  "and effects without outcomes are not measured.",
    }


def collect_diagnostic_categories(event_format, log_path):
    """Retain only known category names, never diagnostic messages or payloads."""
    if event_format != MIO_JSONL:
        return None
    allowed = {
        "network", "authentication", "rate_limit", "service", "request", "response",
        "provider", "tool_call", "context_budget", "max_turns", "scheduling",
        "cancelled", "memory", "internal",
    }
    categories = set()
    stats = empty_event_stats()
    try:
        for event in bounded_log_events(log_path, stats):
            if not isinstance(event, dict) or event.get("type") != "diagnostic":
                continue
            data = event.get("data")
            category = data.get("category") if isinstance(data, dict) else None
            if isinstance(category, str) and category in allowed:
                categories.add(category)
    except OSError:
        return None
    return None if stats["truncated"] else sorted(categories)


def collect_timing(event_format, log_path):
    """Aggregate sanitized measured spans; never derive model time from wall time."""
    timing = unknown_timing()
    if event_format != MIO_JSONL:
        return timing
    stats = empty_event_stats()
    spans = {"model": [], "tool": []}
    seen = set()
    try:
        for event in bounded_log_events(log_path, stats):
            if not isinstance(event, dict) or event.get("type") != "execution_timing":
                continue
            data = event.get("data")
            if not isinstance(data, dict):
                stats["malformed"] += 1
                continue
            run_id, effect_id = data.get("runId"), counter_value(data.get("effectId"))
            kind, status = data.get("kind"), data.get("status")
            duration_ms = counter_value(data.get("durationMs"))
            if (not isinstance(run_id, str) or not run_id or effect_id is None
                    or not isinstance(kind, str) or kind not in spans
                    or status not in ("success", "error")
                    or duration_ms is None):
                stats["malformed"] += 1
                continue
            key = (run_id, effect_id)
            if key in seen:
                stats["duplicates"] += 1
                continue
            seen.add(key)
            spans[kind].append((duration_ms, status))
            stats["accepted"] += 1
            if stats["accepted"] >= MAX_EVENT_CALLS:
                stats["truncated"] = True
                break
    except OSError:
        return timing
    timing["source"] = MIO_JSONL
    timing["events"] = stats
    if stats["malformed"] or stats["truncated"]:
        timing["coverage"] = "partial"
        return timing
    if not seen:
        return timing
    timing["coverage"] = "observed_outcomes"
    for kind, values in spans.items():
        # Zero is justified only for this scan's observed spans, not for all
        # work attempted by an interrupted run.
        timing[kind] = {
            "calls": len(values),
            "errors": sum(status == "error" for _, status in values),
            "duration_seconds_sum": sum(ms for ms, _ in values) / 1000,
        }
    return timing


def sum_task_timing(timings):
    result = unknown_timing()
    if not timings:
        return result
    if any(t.get("source") == MIO_JSONL for t in timings):
        result["source"] = MIO_JSONL
    observed = [t.get("coverage") == "observed_outcomes" for t in timings]
    if not all(observed):
        result["coverage"] = "partial" if any(observed) else "unknown"
        return result
    result["coverage"] = "observed_outcomes"
    for kind in ("model", "tool"):
        result[kind] = {
            key: round(sum(t[kind][key] for t in timings), 6)
            for key in ("calls", "errors", "duration_seconds_sum")
        }
    return result


# --- Bounded diagnostics integration ----------------------------------------
# The collector stays the only thing that reads the log for diagnostics. The
# runner never widens its allowlist, never enables raw artifact retention for it,
# and turns every missing source, overrun, unsupported format or collector fault
# into an explicit status instead of a complete-looking zero.


def normalize_diagnostics_constraints(values):
    """Validate and bound the explicit offline constraint allowlist.

    Reuses the collector's identifier rule and count bound so the runner cannot
    accept an id the collector would silently drop. A rejection reports the entry
    index only, never the offending value, so a secret-looking string is not
    copied into an error message.
    """
    if values is None or values == ():
        return ()
    if isinstance(values, (str, bytes)) or not isinstance(values, (list, tuple)):
        raise ValueError("must be a list of identifiers, not a single string")
    limits = eval_diagnostics.DEFAULT_LIMITS
    normalized = []
    seen = set()
    for index, value in enumerate(values):
        if not isinstance(value, str):
            raise ValueError(f"entry #{index} must be a string")
        candidate = value.strip()
        if (
            not candidate
            or len(candidate) > limits.max_field_chars
            or not eval_diagnostics.CONSTRAINT_ID_RE.match(candidate)
        ):
            raise ValueError(
                f"entry #{index} must be a bounded identifier of letters, digits, "
                "'_', '.', ':' or '-', not free text"
            )
        if candidate in seen:
            continue
        seen.add(candidate)
        normalized.append(candidate)
        if len(normalized) > eval_diagnostics.MAX_CONSTRAINTS:
            raise ValueError(
                f"at most {eval_diagnostics.MAX_CONSTRAINTS} identifiers are allowed"
            )
    return tuple(sorted(normalized))


def diagnostics_evidence_path(directory, task_id):
    """A per-task evidence path inside the diagnostics directory.

    The task id must be a plain file name: separators, traversal and control
    characters are refused so diagnostics can never write outside its directory.
    """
    if (
        not isinstance(task_id, str)
        or not task_id
        or task_id in (".", "..")
        or "/" in task_id
        or "\\" in task_id
        or any(ord(character) < 0x20 or ord(character) == 0x7F for character in task_id)
    ):
        raise ValueError("task id is not a safe diagnostics file name")
    return Path(directory) / f"{task_id}.json"


def diagnostics_status_block(status, reason=None):
    """An explicit diagnostics state with every evidence-derived count unknown."""
    return {
        "status": status,
        "reason": reason,
        "written": False,
        "source": {"opened": None, "unreadable": None, "events_scanned": None},
        "coverage": {key: None for key in DIAGNOSTICS_COVERAGE_KEYS},
        "truncation": {key: None for key in DIAGNOSTICS_TRUNCATION_KEYS},
    }


def diagnostics_evidence_block(evidence):
    """Summarize one collected evidence dict without copying paths or raw text."""
    source = evidence.get("source") if isinstance(evidence.get("source"), dict) else {}
    reads = evidence.get("reads") if isinstance(evidence.get("reads"), dict) else {}
    compaction = (
        evidence.get("compaction") if isinstance(evidence.get("compaction"), dict) else {}
    )
    constraint_coverage = (
        evidence.get("constraint_coverage")
        if isinstance(evidence.get("constraint_coverage"), dict) else {}
    )
    tool_activity = (
        evidence.get("tool_activity")
        if isinstance(evidence.get("tool_activity"), dict) else {}
    )
    tool_request = (
        tool_activity.get("request")
        if isinstance(tool_activity.get("request"), dict) else {}
    )
    truncation = (
        evidence.get("truncation") if isinstance(evidence.get("truncation"), dict) else {}
    )
    truncation_block = {key: truncation.get(key) for key in DIAGNOSTICS_TRUNCATION_KEYS}
    opened = bool(source.get("opened"))
    if not opened:
        status, reason = "unknown", source.get("unreadable") or "source_missing"
    elif any(truncation_block.get(key) for key in DIAGNOSTICS_TRUNCATION_KEYS):
        status, reason = "truncated", None
    elif source.get("events_scanned") == 0:
        status, reason = "unknown", "no_events"
    elif source.get("events_recognized") == 0:
        status, reason = "unknown", "no_supported_events"
    elif source.get("malformed", 0):
        status, reason = "unknown", "malformed_events"
    else:
        status, reason = "collected", None
    model_calls = (
        evidence.get("model_calls") if isinstance(evidence.get("model_calls"), dict) else {}
    )
    if status == "unknown":
        # No usable source or event stream: the counts are not evidence of zero.
        coverage = {key: None for key in DIAGNOSTICS_COVERAGE_KEYS}
    else:
        coverage = {
            "model_calls_completed": model_calls.get("completed"),
            "model_calls_prepared": model_calls.get("prepared"),
            "model_calls_dispatched": model_calls.get("dispatched"),
            "read_entries": reads.get("distinct"),
            "repeated_reads": reads.get("repeated_reads"),
            "range_unknown_reads": reads.get("range_unknown_entries"),
            "tool_request_events": tool_request.get("events"),
            "compaction_events": compaction.get("events"),
            "constraint_evidence_ids": constraint_coverage.get("evidence_ids"),
        }
    return {
        "status": status,
        "reason": reason,
        "written": True,
        "source": {
            "opened": opened,
            "unreadable": source.get("unreadable"),
            "events_scanned": source.get("events_scanned"),
        },
        "coverage": coverage,
        "truncation": truncation_block,
    }


def collect_task_diagnostics(target, log_path, workspace_root, constraints):
    """Collect one log into exclusive evidence; never overwrite or leak a fault."""
    try:
        evidence = eval_diagnostics.collect_and_write(
            log_path,
            workspace_root,
            target,
            allowed_constraints=constraints,
            create_parents=True,
        )
    except eval_diagnostics.EvidenceAlreadyExists:
        return diagnostics_status_block("error", "evidence_exists")
    except OSError:
        return diagnostics_status_block("error", "write_failed")
    except Exception:  # pragma: no cover - defensive; a collector bug must not mask the task
        # The collector's exception text, paths and any embedded secrets are dropped;
        # only the fixed category is recorded.
        return diagnostics_status_block("error", "collector_failed")
    return diagnostics_evidence_block(evidence)


def _prepare_task_diagnostics(diagnostics_dir, event_format, task_id):
    """Resolve diagnostics state before the agent starts.

    Modes: ``disabled`` (the flag is off), ``blocked`` (this format or task id
    cannot produce evidence), ``refused`` (evidence already exists, so the task is
    skipped before the agent can add a duplicate side effect) and ``ready`` with an
    exclusive target path.
    """
    if diagnostics_dir is None:
        return {"mode": "disabled"}
    if event_format != MIO_JSONL:
        return {
            "mode": "blocked",
            "status": "unsupported",
            "reason": DIAGNOSTICS_UNSUPPORTED_REASONS.get(
                event_format, DIAGNOSTICS_UNSUPPORTED_DEFAULT
            ),
        }
    try:
        private_directory(diagnostics_dir)
    except (OSError, ValueError):
        return {"mode": "blocked", "status": "error", "reason": "directory_invalid"}
    try:
        target = diagnostics_evidence_path(diagnostics_dir, task_id)
    except ValueError:
        return {"mode": "blocked", "status": "error", "reason": "invalid_task_id"}
    if os.path.lexists(target):
        return {"mode": "refused", "block": diagnostics_status_block("error", "evidence_exists")}
    return {"mode": "ready", "target": target}


def _task_diagnostics_result(prepared, constraints, log_path, workspace_root):
    """Per-task diagnostics block for the completed (or blocked) task."""
    if prepared["mode"] == "disabled":
        return None
    if prepared["mode"] == "blocked":
        return diagnostics_status_block(prepared["status"], prepared["reason"])
    try:
        normalized = normalize_diagnostics_constraints(constraints)
    except ValueError:
        return diagnostics_status_block("error", "invalid_constraints")
    return collect_task_diagnostics(
        prepared["target"], log_path, workspace_root, normalized
    )


def skipped_diagnostics_result(task, event_format, diagnostics_block):
    """A task intentionally not run because its evidence file already exists.

    The agent never starts, so the run cannot add a duplicate side effect or
    overwrite the earlier evidence. Task correctness and diagnostics stay separate:
    the task is recorded as an error, never as passed or failed.
    """
    failure_class = "diagnostics_evidence_exists"
    return {
        "task": task["id"],
        "status": "error",
        "failure_class": failure_class,
        "accepted": False,
        "duration_seconds": 0.0,
        "agent": {"exit_code": None, "timed_out": False, "seconds": 0.0,
                  "skipped": failure_class},
        "acceptance": {"exit_code": None, "timed_out": False, "seconds": 0.0,
                       "skipped": failure_class},
        "usage": unknown_usage(event_format),
        "timing": unknown_timing(),
        "diagnostic_categories": None,
        "artifacts": None,
        "diagnostics": diagnostics_block,
    }


def diagnostics_coverage_label(enabled, event_format, counts):
    """Top-level diagnostics coverage; never upgrades an unknown into complete."""
    if not enabled:
        return "disabled"
    if event_format != MIO_JSONL:
        return "unsupported"
    if counts.get("error"):
        return "error"
    if counts.get("unsupported"):
        return "unsupported"
    total = sum(counts.values())
    if total == 0:
        return "unknown"
    if counts.get("unknown"):
        return "partial" if counts.get("collected") or counts.get("truncated") else "unknown"
    if counts.get("truncated"):
        return "partial" if counts.get("collected") else "truncated"
    return "complete" if counts.get("collected") == total else "unknown"


def build_diagnostics_report(results, event_format, diagnostics_dir, constraints):
    """Report-level diagnostics state: enabled, limits, coverage, per-task statuses."""
    enabled = diagnostics_dir is not None
    counts = {status: 0 for status in DIAGNOSTICS_STATUSES}
    if enabled:
        for result in results:
            block = result.get("diagnostics") if isinstance(result, dict) else None
            if isinstance(block, dict) and block.get("status") in counts:
                counts[block["status"]] += 1
    if enabled:
        note = (
            "One sanitized evidence file per task was requested before the temporary "
            "logs were deleted. Only the collector's bounded output is stored: "
            "workspace-relative read paths with explicitly reported ranges, counts, "
            "fixed categories, fixed builtin tool-operation buckets "
            "(read/write/edit/grep/find/ls/shell/bash/workspace/other) counted "
            "from observed tool_started/tool_finished events, and "
            "caller-allowlisted constraint ids. Host paths, tool "
            "or model text, argv and credentials are never copied, and --diagnostics-dir "
            "never enables raw artifact retention. Missing sources, bound overruns, "
            "unsupported event formats and collector faults stay explicitly "
            "unknown/truncated/unsupported/error instead of being reported as complete; "
            "task correctness is recorded separately."
        )
    else:
        note = (
            "Diagnostics collection is off by default: no evidence file is written and "
            "the agent log is only read for the selected usage event format."
        )
    return {
        "enabled": enabled,
        "event_format": event_format,
        "supported": event_format == MIO_JSONL,
        "constraints": list(constraints),
        "limits": eval_diagnostics.DEFAULT_LIMITS.as_dict() if enabled else None,
        "coverage": diagnostics_coverage_label(enabled, event_format, counts),
        "status_counts": counts,
        "requires_instrumentation": list(eval_diagnostics.REQUIRES_INSTRUMENTATION),
        "note": note,
    }


# --- Opt-in sanitized acceptance evidence -----------------------------------
# A failing acceptance script normally leaves only its exit code: the temporary
# check.log is deleted with the task directory. When --diagnostics-dir is set the
# runner also reads that log once, before cleanup, and records a bounded sanitized
# summary of the reported failure location. It never copies traceback text,
# assertion messages, stdout/stderr, paths, arguments, call ids or credentials.
#
# The reported frame line numbers are UNTRUSTED merged check-log observations. Anything able
# to write to the acceptance stdout/stderr can forge them, so they locate a reported
# failure but prove neither which check executed nor why it failed. A successful
# acceptance run is recorded as not_applicable: the absence of a traceback is not
# evidence that every check passed.

ACCEPTANCE_EVIDENCE_KIND = "mio-eval-acceptance-evidence"
ACCEPTANCE_EVIDENCE_VERSION = 1
MAX_ACCEPTANCE_LOG_BYTES = 256 * 1024
MAX_ACCEPTANCE_SOURCE_BYTES = 8 * 1024 * 1024
MAX_ACCEPTANCE_FRAMES = 16
MAX_ACCEPTANCE_EVIDENCE_BYTES = 4096

# Fixed exception-kind labels; a concrete type outside this allowlist collapses to
# "other" and a missing or unrecognized type stays "unknown", so no arbitrary name
# is ever stored.
ACCEPTANCE_EXCEPTION_KINDS = (
    "ArithmeticError", "AssertionError", "AttributeError", "EOFError", "ImportError",
    "IndexError", "KeyError", "KeyboardInterrupt", "LookupError", "MemoryError",
    "NameError", "NotImplementedError", "OSError", "RecursionError", "RuntimeError",
    "StopIteration", "SystemExit", "TypeError", "UnboundLocalError", "ValueError",
    "ZeroDivisionError",
)
ACCEPTANCE_EXCEPTION_OTHER = "other"
ACCEPTANCE_EXCEPTION_UNKNOWN = "unknown"

# Python's standard traceback shape: a header, `File "<path>", line N` frames with
# an indented source line each, and a final column-zero exception line. Only the
# frame path, the frame line number and the exception type token are ever read.
ACC_TRACEBACK_HEADER = re.compile(r"^Traceback \(most recent call last\):\s*$")
ACC_TRACEBACK_FRAME = re.compile(
    r'^\s*File "(?P<path>.+)", line (?P<line>\d+)(?:, in (?P<name>.+))?\s*$'
)
ACC_EXCEPTION_LINE = re.compile(r"^(?P<kind>[A-Za-z_][A-Za-z0-9_.]*)(?::.*)?$")


def _read_bounded_regular(path, limit):
    """Read up to `limit` bytes of a regular, non-symlink file.

    Returns ``(data, size, truncated, reason)``. `reason` is a fixed label
    (``missing`` / ``unreadable`` / ``not_regular``) when the file cannot be read as
    a bounded regular file, otherwise None. The underlying error text is never
    returned, so a hostile path cannot be quoted back into the report.
    """
    try:
        # Inspect before opening: opening a FIFO can block before fstat runs.
        # Nonblocking open also covers replacement between this check and open.
        if not stat.S_ISREG(os.lstat(path).st_mode):
            return b"", None, False, "not_regular"
        descriptor = os.open(
            path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        )
    except FileNotFoundError:
        return b"", None, False, "missing"
    except OSError:
        return b"", None, False, "unreadable"
    try:
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode):
            return b"", None, False, "not_regular"
        size = info.st_size
        remaining = min(size, limit)
        chunks = []
        while remaining > 0:
            chunk = os.read(descriptor, min(65536, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        return b"".join(chunks), size, size > limit, None
    except OSError:
        return b"", None, False, "unreadable"
    finally:
        os.close(descriptor)


def _acceptance_source_info(path):
    """SHA-256 and line count of the trusted acceptance source, bounded."""
    data, _size, truncated, reason = _read_bounded_regular(path, MAX_ACCEPTANCE_SOURCE_BYTES)
    if reason is not None:
        return {"readable": False, "reason": f"source_{reason}", "sha256": None,
                "lines": None, "truncated": False}
    if truncated:
        return {"readable": True, "reason": "source_oversized", "sha256": None,
                "lines": None, "truncated": True}
    digest = hashlib.sha256(data).hexdigest()
    lines = data.count(b"\n") + (1 if data and not data.endswith(b"\n") else 0)
    return {"readable": True, "reason": None, "sha256": digest, "lines": lines,
            "truncated": False}


def _split_traceback_blocks(lines):
    """Split log lines into standard Python traceback blocks."""
    blocks = []
    current = None
    for raw in lines:
        if ACC_TRACEBACK_HEADER.match(raw.rstrip("\r")):
            current = []
            blocks.append(current)
        elif current is not None:
            current.append(raw)
    return blocks


def _parse_traceback_block(lines):
    """Return ``([(frame_path, line_number)], exception_label)`` for one block.

    The block ends at the first column-zero non-empty line, which carries the
    exception type. Only the type token is read and only as a fixed label; the
    message is dropped. A block without a recognizable exception line returns
    ``None`` as its label so the caller can mark the traceback malformed.
    """
    frames = []
    for raw in lines:
        line = raw.rstrip("\r")
        match = ACC_TRACEBACK_FRAME.match(line)
        if match:
            number = match.group("line")
            # Bound numeric parsing work while preserving ordinary out-of-range
            # frame counts; oversized tokens must not make metadata collection fail.
            if len(number) > 32:
                return frames, None
            frames.append((match.group("path"), int(number)))
            continue
        if not line.strip() or line[:1].isspace():
            continue
        token_match = ACC_EXCEPTION_LINE.match(line)
        if not token_match:
            return frames, None
        token = token_match.group("kind").rsplit(".", 1)[-1]
        return frames, (
            token if token in ACCEPTANCE_EXCEPTION_KINDS else ACCEPTANCE_EXCEPTION_OTHER
        )
    return frames, None


def _acceptance_evidence_within_limit(block):
    """Keep the evidence block under the fixed serialized byte cap.

    The skeleton keeps every fixed schema key and only drops the reported frame
    list, so an over-cap block is explicitly truncated rather than silently huge.
    """
    encoded = json.dumps(block, separators=(",", ":")).encode("utf-8")
    if len(encoded) <= MAX_ACCEPTANCE_EVIDENCE_BYTES:
        return block
    return {
        "kind": block["kind"],
        "version": block["version"],
        "status": "truncated",
        "reason": "output_truncated",
        "trust": block["trust"],
        "source": block["source"],
        "log": block["log"],
        "exception_kind": block["exception_kind"],
        "frames": {"lines": [], "count": 0, "truncated": True,
                   "out_of_range": block["frames"]["out_of_range"]},
        "note": block["note"],
    }


def collect_acceptance_evidence(acceptance, acceptance_path, log_path):
    """Bounded, sanitized summary of one acceptance run's temporary check log.

    Runs after acceptance finished and before the temporary directory is removed.
    The result contains fixed schema keys only: the trusted acceptance source's
    SHA-256 and line count, bounded reported frame line numbers that exactly match
    the acceptance script, an allowlisted exception-kind label, and explicit
    unknown/truncated flags. A successful acceptance run is `not_applicable`; the
    absence of a traceback is never reported as proof that all checks passed.
    """
    source_info = _acceptance_source_info(acceptance_path)
    source = {
        "readable": source_info["readable"],
        "reason": source_info["reason"],
        "sha256": source_info["sha256"],
        "lines": source_info["lines"],
        "truncated": source_info["truncated"],
    }
    empty_log = {"opened": False, "bytes_read": 0, "truncated": False,
                 "format": "not_read"}
    empty_frames = {"lines": [], "count": 0, "truncated": False, "out_of_range": 0}
    note = (
        "Bounded summary of the temporary acceptance check log, read once before "
        "it is deleted. Reported frame lines come from merged stdout/stderr and can be forged by "
        "the failing run: they locate a reported failure but prove neither which "
        "check executed nor why it failed. No source text, assertion message, "
        "stdout/stderr body, path, argument, call id or credential is retained."
    )

    def block(status, reason, log=empty_log, kind=ACCEPTANCE_EXCEPTION_UNKNOWN,
              frames=empty_frames):
        return _acceptance_evidence_within_limit({
            "kind": ACCEPTANCE_EVIDENCE_KIND,
            "version": ACCEPTANCE_EVIDENCE_VERSION,
            "status": status,
            "reason": reason,
            "trust": "untrusted_reported_check_log",
            "source": source,
            "log": log,
            "exception_kind": kind,
            "frames": frames,
            "note": note,
        })

    if acceptance.get("error"):
        return block("unknown", "acceptance_error")
    if acceptance.get("timed_out"):
        return block("unknown", "acceptance_timeout")
    if acceptance.get("exit_code") == 0:
        return block("not_applicable", "acceptance_passed")
    if source_info["reason"] is not None or not source_info["readable"]:
        return block("unknown", source_info["reason"] or "source_unreadable")

    data, _size, truncated, reason = _read_bounded_regular(log_path, MAX_ACCEPTANCE_LOG_BYTES)
    if reason is not None:
        return block("unknown", f"log_{reason}")
    log = {"opened": True, "bytes_read": len(data), "truncated": truncated,
           "format": "unknown"}
    blocks = _split_traceback_blocks(data.decode("utf-8", "replace").splitlines())
    if not blocks:
        return block("unknown" if truncated else "unsupported",
                     "log_truncated" if truncated else "no_python_traceback", log)

    expected = str(acceptance_path)
    valid_lines = []
    out_of_range = 0
    final_kind = None
    for traceback_lines in blocks:
        frames, label = _parse_traceback_block(traceback_lines)
        if label is not None:
            final_kind = label
        for frame_path, line_number in frames:
            if frame_path != expected:
                continue
            if 1 <= line_number <= source_info["lines"]:
                valid_lines.append(line_number)
            else:
                out_of_range += 1
    log["format"] = "python_traceback"
    if final_kind is None:
        return block("unknown", "malformed_traceback", log)
    distinct = sorted(set(valid_lines))
    frames_truncated = len(distinct) > MAX_ACCEPTANCE_FRAMES
    if frames_truncated:
        distinct = distinct[:MAX_ACCEPTANCE_FRAMES]
    frames = {"lines": distinct, "count": len(distinct),
              "truncated": frames_truncated, "out_of_range": out_of_range}
    return block("truncated" if (truncated or frames_truncated) else "collected",
                 None, log, final_kind, frames)


def acceptance_argv(acceptance, work):
    """Build an isolated interpreter invocation for a task's acceptance script."""
    shim = (
        "import runpy, sys\n"
        "work, script = sys.argv[1], sys.argv[2]\n"
        "sys.path.insert(0, work)\n"
        "sys.argv = [script]\n"
        "runpy.run_path(script, run_name='__main__')\n"
    )
    return [sys.executable, "-I", "-c", shim, str(work), str(acceptance)]


def private_directory(path):
    """Create a directory and make it owner-only, so retained data stays private."""
    path = Path(path)
    path.mkdir(parents=True, exist_ok=True)
    if path.is_symlink() or not path.is_dir():
        raise ValueError(f"private directory must be a real directory: {path}")
    if os.name == "posix":
        path.chmod(0o700)
    return path


def private_file(path):
    """Restrict one written file to its owner."""
    if os.name == "posix":
        Path(path).chmod(0o600)
    return path


def build_agent_env(task_root, base_env=None):
    """Build the child environment for --isolate-agent-env.

    The returned mapping is a copy of the ambient environment with every `MIO_*`
    and `PI_*` variable removed (except `ISOLATED_ENV_KEEP`), so an inherited
    model/provider override or Pi session marker cannot change what the measured run
    does. Pi's documented agent home and session directory are then pointed at fresh
    empty directories under the task's private root, which keeps the run away from
    the user's `~/.pi/agent` (auth.json, models.json, settings.json) and real
    sessions.

    HOME, PATH, proxy settings and provider credentials such as `DEEPSEEK_API_KEY`
    are inherited unchanged: credentials are meant to come from the environment,
    and the official Pi wrapper resolves `~/.local/bin/pi-run` through HOME.
    `PI_DELEGATE_ACTIVE` is also preserved on purpose so Pi's recursion guard still
    refuses a runner that was itself started inside a delegated session.
    """
    env = dict(os.environ if base_env is None else base_env)
    for name in list(env):
        if name.startswith(ISOLATED_ENV_PREFIXES) and name not in ISOLATED_ENV_KEEP:
            del env[name]
    home = private_directory(Path(task_root) / "agent-home")
    env[PI_AGENT_DIR_ENV] = str(private_directory(home / "pi-agent"))
    env[PI_SESSION_DIR_ENV] = str(private_directory(home / "pi-sessions"))
    return env


def _copy_bounded(source, destination, limit):
    """Copy at most `limit` bytes; return `(bytes_written, truncated)`."""
    written = 0
    truncated = False
    source = Path(source)
    if source.is_symlink():
        raise ValueError("artifact source must not be a symlink")
    read_fd = os.open(source, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
                      | getattr(os, "O_NONBLOCK", 0))
    with os.fdopen(read_fd, "rb") as reader:
        if not stat.S_ISREG(os.fstat(reader.fileno()).st_mode):
            raise ValueError("artifact source must be a regular file")
        write_fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL
                           | getattr(os, "O_NOFOLLOW", 0), 0o600)
        with os.fdopen(write_fd, "wb") as writer:
            while written < limit:
                chunk = reader.read(min(65536, limit - written))
                if not chunk:
                    break
                writer.write(chunk)
                written += len(chunk)
            if reader.read(1):
                truncated = True
    return written, truncated


def retain_task_artifacts(task_root, artifacts_dir, task_id):
    """Copy one task's workspace and logs into an opt-in artifact directory.

    Everything is written owner-only (0700 directories, 0600 files), symlinks are
    never followed so a workspace cannot exfiltrate a path outside itself, and every
    copied file is cut at a fixed byte limit. The returned block records paths, byte
    counts and truncation flags only; it never contains file contents, so the report
    stays free of raw agent output and credentials even though the retained logs
    themselves may contain them.
    """
    task_root = Path(task_root)
    parent = private_directory(artifacts_dir)
    destination = parent / task_id
    # Never merge a new run into old evidence or follow a pre-planted target.
    destination.mkdir(mode=0o700, exist_ok=False)
    workspace_dir = private_directory(destination / "workspace")
    work = task_root / "workspace"
    workspace_files = []
    workspace_bytes = 0
    workspace_truncated = False
    if work.is_dir():
        for scanned, path in enumerate(work.rglob("*")):
            if (scanned >= MAX_ARTIFACT_SCAN_ENTRIES
                    or len(workspace_files) >= MAX_ARTIFACT_FILES
                    or workspace_bytes >= MAX_ARTIFACT_WORKSPACE_BYTES):
                workspace_truncated = True
                break
            if path.is_symlink() or not path.is_file():
                continue
            relative = path.relative_to(work)
            target = workspace_dir / relative
            private_directory(target.parent)
            written, truncated = _copy_bounded(
                path, target,
                min(MAX_ARTIFACT_FILE_BYTES, MAX_ARTIFACT_WORKSPACE_BYTES - workspace_bytes),
            )
            workspace_bytes += written
            workspace_truncated |= truncated
            workspace_files.append({
                "path": relative.as_posix(), "bytes": written, "truncated": truncated,
            })
    logs = {}
    for name in ("agent.log", "check.log"):
        source = task_root / name
        if not source.is_file():
            continue
        target = destination / name
        written, truncated = _copy_bounded(source, target, MAX_ARTIFACT_LOG_BYTES)
        logs[name] = {"path": str(target), "bytes": written, "truncated": truncated}
    return {
        "directory": str(destination),
        "workspace": str(workspace_dir),
        "workspace_files": workspace_files,
        "workspace_bytes": workspace_bytes,
        "workspace_truncated": workspace_truncated,
        "logs": logs,
        "limits": {
            "max_log_bytes": MAX_ARTIFACT_LOG_BYTES,
            "max_workspace_file_bytes": MAX_ARTIFACT_FILE_BYTES,
            "max_workspace_bytes": MAX_ARTIFACT_WORKSPACE_BYTES,
            "max_workspace_files": MAX_ARTIFACT_FILES,
            "max_scan_entries": MAX_ARTIFACT_SCAN_ENTRIES,
        },
        "note": (
            "Raw workspace and agent/check logs, written with owner-only permissions. "
            "They may contain model output, file contents or secrets; the report itself "
            "records only these paths, sizes and truncation flags."
        ),
    }


def run_process(argv, cwd, timeout, log, env=None):
    """Bound wall time, reap the process group, and keep logs off the Python heap."""
    started = time.monotonic()
    with log.open("wb") as output:
        process = subprocess.Popen(
            argv, cwd=cwd, stdin=subprocess.DEVNULL, stdout=output,
            stderr=subprocess.STDOUT, start_new_session=(os.name == "posix"),
            env=env,
        )
        timed_out = False
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
        finally:
            # Also clean up descendants after a successful parent exit.
            if os.name == "posix":
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            elif process.poll() is None:
                process.kill()
            process.wait()
    return {
        "exit_code": process.returncode, "timed_out": timed_out,
        "seconds": round(time.monotonic() - started, 3),
    }


def classify(agent, acceptance):
    """Map process outcomes onto a stable status/failure_class pair."""
    if agent.get("error"):
        return "error", "environment_error"
    if agent["timed_out"]:
        return "timeout", "agent_timeout"
    if agent["exit_code"] != 0:
        return "failed", "agent_exit_nonzero"
    if acceptance.get("error"):
        return "error", "environment_error"
    if acceptance["timed_out"]:
        return "failed", "acceptance_timeout"
    if acceptance["exit_code"] != 0:
        return "failed", "acceptance_failed"
    return "passed", None


def evaluate(task, template, timeout, root, event_format=None, *, isolate_agent_env=False,
             artifacts_dir=None, diagnostics_dir=None, diagnostics_constraints=()):
    """Run one task and classify it.

    `isolate_agent_env`, `artifacts_dir` and `diagnostics_dir` are opt-in: with their
    defaults the child inherits this process's environment and the task directory is
    deleted with the caller's temporary root, exactly as before these options existed.
    `diagnostics_dir` only writes the sanitized collector output before the temporary
    logs are deleted; it never enables artifact retention, and a pre-existing evidence
    file skips the task before the agent starts so no duplicate side effect happens.
    """
    prepared = _prepare_task_diagnostics(diagnostics_dir, event_format, task["id"])
    if prepared["mode"] == "refused":
        return skipped_diagnostics_result(task, event_format, prepared["block"])
    task_root = private_directory(Path(root) / task["id"]) if isolate_agent_env \
        else (Path(root) / task["id"])
    work = task_root / "workspace"
    work.mkdir(parents=True)
    for name, content in task["files"].items():
        target = work / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)
    argv = [
        part.replace("{repo}", str(REPO_ROOT))
        .replace("{cwd}", str(work))
        .replace("{prompt}", task["prompt"])
        for part in template
    ]
    env = build_agent_env(task_root) if isolate_agent_env else None
    try:
        agent = run_process(argv, work, timeout, task_root / "agent.log", env=env)
    except OSError as error:
        agent = {"exit_code": None, "timed_out": False, "seconds": 0.0, "error": str(error)}
    # Even a failing agent may have changed files, so record validation separately.
    try:
        acceptance = run_process(
            acceptance_argv(task["acceptance"], work), work,
            task["acceptance_timeout_seconds"], task_root / "check.log", env=env,
        )
    except OSError as error:
        acceptance = {"exit_code": None, "timed_out": False, "seconds": 0.0, "error": str(error)}

    status, failure_class = classify(agent, acceptance)
    return {
        "task": task["id"],
        "status": status,
        "failure_class": failure_class,
        "accepted": status == "passed",
        "duration_seconds": round(agent["seconds"] + acceptance["seconds"], 3),
        "agent": agent,
        "acceptance": acceptance,
        "usage": collect_usage(event_format, task_root / "agent.log"),
        "timing": collect_timing(event_format, task_root / "agent.log"),
        "diagnostic_categories": collect_diagnostic_categories(event_format, task_root / "agent.log"),
        "artifacts": (
            retain_task_artifacts(task_root, artifacts_dir, task["id"])
            if artifacts_dir is not None else None
        ),
        # Collected after the agent ran and before the temporary log is deleted.
        "diagnostics": _task_diagnostics_result(
            prepared, diagnostics_constraints, task_root / "agent.log", work
        ),
        # Also collected before cleanup, and only when diagnostics are opted in.
        **(
            {
                "acceptance_evidence": collect_acceptance_evidence(
                    acceptance, task["acceptance"], task_root / "check.log"
                )
            }
            if diagnostics_dir is not None
            else {}
        ),
    }


def build_report(label, tasks, results, budget, agent_command, started_at, duration_seconds,
                 event_format=None, timeout_seconds=None, isolate_agent_env=False,
                 artifacts_dir=None, diagnostics_dir=None, diagnostics_constraints=()):
    sets = {(task["task_set"], task["task_set_version"]) for task in tasks}
    task_set, task_set_version = next(iter(sets))
    usage_note = {
        MIO_JSONL: (
            "summed from mio completion_call events; each counter stays null unless "
            "every observed call reported it, and cost is never inferred"
        ),
        PI_JSONL: (
            "summed from pi message_end assistant events; pi's uncached input is remapped "
            "to the inclusive prompt total only when input, cacheRead and cacheWrite are "
            "all reported, each counter stays null unless every observed call reported it, "
            "and pi's catalog-priced cost is never treated as payment. Pi emits numeric "
            "defaults for omitted upstream counters, so its explicit zeros cannot always "
            "be distinguished from unreported usage; coverage describes agent events, not billing certainty"
        ),
    }.get(event_format, "not collected by this probe")
    return {
        "schema_version": REPORT_SCHEMA_VERSION,
        "report_version": REPORT_VERSION,
        "report_type": REPORT_TYPE,
        "label": label,
        "generated_at": datetime.datetime.now(datetime.timezone.utc)
        .isoformat(timespec="seconds")
        .replace("+00:00", "Z"),
        "started_at": started_at,
        "duration_seconds": duration_seconds,
        "task_set": {
            "id": task_set,
            "version": task_set_version,
            "tasks": [task["id"] for task in tasks],
        },
        "budget": budget,
        "enforced": {
            "wall_timeout_seconds": timeout_seconds,
            "budget_enforced_by_runner": False,
            "note": (
                "The runner enforces only the per-task wall-clock timeout and the "
                "process-group reaping that follows it. The budget block is declared "
                "metadata: it is recorded for comparability but is not enforced or "
                "measured by the runner, so the report never claims a token or model-call "
                "hard limit."
            ),
        },
        "agent_command": redact_command(list(agent_command)),
        "event_format": event_format,
        "isolation": {
            "agent_env_isolated": bool(isolate_agent_env),
            "cleared_env_prefixes": list(ISOLATED_ENV_PREFIXES) if isolate_agent_env else [],
            "kept_env_vars": list(ISOLATED_ENV_KEEP) if isolate_agent_env else [],
            "pi_agent_home": "temporary private directory per task" if isolate_agent_env else None,
            "note": (
                "When enabled, the child agent's MIO_* and PI_* variables are removed "
                "(except PI_DELEGATE_ACTIVE, which Pi's delegation wrapper must still "
                "see so it refuses a nested run) and Pi's agent home and session "
                "directory point at a fresh empty directory under the task's private "
                "root. Provider credentials and proxy settings that use other "
                "environment variables are inherited unchanged, and HOME is left "
                "alone so the official Pi wrapper still resolves its ~/.local/bin/pi-run "
                "helper."
            ) if isolate_agent_env else (
                "The child agent inherits this runner's environment, including any "
                "ambient model, provider or session override. Enable isolation with "
                "--isolate-agent-env for a comparison run."
            ),
        },
        "artifacts": {
            "enabled": artifacts_dir is not None,
            "root": str(artifacts_dir) if artifacts_dir is not None else None,
            "note": (
                "Raw per-task workspaces and agent/check logs are retained in the "
                "root above with owner-only permissions. Each result carries its own "
                "directory, byte counts and truncation flags; the report never copies "
                "file contents or credentials."
            ) if artifacts_dir is not None else (
                "Task workspaces and logs stay in a temporary directory and are "
                "deleted when the run ends. Failing samples are therefore not "
                "retained unless --artifacts-dir is set."
            ),
        },
        "diagnostics": build_diagnostics_report(
            results, event_format, diagnostics_dir, diagnostics_constraints
        ),
        "timing": sum_task_timing([result.get("timing", unknown_timing()) for result in results]),
        "accepted": sum(result["accepted"] for result in results),
        "total": len(results),
        "results": results,
        "usage": sum_task_usage([result["usage"] for result in results], note=usage_note),
        "unavailable_metrics": unavailable_metrics(event_format),
        "limits": LIMITS,
    }


def build_parser():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--config", type=Path, help="runner config JSON; CLI flags override it")
    parser.add_argument("--agent-command", help="argv template containing {prompt}")
    parser.add_argument("--label", help="report label for the agent under test")
    parser.add_argument("--timeout", dest="timeout_seconds", type=float,
                        help="per-task agent wall-clock limit in seconds")
    parser.add_argument("--task", dest="tasks", action="append",
                        help="run only this task id; repeatable")
    parser.add_argument("--tasks-dir", type=Path, help="task directory root")
    parser.add_argument("--event-format", dest="event_format", choices=EVENT_FORMATS,
                        help="parse agent stdout events for real usage; supported: "
                             + ", ".join(EVENT_FORMATS))
    parser.add_argument("--isolate-agent-env", dest="isolate_agent_env",
                        action="store_true", default=None,
                        help="clear the child's MIO_*/PI_* variables and give Pi an "
                             "empty temporary agent home and session directory")
    parser.add_argument("--artifacts-dir", dest="artifacts_dir", type=Path,
                        help="keep each task's workspace and agent/check logs under "
                             "this owner-only directory instead of deleting them")
    parser.add_argument("--diagnostics-dir", dest="diagnostics_dir", type=Path,
                        help="write one sanitized, bounded diagnostics evidence file per "
                             "task under this owner-only directory; requires "
                             "--event-format mio-jsonl and never enables --artifacts-dir")
    parser.add_argument("--diagnostics-constraint", dest="diagnostics_constraints",
                        action="append", metavar="ID",
                        help="offline constraint id to track in diagnostics coverage; "
                             "repeatable, requires --diagnostics-dir")
    parser.add_argument("--output", type=Path, help="report path, e.g. target/evals/mio.json")
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)

    config = {}
    if args.config is not None:
        try:
            config = load_config(args.config)
        except (OSError, ValueError) as error:
            parser.error(f"cannot load --config: {error}")

    settings = resolve_settings(config, args)
    if settings["output"] is None:
        parser.error("--output is required (or set 'output' in --config)")
    if settings["timeout_seconds"] <= 0:
        parser.error("--timeout must be positive")

    try:
        diagnostics_constraints = normalize_diagnostics_constraints(
            settings["diagnostics_constraints"]
        )
    except ValueError as error:
        parser.error(f"invalid diagnostics constraints: {error}")
    settings["diagnostics_constraints"] = diagnostics_constraints
    diagnostics_dir = settings["diagnostics_dir"]
    if diagnostics_constraints and diagnostics_dir is None:
        parser.error("--diagnostics-constraint requires --diagnostics-dir")
    if diagnostics_dir is not None and settings["event_format"] == MIO_JSONL:
        try:
            private_directory(diagnostics_dir)
        except (OSError, ValueError) as error:
            parser.error(f"cannot prepare --diagnostics-dir: {error}")

    template = shlex.split(settings["agent_command"])
    if not template or not any("{prompt}" in part for part in template):
        parser.error("--agent-command must contain {prompt}")
    if "/" in template[0]:
        template[0] = str(Path(template[0]).resolve())

    try:
        tasks = load_tasks(settings["tasks_dir"])
    except (OSError, ValueError) as error:
        parser.error(str(error))
    selected = settings["tasks"]
    if selected:
        known = {task["id"] for task in tasks}
        missing = [task_id for task_id in selected if task_id not in known]
        if missing:
            parser.error(f"unknown task id(s): {', '.join(missing)}")
        tasks = [task for task in tasks if task["id"] in selected]

    budget = resolve_budget(config)
    try:
        verify_declared_sampling(budget, template)
    except ValueError as error:
        parser.error(f"declared sampling is not applied: {error}")
    started_at = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds").replace(
        "+00:00", "Z"
    )
    started = time.monotonic()
    artifacts_dir = settings["artifacts_dir"]
    if artifacts_dir is not None:
        private_directory(artifacts_dir)
    results = []
    with tempfile.TemporaryDirectory(prefix="mio-eval-") as temporary:
        root = Path(temporary)
        for task in tasks:
            result = evaluate(
                task, template, settings["timeout_seconds"], root,
                settings["event_format"],
                isolate_agent_env=settings["isolate_agent_env"],
                artifacts_dir=artifacts_dir,
                diagnostics_dir=diagnostics_dir,
                diagnostics_constraints=diagnostics_constraints,
            )
            results.append(result)
            print(json.dumps(result), flush=True)
    report = build_report(
        settings["label"], tasks, results, budget, template,
        started_at, round(time.monotonic() - started, 3), settings["event_format"],
        settings["timeout_seconds"], settings["isolate_agent_env"], artifacts_dir,
        diagnostics_dir=diagnostics_dir,
        diagnostics_constraints=diagnostics_constraints,
    )
    settings["output"].parent.mkdir(parents=True, exist_ok=True)
    settings["output"].write_text(json.dumps(report, indent=2) + "\n")
    return 0 if report["accepted"] == report["total"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
