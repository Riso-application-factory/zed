import contextlib
import copy
import json
import io
import os
from pathlib import Path
import re
import shutil
import sys
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

from riso_common import git, load_config
from riso_prepare import Executor, merge, prepare, publish_pull_request, resolve, sync_mirror, dispatch


def workflow_jobs(file_name):
    workflow = (Path(__file__).resolve().parents[3] / '.github/workflows' / file_name).read_text()
    blocks = re.split(r'^  ([a-z0-9-]+):\n', workflow.split('\njobs:\n', 1)[1], flags=re.MULTILINE)
    return dict(zip(blocks[1::2], blocks[2::2]))


class LocalExecutor(Executor):
    """Run local git mutations while recording remote writes without executing them."""

    def __init__(self, root, replies=None):
        super().__init__(root)
        self.replies = replies or {}

    def run(self, *command, **keywords):
        if command[0] == 'gh':
            assert '--repo' in command and command[command.index('--repo') + 1] == 'fork-owner/zed', command
            assert 'zed-industries/zed' not in command, command
        if command[0] == 'gh' or command[:2] == ('git', 'push'):
            self.commands.append(list(command))
            return self.replies.get(tuple(command[:-2]) if command[0] == 'gh' else tuple(command), '101' if command[:3] == ('gh', 'pr', 'view') else '')
        return super().run(*command, **keywords)

    def body_command(self, command, body):
        assert '--repo' in command and command[command.index('--repo') + 1] == 'fork-owner/zed', command
        assert 'zed-industries/zed' not in command, command
        self.commands.append([*command, '--body-file', '<generated-body>'])
        self.last_body = body
        return ''


class PrepareTests(unittest.TestCase):
    def setUp(self):
        repository_environment = patch.dict(os.environ, {'GH_REPO': 'fork-owner/zed'})
        repository_environment.start()
        self.addCleanup(repository_environment.stop)
        renderers = SimpleNamespace(
            render_auto_body=lambda plan, config, verdicts=None: 'Bản đồng bộ tự động' + (f' {verdicts}' if verdicts else ''),
            render_blocked_body=lambda plan, config, verdicts=None: 'Bản đồng bộ bị chặn' + (f' {verdicts}' if verdicts else ''),
            render_waiting_comment=lambda plan, number: f'Đang chờ #{number}',
        )
        renderer_patch = patch.dict(sys.modules, {'riso_report': renderers})
        renderer_patch.start()
        self.addCleanup(renderer_patch.stop)
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name) / 'work'
        self.root.mkdir()
        git(self.root, 'init', '-b', 'product')
        git(self.root, 'config', 'user.name', 'Test')
        git(self.root, 'config', 'user.email', 'test@example.invalid')
        (self.root / 'riso').mkdir()
        shutil.copy(Path(__file__).resolve().parents[2] / 'config.toml', self.root / 'riso/config.toml')
        (self.root / 'riso/hooks.toml').write_text('')
        self.write('file.txt', 'base\n')
        self.base = self.commit('base')
        for name in ('origin', 'upstream'):
            remote = Path(self.directory.name) / name
            remote.mkdir()
            git(remote, 'init', '--bare')
            git(remote, 'fetch', str(self.root), 'HEAD:refs/heads/main')
            git(self.root, 'remote', 'add', name, str(remote))
            git(self.root, 'fetch', name)
        git(self.root, 'update-ref', 'refs/remotes/origin/riso/main', self.base)
        self.config = load_config(self.root)
        self.output = contextlib.redirect_stdout(io.StringIO())
        self.output.__enter__()
        self.addCleanup(self.output.__exit__, None, None, None)

    def write(self, path, text):
        (self.root / path).write_text(text)

    def commit(self, message):
        git(self.root, 'add', '.')
        git(self.root, 'commit', '-m', message)
        return git(self.root, 'rev-parse', 'HEAD').strip()

    def upstream(self, path='upstream.txt', text='upstream\n'):
        git(self.root, 'checkout', '-B', 'upstream-work', self.base)
        self.write(path, text)
        target = self.commit('upstream')
        git(self.root, 'checkout', 'product')
        git(self.root, 'update-ref', 'refs/remotes/upstream/main', target)
        return target

    def plan(self, target=None, blocker=None, level=1, eligible=None):
        head = git(self.root, 'rev-parse', 'HEAD').strip()
        git(self.root, 'update-ref', 'refs/remotes/origin/riso/main', head)
        upstream = blocker['sha'] if blocker else target or self.base
        records = []
        for sha in git(self.root, 'rev-list', '--reverse', '--first-parent', f'{self.base}..{upstream}').splitlines():
            current_level = blocker['level'] if blocker and sha == blocker['sha'] else level
            records.append({'sha': sha, 'short': sha[:7], 'subject': 'upstream', 'upstream_pr': None,
                            'author': 'Test', 'date': '2026-01-01T00:00:00Z', 'files': [],
                            'baseline_level': current_level, 'level': current_level,
                            'reasons': [], 'adrs': [], 'conflicts': {'mechanical': [], 'real': []}, 'jev': []})
        maximum = level if target else 0
        return {'version': 1, 'generated_at': '2026-01-01T00:00:00Z', 'head': head,
                'base': self.base, 'upstream': upstream, 'upstream_repo': 'test/upstream',
                'jev': {'enabled': False, 'model': 'test'}, 'commits': records,
                'summary': {str(number): sum(record['level'] == number for record in records) for number in range(5)},
                'auto': {'target': target, 'count': int(bool(target)), 'max_level': maximum,
                         'auto_merge_eligible': eligible if eligible is not None else (
                             maximum <= self.config.policy.auto_merge_max_level and not self.config.policy.shadow)},
                'blocker': blocker}

    def test_fork_repository_from_https_and_ssh_origin(self):
        for url in ('https://github.com/fork-owner/zed.git', 'https://github.com/fork-owner/zed',
                    'git@github.com:fork-owner/zed.git', 'ssh://git@github.com/fork-owner/zed.git'):
            with self.subTest(url=url), patch.dict(os.environ):
                os.environ.pop('GH_REPO', None)
                git(self.root, 'remote', 'set-url', 'origin', url)
                executor = Executor(self.root, True)
                publish_pull_request(executor, self.config, 'branch', 'title', 'body', 1)
                self.assert_fork_commands(executor)

    def test_repository_environment_takes_precedence_and_resolves_once(self):
        git(self.root, 'remote', 'set-url', 'origin', 'https://github.com/zed-industries/zed.git')
        executor = Executor(self.root, True)
        publish_pull_request(executor, self.config, 'branch', 'title', 'body', 1)
        with patch.dict(os.environ, {'GH_REPO': 'zed-industries/zed'}):
            publish_pull_request(executor, self.config, 'branch', 'title', 'body', 1)
        self.assert_fork_commands(executor)

    def test_refuse_upstream_or_empty_repository_before_side_effects(self):
        target = self.upstream()
        plan = self.plan(target)
        for repository in ('zed-industries/zed', 'ZED-INDUSTRIES/ZED', ''):
            with self.subTest(repository=repository), patch.dict(os.environ, {'GH_REPO': repository}):
                executor = LocalExecutor(self.root)
                with self.assertRaisesRegex(ValueError, 'upstream repository|Cannot resolve fork repository'):
                    prepare(executor, self.config, plan, push=True, github=True)
                self.assertEqual(executor.commands, [])
        with patch.dict(os.environ):
            os.environ.pop('GH_REPO', None)
            git(self.root, 'remote', 'set-url', 'origin', 'git@github.com:zed-industries/zed.git')
            executor = LocalExecutor(self.root)
            with self.assertRaisesRegex(ValueError, 'upstream repository'):
                prepare(executor, self.config, plan, push=True, github=True)
            self.assertEqual(executor.commands, [])

    def test_upstream_repository_cli_returns_nonzero(self):
        target = self.upstream()
        plan_path = Path(self.directory.name) / 'plan.json'
        plan_path.write_text(json.dumps(self.plan(target)))
        arguments = SimpleNamespace(command='prepare', dry_run=True, plan=str(plan_path), push=True, github=True)
        with patch('riso_prepare.repository_root', return_value=self.root), patch.dict(os.environ, {'GH_REPO': 'zed-industries/zed'}), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(dispatch(arguments), 1)

    def assert_fork_commands(self, executor):
        commands = [command for command in executor.commands if command[0] == 'gh']
        self.assertTrue(commands)
        for command in commands:
            self.assertIn('--repo', command)
            self.assertEqual(command[command.index('--repo') + 1], 'fork-owner/zed')
            self.assertNotIn('zed-industries/zed', command)

    def test_all_github_commands_target_fork_with_upstream_remote(self):
        target = self.upstream()
        executor = Executor(self.root, True)
        with patch.dict(os.environ, {'GH_REPO': 'fork-owner/zed'}):
            prepare(executor, self.config, self.plan(target), push=True, github=True)
        self.assertIn('upstream', git(self.root, 'remote').splitlines())
        commands = [command for command in executor.commands if command[0] == 'gh']
        self.assertTrue(commands)
        for command in commands:
            self.assertIn('--repo', command)
            self.assertEqual(command[command.index('--repo') + 1], 'fork-owner/zed')
            self.assertNotIn('zed-industries/zed', command)

    def test_mirror_fast_forward_command_sequence(self):
        target = self.upstream()
        executor = Executor(self.root, True)
        sync_mirror(executor, self.config)
        self.assertEqual(executor.commands, [
            ['git', 'fetch', 'upstream', 'main'],
            ['git', 'push', 'origin', f'{target}:refs/heads/main']])
        self.assertEqual(git(self.root, 'status', '--porcelain'), '')

    def test_mirror_refuses_non_fast_forward(self):
        self.write('file.txt', 'fork\n')
        fork = self.commit('fork')
        self.upstream('file.txt', 'other\n')
        git(self.root, 'update-ref', 'refs/remotes/origin/main', fork)
        executor = Executor(self.root, True)
        with self.assertRaisesRegex(RuntimeError, 'not a fast-forward'):
            sync_mirror(executor, self.config)
        self.assertEqual(executor.commands, [['git', 'fetch', 'upstream', 'main']])

    def test_clean_auto_merge(self):
        target = self.upstream()
        self.write('fork.txt', 'fork\n')
        self.commit('fork')
        executor = LocalExecutor(self.root)
        prepare(executor, self.config, self.plan(target), push=True)
        self.assertEqual(git(self.root, 'branch', '--show-current').strip(), 'riso/sync/auto')
        self.assertEqual(len(git(self.root, 'rev-list', '--parents', '-n', '1', 'HEAD').split()), 3)
        self.assertEqual(executor.commands[-1], ['git', 'push', '--force-with-lease', 'origin', 'riso/sync/auto'])

    def test_cargo_lock_mechanical_resolution(self):
        self.write('Cargo.lock', 'base\n')
        self.base = self.commit('lock base')
        target = self.upstream('Cargo.lock', 'upstream\n')
        self.write('Cargo.lock', 'fork\n')
        self.commit('fork lock')
        tools = Path(self.directory.name) / 'bin'
        tools.mkdir()
        cargo = tools / 'cargo'
        cargo.write_text('#!/bin/sh\n[ "$1 $2" = "update --workspace" ] || exit 1\nprintf "resolved\\n" > Cargo.lock\n')
        cargo.chmod(0o755)
        executor = LocalExecutor(self.root)
        with patch.dict(os.environ, {'PATH': f'{tools}:{os.environ["PATH"]}'}):
            self.assertTrue(merge(executor, target))
        self.assertEqual((self.root / 'Cargo.lock').read_text(), 'resolved\n')
        self.assertIn(['cargo', 'update', '--workspace'], executor.commands)
        self.assertEqual(executor.commands[-1], ['git', 'commit', '--no-edit'])

    def test_hook_only_resolution(self):
        self.write('riso/hooks.toml', '[[hooks]]\nid="init"\nfile="file.txt"\nanchor="base"\nposition="after"\ntext="custom"\n')
        self.base = self.commit('hook declaration')
        target = self.upstream('file.txt', 'base\nupstream\n')
        self.write('file.txt', 'base\ncustom\n')
        self.commit('hook insertion')
        executor = LocalExecutor(self.root)
        self.assertTrue(merge(executor, target))
        self.assertEqual((self.root / 'file.txt').read_text(), 'base\ncustom\nupstream\n')
        self.assertIn(['git', 'checkout', '--theirs', '--', 'file.txt'], executor.commands)

    def conflict(self):
        target = self.upstream('file.txt', 'upstream\n')
        self.write('file.txt', 'fork\n')
        self.commit('fork')
        return target

    def test_real_conflict_aborts(self):
        target = self.conflict()
        executor = LocalExecutor(self.root)
        self.assertFalse(merge(executor, target))
        self.assertEqual(executor.commands[-1], ['git', 'merge', '--abort'])
        self.assertEqual(git(self.root, 'status', '--porcelain'), '')

    def assert_auto_failure_blocked(self, executor, target, message):
        self.assertTrue(any(command[:3] == ['gh', 'issue', 'create'] for command in executor.commands))
        self.assertFalse(any(command[:2] == ['git', 'push'] for command in executor.commands))
        self.assertFalse(any(command[:3] == ['gh', 'pr', 'create'] for command in executor.commands))
        self.assertFalse(any('--auto' in command for command in executor.commands))
        self.assertIn(['gh', 'label', 'create', 'riso-sync-blocked', '--force', '--repo', 'fork-owner/zed'], executor.commands)
        self.assertIn(['gh', 'label', 'create', 'sync:L3', '--force', '--repo', 'fork-owner/zed'], executor.commands)
        self.assertIn(target, executor.last_body)
        self.assertIn(message, executor.last_body)
        self.assertEqual(git(self.root, 'status', '--porcelain'), '')

    def test_auto_real_conflict_fails(self):
        target = self.conflict()
        executor = LocalExecutor(self.root)
        with self.assertRaisesRegex(RuntimeError, 'Unexpected real conflict'):
            prepare(executor, self.config, self.plan(target), push=True, github=True)
        self.assert_auto_failure_blocked(executor, target, 'file.txt')

    def test_auto_cargo_failure_creates_blocked_issue(self):
        self.write('Cargo.lock', 'base\n')
        self.base = self.commit('lock base')
        target = self.upstream('Cargo.lock', 'upstream\n')
        self.write('Cargo.lock', 'fork\n')
        self.commit('fork lock')
        tools = Path(self.directory.name) / 'bin'
        tools.mkdir()
        cargo = tools / 'cargo'
        cargo.write_text('#!/bin/sh\nprintf "lock regeneration failed\\n" >&2\nexit 1\n')
        cargo.chmod(0o755)
        executor = LocalExecutor(self.root)
        with patch.dict(os.environ, {'PATH': f'{tools}:{os.environ["PATH"]}'}):
            with self.assertRaisesRegex(RuntimeError, 'lock regeneration failed'):
                prepare(executor, self.config, self.plan(target), push=True, github=True)
        self.assert_auto_failure_blocked(executor, target, 'Cargo.lock')
        self.assertIn('lock regeneration failed', executor.last_body)
        self.assertIn(['git', 'merge', '--abort'], executor.commands)

    def test_auto_hook_verification_failure_creates_blocked_issue(self):
        target = self.upstream()
        executor = LocalExecutor(self.root)
        with patch.object(executor, 'verify_hooks', side_effect=RuntimeError('Hooks are not applied in file.txt')):
            with self.assertRaisesRegex(RuntimeError, 'Hooks are not applied'):
                prepare(executor, self.config, self.plan(target), push=True, github=True)
        self.assert_auto_failure_blocked(executor, target, 'file.txt')

    def test_auto_push_failure_never_opens_pull_request(self):
        target = self.upstream()
        executor = LocalExecutor(self.root)
        original = executor.run

        def run(*command, **keywords):
            if command[:2] == ('git', 'push'):
                executor.commands.append(list(command))
                raise RuntimeError('push rejected')
            return original(*command, **keywords)

        executor.run = run
        with self.assertRaisesRegex(RuntimeError, 'push rejected'):
            prepare(executor, self.config, self.plan(target), push=True, github=True)
        self.assertTrue(any(command[:3] == ['gh', 'issue', 'create'] for command in executor.commands))
        self.assertFalse(any(command[:3] == ['gh', 'pr', 'create'] for command in executor.commands))
        self.assertFalse(any('--auto' in command for command in executor.commands))
        self.assertIn('push rejected', executor.last_body)

    def test_auto_failure_updates_existing_blocked_issue(self):
        target = self.conflict()
        executor = LocalExecutor(self.root)
        original = executor.run
        issue_lists = 0

        def run(*command, **keywords):
            nonlocal issue_lists
            if command[:3] == ('gh', 'issue', 'list'):
                issue_lists += 1
                if issue_lists > 1:
                    executor.commands.append(list(command))
                    return '[{"number": 77}]'
            return original(*command, **keywords)

        executor.run = run
        with self.assertRaisesRegex(RuntimeError, 'Unexpected real conflict'):
            prepare(executor, self.config, self.plan(target), push=True, github=True)
        self.assertTrue(any(command[:4] == ['gh', 'issue', 'edit', '77'] for command in executor.commands))
        self.assertFalse(any(command[:3] == ['gh', 'issue', 'create'] for command in executor.commands))

    def test_stale_plan_refused_before_any_side_effect(self):
        target = self.upstream()
        plan = self.plan(target)
        self.write('fork.txt', 'new product tip\n')
        current = self.commit('product advanced')
        git(self.root, 'update-ref', 'refs/remotes/origin/riso/main', current)
        executor = LocalExecutor(self.root)
        with self.assertRaisesRegex(ValueError, 'Stale plan'):
            prepare(executor, self.config, plan, push=True, github=True)
        self.assertEqual(executor.commands, [])

    def test_stale_plan_refused_against_local_product_tip(self):
        target = self.upstream()
        plan = self.plan(target)
        self.write('fork.txt', 'local product advanced\n')
        current = self.commit('product advanced')
        git(self.root, 'update-ref', 'refs/heads/riso/main', current)
        executor = LocalExecutor(self.root)
        with self.assertRaisesRegex(ValueError, 'Stale plan'):
            prepare(executor, self.config, plan, push=True, github=True)
        self.assertEqual(executor.commands, [])

    def test_malformed_plans_refused_before_any_side_effect(self):
        target = self.upstream()
        valid = self.plan(target)
        malformed = [[], {'version': 1}]
        for name, value in [('head', '--invalid'), ('upstream', '0' * 40), ('commits', []),
                            ('auto', {'target': self.base, 'count': 1, 'max_level': 1, 'auto_merge_eligible': False}),
                            ('blocker', {'sha': self.base, 'level': 3, 'mechanical_conflicts': [], 'real_conflicts': []}),
                            ('summary', {'0': True})]:
            plan = copy.deepcopy(valid)
            plan[name] = value
            malformed.append(plan)
        for plan in malformed:
            with self.subTest(plan=plan):
                executor = LocalExecutor(self.root)
                with self.assertRaises((ValueError, RuntimeError)):
                    prepare(executor, self.config, plan, push=True, github=True)
                self.assertEqual(executor.commands, [])

    def test_malformed_plan_cli_returns_nonzero(self):
        plan_path = Path(self.directory.name) / 'plan.json'
        plan_path.write_text(json.dumps({'version': 1}))
        arguments = SimpleNamespace(command='prepare', dry_run=True, plan=str(plan_path), push=True, github=True)
        with patch('riso_prepare.repository_root', return_value=self.root), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(dispatch(arguments), 1)

    def test_blocker_opens_pr_or_issue(self):
        for real_conflict in (False, True):
            with self.subTest(real_conflict=real_conflict):
                target = self.upstream('file.txt', 'upstream\n')
                if real_conflict:
                    self.write('file.txt', 'fork\n')
                    self.commit('fork')
                blocker = {'sha': target, 'level': 3, 'mechanical_conflicts': [], 'real_conflicts': []}
                executor = LocalExecutor(self.root)
                prepare(executor, self.config, self.plan(blocker=blocker), push=True, github=True)
                expected = 'issue' if real_conflict else 'pr'
                self.assertTrue(any(command[:3] == ['gh', expected, 'create'] for command in executor.commands))
                self.assertFalse(any('--auto' in command for command in executor.commands))
                if real_conflict:
                    self.assertIn('resolve ' + target, executor.last_body)
                git(self.root, 'checkout', 'product')

    def test_claude_reviews_reach_the_report_and_notes(self):
        sys.modules['riso_report'].append_explanation = lambda body, text: f'{body}|{text}'
        target = self.upstream()
        executor = LocalExecutor(self.root)
        verdicts = {'abc1234': {'verdict': 'safe', 'reason': 'ổn'}}
        prepare(executor, self.config, self.plan(target), push=True, github=True,
                reviews={'auto': {'notes': 'Ghi chú auto', 'verdicts': verdicts}, 'blocked': {'notes': '', 'verdicts': {}}})
        self.assertEqual(executor.last_body, f'Bản đồng bộ tự động {verdicts}|Ghi chú auto')
        git(self.root, 'checkout', 'product')
        blocker = {'sha': self.upstream('file.txt', 'upstream\n'), 'level': 3, 'mechanical_conflicts': [], 'real_conflicts': []}
        executor = LocalExecutor(self.root)
        prepare(executor, self.config, self.plan(blocker=blocker), push=True, github=True,
                reviews={'blocked': {'notes': 'Ghi chú blocked', 'verdicts': {}}})
        self.assertEqual(executor.last_body, 'Bản đồng bộ bị chặn|Ghi chú blocked')

    def test_dry_run_prints_generated_bodies(self):
        target = self.upstream()
        executor = Executor(self.root, True)
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            prepare(executor, self.config, self.plan(target), push=True, github=True)
        self.assertIn('Bản đồng bộ tự động', output.getvalue())

    def test_generated_bodies_reach_the_step_summary(self):
        target = self.upstream()
        summary = Path(self.directory.name) / 'summary.md'
        with patch.dict(os.environ, {'GITHUB_STEP_SUMMARY': str(summary)}):
            prepare(Executor(self.root, True), self.config, self.plan(target), push=True, github=True)
        written = summary.read_text()
        self.assertIn('<!-- pr create -->\nBản đồng bộ tự động', written)
        self.assertEqual(written.count('Bản đồng bộ'), 1)

    def test_waiting_blocked_exits_before_git_mutation(self):
        for kind in ('pr', 'issue'):
            with self.subTest(kind=kind):
                command = ('gh', kind, 'list', '--state', 'open', '--label', 'riso-sync-blocked', '--json', 'number')
                executor = LocalExecutor(self.root, {command: '[{"number": 42}]'})
                self.assertEqual(prepare(executor, self.config, self.plan(), github=True), 0)
                self.assertFalse(any(command[0] == 'git' for command in executor.commands))
                self.assertEqual(executor.commands[-1][:4], ['gh', kind, 'comment', '42'])

    def test_auto_merge_enable_disable_and_shadow(self):
        for shadow, level, eligible, enabled in ((False, 1, True, True), (True, 1, True, False),
                                                  (False, 2, True, False), (False, 1, False, False)):
            with self.subTest(shadow=shadow, level=level, eligible=eligible):
                self.config.policy['shadow'] = shadow
                executor = Executor(self.root, True)
                publish_pull_request(executor, self.config, 'branch', 'title', 'body', level, eligible=eligible)
                expected = ['gh', 'pr', 'merge', '--auto', '--merge', '<new-pr-number>', '--repo', 'fork-owner/zed'] if enabled else ['gh', 'pr', 'merge', '--disable-auto', '<new-pr-number>', '--repo', 'fork-owner/zed']
                self.assertEqual(executor.commands[-1], expected)
                self.assertIn(['gh', 'label', 'create', f'sync:L{level}', '--force', '--repo', 'fork-owner/zed'], executor.commands)

    def test_existing_pr_edited(self):
        command = ('gh', 'pr', 'list', '--state', 'open', '--head', 'branch', '--base', 'riso/main', '--json', 'number')
        executor = LocalExecutor(self.root, {command: '[{"number": 9}]'})
        publish_pull_request(executor, self.config, 'branch', 'title', 'body', 1)
        self.assertTrue(any(command[:4] == ['gh', 'pr', 'edit', '9'] for command in executor.commands))
        self.assertEqual(executor.commands[-1], ['gh', 'pr', 'merge', '--disable-auto', '9', '--repo', 'fork-owner/zed'])

    def test_resolve_leaves_conflicts_then_finish_detects_markers(self):
        target = self.conflict()
        git(self.root, 'update-ref', 'refs/remotes/origin/riso/main', 'HEAD')
        executor = LocalExecutor(self.root)
        resolve(executor, self.config, target)
        self.assertTrue(git(self.root, 'diff', '--name-only', '--diff-filter=U').strip())
        self.assertNotIn(['git', 'merge', '--abort'], executor.commands)
        self.write('file.txt', '<<<<<<< remaining\n')
        git(self.root, 'add', '.')
        with self.assertRaises(RuntimeError):
            resolve(executor, self.config, target, True)
        git(self.root, 'commit', '-m', 'unresolved marker')
        with self.assertRaisesRegex(RuntimeError, 'Conflict marker'):
            resolve(executor, self.config, target, True)
        self.write('file.txt', 'resolved\n')
        self.commit('resolved')
        resolve(executor, self.config, target, True)
        self.assertIn(['git', 'push', '--force-with-lease', 'origin', 'riso/sync/blocked/' + target[:7]], executor.commands)
        self.assertFalse(any('--auto' in command for command in executor.commands))

    def test_resolve_from_explicit_start_and_publish_claude_summary(self):
        target = self.conflict()
        fork = git(self.root, 'rev-parse', 'HEAD').strip()
        git(self.root, 'update-ref', 'refs/remotes/origin/riso/sync/auto', self.base)
        executor = LocalExecutor(self.root)
        with self.assertRaisesRegex(RuntimeError, 'does not exist'):
            resolve(executor, self.config, target, start='origin/missing')
        resolve(executor, self.config, target, start=fork)
        self.assertIn(['git', 'checkout', '-B', 'riso/sync/blocked/' + target[:7], fork], executor.commands)
        self.write('file.txt', 'resolved\n')
        self.commit('resolved')
        sys.modules['riso_report'].append_explanation = lambda body, text, **keywords: f'{body}|{keywords["heading"]}|{text}'
        resolve(executor, self.config, target, True, claude_summary='Giữ cả hai thay đổi')
        self.assertIn('Claude sửa các file mã', executor.last_body)
        self.assertTrue(executor.last_body.endswith('|Cách Claude giải xung đột|Giữ cả hai thay đổi'))
        self.assertTrue(any(command[:3] == ['gh', 'pr', 'create'] and '--label' in command and 'riso-sync-blocked' in command
                            for command in executor.commands))

    def test_disable_auto_handles_only_not_enabled_errors(self):
        executor = Executor(self.root)
        response = SimpleNamespace(returncode=1, stderr='auto-merge is not enabled', stdout='')
        with patch('riso_prepare.subprocess.run', return_value=response):
            executor.run('gh', 'pr', 'merge', '--disable-auto', '9', '--repo', 'fork-owner/zed', allowed_errors=('not enabled',))
        response.stderr = 'authentication failed'
        with patch('riso_prepare.subprocess.run', return_value=response):
            with self.assertRaisesRegex(RuntimeError, 'authentication failed'):
                executor.run('gh', 'pr', 'merge', '--disable-auto', '9', '--repo', 'fork-owner/zed', allowed_errors=('not enabled',))

    def test_dry_run_blocker_issue_sequence(self):
        target = self.upstream()
        blocker = {'sha': target, 'level': 3, 'real_conflicts': ['file.txt'], 'mechanical_conflicts': []}
        executor = Executor(self.root, True)
        prepare(executor, self.config, self.plan(blocker=blocker), github=True, push=True)
        self.assertIn(['git', 'merge', '--abort'], executor.commands)
        self.assertTrue(any(command[:3] == ['gh', 'issue', 'create'] for command in executor.commands))
        self.assertFalse(any(command[:2] == ['git', 'push'] for command in executor.commands))

    def test_workflow_pull_request_credentials_are_read_only(self):
        jobs = workflow_jobs('riso_ci.yml')
        self.assertEqual(list(jobs), ['preflight', 'lint', 'adr-policy', 'clippy', 'build-and-test', 'ci-passed', 'notify-failure'])
        for name in ('lint', 'adr-policy', 'clippy', 'build-and-test'):
            block = jobs[name]
            self.assertIn('    needs: preflight\n' if name in ('lint', 'adr-policy') else '    needs: [preflight, lint]\n', block)
            self.assertIn("    if: needs.preflight.outputs.run-checks == 'true'\n", block)
            self.assertIn('    permissions:\n      contents: read\n', block)
            self.assertNotIn(': write', block)
            self.assertIn('persist-credentials: false', block)
        preflight = jobs['preflight']
        self.assertIn("github.repository_owner != 'zed-industries'", preflight)
        self.assertIn('    permissions:\n      contents: read\n      pull-requests: read\n', preflight)
        self.assertNotIn(': write', preflight)
        self.assertNotIn('actions/checkout', preflight)
        self.assertNotIn('python3 riso/', preflight)
        aggregate = jobs['ci-passed']
        self.assertIn('    permissions: {}\n', aggregate)
        self.assertIn('!cancelled()', aggregate)
        self.assertNotIn('actions/checkout', aggregate)
        reporting = jobs['notify-failure']
        self.assertIn('    needs: ci-passed\n', reporting)
        self.assertIn("needs.ci-passed.result == 'failure'", reporting)
        self.assertIn("github.event_name == 'push'", reporting)
        self.assertIn("github.repository_owner != 'zed-industries'", reporting)
        self.assertIn('issues: write', reporting)
        self.assertNotIn('actions/checkout', reporting)
        self.assertNotIn('python3 riso/', reporting)

    def test_workflow_uses_app_token_and_bot_identity(self):
        jobs = workflow_jobs('riso_upstream_sync.yml')
        self.assertEqual(list(jobs), ['mirror', 'plan', 'explain', 'apply', 'resolve', 'notify-failure'])
        for name in ('mirror', 'apply', 'notify-failure'):
            first_step = jobs[name].split('    steps:\n', 1)[1].split('\n      - ', 1)[0]
            self.assertIn('uses: actions/create-github-app-token@fee1f7d63c2ff003460e3d139729b119787bc349 # v2.2.2', first_step)
            self.assertIn('id: app-token', first_step)
            self.assertIn('app-id: ${{ secrets.RISO_BOT_APP_ID }}', first_step)
            self.assertIn('private-key: ${{ secrets.RISO_BOT_PRIVATE_KEY }}', first_step)
            self.assertNotIn('owner:', first_step)
            self.assertNotIn('repositories:', first_step)
        for name in ('mirror', 'apply'):
            self.assertIn('token: ${{ steps.app-token.outputs.token }}', jobs[name])
        apply = jobs['apply']
        self.assertIn('    needs: [plan, explain]\n', apply)
        self.assertIn("needs.plan.result == 'success'", apply)
        preparation = apply.split('      - name: Prepare review requests\n', 1)[1]
        self.assertIn('GH_TOKEN: ${{ steps.app-token.outputs.token }}', preparation)
        self.assertIn('GH_REPO: ${{ github.repository }}', preparation)
        self.assertNotIn('github.token', preparation)
        self.assertIn("git config user.name 'riso-tech-bot[bot]'", apply)
        self.assertIn("git config user.email '351886810+riso-tech-bot[bot]@users.noreply.github.com'", apply)
        self.assertIn('    permissions:\n      contents: read\n      issues: write\n', jobs['notify-failure'])
        checks = (Path(__file__).resolve().parents[3] / '.github/workflows/riso_ci.yml').read_text()
        self.assertIn('GH_TOKEN: ${{ github.token }}', checks)
        self.assertNotIn('app-token', checks)

    def test_workflow_keeps_jev_key_away_from_write_credentials(self):
        jobs = workflow_jobs('riso_upstream_sync.yml')
        plan = jobs['plan']
        self.assertIn('JEV_API_KEY: ${{ secrets.JEV_API_KEY }}', plan)
        self.assertIn('persist-credentials: false', plan)
        self.assertIn('    permissions:\n      contents: read\n', plan)
        self.assertNotIn(': write', plan)
        self.assertNotIn('app-token', plan)
        self.assertNotIn('RISO_BOT', plan)
        for name in ('mirror', 'explain', 'apply', 'resolve', 'notify-failure'):
            self.assertNotIn('JEV_API_KEY', jobs[name])

    def test_workflow_runs_claude_without_write_credentials(self):
        jobs = workflow_jobs('riso_upstream_sync.yml')
        for name in ('mirror', 'plan', 'apply', 'notify-failure'):
            self.assertNotIn('CLAUDE_CODE_OAUTH_TOKEN', jobs[name])
        for name in ('explain', 'resolve'):
            block = jobs[name]
            self.assertIn('persist-credentials: false', block)
            invocations = [line for line in block.splitlines() if 'claude --print' in line]
            self.assertTrue(invocations)
            for line in invocations:
                self.assertIn('--restricted', line)
                self.assertIn('--strict-mcp-config', line)
                self.assertIn('--permission-mode dontAsk', line)
                tools = line.split('--tools ', 1)[1].split()[0].split(',')
                self.assertNotIn('Bash', tools)
                self.assertNotIn('WebFetch', tools)
        explain = jobs['explain']
        self.assertIn('    permissions:\n      contents: read\n    steps:', explain)
        self.assertNotIn('app-token', explain)
        self.assertNotIn('--tools Read,Grep,Glob,Edit', explain)
        resolve = jobs['resolve']
        self.assertIn('    needs: [plan, apply]\n', resolve)
        self.assertIn('inputs.dry_run != true', resolve)
        claude_step = resolve.split('      - name: Resolve conflicts with Claude\n', 1)[1].split('\n      - ', 1)[0]
        self.assertIn('CLAUDE_CODE_OAUTH_TOKEN', claude_step)
        self.assertNotIn('GH_TOKEN', claude_step)
        self.assertNotIn('github.token', claude_step)
        self.assertLess(resolve.index('Resolve conflicts with Claude'), resolve.index('uses: actions/create-github-app-token'))
        self.assertLess(resolve.index('claude-stage-resolution'), resolve.index('git remote set-url origin'))

    def test_workflow_records_sync_failure_and_honors_dry_run(self):
        reporting = workflow_jobs('riso_upstream_sync.yml')['notify-failure']
        self.assertIn('    needs: [mirror, plan, apply]\n', reporting)
        condition = next(line for line in reporting.splitlines() if line.startswith('    if: '))
        self.assertIn("contains(needs.*.result, 'failure')", condition)
        self.assertNotIn('failure()', condition)
        self.assertNotIn('actions/checkout', reporting)
        handler = reporting.split('      - name: Record unexpected sync failure\n', 1)[1]
        self.assertIn('GH_TOKEN: ${{ steps.app-token.outputs.token || github.token }}', handler)
        self.assertIn('if [ "$DRY_RUN" = true ]', handler)
        self.assertIn('--label riso-sync-blocked', handler)
        self.assertIn('--label sync:L3', handler)
        self.assertIn('gh issue comment', handler)
        self.assertIn('gh issue create', handler)

    def test_dry_run_preserves_worktree(self):
        target = self.upstream()
        before = git(self.root, 'rev-parse', 'HEAD')
        executor = Executor(self.root, True)
        prepare(executor, self.config, self.plan(target), push=True, github=True)
        self.assertEqual(git(self.root, 'rev-parse', 'HEAD'), before)
        self.assertEqual(git(self.root, 'status', '--porcelain'), '')
        expected = [
            ['gh', 'pr', 'list', '--state', 'open', '--label', 'riso-sync-blocked', '--json', 'number'],
            ['gh', 'issue', 'list', '--state', 'open', '--label', 'riso-sync-blocked', '--json', 'number'],
            ['git', 'checkout', '-B', 'riso/sync/auto', before.strip()],
            ['git', 'merge', '--no-ff', '-m', f'Merge upstream {target[:7]} (riso sync, max L1)', target],
            ['python3', 'riso/sync/riso_sync.py', 'hooks', 'verify'],
            ['git', 'push', '--force-with-lease', 'origin', 'riso/sync/auto'],
            ['gh', 'label', 'create', 'riso-sync', '--force'],
            ['gh', 'label', 'create', 'sync:L1', '--force'],
            ['gh', 'pr', 'list', '--state', 'open', '--head', 'riso/sync/auto', '--base', 'riso/main', '--json', 'number'],
            ['gh', 'pr', 'create', '--base', 'riso/main', '--head', 'riso/sync/auto',
             '--title', f'Sync upstream → {target[:7]} (L1)', '--label', 'riso-sync', '--label', 'sync:L1',
             '--repo', 'fork-owner/zed', '--body-file', '<generated-body>'],
            ['gh', 'pr', 'view', 'riso/sync/auto', '--json', 'number', '--jq', '.number'],
            ['gh', 'pr', 'merge', '--disable-auto', '<new-pr-number>'],
        ]
        expected = [command + ['--repo', 'fork-owner/zed'] if command[0] == 'gh' and '--repo' not in command else command for command in expected]
        self.assertEqual(executor.commands, expected)


if __name__ == '__main__':
    unittest.main()
