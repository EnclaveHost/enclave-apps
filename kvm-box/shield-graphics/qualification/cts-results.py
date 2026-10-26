"""Evaluate CTS evidence without treating missing or unsupported tests as passes."""
from collections import Counter


def matches_optional_reference(expected, results, reviewed_unsupported):
    """Compare a complete Vulkan run without turning optional features into passes.

    Each reviewed exception binds a case name AND its exact unsupported reason.
    A previously passing case may not regress; unknown failures or changed
    unsupported reasons cannot be authorized by a reference result.
    """
    if not expected or set(reviewed_unsupported) - set(expected):
        return False
    for name, record in expected.items():
        if record['status'] != 'Pass' and not (
                record['status'] == 'NotSupported' and
                name in reviewed_unsupported and
                record['details'] == reviewed_unsupported[name]):
            return False
    names = [r['name'] for r in results]
    if len(names) != len(expected) or set(names) != set(expected):
        return False
    return all(r['status'] == 'Pass' or (
        r['status'] == expected[r['name']]['status'] == 'NotSupported' and
        r['name'] in reviewed_unsupported and
        r['details'] == expected[r['name']]['details'] == reviewed_unsupported[r['name']]
    ) for r in results)


def matches_reference(expected, results, known_compatibility_warnings):
    """A reference may contain only passes and explicitly reviewed warnings.

    Never let a failed baseline bless the same failure in a later native run.
    The caller supplies reviewed case names, not arbitrary acceptable statuses.
    """
    if not expected or any(status != 'Pass' and not (
            status == 'CompatibilityWarning' and name in known_compatibility_warnings)
            for name, status in expected.items()):
        return False
    names = [r['name'] for r in results]
    if len(names) != len(expected) or set(names) != set(expected):
        return False
    return all(r['status'] == 'Pass' or (
        r['name'] in known_compatibility_warnings and
        r['status'] == expected[r['name']] == 'CompatibilityWarning') for r in results)


def evaluate(expected, results, returncode, timed_out, incomplete, mapped, required):
    names = [r['name'] for r in results]
    counts = Counter(r['status'] for r in results)
    missing = sorted(set(expected) - set(names))
    unexpected = sorted(set(names) - set(expected))
    duplicates = sorted(name for name, count in Counter(names).items() if count != 1)
    bad = [r for r in results if r['status'] not in ('Pass', 'NotSupported')]
    missing_libraries = sorted(set(required) - set(mapped))
    complete = bool(expected) and len(expected) == len(set(expected)) and not (
        missing or unexpected or duplicates or incomplete or timed_out or returncode != 0)
    return {
        'complete': complete,
        'allSelectedPassed': complete and not missing_libraries and counts['Pass'] == len(expected),
        'noFailuresWithUnsupported': complete and not missing_libraries and not bad,
        'counts': dict(counts), 'expectedCount': len(expected),
        'missing': missing, 'unexpected': unexpected, 'duplicates': duplicates,
        'missingLibraries': missing_libraries, 'failures': bad,
    }
