#!/usr/bin/env python3
r"""Run small coding tasks against an agent and verify the resulting files.

This is a self-acceptance probe set, not a public benchmark. It measures whether
one fixed task set is solved under one declared model/budget profile; it does not
collect token, cache or cost data, and its score is not comparable to leaderboard
numbers. See evals/README.md for the full caveats.

Example (uses the model credentials already configured in the environment):
  python3 scripts/agent_eval.py --label mio --output target/evals/mio.json
  python3 scripts/agent_eval.py --config evals/configs/mio-local.json
  python3 scripts/agent_eval.py --config evals/configs/pi-example.json \
    --label pi --timeout 240 --output target/evals/pi.json

Each agent runs in a fresh task directory. Commands are argv templates, never
shell scripts. A command may include {cwd} and {prompt}; absent {cwd}, its working
directory is still the task directory. Use environment variables for credentials:
reports never contain the process environment, and argv templates are redacted
before they are written out.
"""

import argparse
import datetime
import json
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import sys
import tempfile
import time

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TASKS_DIR = REPO_ROOT / "evals" / "tasks" / "coding"
DEFAULT_AGENT_COMMAND = (
    "target/debug/mio --cwd {cwd} --no-session --no-context --no-mcp -p {prompt}"
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
    "budget",
)

OVERRIDABLE = ("label", "agent_command", "timeout_seconds", "output", "tasks_dir", "tasks")

DEFAULT_SETTINGS = {
    "label": DEFAULT_LABEL,
    "agent_command": DEFAULT_AGENT_COMMAND,
    "timeout_seconds": DEFAULT_TIMEOUT_SECONDS,
    "output": None,
    "tasks_dir": DEFAULT_TASKS_DIR,
    "tasks": None,
}

DEFAULT_BUDGET = {
    "version": DEFAULT_BUDGET_VERSION,
    "model": None,
    "thinking": None,
    "max_model_calls": None,
    "max_total_tokens": None,
}

# Fields this probe cannot obtain from an opaque argv template.
UNKNOWN_USAGE = {
    "input_tokens": None,
    "output_tokens": None,
    "total_tokens": None,
    "cost_usd": None,
    "cache_hit_tokens": None,
    "source": "unknown",
}

UNAVAILABLE_METRICS = [
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "cost_usd",
    "cache_hit_tokens",
    "quality_leadership",
    "cross_vendor_comparison",
]

LIMITS = (
    "Small self-acceptance probes; the runner does not read model usage, so token, "
    "cache and cost fields stay null. Scores are meaningful only for the same task "
    "set, model and budget profile, and are not a public-benchmark claim. Agent logs "
    "are written under a temporary directory and discarded to avoid persisting model "
    "output or credentials."
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
    for key in ("tasks_dir", "output"):
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
    budget = config.get("budget")
    if budget is not None:
        if not isinstance(budget, dict):
            raise ValueError(f"{path}: 'budget' must be an object")
        if not isinstance(budget.get("version"), str) or not budget["version"]:
            raise ValueError(f"{path}: 'budget.version' must be a non-empty string")
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


def run_process(argv, cwd, timeout, log):
    """Bound wall time, reap the process group, and keep logs off the Python heap."""
    started = time.monotonic()
    with log.open("wb") as output:
        process = subprocess.Popen(
            argv, cwd=cwd, stdin=subprocess.DEVNULL, stdout=output,
            stderr=subprocess.STDOUT, start_new_session=(os.name == "posix"),
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


def evaluate(task, template, timeout, root):
    task_root = Path(root) / task["id"]
    work = task_root / "workspace"
    work.mkdir(parents=True)
    for name, content in task["files"].items():
        target = work / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)
    argv = [part.replace("{cwd}", str(work)).replace("{prompt}", task["prompt"])
            for part in template]
    try:
        agent = run_process(argv, work, timeout, task_root / "agent.log")
    except OSError as error:
        agent = {"exit_code": None, "timed_out": False, "seconds": 0.0, "error": str(error)}
    # Even a failing agent may have changed files, so record validation separately.
    try:
        acceptance = run_process(
            acceptance_argv(task["acceptance"], work), work,
            task["acceptance_timeout_seconds"], task_root / "check.log",
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
        "usage": dict(UNKNOWN_USAGE),
    }


def build_report(label, tasks, results, budget, agent_command, started_at, duration_seconds):
    sets = {(task["task_set"], task["task_set_version"]) for task in tasks}
    task_set, task_set_version = next(iter(sets))
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
        "agent_command": redact_command(list(agent_command)),
        "accepted": sum(result["accepted"] for result in results),
        "total": len(results),
        "results": results,
        "usage": dict(UNKNOWN_USAGE, note="not collected by this probe"),
        "unavailable_metrics": list(UNAVAILABLE_METRICS),
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
    started_at = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds").replace(
        "+00:00", "Z"
    )
    started = time.monotonic()
    results = []
    with tempfile.TemporaryDirectory(prefix="mio-eval-") as temporary:
        root = Path(temporary)
        for task in tasks:
            result = evaluate(task, template, settings["timeout_seconds"], root)
            results.append(result)
            print(json.dumps(result), flush=True)
    report = build_report(
        settings["label"], tasks, results, budget, template,
        started_at, round(time.monotonic() - started, 3),
    )
    settings["output"].parent.mkdir(parents=True, exist_ok=True)
    settings["output"].write_text(json.dumps(report, indent=2) + "\n")
    return 0 if report["accepted"] == report["total"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
