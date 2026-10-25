#!/usr/bin/env python3
"""Negative evidence cases: incomplete CTS logs must not become green checks."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('cts_results', Path(__file__).with_name('cts-results.py'))
results = importlib.util.module_from_spec(spec)
spec.loader.exec_module(results)


class EvidenceTests(unittest.TestCase):
    def verdict(self, records=None, **kw):
        values = dict(expected=['a', 'b'], results=records if records is not None else [
            {'name': 'a', 'status': 'Pass'}, {'name': 'b', 'status': 'Pass'}],
            returncode=0, timed_out=False, incomplete=False, mapped=['pinned'], required=['pinned'])
        values.update(kw)
        return results.evaluate(**values)

    def test_complete(self):
        self.assertTrue(self.verdict()['allSelectedPassed'])

    def test_empty_selection(self):
        self.assertFalse(self.verdict([], expected=[])['allSelectedPassed'])

    def test_lost_last_case(self):
        v = self.verdict([{'name': 'a', 'status': 'Pass'}])
        self.assertFalse(v['complete'])
        self.assertEqual(v['missing'], ['b'])

    def test_duplicate_cannot_replace_missing(self):
        v = self.verdict([{'name': 'a', 'status': 'Pass'}]*2)
        self.assertFalse(v['complete'])
        self.assertEqual(v['duplicates'], ['a'])

    def test_extra_case(self):
        self.assertFalse(self.verdict(expected=['a'])['complete'])

    def test_unsupported_is_not_pass(self):
        v = self.verdict([{'name': 'a', 'status': 'Pass'}, {'name': 'b', 'status': 'NotSupported'}])
        self.assertTrue(v['complete'])
        self.assertTrue(v['noFailuresWithUnsupported'])
        self.assertFalse(v['allSelectedPassed'])

    def test_warning_and_failure_not_pass(self):
        for status in ['Fail', 'QualityWarning', 'CompatibilityWarning', 'Crash', 'InternalError', 'Pending']:
            with self.subTest(status=status):
                v = self.verdict([{'name': 'a', 'status': 'Pass'}, {'name': 'b', 'status': status}])
                self.assertFalse(v['allSelectedPassed'])
                self.assertFalse(v['noFailuresWithUnsupported'])

    def test_nonzero_timeout_truncation(self):
        for change in [dict(returncode=1), dict(timed_out=True), dict(incomplete=True)]:
            with self.subTest(change=change):
                self.assertFalse(self.verdict(**change)['complete'])

    def test_wrong_driver(self):
        v = self.verdict(mapped=['host-mesa'])
        self.assertFalse(v['allSelectedPassed'])
        self.assertEqual(v['missingLibraries'], ['pinned'])

    def test_native_reference_cannot_bless_failure(self):
        self.assertFalse(results.matches_reference({'a': 'Fail'}, [{'name': 'a', 'status': 'Fail'}], {'a'}))
        self.assertFalse(results.matches_reference({'a': 'Pass'}, [{'name': 'a', 'status': 'Fail'}], set()))

    def test_only_reviewed_warning_or_improvement(self):
        baseline = {'a': 'CompatibilityWarning'}
        self.assertTrue(results.matches_reference(baseline, [{'name': 'a', 'status': 'CompatibilityWarning'}], {'a'}))
        self.assertTrue(results.matches_reference(baseline, [{'name': 'a', 'status': 'Pass'}], {'a'}))
        self.assertFalse(results.matches_reference(baseline, [{'name': 'a', 'status': 'CompatibilityWarning'}], set()))
        self.assertFalse(results.matches_reference({'a': 'Pass'}, [{'name': 'a', 'status': 'CompatibilityWarning'}], {'a'}))

    def test_native_reference_rejects_duplicate_or_missing(self):
        self.assertFalse(results.matches_reference({'a': 'Pass', 'b': 'Pass'}, [{'name': 'a', 'status': 'Pass'}]*2, set()))
        self.assertFalse(results.matches_reference({'a': 'Pass'}, [], set()))

    def test_optional_reference_requires_case_and_reason(self):
        ref = {'a': {'status': 'NotSupported', 'details': 'optional feature absent'}}
        records = [dict(name='a', **ref['a'])]
        approved = {'a': 'optional feature absent'}
        self.assertTrue(results.matches_optional_reference(ref, records, approved))
        self.assertFalse(results.matches_optional_reference(ref, records, {}))
        self.assertFalse(results.matches_optional_reference(ref, records, {'a': 'different reason'}))
        self.assertFalse(results.matches_optional_reference(ref, records, dict(approved, extra='unknown case')))
        changed = [dict(name='a', status='NotSupported', details='new unsupported reason')]
        self.assertFalse(results.matches_optional_reference(ref, changed, approved))
        self.assertTrue(results.matches_optional_reference(ref, [dict(name='a', status='Pass')], approved))

    def test_optional_reference_rejects_failures_and_regressions(self):
        for status in ['Fail', 'Crash', 'InternalError', 'CompatibilityWarning', 'QualityWarning', 'Pending']:
            ref = {'a': {'status': status, 'details': 'reason'}}
            self.assertFalse(results.matches_optional_reference(ref, [dict(name='a', **ref['a'])], {'a': 'reason'}))
            self.assertFalse(results.matches_optional_reference({'a': {'status': 'Pass'}}, [dict(name='a', **ref['a'])], {}))
        self.assertFalse(results.matches_optional_reference({'a': {'status': 'Pass'}},
                         [dict(name='a', status='NotSupported', details='reason')], {'a': 'reason'}))

    def test_optional_reference_rejects_incomplete_inventory(self):
        ref = {'a': {'status': 'Pass'}, 'b': {'status': 'Pass'}}
        self.assertFalse(results.matches_optional_reference(ref, [dict(name='a', status='Pass')]*2, {}))
        self.assertFalse(results.matches_optional_reference(ref, [dict(name='a', status='Pass')], {}))
        self.assertFalse(results.matches_optional_reference({}, [], {}))


if __name__ == '__main__':
    unittest.main()
