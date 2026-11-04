import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from riso_claude import REVIEW_SCHEMA, load_review, parse_review, stage_resolution, write_explain_context, write_resolve_context
from riso_common import git, load_config


class ClaudeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name) / 'work'
        self.context = Path(self.directory.name) / 'context'
        self.root.mkdir()
        git(self.root, 'init', '-b', 'product')
        git(self.root, 'config', 'user.name', 'Test')
        git(self.root, 'config', 'user.email', 'test@example.invalid')
        (self.root / 'riso').mkdir()
        shutil.copy(Path(__file__).resolve().parents[2] / 'config.toml', self.root / 'riso/config.toml')
        (self.root / 'riso/hooks.toml').write_text('')
        self.write('README.md', '# Zed\n')
        self.write('file.txt', 'base\n')
        self.write('Cargo.lock', 'lock base\n')
        self.base = self.commit('base')
        self.config = load_config(self.root)

    def write(self, path, text):
        (self.root / path).write_text(text)

    def commit(self, message):
        git(self.root, 'add', '.')
        git(self.root, 'commit', '-m', message)
        return git(self.root, 'rev-parse', 'HEAD').strip()

    def conflict(self, files):
        git(self.root, 'checkout', '-B', 'upstream-work', self.base)
        for path in files:
            self.write(path, f'upstream {path}\n')
        target = self.commit('upstream change')
        git(self.root, 'checkout', 'product')
        for path in files:
            self.write(path, f'fork {path}\n')
        self.commit('fork change')
        result = subprocess.run(['git', 'merge', '--no-ff', target], cwd=self.root, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        return target

    def test_explain_context_only_covers_level_one_commits(self):
        zero = self.base
        self.write('file.txt', 'relevant\n')
        relevant = self.commit('relevant')
        commits = [{'sha': zero, 'short': zero[:7], 'subject': 'zero', 'level': 0, 'adrs': [], 'reasons': []},
                   {'sha': relevant, 'short': relevant[:7], 'subject': 'relevant', 'level': 1, 'adrs': ['ADR-0002'],
                    'reasons': [{'level': 1, 'code': 'dependency_crate'}]}]
        plan = {'commits': commits, 'auto': {'count': 2}, 'blocker': None}
        self.assertTrue(write_explain_context(self.root, plan, 'auto', self.context / 'auto'))
        self.assertEqual(sorted(path.name for path in (self.context / 'auto').iterdir()),
                         sorted(['commits.json', 'prompt.md', 'schema.json', relevant[:7] + '.patch']))
        self.assertEqual(json.loads((self.context / 'auto/schema.json').read_text()), REVIEW_SCHEMA)
        self.assertIn('relevant', (self.context / 'auto' / f'{relevant[:7]}.patch').read_text())
        self.assertEqual([commit['sha'] for commit in json.loads((self.context / 'auto/commits.json').read_text())], [relevant])
        prompt = (self.context / 'auto/prompt.md').read_text()
        self.assertIn('advisory', prompt)
        self.assertIn('never change the level', prompt)
        self.assertIn('Vietnamese', prompt)
        self.assertFalse(write_explain_context(self.root, plan, 'blocked', self.context / 'blocked'))
        self.assertFalse((self.context / 'blocked/prompt.md').exists())

    def test_resolve_context_excludes_cargo_lock(self):
        target = self.conflict(['file.txt', 'Cargo.lock'])
        self.assertTrue(write_resolve_context(self.root, target, self.context))
        self.assertEqual((self.context / 'conflicts.txt').read_text(), 'file.txt\n')
        self.assertIn('upstream file.txt', (self.context / 'upstream.patch').read_text())
        self.assertIn('fork file.txt', (self.context / 'fork.diff').read_text())
        prompt = (self.context / 'prompt.md').read_text()
        self.assertIn('- file.txt', prompt)
        self.assertNotIn('- Cargo.lock', prompt)
        self.assertIn('Do not edit Cargo.lock', prompt)

    def test_resolve_context_skips_claude_for_lock_only_conflicts(self):
        target = self.conflict(['Cargo.lock'])
        self.assertFalse(write_resolve_context(self.root, target, self.context))
        self.assertFalse((self.context / 'prompt.md').exists())

    def test_stage_rejects_markers_and_outside_edits_then_commits(self):
        self.conflict(['file.txt'])
        with self.assertRaisesRegex(RuntimeError, 'Conflict marker remains in file.txt'):
            stage_resolution(self.root, self.config)
        self.write('file.txt', 'resolved\n')
        self.write('other.txt', 'not part of the conflict\n')
        with self.assertRaisesRegex(RuntimeError, 'outside the conflicted paths: other.txt'):
            stage_resolution(self.root, self.config)
        (self.root / 'other.txt').unlink()
        stage_resolution(self.root, self.config)
        self.assertEqual(len(git(self.root, 'rev-list', '--parents', '-n', '1', 'HEAD').split()), 3)
        self.assertEqual(git(self.root, 'status', '--porcelain'), '')
        readme = (self.root / 'README.md').read_text().splitlines()
        self.assertEqual(readme[:2], self.config.policy.review_marker_lines)
        self.assertEqual(readme.count(self.config.policy.review_marker_lines[0]), 1)
        with self.assertRaisesRegex(RuntimeError, 'No merge is in progress'):
            stage_resolution(self.root, self.config)

    def test_stage_regenerates_cargo_lock(self):
        self.conflict(['file.txt', 'Cargo.lock'])
        self.write('file.txt', 'resolved\n')
        real_run = subprocess.run
        cargo_commands = []

        def run(command, **keywords):
            if command[0] == 'cargo':
                cargo_commands.append(command)
                return subprocess.CompletedProcess(command, 0, '', '')
            return real_run(command, **keywords)

        with patch('subprocess.run', side_effect=run):
            stage_resolution(self.root, self.config)
        self.assertEqual(cargo_commands, [['cargo', 'update', '--workspace']])
        self.assertEqual((self.root / 'Cargo.lock').read_text(), 'upstream Cargo.lock\n')
        self.assertEqual(git(self.root, 'status', '--porcelain'), '')

    def test_parse_review_reads_envelope_bare_object_and_fences(self):
        review = {'commits': [{'short': '4ebf3eac14', 'verdict': 'review', 'reason': ' Kiểm tra Cargo.lock '},
                              {'short': 'bad', 'verdict': 'merge it', 'reason': 'x'},
                              {'short': 'nolabel', 'verdict': 'safe'}],
                  'notes': ' Ghi chú '}
        expected = {'notes': 'Ghi chú', 'verdicts': {'4ebf3ea': {'verdict': 'review', 'reason': 'Kiểm tra Cargo.lock'}}}
        envelope = {'type': 'result', 'is_error': False, 'result': '', 'structured_output': review}
        self.assertEqual(parse_review(json.dumps(envelope)), expected)
        self.assertEqual(parse_review(json.dumps(review)), expected)
        fenced = {'type': 'result', 'is_error': False, 'result': '```json\n' + json.dumps(review) + '\n```'}
        self.assertEqual(parse_review(json.dumps(fenced)), expected)

    def test_parse_review_drops_errors_and_keeps_plain_notes(self):
        empty = {'notes': '', 'verdicts': {}}
        self.assertEqual(parse_review(json.dumps({'type': 'result', 'is_error': True, 'result': 'Not logged in'})), empty)
        self.assertEqual(parse_review('Chỉ có văn xuôi'), {'notes': 'Chỉ có văn xuôi', 'verdicts': {}})
        self.assertEqual(parse_review(json.dumps({'type': 'result', 'is_error': False, 'result': 'Văn xuôi'})),
                         {'notes': 'Văn xuôi', 'verdicts': {}})
        self.assertEqual(parse_review(json.dumps([1, 2])), empty)
        self.assertEqual(parse_review(json.dumps({'commits': 'oops', 'notes': 3})), empty)
        self.assertEqual(load_review(str(self.context / 'missing.json')), empty)
        self.assertEqual(load_review(None), empty)


if __name__ == '__main__':
    unittest.main()
