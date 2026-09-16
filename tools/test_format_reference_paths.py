import argparse
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

# Optional alternate driver path supports a preserved broken-code control.
driver_path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).with_name('format_reference_compat.py')
spec = importlib.util.spec_from_file_location('reference_driver', driver_path)
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)

class ArtifactPathTests(unittest.TestCase):
    def check_refusal(self, spelling):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            corpus = root / 'corpus'
            corpus.mkdir()
            corpus = corpus.resolve()
            (corpus / 'sentinel.txt').write_text('preserved source inventory')
            baseline = root / 'baseline'
            current = root / 'current'
            baseline.write_text('baseline identity')
            current.write_text('current identity')
            alias = root / 'alias'
            alias.symlink_to(corpus, target_is_directory=True)
            paths = {
                'nested': corpus / 'new-parent' / 'work',
                'dotdot': root / 'unused' / '..' / 'corpus' / 'work',
                'symlink': alias / 'work',
            }
            (root / 'unused').mkdir()
            before = sorted(p.relative_to(corpus).as_posix() for p in corpus.rglob('*'))
            args = argparse.Namespace(corpus=corpus, index_sha256='pinned', baseline_bin=baseline,
                                      current_bin=current, work=paths[spelling])
            caught = None
            # Simulate the Linux artifact policy: only sentinel files are made
            # in local temporary directories, never database artifacts.
            # No database or executable is opened: replace corpus validation and
            # make any subprocess reach a test failure, including --version.
            with patch.object(driver.sys, 'platform', 'linux'), patch.object(driver, 'validate_corpus', return_value=([], driver.inventory(corpus))), \
                 patch.object(driver.subprocess, 'run', side_effect=RuntimeError('reached executable')):
                try:
                    driver.qualify(args)
                except Exception as error:
                    caught = error
            after = sorted(p.relative_to(corpus).as_posix() for p in corpus.rglob('*'))
            self.assertEqual(after, before, f'{spelling}: source inventory changed')
            self.assertIsInstance(caught, AssertionError)
            self.assertIn('work must be outside', str(caught))
            self.assertEqual((corpus / 'sentinel.txt').read_text(), 'preserved source inventory')

    def test_nested_new_parent_rejected_without_creation(self): self.check_refusal('nested')
    def test_dotdot_alias_rejected_without_creation(self): self.check_refusal('dotdot')
    def test_symlink_parent_rejected_without_creation(self): self.check_refusal('symlink')

if __name__ == '__main__':
    unittest.main(argv=[sys.argv[0]], verbosity=2)
