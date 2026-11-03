import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from riso_common import git, load_config
from riso_triage import build_plan
from riso_jev import JevClient


class RepositoryTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        git(self.root, "init", "-b", "main")
        git(self.root, "config", "user.email", "test@example.com")
        git(self.root, "config", "user.name", "Test")
        self.configuration = load_config(Path(__file__).resolve().parents[3])
        self.write("Cargo.toml", '[workspace.dependencies]\ndep = {path = "crates/dep"}\n')
        self.write("crates/dep/Cargo.toml", '[package]\nname = "dep"\nversion = "0.1.0"\n')
        self.write("crates/dep/src/lib.rs", 'pub fn used() {}\npub fn other() {}\n')
        self.write("hook.rs", "anchor();\n")
        self.write("watched.rs", "first\nsecond\nthird\n")
        self.write("Cargo.lock", "base lock\n")
        self.base = self.commit("base")
        git(self.root, "branch", "upstream")
        self.write("riso/hooks.toml", "")
        self.write("custom/Cargo.toml", '[dependencies]\ndep = {workspace = true}\n')
        self.write("custom/lib.rs", "use dep::used;\n")
        self.adr()
        self.head = self.commit("customization")

    def write(self, path, text):
        destination = self.root / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(text)

    def commit(self, title):
        git(self.root, "add", ".")
        git(self.root, "commit", "-m", title)
        return git(self.root, "rev-parse", "HEAD").strip()

    def adr(self, review="llm", patches=None, watch=None, hooks=None):
        metadata = {'id': 'ADR-0001', 'title': 'Custom', 'status': 'active', 'review': review,
                    'intent': 'Keep the custom behavior.', 'owns': ['custom/**'], 'patches': patches or [],
                    'watch': watch or [], 'hooks': hooks or [], 'relies_on': ['dep::used'],
                    'contracts': ['true']}
        self.write("riso/adr/0001-custom.md", "+++\n" + "\n".join(f'{name} = {json.dumps(value)}' for name, value in metadata.items()) +
                   "\n+++\n## Context\n## Decision\n## Consequences\n## Merge notes\n")

    def fork_commit(self):
        self.head = self.commit("customization update")

    def upstream(self, updates, title="upstream (#123)"):
        git(self.root, "checkout", "upstream")
        for path, text in updates.items():
            if text is None:
                git(self.root, "rm", path)
            else:
                self.write(path, text)
        return self.commit(title)

    def plan(self, no_jev=True, client=None):
        return build_plan(self.root, self.head, "upstream", no_jev, self.configuration, client)

    def codes(self, plan):
        return {reason['code'] for reason in plan['commits'][-1]['reasons']}

    def test_l0_and_empty(self):
        empty = self.plan()
        self.assertEqual(empty['commits'], [])
        self.upstream({'unrelated': 'hello'})
        plan = self.plan()
        self.assertEqual(plan['summary']['0'], 1)
        self.assertEqual(plan['auto']['count'], 1)
        self.assertEqual(plan['commits'][0]['upstream_pr'], 123)

    def test_dependency_and_symbol(self):
        self.upstream({'crates/dep/src/lib.rs': 'pub fn used() {}\npub fn other() { changed(); }\n'})
        self.assertEqual(self.plan()['commits'][-1]['level'], 1)
        self.upstream({'crates/dep/src/lib.rs': 'pub fn used(argument: bool) {}\npub fn other() { changed(); }\n'})
        plan = self.plan()
        self.assertEqual(plan['commits'][-1]['level'], 2)
        self.assertIn('symbol_definition', self.codes(plan))
        with patch.dict('os.environ', {}, clear=True):
            unavailable = self.plan(no_jev=False)
        self.assertIn('jev_unavailable', self.codes(unavailable))
        self.assertEqual(unavailable['commits'][-1]['level'], 3)

    def test_dependency_jev_escalates_with_fake_opener(self):
        self.upstream({'crates/dep/src/lib.rs': 'pub fn used() {}\npub fn other() { changed(); }\n'})
        calls = []
        def opener(request, timeout):
            payload = json.loads(request.data)
            calls.append(payload)
            answers = {'relied_behavior_changed': {'noul': .8}}
            if 'impact' in payload['questions']:
                answers.update({'impact': {'score': 1, 'confidence': .9},
                                'contract_0_broken': {'noul': .1}, 'restructure': {'noul': .9}})
            return io.StringIO(json.dumps({'model': 'jev-1.14.0', 'answers': answers}))
        with patch.dict('os.environ', {'JEV_API_KEY': 'fake'}, clear=True):
            plan = self.plan(no_jev=False, client=JevClient(self.configuration, opener))
        self.assertEqual(plan['commits'][0]['baseline_level'], 1)
        self.assertEqual(plan['commits'][0]['level'], 4)
        self.assertEqual(len(calls), 2)
        self.assertEqual(len(plan['commits'][0]['jev']), 2)

    def test_watch_and_human_review(self):
        self.adr(review='human', watch=['watched.rs'])
        self.fork_commit()
        self.upstream({'watched.rs': 'new\nsecond\nthird\n'})
        plan = self.plan()
        self.assertEqual(plan['commits'][0]['baseline_level'], 3)
        self.assertIn('review_policy', self.codes(plan))
        self.assertIsNone(plan['auto']['target'])

    def test_llm_policy_and_state_limit(self):
        self.adr(watch=['watched.rs'])
        self.fork_commit()
        self.upstream({'watched.rs': 'new\nsecond\nthird\n'})
        self.assertEqual(self.plan()['commits'][0]['level'], 2)
        self.configuration.jev['max_state_characters'] = 1
        self.assertIn('jev_state_too_large', self.codes(self.plan(no_jev=False)))

    def test_patched_file_and_surface_removed(self):
        self.write('watched.rs', 'first\nsecond\nfork\n')
        self.adr(patches=['watched.rs'])
        self.fork_commit()
        self.upstream({'watched.rs': 'upstream\nsecond\nthird\n'})
        self.assertIn('patched_file', self.codes(self.plan()))
        self.upstream({'watched.rs': None})
        self.assertIn('surface_removed', self.codes(self.plan()))
        self.assertEqual(self.plan()['commits'][-1]['level'], 4)

    def test_conflict_attributed_once(self):
        self.write('watched.rs', 'fork\nsecond\nthird\n')
        self.fork_commit()
        self.upstream({'watched.rs': 'upstream\nsecond\nthird\n'})
        self.upstream({'other': 'something'})
        plan = self.plan()
        self.assertEqual(plan['commits'][0]['level'], 3)
        self.assertEqual(plan['commits'][0]['conflicts']['real'], ['watched.rs'])
        self.assertEqual(plan['commits'][1]['conflicts']['real'], [])

    def test_mechanical_lock(self):
        self.write('Cargo.lock', 'fork lock\n')
        self.fork_commit()
        self.upstream({'Cargo.lock': 'upstream lock\n'})
        plan = self.plan()
        self.assertIn('mechanical_conflict', self.codes(plan))
        self.assertEqual(plan['commits'][0]['conflicts']['mechanical'], ['Cargo.lock'])
        self.assertFalse(plan['commits'][0]['conflicts']['real'])

    def hook_setup(self):
        self.write('riso/hooks.toml', '[[hooks]]\nid="custom-init"\nfile="hook.rs"\nanchor="anchor();"\nposition="after"\ntext="custom();"\n')
        self.write('hook.rs', 'anchor();\ncustom();\n')
        self.adr(hooks=['custom-init'])
        self.fork_commit()

    def test_hook_file_and_missing_anchor(self):
        self.hook_setup()
        self.upstream({'hook.rs': 'before();\nanchor();\n'})
        self.assertIn('hook_file', self.codes(self.plan()))
        self.assertEqual(self.plan()['commits'][0]['level'], 1)
        self.upstream({'hook.rs': 'before();\n'})
        self.assertIn('hook_anchor', self.codes(self.plan()))
        self.assertEqual(self.plan()['commits'][-1]['level'], 4)

    def test_hook_ambiguous_anchor(self):
        self.hook_setup()
        self.upstream({'hook.rs': 'anchor();\nanchor();\n'})
        self.assertIn('hook_anchor', self.codes(self.plan()))
        self.assertEqual(self.plan()['commits'][0]['level'], 4)

    def test_hook_mechanical_conflict(self):
        self.hook_setup()
        self.upstream({'hook.rs': 'anchor();\nupstream();\n'})
        plan = self.plan()
        self.assertEqual(plan['commits'][0]['conflicts']['mechanical'], ['hook.rs'])
        self.assertEqual(plan['commits'][0]['level'], 1)

    def test_rename_watch_and_auto_prefix(self):
        self.adr(watch=['watched.rs'])
        self.fork_commit()
        self.upstream({'unrelated': 'hello'})
        git(self.root, 'mv', 'watched.rs', 'renamed.rs')
        self.commit('rename')
        plan = self.plan()
        self.assertEqual(plan['auto']['count'], 1)
        self.assertEqual(plan['blocker']['level'], 4)
        self.assertEqual(plan['commits'][-1]['files'][0]['old_path'], 'watched.rs')


if __name__ == '__main__':
    unittest.main()
