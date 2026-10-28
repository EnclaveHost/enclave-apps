import copy
import unittest
from accept_capture import accept

class Acceptance(unittest.TestCase):
    def setUp(self):
        identity = '01234567-0123-4567-89ab-0123456789ab'
        self.before = {'vms': [{'Id': 'existing', 'State': 'Running'}], 'inputs': [{'path': 'C:\\type1.vmgs', 'sha256': 'a' * 64}]}
        self.after = copy.deepcopy(self.before)
        capture = '\n'.join(['SNAPPROBE BEGIN capture-discard-v1', 'SNAPPROBE FREEZE begin', 'SNAPPROBE CREATE_IMAGE begin', 'SNAPPROBE CREATE_IMAGE OK original=1', 'SNAPPROBE IMAGE_SIZE 4096', 'SNAPPROBE IMAGE_READ OK bytes=4096 diagnostic_fnv64=0123456789abcdef', 'SNAPPROBE FREE OK', 'SNAPPROBE UNFREEZE OK', 'SNAPPROBE CLOSE OK', 'SNAPPROBE RESULT PASS', 'SNAPPROBE COMPLETE capture-discard-v1'])
        self.report = {'result.json': {'id': identity, 'cleanup': {'id': identity, 'absent': True}, 'masterSha': 'a' * 64, 'capture': capture}, 'watchdog-receipt.json': {'absent': True, 'secondAbsenceCheck': {'absent': True}}}
    def check(self):
        return accept(self.report, self.before, self.after)
    def test_capture_is_not_restore(self):
        result = self.check()
        self.assertTrue(result['captureReadDiscardQualified'])
        self.assertFalse(result['restoreQualified'])
        self.assertFalse(result['privateSnapshotQualified'])
    def test_missing_or_duplicate_every_result(self):
        original = self.report['result.json']['capture']
        for line in original.splitlines():
            for bad in [original.replace(line, ''), original + '\n' + line]:
                self.report['result.json']['capture'] = bad
                with self.assertRaises(ValueError): self.check()
        self.report['result.json']['capture'] = original
    def test_early_free_rejected(self):
        r = self.report['result.json']
        r['capture'] = r['capture'].replace('SNAPPROBE FREEZE begin', 'SNAPPROBE FREE OK\nSNAPPROBE FREEZE begin').replace('\nSNAPPROBE FREE OK\nSNAPPROBE UNFREEZE', '\nSNAPPROBE UNFREEZE')
        with self.assertRaises(ValueError): self.check()
    def test_operation_error_even_with_pass(self):
        self.report['result.json']['capture'] += '\nSNAPPROBE FREE ERROR 5'
        with self.assertRaises(ValueError): self.check()
    def test_changed_inventory(self):
        self.after['vms'][0]['State'] = 'Off'
        with self.assertRaises(ValueError): self.check()
    def test_changed_master(self):
        self.after['inputs'][0]['sha256'] = 'b' * 64
        with self.assertRaises(ValueError): self.check()
    def test_unconfirmed_watchdog(self):
        self.report['watchdog-receipt.json']['secondAbsenceCheck']['absent'] = False
        with self.assertRaises(ValueError): self.check()
    def test_wrong_read_length(self):
        self.report['result.json']['capture'] = self.report['result.json']['capture'].replace('bytes=4096', 'bytes=4095')
        with self.assertRaises(ValueError): self.check()

if __name__ == '__main__': unittest.main()
