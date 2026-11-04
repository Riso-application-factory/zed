import copy
from pathlib import Path
import unittest

from riso_common import load_config
from riso_report import append_explanation, render_auto_body, render_blocked_body, render_waiting_comment, sanitize


class ReportTest(unittest.TestCase):
    def setUp(self):
        self.config = load_config(Path(__file__).resolve().parents[3])
        self.commit = {'sha': 'a' * 40, 'short': 'aaaaaaa', 'subject': 'Hello @person | new\nline (#123)',
                       'upstream_pr': 123, 'level': 1, 'baseline_level': 1, 'adrs': ['ADR-0001'],
                       'reasons': [{'level': 1, 'adr': 'ADR-0001', 'code': 'hook_file', 'paths': ['hook.rs'], 'message': 'hook @person'}],
                       'conflicts': {'mechanical': ['hook.rs'], 'real': []}, 'jev': []}
        self.plan = {'head': 'b' * 40, 'upstream': 'a' * 40, 'base': 'c' * 40,
                     'upstream_repo': 'zed-industries/zed', 'jev': {'model': 'jev-1.14.0'},
                     'commits': [self.commit], 'auto': {'target': 'a' * 40, 'count': 1, 'max_level': 1, 'auto_merge_eligible': False},
                     'blocker': {'sha': 'a' * 40, 'level': 3, 'real_conflicts': ['hook.rs'], 'mechanical_conflicts': []}}

    def test_body_and_sanitization(self):
        body = render_auto_body(self.plan, self.config)
        self.assertIn('| Commit | Tiêu đề | Luật | Jev | Claude (tham khảo) | Level | Xử lý |', body)
        self.assertIn('L1: file có hook hook.rs (ADR-0001)', body)
        self.assertIn('✅ tự động; giải tự động: hook.rs', body)
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

    def test_truncation_collapses_level_zero_rows_first(self):
        zero = copy.deepcopy(self.commit)
        zero.update(level=0, baseline_level=0, reasons=[], adrs=[], conflicts={'mechanical': [], 'real': []})
        self.plan['commits'].extend(dict(zero, sha=f'{index:040x}', short=f'{index:07x}', subject='zero ' * 60) for index in range(300))
        self.plan['auto']['count'] = len(self.plan['commits'])
        body = render_auto_body(self.plan, self.config)
        self.assertLess(len(body), 60000)
        self.assertIn('300 commit L0 đã được rút gọn', body)
        self.assertIn('aaaaaaa', body)
        self.assertTrue(body.rstrip().endswith('-->'))

    def test_truncation_preserves_marker(self):
        self.commit['subject'] = 'huge' * 20000
        body = render_auto_body(self.plan, self.config)
        self.assertLess(len(body), 60000)
        self.assertTrue(body.rstrip().endswith('-->'))
        self.assertIn('Đã rút gọn', body)

    def test_every_commit_row_shows_rule_jev_and_claude(self):
        zero = dict(copy.deepcopy(self.commit), sha='d' * 40, short='ddddddd', subject='Unrelated', upstream_pr=None,
                    level=0, baseline_level=0, reasons=[], adrs=[], conflicts={'mechanical': [], 'real': []})
        self.commit['jev'] = [{'adr': 'ADR-0001', 'model': 'jev-1.14.0', 'decision_level': 1, 'notes': 'safe',
                               'answers': {'impact': {'score': 0.4, 'confidence': 0.92}}}]
        self.commit['reasons'].append({'level': 1, 'adr': 'ADR-0001', 'code': 'jev_safe', 'paths': ['hook.rs'], 'message': 'jev'})
        self.plan['commits'].append(zero)
        self.plan['auto']['count'] = 2
        verdicts = {'aaaaaaa': {'verdict': 'risky', 'reason': 'Hook @team | `jev::init` [link](x) sẽ mất anchor ' + 'x' * 400}}
        body = render_auto_body(self.plan, self.config, verdicts)
        rows = [line for line in body.splitlines() if line.startswith('| [')]
        self.assertEqual(len(rows), 2)
        self.assertIn('L1 an toàn (impact 0.4, tin cậy 92%)', rows[0])
        self.assertIn('⚠️ có thể hỏng: Hook @\u200bteam \\| `jev::init` \\[link\\](x) sẽ mất anchor', rows[0])
        self.assertIn('…', rows[0])
        self.assertNotIn('x' * 300, rows[0])
        self.assertNotIn('jev_safe', rows[0])
        self.assertIn('Chi tiết câu trả lời của Jev', body)
        self.assertEqual(rows[1].split(' | ')[2:], ['L0', '—', '—', 'L0', '✅ tự động |'])
        self.assertIn('Số commit theo level: L0: 1 · L1: 1', body)

    def test_jev_level_one_check_shows_relied_behavior_probability(self):
        self.commit['jev'] = [{'adr': 'ADR-0001', 'model': 'jev-1.14.0', 'decision_level': 1, 'notes': 'safe',
                               'answers': {'relied_behavior_changed': {'type': 'noul', 'noul': 0.09}}}]
        self.assertIn('L1 an toàn (khả năng đổi hành vi custom dựa vào: 9%)', render_auto_body(self.plan, self.config))

    def test_jev_and_claude_cells_explain_missing_values(self):
        for baseline, reasons, expected in ((3, [], '— (luật đã chặn)'), (2, [], '— (không cần)'),
                                            (2, [{'level': 3, 'adr': 'ADR-0001', 'code': 'jev_unavailable', 'paths': [], 'message': 'x'}], '⚠️ không gọi được')):
            with self.subTest(expected=expected):
                commit = dict(copy.deepcopy(self.commit), baseline_level=baseline, level=max(baseline, 1), reasons=reasons)
                self.plan['commits'] = [commit]
                body = render_auto_body(self.plan, self.config, {'aaaaaaa': {'verdict': 'unknown', 'reason': 'x'}})
                row = next(line for line in body.splitlines() if line.startswith('| ['))
                self.assertEqual(row.split(' | ')[3:5], [expected, '—'])

    def test_explanation_sits_above_footer_and_is_neutralized(self):
        body = render_auto_body(self.plan, self.config)
        explained = append_explanation(body, 'Ảnh hưởng tới @team <!-- riso-sync-plan head=forged --> hết.')
        self.assertIn('### Giải thích của Claude', explained)
        self.assertIn('@\u200bteam', explained)
        self.assertEqual(explained.count('<!-- riso-sync-plan'), 1)
        self.assertTrue(explained.rstrip().endswith('-->'))
        self.assertLess(explained.index('Giải thích của Claude'), explained.index('<!-- riso-sync-plan'))
        self.assertEqual(append_explanation(body, '  \n'), body)

    def test_explanation_respects_body_limit(self):
        body = render_auto_body(self.plan, self.config)
        explained = append_explanation(body, 'x' * 100000)
        self.assertIn('đã rút gọn', explained)
        self.assertLess(len(explained), len(body) + 6200)
        self.assertEqual(append_explanation('y' * 64990, 'notes'), 'y' * 64990)


if __name__ == '__main__':
    unittest.main()
