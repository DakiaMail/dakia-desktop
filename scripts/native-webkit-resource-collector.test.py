#!/usr/bin/env python3
import ast
import ctypes
import re
import unittest
from pathlib import Path
from unittest.mock import patch


COLLECTOR = Path(__file__).with_name('native-webkit-resource-collector.mjs')
MATCH = re.search(
    r"const PYTHON_LIBPROC = String\.raw`\n([\s\S]*?)\n`;",
    COLLECTOR.read_text(encoding='utf-8'),
)
if MATCH is None:
    raise RuntimeError('embedded Python collector was not found')
EMBEDDED = MATCH.group(1)
TREE = ast.parse(EMBEDDED, filename='<embedded-libproc>')
FUNCTIONS = {
    node.name: node
    for node in TREE.body
    if isinstance(node, ast.FunctionDef)
}


def load_functions(*names, globals_override=None):
    namespace = {
        'ctypes': ctypes,
        'PID_SIZE': ctypes.sizeof(ctypes.c_int),
    }
    if globals_override:
        namespace.update(globals_override)
    module = ast.Module(body=[FUNCTIONS[name] for name in names], type_ignores=[])
    exec(compile(ast.fix_missing_locations(module), '<selected-libproc>', 'exec'), namespace)
    return namespace


class FakeProcLibrary:
    def __init__(self, estimate, responses):
        self.estimate = estimate
        self.responses = list(responses)
        self.buffer_byte_sizes = []

    def proc_listallpids(self, buffer, byte_size):
        if buffer is None:
            if byte_size != 0:
                raise AssertionError('count query used a nonzero byte size')
            return self.estimate
        self.buffer_byte_sizes.append(byte_size)
        capacity = byte_size // ctypes.sizeof(ctypes.c_int)
        response = self.responses.pop(0)
        if response == 'full':
            return capacity
        if response == 'failed':
            return 0
        for index, pid in enumerate(response):
            buffer[index] = pid
        return len(response)


class FakeResponsible:
    def __init__(self, responses):
        self.responses = {pid: list(values) for pid, values in responses.items()}
        self.argtypes = None
        self.restype = None

    def __call__(self, pid):
        values = self.responses.get(int(pid))
        if not values:
            raise RuntimeError('unplanned responsibility lookup')
        value = values.pop(0)
        if isinstance(value, Exception):
            raise value
        return value


class FakeSystemLibrary:
    def __init__(self, responsible):
        self.responsibility_get_pid_responsible_for_pid = responsible


class NativeHelperOfflineTests(unittest.TestCase):
    def list_function(self, fake):
        namespace = load_functions('list_all_pids', globals_override={'lib': fake})
        return namespace['list_all_pids']

    def test_proc_listallpids_count_is_pid_count_and_buffer_is_bytes(self):
        fake = FakeProcLibrary(3, [[7, 2, 5]])
        result = self.list_function(fake)()
        expected_capacity = max(3 * 2, 3 + 256)
        self.assertEqual(result, [2, 5, 7])
        self.assertEqual(fake.buffer_byte_sizes, [expected_capacity * ctypes.sizeof(ctypes.c_int)])

    def test_proc_listallpids_exact_capacity_retries_with_larger_byte_buffer(self):
        fake = FakeProcLibrary(2, ['full', [11, 12]])
        result = self.list_function(fake)()
        first_capacity = max(2 * 2, 2 + 256)
        self.assertEqual(result, [11, 12])
        self.assertEqual(fake.buffer_byte_sizes, [
            first_capacity * ctypes.sizeof(ctypes.c_int),
            first_capacity * 2 * ctypes.sizeof(ctypes.c_int),
        ])

    def test_proc_listallpids_persistent_exact_capacity_is_unavailable(self):
        fake = FakeProcLibrary(2, ['full'] * 4)
        with self.assertRaisesRegex(RuntimeError, 'pid_enumeration_truncated'):
            self.list_function(fake)()

    def test_proc_listallpids_rejects_duplicate_or_nonpositive_pids(self):
        for response in ([5, 5], [0, 5], [-1, 5]):
            with self.subTest(response=response):
                fake = FakeProcLibrary(2, [response])
                with self.assertRaisesRegex(RuntimeError, 'pid_enumeration_invalid'):
                    self.list_function(fake)()

    def test_missing_private_responsibility_api_is_unavailable(self):
        namespace = load_functions(
            'unavailable',
            'scan',
            globals_override={
                'list_all_pids': lambda: self.fail('PID enumeration must not run'),
                'identity': lambda pid: self.fail('identity read must not run'),
                'full': lambda pid, expected: self.fail('resource read must not run'),
                'is_webkit': lambda candidate: False,
            },
        )
        with patch.object(ctypes, 'CDLL', side_effect=OSError('symbol library unavailable')):
            result = namespace['scan'](4242, {'uid': 501, 'startTimeMicros': '1000000'})
        self.assertEqual(result['availability'], 'unavailable')
        self.assertEqual(result['reason'], 'private_api_unavailable')
        self.assertEqual(result['enumeratedPidCount'], 0)

    def scan(self, pid_snapshots, responsibility, identities=None, full_result=None):
        snapshots = list(pid_snapshots)

        def list_all_pids():
            if not snapshots:
                raise AssertionError('unplanned PID snapshot')
            return snapshots.pop(0)

        identity_values = list(identities or [])

        def identity(pid):
            if not identity_values:
                raise AssertionError('unplanned identity read')
            return dict(identity_values.pop(0))

        def full(pid, expected):
            if full_result is None:
                raise AssertionError('unplanned resource read')
            return dict(full_result)

        namespace = load_functions(
            'unavailable',
            'scan',
            globals_override={
                'list_all_pids': list_all_pids,
                'identity': identity,
                'full': full,
                'is_webkit': lambda candidate: candidate.get('isWebKit') is True,
            },
        )
        system = FakeSystemLibrary(responsibility)
        with patch.object(ctypes, 'CDLL', return_value=system):
            return namespace['scan'](4242, {'uid': 501, 'startTimeMicros': '1000000'})

    def test_inherited_main_responsibility_fails_before_pid_enumeration(self):
        responsibility = FakeResponsible({4242: [999]})
        result = self.scan([], responsibility)
        self.assertEqual(result['availability'], 'unavailable')
        self.assertEqual(result['reason'], 'main_responsibility_mismatch')
        self.assertEqual(result['enumeratedPidCount'], 0)

    def test_responsibility_lookup_failure_invalidates_stable_census(self):
        responsibility = FakeResponsible({4242: [4242], 99: [0]})
        result = self.scan([[99, 4242], [99, 4242]], responsibility)
        self.assertEqual(result['availability'], 'unavailable')
        self.assertEqual(result['reason'], 'responsibility_lookup_incomplete')
        self.assertEqual(result['processes'], [])

    def test_missing_main_pid_invalidates_census(self):
        responsibility = FakeResponsible({4242: [4242]})
        result = self.scan([[99]], responsibility)
        self.assertEqual(result['availability'], 'unavailable')
        self.assertEqual(result['reason'], 'main_missing_from_pid_census')
        self.assertEqual(result['processes'], [])

    def test_numeric_pid_churn_exhausts_retries_without_accepting_resources(self):
        responsibility = FakeResponsible({
            4242: [4242],
            91: [4242, 4242, 4242],
        })
        result = self.scan([
            [91, 4242], [92, 4242],
            [91, 4242], [93, 4242],
            [91, 4242], [94, 4242],
        ], responsibility)
        self.assertEqual(result['availability'], 'unavailable')
        self.assertEqual(result['reason'], 'pid_census_unstable')
        self.assertEqual(result['processes'], [])

    def test_matched_pid_reuse_after_measurement_invalidates_census(self):
        first = {
            'pid': 99,
            'uid': 501,
            'path': '/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.WebContent.xpc/Contents/MacOS/com.apple.WebKit.WebContent',
            'startTimeMicros': '1000001',
            'isWebKit': True,
        }
        replacement = {**first, 'startTimeMicros': '2000000'}
        measured = {**first, 'rssBytes': 25, 'openFileDescriptors': 2}
        responsibility = FakeResponsible({4242: [4242], 99: [4242, 4242]})
        result = self.scan(
            [[99, 4242], [99, 4242], [99, 4242]],
            responsibility,
            identities=[first, replacement],
            full_result=measured,
        )
        self.assertEqual(result['availability'], 'unavailable')
        self.assertEqual(result['reason'], 'matched_process_unreadable_or_changed')
        self.assertEqual(result['processes'], [])

    def test_responsibility_change_after_measurement_invalidates_census(self):
        first = {
            'pid': 99,
            'uid': 501,
            'path': '/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.WebContent.xpc/Contents/MacOS/com.apple.WebKit.WebContent',
            'startTimeMicros': '1000001',
            'isWebKit': True,
        }
        measured = {**first, 'rssBytes': 25, 'openFileDescriptors': 2}
        responsibility = FakeResponsible({4242: [4242], 99: [4242, 7777]})
        result = self.scan(
            [[99, 4242], [99, 4242], [99, 4242]],
            responsibility,
            identities=[first, first],
            full_result=measured,
        )
        self.assertEqual(result['availability'], 'unavailable')
        self.assertEqual(result['reason'], 'matched_process_unreadable_or_changed')
        self.assertEqual(result['processes'], [])

    def test_stable_post_measurement_responsibility_remains_lower_bound_only(self):
        first = {
            'pid': 99,
            'uid': 501,
            'path': '/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.WebContent.xpc/Contents/MacOS/com.apple.WebKit.WebContent',
            'startTimeMicros': '1000001',
            'isWebKit': True,
        }
        measured = {**first, 'rssBytes': 25, 'openFileDescriptors': 2}
        responsibility = FakeResponsible({4242: [4242], 99: [4242, 4242]})
        result = self.scan(
            [[99, 4242], [99, 4242], [99, 4242]],
            responsibility,
            identities=[first, first],
            full_result=measured,
        )
        self.assertEqual(result['availability'], 'lower_bound_only')
        self.assertIsNone(result['reason'])
        self.assertEqual(result['acceptedWebKitProcessCount'], 1)
        self.assertEqual(result['processes'][0]['responsiblePid'], 4242)
        self.assertTrue(result['processes'][0]['responsibilityVerified'])
        self.assertTrue(result['processes'][0]['identityStable'])


if __name__ == '__main__':
    unittest.main()
