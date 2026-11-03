import copy
import io
import json
from pathlib import Path
import unittest
from unittest.mock import patch
from urllib.error import HTTPError, URLError

from riso_adr import Adr
from riso_common import load_config
from riso_jev import JevClient, JevError, StateTooLarge, build_state, combine, evaluate, questions


class JevTest(unittest.TestCase):
    def setUp(self):
        self.config = load_config(Path(__file__).resolve().parents[3])
        self.adr = Adr('ADR-0001', 'Custom', 'active', 'llm', intent='Keep custom behavior',
                       relies_on=['dep::used'], contracts=['cargo test custom'], obsolete_if='Extension hook')
        self.answers = {'impact': {'type': 'score', 'score': 1, 'confidence': .9},
                        'relied_behavior_changed': {'type': 'noul', 'noul': .1},
                        'contract_0_broken': {'type': 'noul', 'noul': .1},
                        'upstream_overlap': {'type': 'noul', 'noul': .1},
                        'restructure': {'type': 'noul', 'noul': .1}}
        self.state = build_state(self.adr, 'Change', [], 'diff', 'custom diff', self.config)

    def response(self, answers=None):
        return io.StringIO(json.dumps({'model': 'jev-1.14.0', 'answers': answers or self.answers}))

    def test_question_schema_and_filtered_state(self):
        full = questions(self.adr)
        self.assertEqual(len(full['impact']['criteria']), 4)
        self.assertEqual(set(questions(self.adr, False)), {'relied_behavior_changed'})
        for question in full.values():
            if question['type'] == 'noul':
                self.assertEqual(set(question['criteria']), {'true', 'false'})
        self.assertEqual(set(self.state), {'customization', 'upstream_change', 'custom_code'})
        self.assertNotIn('upstream_pr_body', json.dumps(self.state))
        self.config.jev['max_state_characters'] = 1
        with self.assertRaises(StateTooLarge):
            build_state(self.adr, 'Change', [], 'diff', '', self.config)

    def test_combine_exact_order(self):
        self.assertEqual(combine(self.answers, self.config), (2, 'safe'))
        answers = copy.deepcopy(self.answers)
        answers['restructure']['noul'] = .9
        self.assertEqual(combine(answers, self.config)[0], 4)
        answers['contract_0_broken']['noul'] = .95
        self.assertEqual(combine(answers, self.config)[0], 4)
        answers['impact']['confidence'] = .5
        self.assertEqual(combine(answers, self.config), (3, 'jev_uncertain'))
        answers['impact']['confidence'] = .9
        for boundary in (.3, .7):
            answers['relied_behavior_changed']['noul'] = boundary
            self.assertEqual(combine(answers, self.config), (3, 'jev_uncertain'))
        answers = copy.deepcopy(self.answers)
        answers['contract_0_broken']['noul'] = .8
        self.assertEqual(combine(answers, self.config)[0], 3)
        answers = copy.deepcopy(self.answers)
        answers['impact']['score'] = 2.5
        self.assertEqual(combine(answers, self.config)[0], 3)
        answers['impact']['score'] = 1.5
        self.assertEqual(combine(answers, self.config)[0], 3)
        answers['impact']['score'] = 1
        answers['impact']['confidence'] = .75
        self.assertEqual(combine(answers, self.config)[0], 2)
        answers['relied_behavior_changed']['noul'] = .9
        self.assertEqual(combine(answers, self.config)[0], 3)

    @patch.dict('os.environ', {'JEV_API_KEY': 'fake'}, clear=True)
    def test_client_headers_and_retry(self):
        calls = []
        delays = []
        def opener(request, timeout):
            calls.append(request)
            if len(calls) < 3:
                raise HTTPError(request.full_url, 429 if len(calls) == 1 else 529, 'retry', {'retry-after': '3'}, None)
            return self.response()
        result = JevClient(self.config, opener, delays.append).request(self.state, questions(self.adr))
        self.assertEqual(result['model'], 'jev-1.14.0')
        self.assertEqual(delays, [3, 3])
        self.assertEqual(calls[0].get_header('Authorization'), 'Bearer fake')
        self.assertNotIn('Typesafe-danger-zone-long-context', calls[0].headers)
        payload = json.loads(calls[0].data)
        self.assertEqual(set(payload), {'state', 'model', 'questions'})

    @patch.dict('os.environ', {'JEV_API_KEY': 'fake'}, clear=True)
    def test_retry_limit_and_other_failures(self):
        calls = []
        def retry(request, timeout):
            calls.append(request)
            raise HTTPError(request.full_url, 529, 'overloaded', {}, None)
        with self.assertRaises(JevError):
            JevClient(self.config, retry, lambda delay: None).request(self.state, questions(self.adr))
        self.assertEqual(len(calls), self.config.jev.max_retries + 1)
        for status in (400, 401, 422):
            def fail(request, timeout):
                raise HTTPError(request.full_url, status, 'failed', {}, None)
            with self.assertRaises(JevError):
                JevClient(self.config, fail).request(self.state, questions(self.adr))
        def network(request, timeout):
            raise URLError('offline')
        with self.assertRaises(JevError):
            JevClient(self.config, network).request(self.state, questions(self.adr))
        with self.assertRaises(JevError):
            JevClient(self.config, lambda request, timeout: io.StringIO('{}')).request(self.state, questions(self.adr))

    def test_unavailable_and_size_do_not_call_opener(self):
        def forbidden(*arguments, **keywords):
            self.fail('opener must not be called')
        with patch.dict('os.environ', {}, clear=True), self.assertRaises(JevError):
            JevClient(self.config, forbidden).request(self.state, questions(self.adr))
        self.config.jev['max_state_characters'] = 1
        with self.assertRaises(StateTooLarge):
            JevClient(self.config, forbidden).request(self.state, questions(self.adr))

    @patch.dict('os.environ', {'JEV_API_KEY': 'fake'}, clear=True)
    def test_l1_escalation_and_version_records(self):
        calls = []
        def opener(request, timeout):
            payload = json.loads(request.data)
            calls.append(payload)
            if len(calls) == 1:
                return self.response({'relied_behavior_changed': {'type': 'noul', 'noul': .31}})
            return self.response()
        level, notes, records = evaluate(self.adr, self.state, 1, self.config, JevClient(self.config, opener))
        self.assertEqual(level, 2)
        self.assertEqual(len(records), 2)
        self.assertEqual(len(calls[0]['questions']), 1)
        self.assertIn('impact', calls[1]['questions'])
        self.assertEqual(records[-1]['model'], 'jev-1.14.0')
        low = JevClient(self.config, lambda request, timeout: self.response({'relied_behavior_changed': {'noul': .3}}))
        self.assertEqual(evaluate(self.adr, self.state, 1, self.config, low)[0], 1)


if __name__ == '__main__':
    unittest.main()
