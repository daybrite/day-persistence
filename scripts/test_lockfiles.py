"""Synthetic Git repositories exercise the staged/working-tree boundary without commits."""

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check-lockfiles.py")
spec = importlib.util.spec_from_file_location("check_lockfiles", SCRIPT)
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


class LockfileTests(unittest.TestCase):
    def test_allows_owned_paths_and_external_sources(self):
        lock = {"package": [
            {"name": "fixture-owned"},
            {"name": "fixture-git", "source": "git+https://example.invalid/fixture#123"},
            {"name": "fixture-registry", "source": "registry+https://example.invalid/index"},
        ]}
        self.assertEqual(guard.problems(lock, {"fixture-owned"}), [])

    def test_rejects_external_paths_and_unused_local_patches(self):
        lock = {
            "package": [{"name": "fixture-external"}],
            "patch": {"unused": [{"name": "fixture-unused"}]},
        }
        self.assertEqual(len(guard.problems(lock, set())), 2)

    def test_allows_unused_git_patch(self):
        lock = {"patch": {"unused": [
            {"name": "fixture-unused", "source": "git+https://example.invalid/fixture#123"}
        ]}}
        self.assertEqual(guard.problems(lock, set()), [])


class StagedLockfileTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "--quiet")
        for index, manifest in enumerate(guard.MANIFESTS):
            path = self.root / manifest
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f'[package]\nname = "fixture-owned-{index}"\n', encoding="utf-8")
        self.valid = 'version = 4\n[[package]]\nname = "fixture-owned-0"\nversion = "0.1.0"\n'
        for lockfile in guard.LOCKFILES:
            (self.root / lockfile).write_text(self.valid, encoding="utf-8")
        self.git("add", ".")

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)

    def check(self, *args):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args], cwd=self.root, capture_output=True, text=True
        )

    def test_staged_valid_lock_ignores_unstaged_local_build(self):
        (self.root / "Cargo.lock").write_text(
            self.valid.replace("fixture-owned-0", "fixture-local-day"), encoding="utf-8"
        )
        self.assertEqual(self.check("--staged").returncode, 0)
        self.assertNotEqual(self.check().returncode, 0)

    def test_staged_invalid_demo_is_not_hidden_by_worktree_repair(self):
        path = self.root / "demo/Cargo.lock"
        path.write_text(self.valid.replace("fixture-owned-0", "fixture-local-day"), encoding="utf-8")
        self.git("add", "demo/Cargo.lock")
        path.write_text(self.valid, encoding="utf-8")
        result = self.check("--staged")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("demo/Cargo.lock: fixture-local-day", result.stderr)
        self.assertEqual(self.check().returncode, 0)

    def test_missing_staged_lock_fails_closed(self):
        self.git("rm", "--cached", "Cargo.lock")
        self.assertNotEqual(self.check("--staged").returncode, 0)

    def test_invalid_toml_fails_closed(self):
        (self.root / "Cargo.lock").write_text("[invalid", encoding="utf-8")
        self.git("add", "Cargo.lock")
        self.assertNotEqual(self.check("--staged").returncode, 0)

    def test_reads_package_names_from_index(self):
        (self.root / guard.MANIFESTS[0]).write_text(
            '[package]\nname = "fixture-renamed"\n', encoding="utf-8"
        )
        self.assertEqual(self.check("--staged").returncode, 0)
        self.assertNotEqual(self.check().returncode, 0)


if __name__ == "__main__":
    unittest.main()
