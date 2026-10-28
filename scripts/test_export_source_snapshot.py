import importlib.util
from pathlib import Path
from unittest.mock import patch
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('exporter', Path(__file__).with_name('export-source-snapshot.py'))
exporter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(exporter)


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / 'source'
        self.source.mkdir()
        exporter.git(self.source, 'init', '-q', '--template=', '--initial-branch=main')
        exporter.git(self.source, 'config', 'user.name', 'Private Author')
        exporter.git(self.source, 'config', 'user.email', 'private@example.invalid')
        (self.source / 'removed.txt').write_text('historical confidential-customer fixture')
        self.commit('historical confidential-customer message')
        exporter.git(self.source, 'branch', 'private-customer-branch')
        exporter.git(self.source, 'update-ref', 'refs/pull/1/head', 'HEAD')
        exporter.git(self.source, 'remote', 'add', 'origin', 'https://example.invalid/private/repo')
        (self.source / 'removed.txt').unlink()
        (self.source / 'keep.txt').write_text('reviewed source content\n')
        (self.source / 'run.sh').write_text('#!/bin/sh\nexit 0\n')
        (self.source / 'run.sh').chmod(0o755)
        (self.source / 'alias').symlink_to('keep.txt')
        self.commit('private operational details')

    def commit(self, message):
        exporter.git(self.source, 'add', '--all')
        exporter.git(self.source, 'commit', '-qm', message)

    def run_export(self):
        return exporter.export(self.source, 'HEAD', self.root / 'export', self.root / 'private-evidence.json')

    def test_exact_tree_without_history_refs_metadata_or_dirty_files(self):
        (self.source / 'untracked-private.txt').write_text('must not copy')
        (self.source / 'keep.txt').write_text('uncommitted private modification')
        with patch.dict('os.environ', {'GIT_AUTHOR_NAME': 'Leaked Author', 'GIT_AUTHOR_EMAIL': 'leak@example.invalid', 'GIT_COMMITTER_NAME': 'Leaked Committer'}):
            report = self.run_export()
        out = self.root / 'export'
        self.assertEqual((out / 'keep.txt').read_text(), 'reviewed source content\n')
        self.assertFalse((out / 'untracked-private.txt').exists())
        self.assertFalse((out / 'removed.txt').exists())
        self.assertTrue((out / 'alias').is_symlink())
        self.assertEqual(report['commit_count'], 1)
        self.assertFalse(report['file_content_sanitization'])
        self.assertEqual(exporter.git(out, 'remote'), b'')
        self.assertNotIn(b'confidential-customer', exporter.git(out, 'log', '--all', '--format=fuller'))
        self.assertNotIn(b'Private Author', exporter.git(out, 'log', '--all', '--format=fuller'))
        self.assertNotIn(b'Leaked', exporter.git(out, 'log', '--all', '--format=fuller'))
        self.assertEqual(exporter.git(out, 'for-each-ref', '--format=%(refname)').strip(), b'refs/heads/main')
        # Even unreachable old objects are absent; this is not a shallow clone.
        all_objects = exporter.git(out, 'cat-file', '--batch-all-objects', '--batch-check=%(objectname)').splitlines()
        for oid in all_objects:
            self.assertNotIn(b'confidential-customer', exporter.git(out, 'cat-file', '-p', oid.decode()))

    def test_existing_destination_is_never_overwritten(self):
        out = self.root / 'export'
        out.mkdir()
        (out / 'precious').write_text('keep')
        with self.assertRaises(ValueError):
            self.run_export()
        self.assertEqual((out / 'precious').read_text(), 'keep')

    def test_evidence_cannot_be_inside_export(self):
        with self.assertRaises(ValueError):
            exporter.export(self.source, 'HEAD', self.root / 'export', self.root / 'export/evidence.json')

    def test_escaping_symlink_is_rejected_and_partial_output_removed(self):
        (self.source / 'escape').symlink_to('../outside')
        self.commit('add escaping fixture')
        with self.assertRaises(ValueError):
            self.run_export()
        self.assertFalse((self.root / 'export').exists())

    def test_submodule_is_rejected_without_fetching(self):
        commit = exporter.git(self.source, 'rev-parse', 'HEAD').decode().strip()
        exporter.git(self.source, 'update-index', '--add', '--cacheinfo', '160000,' + commit + ',module')
        exporter.git(self.source, 'commit', '-qm', 'submodule fixture')
        with self.assertRaises(ValueError):
            self.run_export()
        self.assertFalse((self.root / 'export').exists())


if __name__ == '__main__':
    unittest.main()
