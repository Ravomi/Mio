"""Offline tests for the mio-workflows-v1 eval tasks; no model is called.

Reference solutions live in this file, never in the task workspaces, so every
workspace under `initial/` stays the unsolved fixture. Each test drives the real
runner (`scripts/agent_eval.py`) through its loading/evaluation functions with a
local "agent" that either writes a reference solution or a deliberately wrong one.
"""
import importlib.util
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "agent_eval.py"
spec = importlib.util.spec_from_file_location("agent_eval", SCRIPT)
evaluation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(evaluation)

TASKS_DIR = REPO_ROOT / "evals" / "tasks" / "workflows"
EXPECTED_TASK_IDS = ["migrate-line-item-api", "resume-cache-build", "retrieve-wire-spec"]


# ---------------------------------------------------------------------------
# Reference solutions (kept here, never in the task workspace).
# ---------------------------------------------------------------------------

TASK1_CORE = r'''"""Line-item totals for customer invoices."""


def _half_up(numerator, denominator):
    return (numerator + denominator // 2) // denominator


def compute_total(items, *, tax_rate_bps=0):
    if not 0 <= tax_rate_bps <= 10000:
        raise ValueError("tax_rate_bps out of range")
    subtotal = 0
    for item in items:
        quantity = item["quantity"]
        if quantity < 0:
            raise ValueError("quantity must be non-negative")
        subtotal += item["unit_cents"] * quantity
    return subtotal + _half_up(subtotal * tax_rate_bps, 10000)
'''

TASK1_REPORT = r'''from .core import compute_total

# Tax applied by the monthly roll-up, in basis points (800 == 8%).
DEFAULT_TAX_RATE_BPS = 800


def monthly_total(items):
    """Subtotal plus the default tax for a month's line items."""
    return compute_total(items, tax_rate_bps=DEFAULT_TAX_RATE_BPS)
'''

TASK1_FILES = {
    "invoicing/core.py": TASK1_CORE,
    "invoicing/report.py": TASK1_REPORT,
}

TASK2_FIXED_CACHE = r'''"""On-disk state for the fixture build pipeline."""
import json
import os


def load_state(path):
    """Return the persisted build state, or a fresh one when none exists yet."""
    try:
        with open(path, "r", encoding="utf-8") as handle:
            return json.load(handle)
    except FileNotFoundError:
        return {"done": []}


def save_state(path, state):
    """Persist the build state atomically."""
    directory = os.path.dirname(path)
    if directory:
        os.makedirs(directory, exist_ok=True)
    temporary = path + ".tmp"
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(state, handle, sort_keys=True)
    os.replace(temporary, path)
'''

TASK3_WIRE = r'''"""Wire record parser used by the ingest tools."""

_SEQ_DIGITS = 4


def _unescape(value):
    out = []
    index = 0
    while index < len(value):
        char = value[index]
        if char != "\\":
            out.append(char)
            index += 1
            continue
        index += 1
        if index >= len(value):
            raise ValueError("dangling escape in value")
        escaped = value[index]
        if escaped not in ("\\", "|"):
            raise ValueError("unknown escape: \\" + escaped)
        out.append(escaped)
        index += 1
    return "".join(out)


def _escape(value):
    return value.replace("\\", "\\\\").replace("|", "\\|")


def _split_pairs(line):
    pairs = []
    current = []
    index = 0
    while index < len(line):
        char = line[index]
        if char == "\\" and index + 1 < len(line):
            current.append(char)
            current.append(line[index + 1])
            index += 2
            continue
        if char == "|":
            pairs.append("".join(current))
            current = []
            index += 1
            continue
        current.append(char)
        index += 1
    pairs.append("".join(current))
    return pairs


def parse(text):
    records = []
    for line in text.splitlines():
        if not line.strip():
            continue
        record = {}
        for pair in _split_pairs(line):
            key, separator, raw = pair.partition("=")
            if not separator:
                raise ValueError("pair without '='")
            key = key.lower()
            if not key:
                raise ValueError("empty key")
            if key in record:
                raise ValueError("duplicate key: " + key)
            value = _unescape(raw)
            if key == "seq" and (len(value) != _SEQ_DIGITS or not value.isdigit()):
                raise ValueError("seq must be four decimal digits")
            record[key] = value
        records.append(record)
    return records


def render(records):
    lines = []
    for record in records:
        pairs = ["{}={}".format(key, _escape(value)) for key, value in record.items()]
        lines.append("|".join(pairs))
    return "\n".join(lines) + "\n" if lines else ""
'''


# ---------------------------------------------------------------------------
# Deliberately wrong solutions: each must fail acceptance.
# ---------------------------------------------------------------------------

WRONG_T1_KEEPS_DEPRECATED = r'''"""Line-item totals for customer invoices."""


def compute(items):
    subtotal = 0
    for item in items:
        subtotal += item["unit_cents"] * item["quantity"]
    return subtotal


def compute_total(items, tax_rate_bps=0):
    subtotal = compute(items)
    return subtotal + int(subtotal * tax_rate_bps / 10000)
'''

WRONG_T3_NO_ESCAPE = r'''"""Wire record parser used by the ingest tools."""


def parse(text):
    records = []
    for line in text.splitlines():
        if not line.strip():
            continue
        record = {}
        for pair in line.split("|"):
            key, separator, value = pair.partition("=")
            if not separator:
                raise ValueError("pair without '='")
            key = key.lower()
            if not key:
                raise ValueError("empty key")
            if key in record:
                raise ValueError("duplicate key")
            if key == "seq" and (len(value) != 4 or not value.isdigit()):
                raise ValueError("seq must be four digits")
            record[key] = value
        records.append(record)
    return records


def render(records):
    lines = []
    for record in records:
        lines.append("|".join("{}={}".format(key, value) for key, value in record.items()))
    return "\n".join(lines) + "\n" if lines else ""
'''


def writer_solver(files):
    """A tiny agent that writes the given {relative path: text} and exits."""
    lines = ["from pathlib import Path"]
    for name, content in files.items():
        lines.append("Path({!r}).write_text({!r})".format(name, content))
    return "\n".join(lines) + "\n"


def recovery_solver(fixed_cache):
    """A reference agent for the recovery task: run, fix, run again."""
    return (
        "import subprocess\n"
        "import sys\n"
        "from pathlib import Path\n"
        "first = subprocess.run([sys.executable, 'fixture_build.py', 'build'], "
        "capture_output=True, text=True)\n"
        "assert first.returncode != 0, 'the fixture was expected to fail before the fix'\n"
        "Path('buildlib/cache.py').write_text(" + repr(fixed_cache) + ")\n"
        "second = subprocess.run([sys.executable, 'fixture_build.py', 'build'], "
        "capture_output=True, text=True)\n"
        "assert second.returncode == 0, second.stderr\n"
    )


class WorkflowTaskCase(unittest.TestCase):
    def evaluate_task(self, task_id, agent_source, timeout=30):
        tasks = {task["id"]: task for task in evaluation.load_tasks(TASKS_DIR)}
        task = tasks[task_id]
        root = Path(tempfile.mkdtemp(prefix="mio-workflows-test-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        solver = root / "solver.py"
        solver.write_text(agent_source, encoding="utf-8")
        result = evaluation.evaluate(task, [sys.executable, str(solver), "{prompt}"], timeout, root)
        check_log = root / task_id / "check.log"
        result["_check_log"] = check_log.read_text() if check_log.is_file() else ""
        return result


class TaskLoadingTests(WorkflowTaskCase):
    def test_workflow_task_set_loads(self):
        tasks = evaluation.load_tasks(TASKS_DIR)
        self.assertEqual([task["id"] for task in tasks], EXPECTED_TASK_IDS)
        for task in tasks:
            with self.subTest(task=task["id"]):
                self.assertEqual(task["task_set"], "mio-workflows-v1")
                self.assertEqual(task["task_set_version"], 2)
                self.assertTrue(task["prompt"])
                self.assertTrue(task["files"])
                self.assertTrue(task["acceptance"].is_file())

    def test_workflow_config_points_at_this_task_set(self):
        config = evaluation.load_config(
            REPO_ROOT / "evals" / "configs" / "mio-deepseek-workflows.json"
        )
        self.assertEqual(config["event_format"], "mio-jsonl")
        self.assertEqual(config["timeout_seconds"], 180)
        self.assertEqual(Path(config["tasks_dir"]).resolve(), TASKS_DIR.resolve())
        self.assertEqual(
            config["budget"]["model"], "deepseek-chat"
        )
        self.assertEqual(config["budget"]["thinking"], "off")
        self.assertEqual(config["budget"]["max_model_calls"], 20)
        self.assertIn("--json", config["agent_command"])
        self.assertEqual(
            config["output"].resolve(),
            (REPO_ROOT / "target" / "evals" / "workflows-deepseek.json").resolve(),
        )
        self.assertNotIn("API_KEY", config["agent_command"])


class InitialFixtureTests(WorkflowTaskCase):
    """Every unsolved workspace must fail acceptance on its own."""

    def test_initial_workspaces_fail(self):
        for task_id in EXPECTED_TASK_IDS:
            with self.subTest(task=task_id):
                result = self.evaluate_task(task_id, "pass\n")
                self.assertEqual(result["agent"]["exit_code"], 0, task_id)
                self.assertFalse(result["accepted"], task_id)
                self.assertEqual(result["status"], "failed", task_id)
                self.assertEqual(result["failure_class"], "acceptance_failed", task_id)

    def test_recovery_fixture_fails_before_the_fix(self):
        # The pinned fixture itself must fail deterministically against the
        # unmodified library; otherwise the recovery evidence would be fake.
        root = Path(tempfile.mkdtemp(prefix="mio-workflows-fixture-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        task = {t["id"]: t for t in evaluation.load_tasks(TASKS_DIR)}["resume-cache-build"]
        workspace = root / "workspace"
        workspace.mkdir()
        for name, content in task["files"].items():
            target = workspace / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content)
        completed = subprocess.run(
            [sys.executable, "fixture_build.py", "build"],
            cwd=workspace,
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(completed.returncode, 0, completed.stdout)
        self.assertIn("FileNotFoundError", completed.stderr)


class ReferenceSolutionTests(WorkflowTaskCase):
    def test_task1_reference_passes(self):
        result = self.evaluate_task("migrate-line-item-api", writer_solver(TASK1_FILES))
        self.assertTrue(result["accepted"], result["_check_log"])
        self.assertIsNone(result["failure_class"])

    def test_task2_reference_passes(self):
        result = self.evaluate_task("resume-cache-build", recovery_solver(TASK2_FIXED_CACHE))
        self.assertTrue(result["accepted"], result["_check_log"])
        self.assertIsNone(result["failure_class"])

    def test_task3_reference_passes(self):
        result = self.evaluate_task("retrieve-wire-spec", writer_solver({"wire.py": TASK3_WIRE}))
        self.assertTrue(result["accepted"], result["_check_log"])
        self.assertIsNone(result["failure_class"])


class WrongSolutionTests(WorkflowTaskCase):
    def test_task1_keeping_deprecated_compute_fails(self):
        files = dict(TASK1_FILES)
        files["invoicing/core.py"] = WRONG_T1_KEEPS_DEPRECATED
        result = self.evaluate_task("migrate-line-item-api", writer_solver(files))
        self.assertFalse(result["accepted"])
        self.assertEqual(result["failure_class"], "acceptance_failed")

    def test_task1_unupdated_caller_fails(self):
        result = self.evaluate_task(
            "migrate-line-item-api", writer_solver({"invoicing/core.py": TASK1_CORE})
        )
        self.assertFalse(result["accepted"])

    def test_task2_source_only_without_recovery_fails(self):
        result = self.evaluate_task(
            "resume-cache-build",
            writer_solver({"buildlib/cache.py": TASK2_FIXED_CACHE}),
        )
        self.assertFalse(result["accepted"])
        self.assertIn("journal", result["_check_log"])

    def test_task3_missing_escaping_fails(self):
        result = self.evaluate_task(
            "retrieve-wire-spec", writer_solver({"wire.py": WRONG_T3_NO_ESCAPE})
        )
        self.assertFalse(result["accepted"])

    def test_task3_touching_unrelated_file_fails(self):
        files = {"wire.py": TASK3_WIRE, "legacy/parse_legacy.py": "raise SystemExit(0)\n"}
        result = self.evaluate_task("retrieve-wire-spec", writer_solver(files))
        self.assertFalse(result["accepted"])
        self.assertIn("unrelated file was modified", result["_check_log"])


if __name__ == "__main__":
    unittest.main()
