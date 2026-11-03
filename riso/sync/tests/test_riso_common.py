import tempfile
from pathlib import Path
import unittest

from riso_common import git, glob_matches, load_config


class CommonTests(unittest.TestCase):
    def test_globs(self):
        for pattern, path, expected in [
            ("dir/**", "dir/file", True), ("dir/**", "dir/sub/file", True),
            ("dir/*", "dir/file", True), ("dir/*", "dir/sub/file", False),
            ("a?c", "abc", True), ("a?c", "a/c", False),
            ("a[bc].rs", "ab.rs", False), ("a[bc].rs", "a[bc].rs", True),
            ("*.rs", "file.rs.bak", False), ("a+b", "a+b", True),
        ]:
            with self.subTest(pattern=pattern, path=path):
                self.assertEqual(glob_matches(pattern, path), expected)

    def test_git_errors_include_stderr(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, "not a git repository"):
                git(directory, "status")

    def test_configuration(self):
        root = Path(__file__).resolve().parents[3]
        configuration = load_config(root)
        self.assertEqual(configuration.branches.product, "riso/main")
        self.assertTrue(configuration["policy"]["shadow"])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "riso"
            path.mkdir()
            (path / "config.toml").write_text('[branches]\nproduct = "test"\n')
            with self.assertRaisesRegex(ValueError, "branches.mirror"):
                load_config(directory)
