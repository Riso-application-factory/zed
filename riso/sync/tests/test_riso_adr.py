from pathlib import Path
import tempfile
import unittest

from riso_adr import (Hook, HookAnchorError, apply_hooks_to_text, check_repository,
                      is_hook_only, load_adrs, load_hooks)
from riso_common import git


TEMPLATE = '''+++
id = "ADR-0001"
title = "Test customization"
status = "active"
review = "human"
intent = "Preserve the test customization."
owns = ["riso/**", "custom/**"]
patches = []
hooks = []
watch = []
relies_on = []
contracts = ["true"]
+++

## Context
## Decision
## Consequences
## Merge notes
'''


class RepositoryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        git(self.root, "init", "-b", "main")
        git(self.root, "config", "user.name", "Test")
        git(self.root, "config", "user.email", "test@example.invalid")
        self.write("README.md", "# Test\n")
        self.write("upstream.txt", "anchor\n")
        git(self.root, "add", ".")
        git(self.root, "commit", "-m", "base")
        self.base = git(self.root, "rev-parse", "HEAD").strip()
        git(self.root, "update-ref", "refs/remotes/upstream/main", self.base)
        source = Path(__file__).resolve().parents[2] / "config.toml"
        self.write("riso/config.toml", source.read_text())
        self.write("riso/hooks.toml", "")
        self.write("riso/adr/0001-test.md", TEMPLATE)
        self.write("custom/file.txt", "custom\n")

    def write(self, path, text):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def codes(self, **arguments):
        return [error[0] for error in check_repository(self.root, self.base, **arguments)]

    def test_valid_worktree_and_tree_loading(self):
        self.assertEqual(self.codes(), [])
        self.assertEqual(load_adrs(self.root)[0].id, "ADR-0001")
        self.write("riso/adr/0000-template.md", "invalid template is skipped")
        git(self.root, "add", ".")
        git(self.root, "commit", "-m", "customization")
        self.write("riso/adr/0001-test.md", "invalid")
        self.assertEqual(load_adrs(self.root, "HEAD")[0].id, "ADR-0001")
        self.assertEqual(load_hooks(self.root, "HEAD"), [])
        self.assertEqual(check_repository(self.root, self.base, "HEAD"), [])

    def test_schema(self):
        variants = [TEMPLATE.replace('review = "human"', 'review = "bad"'),
                    TEMPLATE.replace('intent = "Preserve the test customization."', ''),
                    TEMPLATE.replace('id = "ADR-0001"', 'id = "ADR-0002"'),
                    TEMPLATE.replace('owns = ["riso/**", "custom/**"]', 'owns = "bad"'),
                    TEMPLATE.replace('## Decision', ''),
                    TEMPLATE.replace('status = "active"', 'status = "superseded"'),
                    'not front matter']
        for text in variants:
            with self.subTest(text=text):
                self.write("riso/adr/0001-test.md", text)
                self.assertIn("schema", self.codes())
        self.write("riso/adr/0001-test.md", TEMPLATE)
        self.write("riso/adr/0001-duplicate.md", TEMPLATE)
        self.assertIn("schema", self.codes())

    def test_uncovered_added_modified_deleted_and_rename(self):
        self.write("uncovered.txt", "new\n")
        self.assertIn("uncovered", self.codes())
        (self.root / "uncovered.txt").unlink()
        self.write("upstream.txt", "changed\n")
        self.assertIn("uncovered", self.codes())
        (self.root / "upstream.txt").unlink()
        self.assertIn("uncovered", self.codes())
        git(self.root, "checkout", "--", "upstream.txt")
        (self.root / "upstream.txt").rename(self.root / "renamed.txt")
        git(self.root, "add", "-A")
        errors = check_repository(self.root, self.base)
        self.assertTrue(any(code == "uncovered" and path == "upstream.txt" for code, path, _ in errors))
        self.assertTrue(any(code == "uncovered" and path == "renamed.txt" for code, path, _ in errors))

    def test_stale_owns_patches_and_missing_hook(self):
        for field, replacement in [('owns = ["riso/**", "custom/**"]', 'owns = ["missing/**"]'),
                                   ('patches = []', 'patches = ["missing.txt"]'),
                                   ('hooks = []', 'hooks = ["absent"]')]:
            self.write("riso/adr/0001-test.md", TEMPLATE.replace(field, replacement))
            self.assertIn("stale", self.codes())

    def test_contracts(self):
        self.write("riso/adr/0001-test.md", TEMPLATE.replace('contracts = ["true"]', 'contracts = []'))
        self.assertIn("contracts", self.codes())

    def test_acknowledgement(self):
        git(self.root, "add", ".")
        git(self.root, "commit", "-m", "customization")
        self.write("custom/file.txt", "changed\n")
        comparison = git(self.root, "rev-parse", "HEAD").strip()
        errors = check_repository(self.root, comparison, pr_body="")
        self.assertEqual([error[0] for error in errors], ["acknowledgement"])
        errors = check_repository(self.root, comparison, pr_body="ADR-0001: unchanged — intent preserved")
        self.assertEqual(errors, [])
        errors = check_repository(self.root, comparison, pr_body="", sync_pr=True)
        self.assertEqual(errors, [])

    def test_existing_fork_files_remain_owned_with_product_comparison_base(self):
        self.write("custom/remaining.txt", "keep the ownership glob live\n")
        git(self.root, "add", ".")
        git(self.root, "commit", "-m", "customization")
        comparison = git(self.root, "rev-parse", "HEAD").strip()
        for operation in ["modify", "delete"]:
            with self.subTest(operation=operation):
                if operation == "modify":
                    self.write("custom/file.txt", "changed\n")
                else:
                    (self.root / "custom/file.txt").unlink()
                self.assertEqual(check_repository(self.root, comparison), [])
                git(self.root, "add", "-A")
                git(self.root, "commit", "-m", operation)
                self.assertEqual(check_repository(self.root, comparison, "HEAD"), [])

    def test_upstream_file_requires_patch_even_with_owns_glob(self):
        self.write("riso/adr/0001-test.md", TEMPLATE.replace('"custom/**"]', '"custom/**", "upstream.txt"]'))
        git(self.root, "add", ".")
        git(self.root, "commit", "-m", "customization")
        comparison = git(self.root, "rev-parse", "HEAD").strip()
        self.write("upstream.txt", "changed upstream\n")
        errors = check_repository(self.root, comparison)
        self.assertEqual([error[0] for error in errors], ["uncovered"])
        self.write("riso/adr/0001-test.md", (self.root / "riso/adr/0001-test.md").read_text().replace('patches = []', 'patches = ["upstream.txt"]'))
        self.assertEqual(check_repository(self.root, comparison), [])

    def test_hook_only_coverage_uses_upstream_base_not_product_base(self):
        self.write("riso/hooks.toml", '[[hooks]]\nid = "test"\nfile = "upstream.txt"\nanchor = "anchor"\nposition = "after"\ntext = "inserted"\n')
        self.write("riso/adr/0001-test.md", TEMPLATE.replace('hooks = []', 'hooks = ["test"]'))
        self.write("upstream.txt", "anchor\ninserted\nextra fork patch\n")
        git(self.root, "add", ".")
        git(self.root, "commit", "-m", "customization with extra patch")
        comparison = git(self.root, "rev-parse", "HEAD").strip()
        self.write("upstream.txt", "anchor\ninserted\n")
        self.assertEqual(check_repository(self.root, comparison), [])

    def test_review_marker_only_exempt_from_coverage(self):
        self.write("README.md", "> [!IMPORTANT]\n> Remove this line to confirm you've reviewed this PR before submitting.\n# Test\n")
        self.assertEqual(self.codes(), ["review_marker"])

    def test_lifecycle(self):
        for status in ["upstreamed", "dropped"]:
            self.write("riso/adr/0001-test.md", TEMPLATE.replace('status = "active"', f'status = "{status}"'))
            self.assertIn("lifecycle", self.codes())

    def test_hooks_stale_and_hook_only_coverage(self):
        self.write("riso/hooks.toml", '[[hooks]]\nid = "test"\nfile = "upstream.txt"\nanchor = "anchor"\nposition = "after"\ntext = "inserted"\n')
        self.assertIn("stale", self.codes())
        self.write("riso/adr/0001-test.md", TEMPLATE.replace('hooks = []', 'hooks = ["test"]'))
        self.assertIn("stale", self.codes())
        self.write("upstream.txt", "anchor\ninserted\n")
        self.assertEqual(self.codes(), [])
        self.write("upstream.txt", "anchor\ninserted\nextra\n")
        self.assertIn("uncovered", self.codes())
        self.write("upstream.txt", "removed anchor line entirely\n")
        self.write("riso/hooks.toml", (self.root / "riso/hooks.toml").read_text().replace('anchor = "anchor"', 'anchor = "absent"'))
        self.assertIn("stale", self.codes())


class HookTests(unittest.TestCase):
    def test_apply_and_idempotency(self):
        for position, expected in [("before", "block\nanchor\n"), ("after", "anchor\nblock\n"), ("end", "anchor\nblock\n")]:
            hook = Hook("test", "file", "anchor", position, "block")
            applied = apply_hooks_to_text("anchor\n", [hook])
            self.assertEqual(applied, expected)
            self.assertEqual(apply_hooks_to_text(applied, [hook]), expected)
            self.assertTrue(is_hook_only("anchor\n", applied, [hook]))
            self.assertFalse(is_hook_only("anchor\n", applied + "extra", [hook]))
        self.assertEqual(apply_hooks_to_text("anchor", [Hook("test", "file", "anchor", "after", "one\ntwo")]), "anchor\none\ntwo\n")

    def test_shared_positions_form_ordered_idempotent_blocks(self):
        hooks = [
            Hook("after-first", "file", "anchor", "after", "first();"),
            Hook("before-first", "file", "anchor", "before", "before_one();"),
            Hook("end-first", "file", "anchor", "end", "end_one();"),
            Hook("after-second", "file", "anchor", "after", "second();"),
            Hook("before-second", "file", "anchor", "before", "before_two();"),
            Hook("end-second", "file", "anchor", "end", "end_two();"),
        ]
        for text in ["anchor\n", "anchor"]:
            with self.subTest(text=text):
                expected = "before_one();\nbefore_two();\nanchor\nfirst();\nsecond();\nend_one();\nend_two();\n"
                applied = apply_hooks_to_text(text, hooks)
                self.assertEqual(applied, expected)
                self.assertEqual(apply_hooks_to_text(applied, hooks), applied)
                self.assertTrue(is_hook_only(text, applied, hooks))
        for position in ["before", "after", "end"]:
            with self.subTest(position=position):
                selected = [hook for hook in hooks if hook.position == position]
                applied = apply_hooks_to_text("anchor\n", selected)
                self.assertEqual(apply_hooks_to_text(applied, selected), applied)
                self.assertLess(applied.index(selected[0].text), applied.index(selected[1].text))

    def test_anchor_errors(self):
        hook = Hook("test", "file", "anchor", "after", "block")
        for text, kind in [("nothing\n", "missing"), ("anchor\nanchor\n", "ambiguous")]:
            with self.assertRaises(HookAnchorError) as raised:
                apply_hooks_to_text(text, [hook])
            self.assertEqual(raised.exception.hook_id, "test")
            self.assertEqual(raised.exception.kind, kind)
