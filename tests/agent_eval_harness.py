"""Offline tests of the evaluation runner; does not assess any actual agent."""
import argparse
import importlib.util
import json
import os
import shlex
import shutil
import stat
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "agent_eval.py"
spec = importlib.util.spec_from_file_location("agent_eval", SCRIPT)
evaluation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(evaluation)

NO_OP_AGENT = [sys.executable, "-c", "pass", "{prompt}"]

SOLUTIONS = {
    "median-boundaries": ("stats.py", """def median(values):
    values = sorted(values)
    if not values:
        raise ValueError()
    n = len(values)
    return values[n // 2] if n % 2 else (values[n // 2 - 1] + values[n // 2]) / 2
"""),
    "csv-reader": ("records.py", """import csv
import io
def parse_records(text):
    rows = csv.reader(io.StringIO(text, newline=''))
    keys = next(rows, [])
    if len(set(keys)) != len(keys):
        raise ValueError()
    out = []
    for row in rows:
        if not row:
            continue
        if len(row) != len(keys):
            raise ValueError()
        out.append(dict(zip(keys, row)))
    return out
"""),
    "stable-toposort": ("graph.py", """def order_tasks(dependencies):
    remaining = {key: set(value) for key, value in dependencies.items()}
    for key in set().union(*remaining.values()) if remaining else []:
        remaining.setdefault(key, set())
    out = []
    while remaining:
        ready = sorted(key for key, value in remaining.items() if not value)
        if not ready:
            raise ValueError()
        key = ready[0]
        out.append(key)
        del remaining[key]
        for value in remaining.values():
            value.discard(key)
    return out
"""),
}


def write_solution_command(task_id):
    name, source = SOLUTIONS[task_id]
    return [
        sys.executable, "-c",
        f"from pathlib import Path; Path({name!r}).write_text({source!r})",
        "{prompt}",
    ]


def completion_call_event(run_id, call_index, finish_reason="stop", **usage):
    """One realistic mio --json completion_call line for the fixture tests.

    Mirrors the real wire shape: a camelCase envelope plus observation fields,
    with mio-core's snake_case `usage` counters nested inside. `finish_reason`
    mirrors mio-core's serialized `FinishReason`: a snake_case string, the
    externally tagged `{"other": ...}`, or `None` when the provider reported none.
    """
    return json.dumps({
        "type": "completion_call",
        "data": {
            "runId": run_id,
            "callIndex": call_index,
            "usage": usage,
            "responseId": f"resp-{call_index}",
            "providerRequestId": f"req-{call_index}",
            "finishReason": finish_reason,
        },
    })


def pi_message_end(response_id="resp-1", stop_reason="stop", timestamp=1700000000000,
                   usage=None, provider="deepseek", model="deepseek-chat", **message_overrides):
    """One realistic `pi --mode json` message_end line for the fixture tests.

    Mirrors Pi's authoritative final assistant message: an `AssistantMessage` with a
    non-optional `usage` object, a terminal `stopReason`, and the catalog-priced `cost`
    estimate that the parser must ignore.
    """
    if usage is None:
        usage = {
            "input": 100, "output": 10, "cacheRead": 50, "cacheWrite": 0,
            "totalTokens": 160, "reasoning": 4,
            "cost": {"input": 0.0001, "output": 0.0002, "cacheRead": 0.0,
                     "cacheWrite": 0.0, "total": 0.0003},
        }
    message = {
        "role": "assistant",
        "content": [{"type": "text", "text": "PRIVATE MODEL TEXT"}],
        "api": "openai-completions",
        "provider": provider,
        "model": model,
        "responseId": response_id,
        "usage": usage,
        "stopReason": stop_reason,
        "timestamp": timestamp,
    }
    message.update(message_overrides)
    return json.dumps({"type": "message_end", "message": message})


def write_task(root, directory, task_id, task_set="probe-set", version=1, **overrides):
    """Create a minimal on-disk task directory; overrides can break it on purpose."""
    task_root = Path(root) / directory
    (task_root / "initial").mkdir(parents=True, exist_ok=True)
    (task_root / "instruction.md").write_text("do the thing\n")
    (task_root / "initial" / "module.py").write_text("value = 1\n")
    (task_root / "acceptance.py").write_text("import module\nassert module.value == 1\n")
    metadata = {
        "id": task_id, "task_set": task_set, "task_set_version": version,
        "instruction": "instruction.md", "initial": "initial",
        "acceptance": "acceptance.py", "acceptance_timeout_seconds": 10,
    }
    metadata.update(overrides)
    (task_root / "task.json").write_text(json.dumps(metadata))
    return task_root


class TaskLoadingTests(unittest.TestCase):
    def test_default_task_set_loads(self):
        tasks = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)
        self.assertEqual(
            [task["id"] for task in tasks],
            ["csv-reader", "median-boundaries", "stable-toposort"],
        )
        for task in tasks:
            with self.subTest(task=task["id"]):
                self.assertTrue(task["prompt"])
                self.assertTrue(task["files"])
                self.assertTrue(task["acceptance"].is_file())
                self.assertGreater(task["acceptance_timeout_seconds"], 0)
                self.assertEqual(task["task_set"], "mio-python-acceptance-v1")
                self.assertEqual(task["task_set_version"], 1)

    def test_missing_acceptance_script_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            task_root = write_task(temporary, "broken", "broken")
            (task_root / "acceptance.py").unlink()
            with self.assertRaises(ValueError):
                evaluation.load_tasks(temporary)

    def test_duplicate_task_ids_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            write_task(temporary, "one", "same-id")
            write_task(temporary, "two", "same-id")
            with self.assertRaises(ValueError):
                evaluation.load_tasks(temporary)

    def test_task_set_mismatch_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            write_task(temporary, "one", "one", task_set="set-a")
            write_task(temporary, "two", "two", task_set="set-b")
            with self.assertRaises(ValueError):
                evaluation.load_tasks(temporary)


class EvaluationTests(unittest.TestCase):
    def test_unmodified_fixtures_fail(self):
        with tempfile.TemporaryDirectory() as temporary:
            for task in evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR):
                result = evaluation.evaluate(task, NO_OP_AGENT, 5, Path(temporary))
                self.assertEqual(result["agent"]["exit_code"], 0, task["id"])
                self.assertFalse(result["accepted"], task["id"])
                self.assertEqual(result["status"], "failed", task["id"])
                self.assertEqual(result["failure_class"], "acceptance_failed", task["id"])

    def test_reference_fixes_pass(self):
        with tempfile.TemporaryDirectory() as temporary:
            for task in evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR):
                command = write_solution_command(task["id"])
                result = evaluation.evaluate(task, command, 5, Path(temporary))
                self.assertTrue(result["accepted"], task["id"])
                self.assertEqual(result["status"], "passed", task["id"])
                self.assertIsNone(result["failure_class"], task["id"])

    def test_repo_placeholder_resolves_to_repository_root(self):
        with tempfile.TemporaryDirectory() as temporary:
            task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
            # The task's own acceptance fails; this only checks argv substitution.
            command = [
                sys.executable, "-c",
                "import pathlib, sys; pathlib.Path('repo.txt').write_text(sys.argv[1])",
                "{repo}", "{prompt}",
            ]
            evaluation.evaluate(task, command, 5, Path(temporary))
            captured = (
                Path(temporary) / task["id"] / "workspace" / "repo.txt"
            ).read_text()
            self.assertEqual(captured, str(evaluation.REPO_ROOT))
            self.assertTrue(evaluation.REPO_ROOT.is_absolute())

    def test_agent_timeout_is_classified(self):
        with tempfile.TemporaryDirectory() as temporary:
            task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
            command = [sys.executable, "-c", "import time; time.sleep(30)", "{prompt}"]
            result = evaluation.evaluate(task, command, 0.2, Path(temporary))
            self.assertFalse(result["accepted"])
            self.assertEqual(result["status"], "timeout")
            self.assertEqual(result["failure_class"], "agent_timeout")
            self.assertLess(result["agent"]["seconds"], 5)

    def test_agent_failure_is_classified(self):
        with tempfile.TemporaryDirectory() as temporary:
            task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
            command = [sys.executable, "-c", "raise SystemExit(3)", "{prompt}"]
            result = evaluation.evaluate(task, command, 5, Path(temporary))
            self.assertFalse(result["accepted"])
            self.assertEqual(result["failure_class"], "agent_exit_nonzero")
            # Acceptance still runs and is recorded even after a failed agent.
            self.assertIn("exit_code", result["acceptance"])

    def test_timeout_reaps_process(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            result = evaluation.run_process(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                root, 0.05, root / "log",
            )
            self.assertTrue(result["timed_out"])
            self.assertLess(result["seconds"], 5)


class EvaluationIsolationAndDiagnosticsTests(unittest.TestCase):
    def test_mio_profiles_keep_coordination_per_task(self):
        for name in ["mio-local.json", "mio-deepseek.json", "mio-deepseek-workflows.json"]:
            config = evaluation.load_config(REPO_ROOT / "evals" / "configs" / name)
            template = shlex.split(config["agent_command"])
            index = template.index("--shared-limits-dir")
            self.assertEqual(template[index + 1], "{cwd}/../coordination")
        fake = (
            "import sys; from pathlib import Path; "
            "p=Path(sys.argv[sys.argv.index('--shared-limits-dir')+1]); "
            "p.mkdir(parents=True,exist_ok=True); f=p/'calls'; "
            "n=int(f.read_text())+1 if f.exists() else 1; "
            "f.write_text(str(n)); sys.exit(0 if n==1 else 91)"
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixtures = root / "fixtures"
            for name in ("alpha", "beta"):
                write_task(fixtures, name, name)
            template = [sys.executable, "-c", fake, *template[1:]]
            for task in evaluation.load_tasks(fixtures):
                result = evaluation.evaluate(task, template, 5, root / "runs")
                self.assertTrue(result["accepted"])
                self.assertEqual((root / "runs" / task["id"] / "coordination" / "calls").read_text(), "1")

    def test_diagnostic_collection_retains_categories_only(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "agent.log"
            path.write_text("\n".join(json.dumps(event) for event in [
                {"type": "diagnostic", "data": {"category": "context_budget", "message": "PRIVATE MESSAGE"}},
                {"type": "diagnostic", "data": {"category": "scheduling", "raw": "PRIVATE BODY"}},
                {"type": "diagnostic", "data": {"category": "context_budget"}},
                {"type": "diagnostic", "data": {"category": ["bad"]}},
                {"type": "diagnostic", "data": {"category": "PRIVATE CATEGORY"}},
            ]) + "\n")
            categories = evaluation.collect_diagnostic_categories(evaluation.MIO_JSONL, path)
            self.assertEqual(categories, ["context_budget", "scheduling"])
            self.assertNotIn("PRIVATE", json.dumps(categories))
            self.assertIsNone(evaluation.collect_diagnostic_categories(None, path))
            with patch.object(evaluation, "MAX_EVENT_LOG_LINES", 1):
                self.assertIsNone(evaluation.collect_diagnostic_categories(evaluation.MIO_JSONL, path))


EVENT_AGENT_SOURCE = '''
import json
print(json.dumps({"type": "completion_call", "data": {"runId": "run-1",
    "callIndex": 0, "usage": {"input_tokens": 5}, "finishReason": "stop"}}))
print(json.dumps({"type": "read_observation", "data": {"path": "src/a.py",
    "offset": 1, "limit": 5}}))
print(json.dumps({"type": "read_observation", "data": {"path": "src/a.py",
    "offset": 1, "limit": 5}}))
print(json.dumps({"type": "read_observation", "data": {"path": "/etc/passwd"}}))
print(json.dumps({"type": "read_observation", "data": {"path": "x" * 400}}))
print(json.dumps({"type": "constraint_observation", "data": {"constraintId":
    "c-alpha", "status": "preserved"}}))
print(json.dumps({"type": "constraint_observation", "data": {"constraintId":
    "PRIVATE-SECRET-VALUE", "status": "preserved"}}))
'''

COUNTER_AGENT_SOURCE = '''
import json, pathlib
counter = pathlib.Path.cwd().parent / "invocations"
counter.write_text(str(int(counter.read_text()) + 1) if counter.exists() else "1")
print(json.dumps({"type": "completion_call", "data": {"runId": "run-1",
    "callIndex": 0, "usage": {"input_tokens": 5}, "finishReason": "stop"}}))
'''


class DiagnosticsIntegrationTests(unittest.TestCase):
    """Opt-in bounded diagnostics: real evidence files, no model, no raw artifacts."""

    def write_config(self, directory, **extra):
        config = {"schema_version": 1, "agent_command": "agent {prompt}"}
        config.update(extra)
        path = Path(directory) / "config.json"
        path.write_text(json.dumps(config))
        return path

    def script_agent(self, directory, name, source):
        path = Path(directory) / name
        path.write_text(source)
        return [sys.executable, str(path), "{prompt}"]

    def test_default_run_writes_no_evidence_and_returns_no_block(self):
        task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            result = evaluation.evaluate(
                task, NO_OP_AGENT, 10, root / "runs", evaluation.MIO_JSONL
            )
            self.assertIsNone(result["diagnostics"])
            self.assertEqual(sorted(entry.name for entry in root.iterdir()), ["runs"])

    def test_dispatch_coverage_preserves_real_counts_and_absent_preparation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            log = root / "events.jsonl"
            events = [
                {"type": "run_observation", "data": {
                    "kind": "model_dispatched", "runId": "private-run", "effectId": effect
                }} for effect in (1, 2)
            ]
            log.write_text("\n".join(json.dumps(event) for event in events))
            evidence = evaluation.eval_diagnostics.collect(log, root)
            block = evaluation.diagnostics_evidence_block(evidence)
            self.assertEqual(block["status"], "collected")
            self.assertEqual(block["coverage"]["model_calls_dispatched"], 2)
            self.assertIsNone(block["coverage"]["model_calls_prepared"])
            self.assertNotIn("private-run", json.dumps(block))

    def test_tool_request_coverage_counts_observed_events_not_registration(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            log = root / "events.jsonl"
            marker = "sk-live-PRIVATE-MARKER"
            events = [
                {"type": "tool_started", "data": {"operation": "read", "target": "private.txt"}},
                {"type": "tool_finished", "data": {
                    "operation": "read", "target": "private.txt", "result": {"Ok": marker}
                }},
            ]
            log.write_text("\n".join(json.dumps(event) for event in events))
            evidence = evaluation.eval_diagnostics.collect(log, root)
            block = evaluation.diagnostics_evidence_block(evidence)
            self.assertEqual(block["status"], "collected")
            self.assertEqual(block["coverage"]["tool_request_events"], 1)
            self.assertEqual(
                evidence["tool_activity"]["request"]["by_name"]["grep"], 0
            )
            self.assertNotIn("private.txt", json.dumps(block))
            self.assertNotIn(marker, json.dumps(block))

    def test_tool_activity_is_unknown_when_no_tool_event_was_observed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            log = root / "events.jsonl"
            log.write_text(json.dumps({"type": "completion_call", "data": {
                "runId": "run-1", "callIndex": 0, "finishReason": "stop"
            }}))
            block = evaluation.diagnostics_evidence_block(
                evaluation.eval_diagnostics.collect(log, root)
            )
            self.assertIsNone(block["coverage"]["tool_request_events"])

    def test_evidence_is_bounded_sanitized_and_owner_only(self):
        task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            diagnostics_dir = root / "diag"
            agent = self.script_agent(root, "emit.py", EVENT_AGENT_SOURCE)
            result = evaluation.evaluate(
                task, agent, 10, root / "runs", evaluation.MIO_JSONL,
                diagnostics_dir=diagnostics_dir,
                diagnostics_constraints=("c-alpha",),
            )
            block = result["diagnostics"]
            self.assertTrue(block["written"])
            self.assertEqual(block["status"], "truncated")
            self.assertTrue(block["source"]["opened"])
            self.assertEqual(block["coverage"]["model_calls_completed"], 1)
            self.assertEqual(block["coverage"]["constraint_evidence_ids"], 1)

            target = diagnostics_dir / (task["id"] + ".json")
            self.assertTrue(target.is_file())
            evidence = json.loads(target.read_text(encoding="utf-8"))
            self.assertEqual(evidence["kind"], "mio-eval-diagnostics")
            self.assertEqual(evidence["constraint_coverage"]["allowed"], ["c-alpha"])
            self.assertEqual(evidence["constraint_coverage"]["preserved"], ["c-alpha"])
            self.assertEqual(evidence["constraint_coverage"]["non_allowlisted_ids"], 1)
            self.assertEqual(evidence["reads"]["repeated_reads"], 1)
            self.assertEqual(evidence["reads"]["rejected_paths"]["absolute"], 1)
            self.assertEqual(evidence["reads"]["rejected_paths"]["too_long"], 1)
            self.assertEqual(
                [entry["path"] for entry in evidence["reads"]["files"]], ["src/a.py"]
            )
            text = target.read_text(encoding="utf-8")
            for forbidden in ("/etc/passwd", "PRIVATE-SECRET-VALUE", "x" * 64, str(root)):
                self.assertNotIn(forbidden, text)
            self.assertLessEqual(len(text), 64 * 1024)
            if os.name == "posix":
                self.assertEqual(stat.S_IMODE(os.stat(diagnostics_dir).st_mode), 0o700)
                self.assertEqual(stat.S_IMODE(os.stat(target).st_mode), 0o600)
            # The report block records status and counts, never the evidence path.
            self.assertNotIn(str(diagnostics_dir), json.dumps(result))

    def test_duplicate_evidence_refuses_without_rerunning_the_agent(self):
        task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runs = root / "runs"
            diagnostics_dir = root / "diag"
            agent = self.script_agent(root, "counter.py", COUNTER_AGENT_SOURCE)
            first = evaluation.evaluate(
                task, agent, 10, runs, evaluation.MIO_JSONL,
                diagnostics_dir=diagnostics_dir,
            )
            target = diagnostics_dir / (task["id"] + ".json")
            original = target.read_text(encoding="utf-8")
            counter = runs / task["id"] / "invocations"
            self.assertEqual(counter.read_text(), "1")
            self.assertEqual(first["diagnostics"]["status"], "collected")

            second = evaluation.evaluate(
                task, agent, 10, runs, evaluation.MIO_JSONL,
                diagnostics_dir=diagnostics_dir,
            )
            # The agent never starts, so no duplicate side effect is produced.
            self.assertEqual(counter.read_text(), "1")
            self.assertEqual(second["status"], "error")
            self.assertEqual(second["failure_class"], "diagnostics_evidence_exists")
            self.assertFalse(second["accepted"])
            self.assertEqual(second["agent"]["skipped"], "diagnostics_evidence_exists")
            self.assertEqual(second["diagnostics"]["reason"], "evidence_exists")
            self.assertFalse(second["diagnostics"]["written"])
            self.assertEqual(target.read_text(encoding="utf-8"), original)

    def test_pi_jsonl_and_default_format_are_explicitly_unsupported(self):
        task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
        for event_format, reason in (
            (evaluation.PI_JSONL, "pi_jsonl_out_of_scope"),
            (None, "event_format_not_selected"),
        ):
            with self.subTest(event_format=event_format):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    diagnostics_dir = root / "diag"
                    result = evaluation.evaluate(
                        task, NO_OP_AGENT, 10, root / "runs", event_format,
                        diagnostics_dir=diagnostics_dir,
                    )
                    block = result["diagnostics"]
                    self.assertEqual(block["status"], "unsupported")
                    self.assertEqual(block["reason"], reason)
                    self.assertFalse(block["written"])
                    self.assertIsNone(block["coverage"]["model_calls_completed"])
                    # An unsupported format never creates or writes a directory.
                    self.assertFalse(diagnostics_dir.exists())
                    # Task correctness is classified separately from diagnostics.
                    self.assertEqual(result["status"], "failed")
                    self.assertEqual(result["failure_class"], "acceptance_failed")

    def test_empty_and_missing_sources_stay_explicitly_unknown(self):
        task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            result = evaluation.evaluate(
                task, NO_OP_AGENT, 10, root / "runs", evaluation.MIO_JSONL,
                diagnostics_dir=root / "diag",
            )
            block = result["diagnostics"]
            self.assertEqual(block["status"], "unknown")
            self.assertEqual(block["reason"], "no_events")
            self.assertTrue(block["written"])
            self.assertIsNone(block["coverage"]["model_calls_completed"])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace = root / "workspace"
            workspace.mkdir()
            block = evaluation.collect_task_diagnostics(
                root / "evidence.json", root / "missing.log", workspace, ()
            )
        self.assertEqual(block["status"], "unknown")
        self.assertEqual(block["reason"], "missing")
        # The collector still writes evidence that records the source as unopened.
        self.assertTrue(block["written"])
        self.assertIsNone(block["coverage"]["model_calls_completed"])

    def test_collector_faults_are_explicit_and_do_not_leak(self):
        task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
        for error, reason in (
            (evaluation.eval_diagnostics.EvidenceAlreadyExists("race /private/path"),
             "evidence_exists"),
            (OSError("disk full at /private/path"), "write_failed"),
            (RuntimeError("PRIVATE /home/secret body"), "collector_failed"),
        ):
            with self.subTest(reason=reason):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    diagnostics_dir = root / "diag"
                    agent = self.script_agent(root, "emit.py", EVENT_AGENT_SOURCE)
                    with patch.object(
                        evaluation.eval_diagnostics, "collect_and_write", side_effect=error
                    ):
                        result = evaluation.evaluate(
                            task, agent, 10, root / "runs", evaluation.MIO_JSONL,
                            diagnostics_dir=diagnostics_dir,
                        )
                    self.assertEqual(result["diagnostics"]["status"], "error")
                    self.assertEqual(result["diagnostics"]["reason"], reason)
                    self.assertFalse(result["diagnostics"]["written"])
                    self.assertEqual(list(diagnostics_dir.iterdir()), [])
                    # Task correctness and its own fields are still recorded.
                    self.assertEqual(result["status"], "failed")
                    serialized = json.dumps(result)
                    self.assertNotIn("PRIVATE", serialized)
                    self.assertNotIn("/home/secret", serialized)
                    self.assertNotIn("/private/path", serialized)

    def test_diagnostics_settings_resolve_from_config_and_cli(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write_config(
                temporary,
                diagnostics_dir="diag",
                diagnostics_constraints=["c-alpha", "c-beta"],
            )
            config = evaluation.load_config(path)
            self.assertEqual(
                Path(config["diagnostics_dir"]).resolve(),
                (Path(temporary) / "diag").resolve(),
            )
            settings = evaluation.resolve_settings(config, argparse.Namespace())
            self.assertEqual(settings["diagnostics_constraints"], ["c-alpha", "c-beta"])
            overridden = evaluation.resolve_settings(
                config,
                argparse.Namespace(
                    diagnostics_dir=Path("cli-diag"),
                    diagnostics_constraints=["c-gamma"],
                ),
            )
            self.assertEqual(overridden["diagnostics_dir"], Path("cli-diag"))
            self.assertEqual(overridden["diagnostics_constraints"], ["c-gamma"])
            plain = evaluation.resolve_settings(
                evaluation.load_config(self.write_config(temporary)),
                argparse.Namespace(),
            )
            self.assertIsNone(plain["diagnostics_dir"])
            self.assertEqual(plain["diagnostics_constraints"], ())

    def test_invalid_diagnostics_settings_are_rejected(self):
        self.assertEqual(evaluation.normalize_diagnostics_constraints(None), ())
        self.assertEqual(
            evaluation.normalize_diagnostics_constraints(
                [" c-alpha ", "c-alpha", "c-beta"]
            ),
            ("c-alpha", "c-beta"),
        )
        for bad in ("c-alpha", 5, [1], [""], ["a b"], ["x" * 300], [True], ["../etc"]):
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):
                    evaluation.normalize_diagnostics_constraints(bad)
        limit = evaluation.eval_diagnostics.MAX_CONSTRAINTS
        bounded = evaluation.normalize_diagnostics_constraints(
            [f"c{index}" for index in range(limit)]
        )
        self.assertEqual(len(bounded), limit)
        with self.assertRaises(ValueError):
            evaluation.normalize_diagnostics_constraints(
                [f"c{index}" for index in range(limit + 1)]
            )
        with tempfile.TemporaryDirectory() as temporary:
            for bad in ("c-alpha", 5, [1], [""], ["a b"], [1]):
                with self.subTest(config_constraints=bad):
                    path = self.write_config(temporary, diagnostics_constraints=bad)
                    with self.assertRaises(ValueError):
                        evaluation.load_config(path)
            for bad in (5, "", [], True):
                with self.subTest(config_directory=bad):
                    path = self.write_config(temporary, diagnostics_dir=bad)
                    with self.assertRaises(ValueError):
                        evaluation.load_config(path)

    def test_cli_requires_a_directory_for_constraints_and_rejects_bad_ids(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "report.json"
            base = ["--agent-command", "agent {prompt}", "--output", str(output)]
            with self.assertRaises(SystemExit) as raised:
                evaluation.main([*base, "--diagnostics-constraint", "c-alpha"])
            self.assertEqual(raised.exception.code, 2)
            with self.assertRaises(SystemExit) as raised:
                evaluation.main([
                    *base,
                    "--diagnostics-dir", str(Path(temporary) / "diag"),
                    "--diagnostics-constraint", "not an id",
                ])
            self.assertEqual(raised.exception.code, 2)

    def test_report_records_enabled_limits_coverage_and_statuses(self):
        tasks = [
            {"id": "a", "task_set": "probe-set", "task_set_version": 1},
            {"id": "b", "task_set": "probe-set", "task_set_version": 1},
        ]

        def sample(task_id, diagnostics):
            return {
                "task": task_id, "status": "passed", "failure_class": None,
                "accepted": True, "duration_seconds": 1.0,
                "agent": {"exit_code": 0, "timed_out": False, "seconds": 0.9},
                "acceptance": {"exit_code": 0, "timed_out": False, "seconds": 0.1},
                "usage": dict(evaluation.UNKNOWN_USAGE),
                "diagnostics": diagnostics,
            }

        results = [
            sample("a", evaluation.diagnostics_status_block("collected")),
            sample("b", evaluation.diagnostics_status_block("error", "collector_failed")),
        ]
        report = evaluation.build_report(
            "probe", tasks, results, evaluation.resolve_budget({}),
            ["agent", "{prompt}"], "2026-01-01T00:00:00Z", 0.0,
            evaluation.MIO_JSONL, 180.0, False, None,
            Path("/tmp/diagnostics"), ("c-alpha",),
        )
        block = report["diagnostics"]
        self.assertTrue(block["enabled"])
        self.assertTrue(block["supported"])
        self.assertEqual(block["constraints"], ["c-alpha"])
        self.assertEqual(block["limits"], evaluation.eval_diagnostics.DEFAULT_LIMITS.as_dict())
        self.assertEqual(block["coverage"], "error")
        self.assertEqual(block["status_counts"]["error"], 1)
        self.assertEqual(block["status_counts"]["collected"], 1)
        self.assertEqual(
            block["requires_instrumentation"],
            list(evaluation.eval_diagnostics.REQUIRES_INSTRUMENTATION),
        )
        disabled = evaluation.build_report(
            "probe", tasks, results, evaluation.resolve_budget({}),
            ["agent", "{prompt}"], "2026-01-01T00:00:00Z", 0.0,
        )["diagnostics"]
        self.assertFalse(disabled["enabled"])
        self.assertIsNone(disabled["limits"])
        self.assertEqual(disabled["coverage"], "disabled")
        self.assertEqual(disabled["status_counts"]["collected"], 0)

    def test_runner_config_end_to_end_writes_limited_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            agent = root / "emit.py"
            agent.write_text(EVENT_AGENT_SOURCE)
            output = root / "report.json"
            diagnostics_dir = root / "diag"
            config_path = root / "config.json"
            config_path.write_text(json.dumps({
                "schema_version": 1,
                "label": "probe-diagnostics",
                "agent_command": f"{sys.executable} {agent} {{prompt}}",
                "timeout_seconds": 30,
                "tasks_dir": str(evaluation.DEFAULT_TASKS_DIR),
                "output": str(output),
                "tasks": ["csv-reader"],
                "event_format": "mio-jsonl",
                "diagnostics_dir": "diag",
                "diagnostics_constraints": ["c-alpha"],
            }))
            completed = subprocess.run(
                [sys.executable, str(SCRIPT), "--config", str(config_path)],
                capture_output=True, text=True,
            )
            self.assertEqual(completed.returncode, 1, completed.stderr)
            report = json.loads(output.read_text())
            self.assertTrue(report["diagnostics"]["enabled"])
            self.assertEqual(report["diagnostics"]["coverage"], "truncated")
            self.assertEqual(report["diagnostics"]["constraints"], ["c-alpha"])
            # The event parser still aggregates usage from the same log.
            self.assertEqual(report["usage"]["model_calls"], 1)
            result = report["results"][0]
            self.assertEqual(result["diagnostics"]["status"], "truncated")
            self.assertEqual(result["diagnostics"]["coverage"]["model_calls_completed"], 1)
            evidence = json.loads(
                (diagnostics_dir / "csv-reader.json").read_text(encoding="utf-8")
            )
            self.assertEqual(evidence["kind"], "mio-eval-diagnostics")
            self.assertEqual(evidence["constraint_coverage"]["allowed"], ["c-alpha"])
            self.assertNotIn(str(diagnostics_dir), json.dumps(result["diagnostics"]))


class AgentIsolationTests(unittest.TestCase):
    """Real child processes: isolation must change the child, not a mocked call."""

    def capture_child_env(self, temporary, isolate, probe):
        task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
        capture = Path(temporary) / "child-env.json"
        command = [
            sys.executable, "-c",
            "import json, os, pathlib; "
            f"pathlib.Path({str(capture)!r}).write_text(json.dumps(dict(os.environ)))",
            "{prompt}",
        ]
        with patch.dict(os.environ, probe, clear=False):
            evaluation.evaluate(
                task, command, 10, Path(temporary) / "runs",
                isolate_agent_env=isolate,
            )
        return task, json.loads(capture.read_text())

    def test_isolation_clears_overrides_and_keeps_credentials(self):
        probe = {
            "MIO_MODEL": "ambient-model",
            "MIO_PROVIDER": "ambient-provider",
            "MIO_BASE_URL": "http://ambient.invalid/v1",
            "MIO_API_KEY": "ambient-mio-key",
            "MIO_TEMP_DIR": "/ambient/tmp",
            "PI_SESSION_ID": "ambient-session",
            "PI_PROVIDER": "ambient-pi-provider",
            "PI_MODEL": "ambient-pi-model",
            "PI_DELEGATE_ACTIVE": "1",
            "DEEPSEEK_API_KEY": "isolated-probe-secret",
        }
        with tempfile.TemporaryDirectory() as temporary:
            task, child = self.capture_child_env(temporary, True, probe)
            self.assertEqual(
                [
                    name for name in child
                    if name.startswith(evaluation.ISOLATED_ENV_PREFIXES)
                    and name not in (
                        evaluation.PI_AGENT_DIR_ENV,
                        evaluation.PI_SESSION_DIR_ENV,
                        *evaluation.ISOLATED_ENV_KEEP,
                    )
                ],
                [],
            )
            # Pi's delegation recursion guard is deliberately left intact.
            self.assertEqual(child["PI_DELEGATE_ACTIVE"], "1")
            # Credentials that use other environment variables survive.
            self.assertEqual(child["DEEPSEEK_API_KEY"], "isolated-probe-secret")
            agent_dir = Path(child[evaluation.PI_AGENT_DIR_ENV])
            session_dir = Path(child[evaluation.PI_SESSION_DIR_ENV])
            self.assertTrue(agent_dir.is_dir())
            self.assertTrue(session_dir.is_dir())
            # A fresh, empty agent home: no user auth.json/models.json, no session.
            self.assertEqual([entry.name for entry in agent_dir.iterdir()], [])
            self.assertEqual([entry.name for entry in session_dir.iterdir()], [])
            task_root = Path(temporary) / "runs" / task["id"]
            self.assertTrue(str(agent_dir).startswith(str(task_root)))
            if os.name == "posix":
                self.assertEqual(stat.S_IMODE(os.stat(agent_dir).st_mode), 0o700)
                self.assertEqual(stat.S_IMODE(os.stat(task_root).st_mode), 0o700)

    def test_default_run_inherits_the_ambient_environment(self):
        probe = {
            "MIO_MODEL": "ambient-model",
            "PI_SESSION_ID": "ambient-session",
            "DEEPSEEK_API_KEY": "inherited-secret",
        }
        with tempfile.TemporaryDirectory() as temporary:
            _, child = self.capture_child_env(temporary, False, probe)
        self.assertEqual(child["MIO_MODEL"], "ambient-model")
        self.assertEqual(child["PI_SESSION_ID"], "ambient-session")
        self.assertEqual(child["DEEPSEEK_API_KEY"], "inherited-secret")
        # Without the opt-in nothing is relocated, not even Pi's agent home.
        self.assertEqual(
            child.get(evaluation.PI_AGENT_DIR_ENV),
            os.environ.get(evaluation.PI_AGENT_DIR_ENV),
        )

    def test_isolation_keeps_the_delegation_recursion_guard(self):
        with patch.dict(os.environ, {"PI_DELEGATE_ACTIVE": "1"}, clear=False):
            with tempfile.TemporaryDirectory() as temporary:
                environment = evaluation.build_agent_env(Path(temporary) / "task")
        self.assertEqual(environment["PI_DELEGATE_ACTIVE"], "1")

    def test_build_agent_env_does_not_mutate_the_parent_environment(self):
        with patch.dict(os.environ, {"MIO_MODEL": "ambient-model"}, clear=False):
            with tempfile.TemporaryDirectory() as temporary:
                environment = evaluation.build_agent_env(Path(temporary) / "task")
            self.assertNotIn("MIO_MODEL", environment)
            self.assertEqual(os.environ["MIO_MODEL"], "ambient-model")


class ArtifactRetentionTests(unittest.TestCase):
    """Opt-in artifact retention writes real files with private, bounded content."""

    def test_existing_evidence_is_never_merged_or_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / "run/workspace"
            work.mkdir(parents=True)
            (work / "result").write_text("first run")
            artifacts = root / "artifacts"
            evaluation.retain_task_artifacts(work.parent, artifacts, "task")
            (work / "result").write_text("second run")
            with self.assertRaises(FileExistsError):
                evaluation.retain_task_artifacts(work.parent, artifacts, "task")
            self.assertEqual((artifacts / "task/workspace/result").read_text(), "first run")

    @unittest.skipUnless(os.name == "posix", "POSIX symlink fixture")
    def test_artifact_targets_cannot_redirect_writes_or_permission_changes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outside = root / "outside"
            outside.mkdir(mode=0o755)
            link = root / "artifacts"
            link.symlink_to(outside, target_is_directory=True)
            with self.assertRaises(ValueError):
                evaluation.private_directory(link)
            self.assertEqual(stat.S_IMODE(outside.stat().st_mode), 0o755)
            source = root / "source"
            source.write_text("source")
            victim = outside / "victim"
            victim.write_text("keep")
            target = root / "target"
            target.symlink_to(victim)
            with self.assertRaises(OSError):
                evaluation._copy_bounded(source, target, 100)
            self.assertEqual(victim.read_text(), "keep")

    def test_retention_has_a_total_workspace_byte_bound(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / "run/workspace"
            work.mkdir(parents=True)
            for index in range(4):
                (work / str(index)).write_bytes(b"x" * 100)
            with patch.object(evaluation, "MAX_ARTIFACT_WORKSPACE_BYTES", 150):
                block = evaluation.retain_task_artifacts(work.parent, root / "artifacts", "task")
            self.assertEqual(block["workspace_bytes"], 150)
            self.assertTrue(block["workspace_truncated"])
            self.assertEqual(sum(item["bytes"] for item in block["workspace_files"]), 150)

    def agent_writing(self, name="result.txt", source="PROBE-WORKSPACE-FILE"):
        return [
            sys.executable, "-c",
            "import os, pathlib\n"
            f"pathlib.Path({name!r}).write_text({source!r})\n"
            "try:\n"
            "    os.symlink('/etc/hostname', 'escape-link')\n"
            "except OSError:\n"
            "    pass\n",
            "{prompt}",
        ]

    def test_retains_workspace_and_logs_with_private_permissions(self):
        task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
        with tempfile.TemporaryDirectory() as temporary:
            artifacts = Path(temporary) / "artifacts"
            result = evaluation.evaluate(
                task, self.agent_writing(), 10, Path(temporary) / "runs",
                artifacts_dir=artifacts,
            )
            block = result["artifacts"]
            directory = Path(block["directory"])
            self.assertEqual(directory, artifacts / task["id"])
            self.assertEqual(
                (directory / "workspace" / "result.txt").read_text(),
                "PROBE-WORKSPACE-FILE",
            )
            # Symlinks are never followed into the retained copy.
            self.assertFalse((directory / "workspace" / "escape-link").exists())
            self.assertTrue((directory / "agent.log").is_file())
            self.assertTrue((directory / "check.log").is_file())
            self.assertFalse(block["logs"]["agent.log"]["truncated"])
            # The report block carries paths, sizes and flags, never file contents.
            self.assertNotIn("PROBE-WORKSPACE-FILE", json.dumps(block))
            if os.name == "posix":
                self.assertEqual(stat.S_IMODE(os.stat(directory).st_mode), 0o700)
                self.assertEqual(stat.S_IMODE(os.stat(directory / "agent.log").st_mode), 0o600)
                self.assertEqual(
                    stat.S_IMODE(os.stat(directory / "workspace" / "result.txt").st_mode),
                    0o600,
                )

    def test_oversized_log_is_truncated_and_reported(self):
        task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
        with tempfile.TemporaryDirectory() as temporary:
            command = [sys.executable, "-c", "import sys; sys.stdout.write('x' * 5000)",
                       "{prompt}"]
            with patch.object(evaluation, "MAX_ARTIFACT_LOG_BYTES", 512):
                result = evaluation.evaluate(
                    task, command, 10, Path(temporary) / "runs",
                    artifacts_dir=Path(temporary) / "artifacts",
                )
            log = result["artifacts"]["logs"]["agent.log"]
            self.assertEqual(log["bytes"], 512)
            self.assertTrue(log["truncated"])
            self.assertEqual(Path(log["path"]).stat().st_size, 512)
            self.assertEqual(result["artifacts"]["limits"]["max_log_bytes"], 512)

    def test_oversized_workspace_file_is_truncated_and_reported(self):
        task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
        with tempfile.TemporaryDirectory() as temporary:
            command = [
                sys.executable, "-c",
                "import pathlib; pathlib.Path('big.bin').write_bytes(b'x' * 5000)",
                "{prompt}",
            ]
            with patch.object(evaluation, "MAX_ARTIFACT_FILE_BYTES", 256):
                result = evaluation.evaluate(
                    task, command, 10, Path(temporary) / "runs",
                    artifacts_dir=Path(temporary) / "artifacts",
                )
            entry = next(
                item for item in result["artifacts"]["workspace_files"]
                if item["path"] == "big.bin"
            )
            self.assertEqual(entry["bytes"], 256)
            self.assertTrue(entry["truncated"])

    def test_default_run_retains_nothing(self):
        task = evaluation.load_tasks(evaluation.DEFAULT_TASKS_DIR)[0]
        with tempfile.TemporaryDirectory() as temporary:
            result = evaluation.evaluate(task, NO_OP_AGENT, 10, Path(temporary))
        self.assertIsNone(result["artifacts"])


class ExecutionTimingTests(unittest.TestCase):
    def collect(self, lines):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "agent.log"
            path.write_text("\n".join(json.dumps(line) for line in lines) + "\n")
            return evaluation.collect_timing(evaluation.MIO_JSONL, path)

    def event(self, effect_id, kind, duration, status="success", **extra):
        return {"type": "execution_timing", "data": dict(
            runId="run-a", effectId=effect_id, kind=kind,
            durationMs=duration, status=status, **extra,
        )}

    def test_measured_spans_deduplicate_and_record_tool_failures(self):
        model = self.event(1, "model", 500)
        timing = self.collect([
            {"type": "text", "data": "ignore raw model output"}, model, model,
            self.event(2, "tool", 20, "error", arguments="never copy this"),
            self.event(3, "tool", 30),
        ])
        self.assertEqual(timing["coverage"], "observed_outcomes")
        self.assertEqual(timing["model"]["calls"], 1)
        self.assertEqual(timing["model"]["duration_seconds_sum"], 0.5)
        self.assertEqual(timing["tool"]["errors"], 1)
        self.assertEqual(timing["tool"]["duration_seconds_sum"], 0.05)
        self.assertEqual(timing["events"]["duplicates"], 1)
        self.assertIsNone(timing["context_seconds"])
        self.assertIsNone(timing["queue_seconds"])
        self.assertNotIn("never copy this", json.dumps(timing))

    def test_missing_events_remain_unknown(self):
        timing = self.collect([{"type": "completion_call", "data": {}}])
        self.assertEqual(timing["coverage"], "unknown")
        self.assertIsNone(timing["model"]["calls"])
        self.assertIsNone(timing["tool"]["duration_seconds_sum"])

    def test_invalid_and_truncated_spans_do_not_report_totals(self):
        for bad in (self.event(1, "model", -1), self.event(1, "model", True),
                    self.event(1, "bogus", 1), self.event(1, [], 1),
                    {"type": "execution_timing"}):
            with self.subTest(bad=bad):
                timing = self.collect([self.event(2, "model", 100), bad])
                self.assertEqual(timing["coverage"], "partial")
                self.assertIsNone(timing["model"]["duration_seconds_sum"])
        with patch.object(evaluation, "MAX_EVENT_LOG_LINES", 1):
            timing = self.collect([self.event(1, "model", 100), self.event(2, "tool", 20)])
        self.assertEqual(timing["coverage"], "partial")
        self.assertIsNone(timing["model"]["calls"])

    def test_top_level_timing_requires_all_tasks_to_be_observed(self):
        timing = self.collect([self.event(1, "model", 500)])
        total = evaluation.sum_task_timing([timing, timing])
        self.assertEqual(total["model"]["calls"], 2)
        self.assertEqual(total["model"]["duration_seconds_sum"], 1.0)
        partial = evaluation.sum_task_timing([timing, evaluation.unknown_timing()])
        self.assertEqual(partial["coverage"], "partial")
        self.assertIsNone(partial["model"]["duration_seconds_sum"])


class UsageEventTests(unittest.TestCase):
    """Aggregation of mio completion_call JSONL events; no model is called."""

    def write_log(self, directory, lines):
        path = Path(directory) / "agent.log"
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return path

    def collect(self, lines):
        with tempfile.TemporaryDirectory() as temporary:
            return evaluation.collect_usage(
                evaluation.MIO_JSONL, self.write_log(temporary, lines)
            )

    def test_events_are_deduplicated_and_summed(self):
        usage = self.collect([
            completion_call_event("run-a", 0, input_tokens=100, output_tokens=10, total_tokens=110),
            completion_call_event("run-a", 1, input_tokens=200, output_tokens=20, total_tokens=220),
            # Same (runId, callIndex) repeated with different values: counted once.
            completion_call_event("run-a", 0, input_tokens=999, output_tokens=999, total_tokens=1998),
        ])
        self.assertEqual(usage["source"], "mio-jsonl")
        self.assertEqual(usage["model_calls"], 2)
        self.assertEqual(usage["input_tokens"], 300)
        self.assertEqual(usage["output_tokens"], 30)
        self.assertEqual(usage["total_tokens"], 330)
        self.assertEqual(usage["coverage"], "partial")  # cache counters unreported
        self.assertEqual(usage["events"]["duplicates"], 1)
        self.assertEqual(usage["events"]["accepted"], 2)
        self.assertIsNone(usage["cost_usd"])

    def test_missing_counters_stay_null_without_a_synthesized_total(self):
        usage = self.collect([
            completion_call_event("run-a", 0, input_tokens=100, output_tokens=10, total_tokens=110),
            completion_call_event("run-a", 1, input_tokens=200),
        ])
        self.assertEqual(usage["model_calls"], 2)
        self.assertEqual(usage["input_tokens"], 300)
        self.assertIsNone(usage["output_tokens"])
        self.assertIsNone(usage["total_tokens"])
        self.assertEqual(usage["coverage"], "partial")

    def test_event_without_counters_reports_unknown_not_zero(self):
        usage = self.collect([completion_call_event("run-a", 0, usage_missing=True)])
        self.assertEqual(usage["source"], "mio-jsonl")
        self.assertEqual(usage["model_calls"], 1)
        self.assertEqual(usage["coverage"], "unknown")
        for name in evaluation.USAGE_COUNTERS:
            self.assertIsNone(usage[name], name)

    def test_unrelated_diagnostic_lines_do_not_become_complete_zero_counts(self):
        evidence = {
            "source": {"opened": True, "events_scanned": 2, "events_recognized": 0},
            "model_calls": {"completed": None}, "reads": {}, "compaction": {},
            "constraint_coverage": {}, "truncation": {},
        }
        block = evaluation.diagnostics_evidence_block(evidence)
        self.assertEqual(block["status"], "unknown")
        self.assertEqual(block["reason"], "no_supported_events")
        self.assertTrue(all(value is None for value in block["coverage"].values()))

    def test_no_events_at_all_is_unknown(self):
        with tempfile.TemporaryDirectory() as temporary:
            usage = evaluation.collect_usage(
                evaluation.MIO_JSONL, Path(temporary) / "missing.log"
            )
        self.assertEqual(usage["coverage"], "unknown")
        self.assertEqual(usage["source"], "mio-jsonl")
        self.assertIsNone(usage["model_calls"])
        for name in evaluation.USAGE_COUNTERS:
            self.assertIsNone(usage[name], name)

    def test_default_format_does_not_read_agent_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = self.write_log(temporary, [
                completion_call_event("run-a", 0, input_tokens=5, output_tokens=1),
            ])
            usage = evaluation.collect_usage(None, log)
        self.assertEqual(usage["source"], "unknown")
        self.assertIsNone(usage["model_calls"])
        self.assertIsNone(usage["input_tokens"])

    def test_noise_and_malformed_lines_are_ignored(self):
        usage = self.collect([
            "",                                                        # blank
            "plain text merged from stderr",                           # not JSON
            '{"type": "text", "data": "hi"}',                       # other event
            '["not", "an", "event"]',                                 # JSON, not object
            '{"type": "completion_call"}',                             # malformed: no data
            '{"type": "completion_call", "data": {"callIndex": 0}}',  # malformed: no runId
            completion_call_event("run-a", 0, input_tokens=7, output_tokens=1, total_tokens=8),
        ])
        self.assertIsNone(usage["model_calls"])
        self.assertIsNone(usage["input_tokens"])
        self.assertEqual(usage["coverage"], "partial")
        self.assertEqual(usage["events"]["ignored"], 4)
        self.assertEqual(usage["events"]["malformed"], 2)
        self.assertFalse(usage["events"]["truncated"])

    def test_truncated_scan_does_not_claim_complete_totals(self):
        with patch.object(evaluation, "MAX_EVENT_LOG_LINES", 1):
            usage = self.collect([
                completion_call_event("run-a", 0, input_tokens=7),
                completion_call_event("run-a", 1, input_tokens=9),
            ])
        self.assertTrue(usage["events"]["truncated"])
        self.assertEqual(usage["events"]["accepted"], 1)
        self.assertIsNone(usage["input_tokens"])
        self.assertIsNone(usage["model_calls"])
        self.assertEqual(usage["coverage"], "partial")

    def test_oversized_line_respects_total_scan_budget(self):
        with patch.object(evaluation, "MAX_EVENT_LOG_LINE_BYTES", 16), patch.object(
            evaluation, "MAX_EVENT_LOG_BYTES", 64
        ):
            usage = self.collect(["x" * 1000, completion_call_event("run-a", 0, input_tokens=7)])
        self.assertTrue(usage["events"]["truncated"])
        self.assertEqual(usage["events"]["accepted"], 0)
        self.assertIsNone(usage["input_tokens"])

    def test_top_level_envelope_is_required(self):
        usage = self.collect([
            # A bare observation without the {"type": ..., "data": ...} envelope.
            json.dumps({"runId": "run-a", "callIndex": 0, "usage": {"input_tokens": 5}}),
            completion_call_event("run-a", 0, input_tokens=5),
        ])
        self.assertEqual(usage["model_calls"], 1)
        self.assertEqual(usage["events"]["ignored"], 1)

    def test_cache_counters_map_to_the_usage_definition(self):
        usage = self.collect([
            completion_call_event(
                "run-a", 0, input_tokens=1000, output_tokens=50, total_tokens=1050,
                cached_input_tokens=800, cache_creation_input_tokens=100,
            ),
        ])
        self.assertEqual(usage["input_tokens"], 1000)
        self.assertEqual(usage["cached_input_tokens"], 800)
        self.assertEqual(usage["cache_creation_input_tokens"], 100)
        self.assertNotIn("cache_hit_tokens", usage)

    def test_report_totals_require_every_task_to_report(self):
        def call(**counters):
            return {**{name: None for name in evaluation.USAGE_COUNTERS}, **counters}

        first = evaluation.usage_from_calls([call(input_tokens=5, output_tokens=1, total_tokens=6)])
        second = evaluation.usage_from_calls([call(input_tokens=7, output_tokens=2, total_tokens=9)])
        combined = evaluation.sum_task_usage([first, second])
        self.assertEqual(combined["input_tokens"], 12)
        self.assertEqual(combined["output_tokens"], 3)
        self.assertEqual(combined["total_tokens"], 15)
        self.assertEqual(combined["model_calls"], 2)
        self.assertEqual(combined["coverage"], "partial")
        # An event-less task blocks the total instead of silently dropping it.
        blocked = evaluation.sum_task_usage([first, evaluation.unknown_usage()])
        self.assertIsNone(blocked["input_tokens"])
        self.assertIsNone(blocked["total_tokens"])
        self.assertIsNone(blocked["model_calls"])
        self.assertEqual(blocked["coverage"], "partial")

    def test_unavailable_metrics_follow_the_format(self):
        self.assertIn("total_tokens", evaluation.unavailable_metrics(None))
        self.assertNotIn("total_tokens", evaluation.unavailable_metrics(evaluation.MIO_JSONL))
        self.assertIn("cost_usd", evaluation.unavailable_metrics(evaluation.MIO_JSONL))


class MioFinishReasonTests(unittest.TestCase):
    """Mio's native finish-reason buckets; no model is called."""

    def collect(self, lines):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "agent.log"
            path.write_text("\n".join(lines) + "\n", encoding="utf-8")
            return evaluation.collect_usage(evaluation.MIO_JSONL, path)

    def test_each_serialized_reason_lands_in_its_own_bucket(self):
        usage = self.collect([
            completion_call_event("run-a", 0, finish_reason="stop"),
            completion_call_event("run-a", 1, finish_reason="length"),
            completion_call_event("run-a", 2, finish_reason="tool_calls"),
            completion_call_event("run-a", 3, finish_reason="content_filter"),
            completion_call_event("run-a", 4, finish_reason={"other": "max_tokens"}),
            completion_call_event("run-a", 5, finish_reason=None),
        ])
        self.assertEqual(usage["model_calls"], 6)
        self.assertEqual(usage["finish_reasons"]["stop"], 1)
        self.assertEqual(usage["finish_reasons"]["length"], 1)
        self.assertEqual(usage["finish_reasons"]["tool_calls"], 1)
        self.assertEqual(usage["finish_reasons"]["content_filter"], 1)
        self.assertEqual(usage["finish_reasons"]["other"], 1)
        self.assertEqual(usage["finish_reasons"]["unreported"], 1)
        self.assertNotIn("stop_reasons", usage)
        # The provider's wire spelling is never copied into the report.
        self.assertNotIn("max_tokens", json.dumps(usage))

    def test_unclassifiable_reason_is_malformed_not_smoothed_to_stop(self):
        usage = self.collect([
            completion_call_event("run-a", 0, finish_reason="stop"),
            completion_call_event("run-a", 1, finish_reason="pending"),
            completion_call_event("run-a", 2, finish_reason={"other": 7}),
        ])
        self.assertEqual(usage["events"]["malformed"], 2)
        self.assertIsNone(usage["model_calls"])
        self.assertIsNone(usage["finish_reasons"])
        self.assertEqual(usage["coverage"], "partial")

    def test_missing_events_leave_finish_reasons_unknown(self):
        unknown = evaluation.unknown_usage(evaluation.MIO_JSONL)
        self.assertIsNone(unknown["finish_reasons"])
        # A report format that exposes no reason vocabulary adds no key at all.
        self.assertNotIn("finish_reasons", evaluation.unknown_usage())

    def test_top_level_totals_sum_every_task_bucket(self):
        def call(reason):
            counters = {name: None for name in evaluation.USAGE_COUNTERS}
            key, blocks, bucket = evaluation.normalize_completion_call(
                json.loads(completion_call_event("run-a", 0, finish_reason=reason))
            )
            return evaluation.usage_from_calls(
                [blocks], finish_reasons={**{r: 0 for r in evaluation.MIO_FINISH_REASONS}, bucket: 1},
                source=evaluation.MIO_JSONL,
            )

        combined = evaluation.sum_task_usage([call("stop"), call("length")])
        self.assertEqual(combined["finish_reasons"]["stop"], 1)
        self.assertEqual(combined["finish_reasons"]["length"], 1)
        # One task without a reason block blocks the aggregate rather than dropping.
        blocked = evaluation.sum_task_usage(
            [call("stop"), evaluation.unknown_usage(evaluation.MIO_JSONL)]
        )
        self.assertIsNone(blocked["finish_reasons"])


class ConfigTests(unittest.TestCase):
    def test_config_loads_and_resolves_paths(self):
        config = evaluation.load_config(REPO_ROOT / "evals" / "configs" / "mio-local.json")
        self.assertEqual(config["label"], "mio")
        self.assertEqual(config["schema_version"], 1)
        self.assertEqual(
            Path(config["tasks_dir"]).resolve(),
            (REPO_ROOT / "evals" / "tasks" / "coding").resolve(),
        )
        self.assertTrue(Path(config["tasks_dir"]).is_dir())
        self.assertEqual(config["budget"]["version"], "mio-eval-budget-v1")

    def test_deepseek_config_opts_into_json_events(self):
        config = evaluation.load_config(REPO_ROOT / "evals" / "configs" / "mio-deepseek.json")
        self.assertEqual(config["event_format"], "mio-jsonl")
        self.assertEqual(config["timeout_seconds"], 180)
        self.assertIn("--json", config["agent_command"])
        self.assertIn("deepseek-chat", config["agent_command"])
        self.assertEqual(config["budget"]["model"], "deepseek-chat")
        self.assertEqual(config["budget"]["thinking"], "off")
        self.assertEqual(config["budget"]["max_model_calls"], 20)
        # Credentials stay in the environment; no key-shaped value is configured.
        self.assertNotIn("API_KEY", config["agent_command"])

    def test_event_format_resolves_from_config_and_cli(self):
        def namespace(**overrides):
            fields = {
                "label": None, "agent_command": None, "timeout_seconds": None,
                "output": None, "tasks_dir": None, "tasks": None, "event_format": None,
            }
            fields.update(overrides)
            return argparse.Namespace(**fields)

        settings = evaluation.resolve_settings(
            {"event_format": "mio-jsonl"}, namespace()
        )
        self.assertEqual(settings["event_format"], "mio-jsonl")
        overridden = evaluation.resolve_settings(
            {}, namespace(event_format="mio-jsonl")
        )
        self.assertEqual(overridden["event_format"], "mio-jsonl")
        self.assertIsNone(evaluation.resolve_settings({}, namespace())["event_format"])

    def test_cli_overrides_config(self):
        config = evaluation.load_config(REPO_ROOT / "evals" / "configs" / "mio-local.json")
        cli = argparse.Namespace(
            label="cli-label", agent_command=None, timeout_seconds=99.0,
            output=None, tasks_dir=None, tasks=["median-boundaries"],
        )
        settings = evaluation.resolve_settings(config, cli)
        self.assertEqual(settings["label"], "cli-label")
        self.assertEqual(settings["timeout_seconds"], 99.0)
        self.assertEqual(settings["tasks"], ["median-boundaries"])
        # Unset CLI fields keep the config values.
        self.assertEqual(settings["agent_command"], config["agent_command"])
        self.assertEqual(settings["output"], config["output"])

    def test_defaults_apply_without_config(self):
        cli = argparse.Namespace(
            label=None, agent_command=None, timeout_seconds=None,
            output=None, tasks_dir=None, tasks=None,
        )
        settings = evaluation.resolve_settings({}, cli)
        self.assertEqual(settings["label"], evaluation.DEFAULT_LABEL)
        self.assertEqual(settings["agent_command"], evaluation.DEFAULT_AGENT_COMMAND)
        self.assertEqual(settings["timeout_seconds"], evaluation.DEFAULT_TIMEOUT_SECONDS)
        self.assertEqual(settings["tasks_dir"], evaluation.DEFAULT_TASKS_DIR)
        self.assertIsNone(settings["output"])

    def test_declared_sampling_must_be_applied(self):
        evaluation.verify_declared_sampling({}, ["agent", "{prompt}"])
        evaluation.verify_declared_sampling(
            {"temperature": 0, "max_output_tokens": 8192},
            ["mio", "--temperature", "0", "--max-output-tokens", "8192", "{prompt}"],
        )
        evaluation.verify_declared_sampling(
            {"temperature": 0, "max_output_tokens": 8192},
            ["pi", "-e", "/repo/evals/adapters/pi-fixed-sampling.ts", "{prompt}"],
        )
        cases = [
            ({"temperature": 0}, ["agent", "{prompt}"]),
            ({"max_output_tokens": 8192}, ["agent", "{prompt}"]),
            ({"temperature": 0}, ["mio", "--temperature", "1", "{prompt}"]),
            ({"max_output_tokens": 8192},
             ["mio", "--max-output-tokens", "4096", "{prompt}"]),
            ({"temperature": 0}, ["mio", "--temperature", "{prompt}"]),
        ]
        for budget, template in cases:
            with self.subTest(budget=budget, template=template):
                with self.assertRaises(ValueError):
                    evaluation.verify_declared_sampling(budget, template)

    def test_comparison_configs_apply_their_declared_sampling(self):
        for name in ("mio-deepseek-comparison.json", "pi-deepseek-comparison.json"):
            with self.subTest(config=name):
                config = evaluation.load_config(REPO_ROOT / "evals" / "configs" / name)
                template = shlex.split(config["agent_command"])
                evaluation.verify_declared_sampling(
                    evaluation.resolve_budget(config), template
                )

    def test_unknown_config_key_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            path.write_text(json.dumps({"schema_version": 1, "agent_commnd": "x"}))
            with self.assertRaises(ValueError):
                evaluation.load_config(path)

    def test_unknown_event_format_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            path.write_text(json.dumps({"schema_version": 1, "event_format": "pi-json"}))
            with self.assertRaises(ValueError):
                evaluation.load_config(path)

    def test_isolation_and_artifacts_settings_resolve_from_config_and_cli(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            path.write_text(json.dumps({
                "schema_version": 1,
                "agent_command": "agent {prompt}",
                "isolate_agent_env": True,
                "artifacts_dir": "kept-artifacts",
            }))
            config = evaluation.load_config(path)
            self.assertTrue(config["isolate_agent_env"])
            self.assertEqual(
                Path(config["artifacts_dir"]).resolve(),
                (Path(temporary) / "kept-artifacts").resolve(),
            )
            settings = evaluation.resolve_settings(config, argparse.Namespace())
            self.assertTrue(settings["isolate_agent_env"])
            self.assertEqual(
                Path(settings["artifacts_dir"]).resolve(),
                (Path(temporary) / "kept-artifacts").resolve(),
            )
            # A CLI path wins and resolves against the working directory.
            overridden = evaluation.resolve_settings(
                config, argparse.Namespace(artifacts_dir=Path("cli-artifacts"))
            )
            self.assertEqual(overridden["artifacts_dir"], Path("cli-artifacts"))
            self.assertTrue(overridden["isolate_agent_env"])
            # Both stay off when a config does not mention them.
            path.write_text(json.dumps(
                {"schema_version": 1, "agent_command": "agent {prompt}"}
            ))
            plain = evaluation.resolve_settings(evaluation.load_config(path), argparse.Namespace())
            self.assertFalse(plain["isolate_agent_env"])
            self.assertIsNone(plain["artifacts_dir"])

    def test_invalid_isolation_flag_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            for bad in ("yes", 1, [], {}):
                with self.subTest(bad=bad):
                    path.write_text(json.dumps({
                        "schema_version": 1, "agent_command": "agent {prompt}",
                        "isolate_agent_env": bad,
                    }))
                    with self.assertRaises(ValueError):
                        evaluation.load_config(path)

    def test_non_comparison_configs_keep_the_compatible_defaults(self):
        for name in ("mio-local.json", "mio-deepseek.json", "mio-deepseek-workflows.json",
                     "pi-example.json"):
            with self.subTest(config=name):
                config = evaluation.load_config(REPO_ROOT / "evals" / "configs" / name)
                self.assertNotIn("isolate_agent_env", config, name)
                self.assertNotIn("artifacts_dir", config, name)
                settings = evaluation.resolve_settings(config, argparse.Namespace())
                self.assertFalse(settings["isolate_agent_env"], name)
                self.assertIsNone(settings["artifacts_dir"], name)


class ReportTests(unittest.TestCase):
    def test_report_format_is_versioned_and_explicit_about_unknowns(self):
        tasks = [{"id": "sample", "task_set": "probe-set", "task_set_version": 2}]
        results = [{
            "task": "sample", "status": "passed", "failure_class": None,
            "accepted": True, "duration_seconds": 1.0,
            "agent": {"exit_code": 0, "timed_out": False, "seconds": 0.9},
            "acceptance": {"exit_code": 0, "timed_out": False, "seconds": 0.1},
            "usage": dict(evaluation.UNKNOWN_USAGE),
        }]
        report = evaluation.build_report(
            "probe", tasks, results, evaluation.resolve_budget({}),
            ["agent", "--api-key", "SECRETVALUE", "-p", "{prompt}"],
            "2026-01-01T00:00:00Z", 1.5,
        )
        self.assertEqual(report["schema_version"], 2)
        self.assertEqual(report["report_version"], "mio-eval-report-v1")
        self.assertEqual(report["report_type"], "self-acceptance-probe")
        self.assertEqual(report["task_set"], {"id": "probe-set", "version": 2, "tasks": ["sample"]})
        self.assertEqual(report["budget"]["version"], "mio-eval-budget-v1")
        self.assertFalse(report["budget"]["enforced_by_runner"])
        self.assertEqual((report["accepted"], report["total"]), (1, 1))
        self.assertEqual(report["results"], results)
        self.assertIsNone(report["usage"]["total_tokens"])
        self.assertIsNone(report["results"][0]["usage"]["cost_usd"])
        self.assertIn("total_tokens", report["unavailable_metrics"])
        self.assertIn("cost_usd", report["unavailable_metrics"])

    def test_report_redacts_secrets(self):
        report = evaluation.build_report(
            "probe", [{"id": "sample", "task_set": "s", "task_set_version": 1}], [],
            evaluation.resolve_budget({}), ["agent", "--api-key", "SECRETVALUE"],
            "2026-01-01T00:00:00Z", 0.0,
        )
        serialized = json.dumps(report)
        self.assertNotIn("SECRETVALUE", serialized)
        self.assertIn("[redacted]", report["agent_command"])

    def test_redact_command_variants(self):
        self.assertEqual(
            evaluation.redact_command(["mio", "--api-key", "shh", "-p", "{prompt}"]),
            ["mio", "--api-key", "[redacted]", "-p", "{prompt}"],
        )
        self.assertEqual(
            evaluation.redact_command(["mio", "--api-key=shh"]),
            ["mio", "--api-key=[redacted]"],
        )
        blob = "A1b2C3d4" * 6
        self.assertNotIn(blob, evaluation.redact_command([blob]))
        # Paths and templates must survive redaction unchanged.
        path = "/home/user/project/target/debug/mio"
        self.assertEqual(evaluation.redact_command([path, "{prompt}"]), [path, "{prompt}"])


class EndToEndTests(unittest.TestCase):
    def write_config(self, directory, output, label="probe", tasks=None):
        config = {
            "schema_version": 1,
            "label": label,
            "agent_command": f"{sys.executable} -c pass {{prompt}}",
            "timeout_seconds": 30,
            "tasks_dir": str(evaluation.DEFAULT_TASKS_DIR),
            "output": str(output),
        }
        if tasks is not None:
            config["tasks"] = tasks
        path = Path(directory) / "config.json"
        path.write_text(json.dumps(config))
        return path

    def test_runner_config_and_cli_override(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "report.json"
            config_path = self.write_config(temporary, output)
            completed = subprocess.run(
                [sys.executable, str(SCRIPT), "--config", str(config_path)],
                capture_output=True, text=True,
            )
            self.assertEqual(completed.returncode, 1, completed.stderr)
            report = json.loads(output.read_text())
            self.assertEqual(report["label"], "probe")
            self.assertEqual(report["total"], 3)
            self.assertEqual(report["accepted"], 0)
            self.assertEqual(report["task_set"]["id"], "mio-python-acceptance-v1")
            self.assertEqual(report["task_set"]["version"], 1)
            for result in report["results"]:
                self.assertEqual(result["status"], "failed")
                self.assertEqual(result["failure_class"], "acceptance_failed")
                self.assertIsNone(result["usage"]["total_tokens"])

            override = Path(temporary) / "override.json"
            completed = subprocess.run(
                [sys.executable, str(SCRIPT), "--config", str(config_path),
                 "--label", "cli-label", "--task", "csv-reader",
                 "--output", str(override)],
                capture_output=True, text=True,
            )
            self.assertEqual(completed.returncode, 1, completed.stderr)
            report = json.loads(override.read_text())
            self.assertEqual(report["label"], "cli-label")
            self.assertEqual(report["total"], 1)
            self.assertEqual(report["task_set"]["tasks"], ["csv-reader"])

    def test_runner_collects_mio_jsonl_usage_end_to_end(self):
        with tempfile.TemporaryDirectory() as temporary:
            emit = Path(temporary) / "emit_events.py"
            emit.write_text(
                "import json\n"
                "print(json.dumps({'type': 'completion_call', 'data': {\n"
                "    'runId': 'run-fixture', 'callIndex': 0,\n"
                "    'usage': {'input_tokens': 11, 'output_tokens': 2, 'total_tokens': 13},\n"
                "    'responseId': 'resp-0', 'finishReason': 'stop'}}))\n"
            )
            output = Path(temporary) / "report.json"
            config_path = Path(temporary) / "config.json"
            config_path.write_text(json.dumps({
                "schema_version": 1,
                "label": "probe-usage",
                "agent_command": f"{sys.executable} {emit} {{prompt}}",
                "timeout_seconds": 30,
                "tasks_dir": str(evaluation.DEFAULT_TASKS_DIR),
                "output": str(output),
                "event_format": "mio-jsonl",
                "tasks": ["csv-reader", "median-boundaries"],
            }))
            completed = subprocess.run(
                [sys.executable, str(SCRIPT), "--config", str(config_path)],
                capture_output=True, text=True,
            )
            self.assertEqual(completed.returncode, 1, completed.stderr)  # acceptance still fails
            report = json.loads(output.read_text())
            self.assertEqual(report["event_format"], "mio-jsonl")
            self.assertEqual(report["usage"]["model_calls"], 2)
            self.assertEqual(report["usage"]["input_tokens"], 22)
            self.assertEqual(report["usage"]["output_tokens"], 4)
            self.assertEqual(report["usage"]["total_tokens"], 26)
            self.assertEqual(report["usage"]["coverage"], "partial")
            self.assertIsNone(report["usage"]["cost_usd"])
            for result in report["results"]:
                self.assertEqual(result["usage"]["model_calls"], 1)
                self.assertIsNone(result["usage"]["cost_usd"])
            # Raw event payloads are aggregated and dropped, never persisted.
            serialized = output.read_text()
            self.assertNotIn("run-fixture", serialized)
            self.assertNotIn("resp-0", serialized)

    def test_event_format_cli_flag_keeps_usage_unknown_without_events(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "report.json"
            config_path = self.write_config(temporary, output, tasks=["csv-reader"])
            completed = subprocess.run(
                [sys.executable, str(SCRIPT), "--config", str(config_path),
                 "--event-format", "mio-jsonl"],
                capture_output=True, text=True,
            )
            self.assertEqual(completed.returncode, 1, completed.stderr)
            report = json.loads(output.read_text())
            self.assertEqual(report["event_format"], "mio-jsonl")
            self.assertIsNone(report["usage"]["model_calls"])
            self.assertEqual(report["usage"]["coverage"], "unknown")
            self.assertIsNone(report["usage"]["input_tokens"])


class ArtifactEndToEndTests(unittest.TestCase):
    """The runner keeps artifacts only on request, and never copies them into the report."""

    def write_probe_config(self, directory, output, agent_payload):
        agent = Path(directory) / "probe-agent.py"
        agent.write_text(agent_payload)
        config = {
            "schema_version": 1,
            "label": "probe-artifacts",
            "agent_command": f"{sys.executable} {agent} {{prompt}}",
            "timeout_seconds": 30,
            "tasks_dir": str(evaluation.DEFAULT_TASKS_DIR),
            "output": str(output),
            "tasks": ["csv-reader"],
        }
        path = Path(directory) / "config.json"
        path.write_text(json.dumps(config))
        return path

    def test_artifacts_retained_while_the_report_stays_free_of_raw_output(self):
        payload = (
            "import pathlib\n"
            "pathlib.Path('result.txt').write_text('PROBE-WORKSPACE-FILE')\n"
            "print('RAW-AGENT-LOG-MARKER')\n"
        )
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "report.json"
            artifacts = Path(temporary) / "artifacts"
            config = self.write_probe_config(temporary, output, payload)
            completed = subprocess.run(
                [sys.executable, str(SCRIPT), "--config", str(config),
                 "--isolate-agent-env", "--artifacts-dir", str(artifacts)],
                capture_output=True, text=True,
            )
            self.assertEqual(completed.returncode, 1, completed.stderr)
            report_text = output.read_text()
            report = json.loads(report_text)
            self.assertTrue(report["artifacts"]["enabled"])
            self.assertTrue(report["isolation"]["agent_env_isolated"])
            self.assertEqual(report["isolation"]["cleared_env_prefixes"], ["MIO_", "PI_"])
            self.assertEqual(report["isolation"]["kept_env_vars"], ["PI_DELEGATE_ACTIVE"])
            result = report["results"][0]
            directory = Path(result["artifacts"]["directory"])
            self.assertEqual(
                (directory / "workspace" / "result.txt").read_text(),
                "PROBE-WORKSPACE-FILE",
            )
            # The retained log is raw output; the report is not.
            self.assertIn("RAW-AGENT-LOG-MARKER", (directory / "agent.log").read_text())
            self.assertNotIn("RAW-AGENT-LOG-MARKER", report_text)
            self.assertNotIn("PROBE-WORKSPACE-FILE", report_text)

    def test_default_run_keeps_temporary_task_directories(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "report.json"
            config = self.write_probe_config(temporary, output, "pass")
            completed = subprocess.run(
                [sys.executable, str(SCRIPT), "--config", str(config)],
                capture_output=True, text=True,
            )
            self.assertEqual(completed.returncode, 1, completed.stderr)
            report = json.loads(output.read_text())
            self.assertFalse(report["artifacts"]["enabled"])
            self.assertIsNone(report["artifacts"]["root"])
            self.assertFalse(report["isolation"]["agent_env_isolated"])
            self.assertIsNone(report["results"][0]["artifacts"])


class PiUsageEventTests(unittest.TestCase):
    """Aggregation of pi message_end assistant events; no model is called."""

    def collect(self, lines):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "agent.log"
            path.write_text("\n".join(lines) + "\n", encoding="utf-8")
            return evaluation.collect_usage(evaluation.PI_JSONL, path)

    def test_pi_end_maps_cache_inclusive_input_and_drops_catalog_cost(self):
        usage = self.collect([pi_message_end()])
        self.assertEqual(usage["source"], "pi-jsonl")
        self.assertEqual(usage["model_calls"], 1)
        # pi input is the uncached prompt portion; cache is remapped into the total.
        self.assertEqual(usage["input_tokens"], 150)
        self.assertEqual(usage["output_tokens"], 10)
        self.assertEqual(usage["total_tokens"], 160)
        self.assertEqual(usage["cached_input_tokens"], 50)
        self.assertEqual(usage["cache_creation_input_tokens"], 0)
        self.assertEqual(usage["reasoning_tokens"], 4)
        self.assertIsNone(usage["tool_use_prompt_tokens"])
        self.assertEqual(usage["coverage"], "complete")
        self.assertEqual(usage["stop_reasons"]["stop"], 1)
        # pi's usage.cost is a catalog estimate, never actual payment.
        self.assertIsNone(usage["cost_usd"])
        self.assertNotIn("PRIVATE MODEL TEXT", json.dumps(usage))

    def test_pi_missing_cache_counters_stay_null(self):
        usage = self.collect([pi_message_end(usage={"input": 5, "output": 2, "totalTokens": 7})])
        self.assertIsNone(usage["input_tokens"])
        self.assertIsNone(usage["cached_input_tokens"])
        self.assertIsNone(usage["cache_creation_input_tokens"])
        self.assertEqual(usage["output_tokens"], 2)
        self.assertEqual(usage["total_tokens"], 7)
        self.assertIsNone(usage["reasoning_tokens"])
        self.assertEqual(usage["coverage"], "partial")

    def test_pi_explicit_zero_usage_is_reported_not_erased(self):
        zero = {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 0, "reasoning": 0}
        usage = self.collect([pi_message_end(usage=zero)])
        self.assertEqual(usage["input_tokens"], 0)
        self.assertEqual(usage["total_tokens"], 0)
        self.assertEqual(usage["coverage"], "complete")

    def test_pi_self_inconsistent_usage_is_malformed(self):
        inconsistent = {"input": 1, "output": 1, "cacheRead": 1, "cacheWrite": 0,
                        "totalTokens": 999}
        usage = self.collect([
            pi_message_end(response_id="good"),
            pi_message_end(response_id="bad", usage=inconsistent),
        ])
        self.assertEqual(usage["events"]["malformed"], 1)
        self.assertIsNone(usage["model_calls"])
        self.assertIsNone(usage["input_tokens"])
        self.assertEqual(usage["coverage"], "partial")

    def test_pi_deduplicates_by_response_id_then_timestamp(self):
        usage = self.collect([
            pi_message_end(response_id="same"),
            pi_message_end(response_id="same", timestamp=1700000000001),
            pi_message_end(response_id="", timestamp=1700000000002),
            pi_message_end(response_id="", timestamp=1700000000002),
        ])
        self.assertEqual(usage["model_calls"], 2)
        self.assertEqual(usage["input_tokens"], 300)
        self.assertEqual(usage["events"]["duplicates"], 2)

    def test_pi_identity_less_assistant_end_is_malformed(self):
        usage = self.collect([pi_message_end(response_id="", timestamp=12345.5)])
        self.assertEqual(usage["events"]["malformed"], 1)
        self.assertIsNone(usage["model_calls"])

    def test_pi_non_assistant_and_noise_are_ignored(self):
        usage = self.collect([
            "plain stderr text",
            json.dumps({"type": "session", "id": "s", "version": 3}),
            json.dumps({"type": "message_end", "message": {"role": "user", "content": "hi"}}),
            json.dumps({"type": "message_update",
                        "usage": {"input": 5, "output": 1, "cacheRead": 0,
                                  "cacheWrite": 0, "totalTokens": 6},
                        "assistantMessageEvent": {"type": "text_delta", "delta": "x"}}),
            pi_message_end(response_id="r7"),
        ])
        # message_update deltas are cumulative and never counted.
        self.assertEqual(usage["model_calls"], 1)
        self.assertEqual(usage["input_tokens"], 150)
        self.assertEqual(usage["events"]["ignored"], 4)
        self.assertEqual(usage["events"]["malformed"], 0)

    def test_pi_error_and_cancel_coverage_is_explicit(self):
        usage = self.collect([
            pi_message_end(response_id="ok", stop_reason="stop"),
            pi_message_end(response_id="err", stop_reason="error"),
            pi_message_end(response_id="abort", stop_reason="aborted"),
        ])
        self.assertEqual(usage["model_calls"], 3)
        self.assertEqual(usage["stop_reasons"]["error"], 1)
        self.assertEqual(usage["stop_reasons"]["aborted"], 1)
        self.assertEqual(usage["stop_reasons"]["stop"], 1)
        self.assertEqual(usage["stop_reasons"]["length"], 0)

    def test_pi_unknown_stop_reason_is_malformed(self):
        usage = self.collect([pi_message_end(response_id="weird", stop_reason="pending")])
        self.assertEqual(usage["events"]["malformed"], 1)
        self.assertIsNone(usage["model_calls"])

    def test_pi_no_events_stays_unknown(self):
        with tempfile.TemporaryDirectory() as temporary:
            usage = evaluation.collect_usage(
                evaluation.PI_JSONL, Path(temporary) / "missing.log"
            )
        self.assertEqual(usage["coverage"], "unknown")
        self.assertEqual(usage["source"], "pi-jsonl")
        self.assertIsNone(usage["model_calls"])
        self.assertIsNone(usage["stop_reasons"])
        for name in evaluation.USAGE_COUNTERS:
            self.assertIsNone(usage[name], name)

    def test_pi_truncated_scan_does_not_claim_totals(self):
        with patch.object(evaluation, "MAX_EVENT_LOG_LINES", 1):
            usage = self.collect([
                pi_message_end(response_id="r1"),
                pi_message_end(response_id="r2"),
            ])
        self.assertTrue(usage["events"]["truncated"])
        self.assertIsNone(usage["input_tokens"])
        self.assertIsNone(usage["model_calls"])
        self.assertEqual(usage["coverage"], "partial")

    def test_pi_format_is_not_read_by_default(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "agent.log"
            path.write_text(pi_message_end() + "\n", encoding="utf-8")
            usage = evaluation.collect_usage(None, path)
        self.assertEqual(usage["source"], "unknown")
        self.assertIsNone(usage["model_calls"])

    def test_pi_report_totals_require_every_task_to_report(self):
        first = evaluation.usage_from_calls(
            [{"input_tokens": 150, "output_tokens": 10, "total_tokens": 160,
              "cached_input_tokens": 50, "cache_creation_input_tokens": 0,
              "tool_use_prompt_tokens": None, "reasoning_tokens": 4}],
            source=evaluation.PI_JSONL,
        )
        combined = evaluation.sum_task_usage([first, evaluation.unknown_usage(evaluation.PI_JSONL)])
        self.assertIsNone(combined["input_tokens"])
        self.assertEqual(combined["coverage"], "partial")

    def test_pi_unavailable_metrics_mark_only_the_unreportable_counter(self):
        metrics = evaluation.unavailable_metrics(evaluation.PI_JSONL)
        self.assertIn("tool_use_prompt_tokens", metrics)
        self.assertIn("cost_usd", metrics)
        self.assertNotIn("total_tokens", metrics)
        self.assertNotIn("cached_input_tokens", metrics)


class ComparisonConfigTests(unittest.TestCase):
    """The two cross-agent comparison configs must declare the same profile."""

    def load(self, name):
        return evaluation.load_config(REPO_ROOT / "evals" / "configs" / name)

    def test_comparison_profiles_match_and_carry_no_secrets(self):
        mio = self.load("mio-deepseek-comparison.json")
        pi = self.load("pi-deepseek-comparison.json")
        self.assertEqual(mio["timeout_seconds"], pi["timeout_seconds"])
        self.assertEqual(mio["timeout_seconds"], 180)
        self.assertEqual(Path(mio["tasks_dir"]).resolve(), Path(pi["tasks_dir"]).resolve())
        for key in ("provider", "version", "model", "thinking", "temperature",
                    "max_output_tokens", "max_model_calls", "max_total_tokens"):
            self.assertEqual(mio["budget"][key], pi["budget"][key], key)
        self.assertEqual(mio["budget"]["provider"], "deepseek")
        self.assertEqual(mio["budget"]["model"], "deepseek-flash")
        self.assertEqual(mio["budget"]["thinking"], "off")
        self.assertEqual(mio["budget"]["temperature"], 0)
        self.assertEqual(pi["budget"]["temperature"], 0)
        self.assertEqual(mio["budget"]["max_output_tokens"], 8192)
        self.assertEqual(pi["budget"]["max_output_tokens"], 8192)
        # No model-call or token cap is claimed on either side.
        self.assertIsNone(mio["budget"]["max_model_calls"])
        self.assertIsNone(mio["budget"]["max_total_tokens"])
        for config in (mio, pi):
            self.assertNotIn("API_KEY", config["agent_command"])
            self.assertNotIn("--max-model-calls", config["agent_command"])
            self.assertNotIn("--max-total-tokens", config["agent_command"])
            self.assertNotIn("--max-turns", config["agent_command"])

    def test_mio_comparison_command_pins_sampling(self):
        command = self.load("mio-deepseek-comparison.json")["agent_command"]
        self.assertIn("--model deepseek-flash", command)
        self.assertIn("--temperature 0", command)
        self.assertIn("--max-output-tokens 8192", command)
        self.assertNotIn("deepseek-chat", command)

    def test_pi_comparison_command_loads_the_test_extension(self):
        command = self.load("pi-deepseek-comparison.json")["agent_command"]
        self.assertIn("--no-extensions", command)
        self.assertIn(" -e {repo}/evals/adapters/pi-fixed-sampling.ts", command)
        self.assertIn("--model deepseek-flash", command)
        extension = REPO_ROOT / "evals" / "adapters" / "pi-fixed-sampling.ts"
        self.assertTrue(extension.is_file())

    def test_comparison_isolation_is_explicit_on_both_sides(self):
        # Both comparison configs pin isolation, independently of CLI overrides.
        self.assertTrue(self.load("pi-deepseek-comparison.json")["isolate_agent_env"])
        mio = self.load("mio-deepseek-comparison.json")
        self.assertTrue(mio["isolate_agent_env"])
        self.assertEqual(
            evaluation.resolve_settings(mio, argparse.Namespace())["isolate_agent_env"],
            True,
        )
        self.assertTrue(
            evaluation.resolve_settings(mio, argparse.Namespace(isolate_agent_env=True))[
                "isolate_agent_env"
            ]
        )

    def test_pi_comparison_command_uses_the_official_delegated_entry(self):
        config = self.load("pi-deepseek-comparison.json")
        template = shlex.split(config["agent_command"])
        # The only sanctioned Pi entry point wraps pi through pi-delegate.
        self.assertEqual(Path(template[0]).name, "pi-delegate")
        self.assertNotIn("pi", template[:template.index("--")])
        self.assertEqual(template[1:4], ["--owner", "codex", "--heavy"])
        separator = template.index("--")
        self.assertEqual(template[separator + 1:separator + 3], ["--mode", "json"])
        evaluation.verify_declared_sampling(
            evaluation.resolve_budget(config), template
        )

    def test_pi_command_override_drops_only_the_machine_local_wrapper(self):
        config = self.load("pi-deepseek-comparison.json")
        template = shlex.split(config["agent_command"])
        # Overriding the wrapper for a portable or offline check must keep the
        # declared sampling mechanism the profile relies on.
        portable = ["pi"] + template[template.index("--") + 1:]
        self.assertNotIn("pi-delegate", portable)
        evaluation.verify_declared_sampling(
            evaluation.resolve_budget(config), portable
        )

    def test_comparison_event_formats_and_discovery_flags(self):
        mio = self.load("mio-deepseek-comparison.json")
        pi = self.load("pi-deepseek-comparison.json")
        self.assertEqual(mio["event_format"], "mio-jsonl")
        self.assertEqual(pi["event_format"], "pi-jsonl")
        self.assertIn("--json", mio["agent_command"])
        self.assertIn("--mode json", pi["agent_command"])
        for flag in ("--no-mcp", "--no-context", "--no-context-files", "--no-skills",
                     "--no-prompt-templates"):
            self.assertIn(flag, mio["agent_command"], flag)
        for flag in ("--no-mcp", "--no-extensions", "--no-context-files", "--no-skills",
                     "--no-prompt-templates"):
            self.assertIn(flag, pi["agent_command"], flag)
        self.assertIn("--provider deepseek", pi["agent_command"])
        self.assertIn("--thinking off", pi["agent_command"])

    def test_comparison_budget_sampling_fields_are_validated(self):
        base = {
            "schema_version": 1,
            "agent_command": "agent {prompt}",
            "budget": {"version": "probe-v1"},
        }
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            for bad in ({"temperature": -0.1}, {"temperature": 2.5},
                        {"temperature": True}, {"temperature": "0"},
                        {"temperature": float("inf")},
                        {"max_output_tokens": 0}, {"max_output_tokens": -1},
                        {"max_output_tokens": 8192.0}, {"max_output_tokens": True}):
                path.write_text(json.dumps({**base, "budget": {**base["budget"], **bad}}))
                with self.assertRaises(ValueError, msg=bad):
                    evaluation.load_config(path)
            path.write_text(json.dumps({
                **base,
                "budget": {"version": "probe-v1", "temperature": 0,
                           "max_output_tokens": 8192},
            }))
            config = evaluation.load_config(path)
            self.assertEqual(config["budget"]["temperature"], 0)
            self.assertEqual(config["budget"]["max_output_tokens"], 8192)
            budget = evaluation.resolve_budget(config)
            self.assertEqual(budget["temperature"], 0)
            self.assertEqual(budget["max_output_tokens"], 8192)
            # An undeclared profile must not silently invent these fields.
            plain = evaluation.resolve_budget({})
            self.assertNotIn("temperature", plain)
            self.assertNotIn("max_output_tokens", plain)

    def test_pi_jsonl_event_format_is_accepted(self):
        self.assertIn("pi-jsonl", evaluation.EVENT_FORMATS)
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            path.write_text(json.dumps({
                "schema_version": 1, "event_format": "pi-jsonl",
                "agent_command": "pi --mode json -p {prompt}",
            }))
            self.assertEqual(evaluation.load_config(path)["event_format"], "pi-jsonl")


class PiFixedSamplingExtensionTests(unittest.TestCase):
    """Offline checks of the explicit Pi test adapter; no model is contacted."""

    def test_extension_source_pins_sampling(self):
        source = (REPO_ROOT / "evals" / "adapters" / "pi-fixed-sampling.ts").read_text()
        self.assertIn('"deepseek"', source)
        self.assertIn('"deepseek-flash"', source)
        self.assertIn("before_provider_request", source)
        self.assertIn("FIXED_TEMPERATURE = 0", source)
        self.assertIn("FIXED_MAX_OUTPUT_TOKENS = 8192", source)
        self.assertIn("max_tokens", source)

    def test_extension_hook_behaviour(self):
        node = shutil.which("node")
        if node is None:
            self.skipTest("node is not available")
        script = REPO_ROOT / "tests" / "pi_fixed_sampling_extension.mjs"
        completed = subprocess.run(
            [node, str(script)], capture_output=True, text=True, timeout=60
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("ok", completed.stdout)


class EnforcedLimitReportTests(unittest.TestCase):
    def test_report_declares_only_the_wall_timeout_as_enforced(self):
        report = evaluation.build_report(
            "probe", [{"id": "sample", "task_set": "s", "task_set_version": 1}], [],
            evaluation.resolve_budget({}), ["agent", "-p", "{prompt}"],
            "2026-01-01T00:00:00Z", 0.0, "mio-jsonl", 180.0,
        )
        self.assertEqual(report["enforced"]["wall_timeout_seconds"], 180.0)
        self.assertFalse(report["enforced"]["budget_enforced_by_runner"])
        self.assertFalse(report["budget"]["enforced_by_runner"])
        self.assertIn("wall-clock", report["enforced"]["note"])


if __name__ == "__main__":
    unittest.main()
