"""Offline regression for the production dependency boundary guard."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("architecture", ROOT / "scripts/check-architecture.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ArchitectureTests(unittest.TestCase):
    def test_indirect_upward_dependency_is_rejected(self):
        graph = {"mio-core": {"transport"}, "transport": {"mio-coding-agent"}}
        self.assertEqual(MODULE.violations(graph), ["mio-core -> transport -> mio-coding-agent"])

    def test_downward_dependencies_and_cycles_terminate(self):
        graph = {"mio-coding-agent": {"mio-agent"}, "mio-agent": {"mio-core"},
                 "mio-core": {"transport"}, "transport": {"mio-core"}}
        self.assertEqual(MODULE.violations(graph), [])

    def test_renamed_inherited_target_dependency_is_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text('''[workspace]
members = [".", "core"]
[workspace.dependencies]
upper = {package = "mio-agent", version = "0.1"}
''')
            (root / "core").mkdir()
            (root / "core/Cargo.toml").write_text('''[package]
name = "mio-core"
version = "0.1.0"
[target.'cfg(unix)'.dependencies]
upper = {workspace = true}
[dev-dependencies]
mio-coding-agent = "0.1"
''')
            graph = MODULE.load_graph(root)
            self.assertEqual(MODULE.violations(graph), ["mio-core -> mio-agent"])


if __name__ == "__main__":
    unittest.main()
