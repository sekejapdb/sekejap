#!/usr/bin/env python3
"""Pure provenance controls: no database execution or artifact generation."""
import copy
import unittest

from phase2_rollback_compat import validate_versions


def versions(revision):
    result = {}
    for label, retained in (('default', False), ('retained', True)):
        result[label] = {
            'harness': 'phase2-lifecycle-fixture-v1',
            'engine_revision': revision,
            'rollback_cycle_version': 1,
            'create_compact_cells': retained,
            'compile_features': {name: retained for name in (
                'compact-cells', 'sqlite-balance', 'keyspace-append', 'slotref-split')},
        }
    return result


class RollbackProvenanceTests(unittest.TestCase):
    def test_captured_same_revision_build_pair_is_explicitly_cross_build(self):
        baseline = versions('captured-source-A')
        validate_versions(baseline, copy.deepcopy(baseline), 'cross-build')
        with self.assertRaises(AssertionError):
            validate_versions(baseline, baseline, 'cross-revision')

    def test_different_revision_pair_is_explicitly_cross_revision(self):
        baseline, current = versions('captured-source-A'), versions('captured-source-B')
        validate_versions(baseline, current, 'cross-revision')
        with self.assertRaises(AssertionError):
            validate_versions(baseline, current, 'cross-build')

    def test_crossed_equal_revisions_cannot_hide_a_mixed_build_group(self):
        baseline, current = versions('A'), versions('B')
        baseline['retained']['engine_revision'] = 'B'
        current['retained']['engine_revision'] = 'A'
        # Each opposite-label pair agrees; neither group is a single revision.
        for kind in ('cross-build', 'cross-revision'):
            with self.subTest(kind=kind), self.assertRaises(AssertionError):
                validate_versions(baseline, current, kind)

    def test_one_mixed_comparison_group_is_not_a_new_revision(self):
        baseline, current = versions('A'), versions('B')
        current['retained']['engine_revision'] = 'C'
        with self.assertRaises(AssertionError):
            validate_versions(baseline, current, 'cross-revision')

    def test_missing_or_unknown_cycle_capability_refuses(self):
        for group in ('baseline', 'comparison'):
            for label in ('default', 'retained'):
                for capability in (None, 0, 2):
                    baseline, current = versions('A'), versions('B')
                    target = baseline if group == 'baseline' else current
                    target[label]['rollback_cycle_version'] = capability
                    with self.subTest(group=group, label=label, capability=capability):
                        with self.assertRaises(AssertionError):
                            validate_versions(baseline, current, 'cross-revision')

    def test_unrecorded_or_empty_revision_refuses(self):
        for revision in ('unrecorded', '', None):
            with self.subTest(revision=revision), self.assertRaises(AssertionError):
                validate_versions(versions('A'), versions(revision), 'cross-revision')

    def test_helper_protocol_mismatch_refuses(self):
        current = versions('B')
        current['default']['harness'] = 'different-protocol'
        with self.assertRaises(AssertionError):
            validate_versions(versions('A'), current, 'cross-revision')

    def test_creation_and_compile_profiles_must_match_same_labels(self):
        for field in ('create_compact_cells', 'compile_features'):
            for missing in (False, True):
                current = versions('B')
                if missing:
                    del current['default'][field]
                else:
                    current['default'][field] = copy.deepcopy(current['retained'][field])
                with self.subTest(field=field, missing=missing), self.assertRaises(AssertionError):
                    validate_versions(versions('A'), current, 'cross-revision')


if __name__ == '__main__':
    unittest.main()
