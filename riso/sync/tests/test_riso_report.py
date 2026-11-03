import copy
from pathlib import Path
import unittest

from riso_common import load_config
from riso_report import render_auto_body, render_blocked_body, render_waiting_comment, sanitize


class ReportTest(unittest.TestCase):
    def setUp(self):
        self.config = load_config(Path(__file__).resolve().parents[3])
        self.commit = {'sha': 'a' * 40, 'short': 'aaaaaaa', 'subject': 'Hello @person | new\nline (#123)',
                       'upstream_pr': 123, 'level': 1, 'adrs': ['ADR-0001'],
                       'reasons': [{'level': 1, 'adr': 'ADR-0001', 'code': 'hook_file', 'paths': ['hook.rs'], 'message': 'hook @person'}],
                       'conflicts': {'mechanical': ['hook.rs'], 'real': []}, 'jev': []}
        self.plan = {'head': 'b' * 40, 'upstream': 'a' * 40, 'base': 'c' * 40,
                     'upstream_repo': 'zed-industries/zed', 'jev': {'model': 'jev-1.14.0'},
                     'commits': [self.commit], 'auto': {'target': 'a' * 40, 'count': 1, 'max_level': 1, 'auto_merge_eligible': False},
                     'blocker': {'sha': 'a' * 40, 'level': 3, 'real_conflicts': ['hook.rs'], 'mechanical_conflicts': []}}

    def test_body_and_sanitization(self):
        body = render_auto_body(self.plan, self.config)
        self.assertIn('Ảnh hưởng tới custom', body)
        self.assertIn('⏸ (shadow)', body)
        self.assertIn('`123`', body)
        self.assertNotIn('/pull/', body)
        self.assertNotIn('(#123)', body)
        self.assertNotIn('@person', body)
        self.assertIn('@\u200bperson', body)
        self.assertIn('\\|', body)
        self.assertIn('/commit/' + 'a' * 40, body)
        self.assertIn('<!-- riso-sync-plan head=', body)
        value = sanitize('[click](https://github.com/zed-industries/zed/pull/123) zed-industries/zed#123 <script>@all</script>')
        self.assertNotIn('/pull/', value)
        self.assertNotIn('zed#123', value)
        self.assertNotIn('<script>', value)
        self.assertNotIn('@all', value)

    def test_blocked_waiting_and_jev(self):
        self.commit['jev'] = [{'adr': 'ADR-0001', 'model': 'jev-1.14.0', 'decision_level': 3,
                               'notes': 'jev_uncertain', 'answers': {'impact': {'score': 2, 'confidence': .4}}}]
        self.assertIn('jev_uncertain', render_blocked_body(self.plan, self.config))
        self.assertIn('Cần người kiểm tra', render_blocked_body(self.plan, self.config))
        self.assertIn('`27`', render_waiting_comment(self.plan, 27))

    def test_truncation_preserves_marker(self):
        zero = copy.deepcopy(self.commit)
        zero['level'] = 0
        zero['subject'] = 'zero' * 20000
        self.plan['commits'].append(zero)
        self.plan['auto']['count'] = 2
        body = render_auto_body(self.plan, self.config)
        self.assertLess(len(body), 60000)
        self.assertNotIn('<details>', body)
        self.assertIn('riso-sync-plan', body)
        self.commit['subject'] = 'huge' * 20000
        body = render_auto_body(self.plan, self.config)
        self.assertLess(len(body), 60000)
        self.assertTrue(body.rstrip().endswith('-->'))
        self.assertIn('Đã rút gọn', body)


if __name__ == '__main__':
    unittest.main()
