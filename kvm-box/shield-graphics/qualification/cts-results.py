"""Evaluate CTS evidence without treating missing or unsupported tests as passes."""
from collections import Counter


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
