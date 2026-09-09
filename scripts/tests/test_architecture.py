"""Regression checks for dependency paths that can bypass a naive manifest check."""
import importlib.util
import pathlib
import unittest

SPEC = importlib.util.spec_from_file_location("architecture", pathlib.Path(__file__).resolve().parents[1] / "check-architecture.py")
architecture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(architecture)


class ArchitectureTests(unittest.TestCase):
    def check(self, **tables):
        return architecture.violations([{"package": {"name": "oracle-core"}, **tables}], {})

    def test_core_ports_allow_contracts_and_test_adapters(self):
        self.assertEqual(self.check(dependencies={"oracle-contracts": {}}, **{"dev-dependencies": {"oracle-storage": {}, "serenity": "1"}}), [])

    def test_renamed_target_dependency_cannot_hide_adapter(self):
        errors = self.check(target={"cfg(unix)": {"dependencies": {"discord": {"package": "serenity", "version": "1"}}}})
        self.assertTrue(any("serenity" in error for error in errors))

    def test_build_dependency_cannot_invert_core_boundary(self):
        errors = self.check(**{"build-dependencies": {"oracle-storage": {"path": "../oracle-storage"}}})
        self.assertTrue(any("oracle-storage" in error for error in errors))

    def test_workspace_alias_cannot_hide_host_from_sdk(self):
        manifest = {"package": {"name": "oracle-module-sdk"}, "dependencies": {"host": {"workspace": True}}}
        errors = architecture.violations([manifest], {"host": {"package": "oracle-modules", "path": "crates/oracle-modules"}})
        self.assertTrue(any("oracle-modules" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
