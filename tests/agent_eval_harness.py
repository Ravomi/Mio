"""Offline tests of the evaluation runner; does not assess any actual agent."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

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

    def test_unknown_config_key_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            path.write_text(json.dumps({"schema_version": 1, "agent_commnd": "x"}))
            with self.assertRaises(ValueError):
                evaluation.load_config(path)


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


if __name__ == "__main__":
    unittest.main()
