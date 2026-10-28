"""Scanner failures/findings must fail closed without exposing raw candidate values."""
import json
import importlib.util
from datetime import date
from copy import deepcopy
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('scan-release-image.py').resolve()


class ImageScanTests(unittest.TestCase):
    def run_case(self, records, scanner_exit, logged_error=False, scanned_bytes=100):
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            docker = root / 'docker'
            docker.write_text('#!/usr/bin/env python3\nimport sys,pathlib\nif sys.argv[1] == "save": pathlib.Path(sys.argv[3]).write_bytes(b"synthetic-image-tar")\nelse: print("sha256:synthetic-image")\n')
            docker.chmod(0o755)
            scanner = root / 'scanner'
            scanner.write_text('#!/usr/bin/env python3\nimport sys\n'
                               'assert "--no-verification" in sys.argv\n'
                               'assert "--no-update" in sys.argv\n'
                               'assert any(arg.startswith("file://") and arg.endswith("image.tar") for arg in sys.argv)\n'
                               + 'print(' + repr('\n'.join(json.dumps(r) for r in records)) + ')\n'
                               + 'print(' + repr(json.dumps({'level': 'error' if logged_error else 'info-0',
                                    'msg': 'finished scanning', 'bytes': scanned_bytes, 'chunks': 1,
                                    'trufflehog_version': '3.99.7'})) + ', file=sys.stderr)\n'
                               + f'sys.exit({scanner_exit})\n')
            scanner.chmod(0o755)
            report = root / 'report.json'
            result = subprocess.run(['python3', str(SCRIPT), '--image', 'local-test',
                                     '--scanner', str(scanner), '--report', str(report)],
                                    env={**os.environ, 'PATH': str(root) + os.pathsep + os.environ['PATH']},
                                    capture_output=True, text=True)
            return result, report.read_text()

    def test_findings_fail_and_redact_even_candidate_in_metadata(self):
        secret = 'synthetic-sensitive-value-123456789'
        record = {'Raw': secret, 'RawV2': secret + '-v2', 'DetectorName': 'Synthetic',
                  'SourceMetadata': {'Data': {'Docker': {'file': '/test/' + secret, 'line': 4}}}}
        result, report = self.run_case([record], 0)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn(secret, report + result.stdout + result.stderr)
        self.assertEqual(json.loads(report)['status'], 'requires_review')

    def test_scanner_error_is_not_a_clean_scan(self):
        result, report = self.run_case([], 2)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads(report)['status'], 'scanner_failed')

    def test_logged_error_fails_even_with_exit_zero(self):
        result, report = self.run_case([], 0, logged_error=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads(report)['status'], 'scanner_failed')

    def test_zero_bytes_is_not_success(self):
        result, report = self.run_case([], 0, scanned_bytes=0)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads(report)['status'], 'scanner_failed')

    def test_empty_success_records_exact_image(self):
        result, report = self.run_case([], 0)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(json.loads(report)['image_id'], 'sha256:synthetic-image')
        self.assertEqual(json.loads(report)['status'], 'no_detector_findings')


class ReviewTests(unittest.TestCase):
    def test_review_requires_exact_identity_and_valid_date(self):
        spec = importlib.util.spec_from_file_location('image_scan', SCRIPT)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        review = {'detector': 'Synthetic', 'file': '/dependency/data',
                  'candidate_sha256': 'synthetic-fingerprint', 'reviewed_on': '2026-10-26',
                  'review_by': '2027-01-26', 'reason': 'Synthetic fixture',
                  'evidence': 'https://example.com/fixture'}
        finding = {'detector': review['detector'], 'location': {'file': review['file']},
                   'candidate_sha256': review['candidate_sha256'], 'disposition': 'requires_review'}
        with tempfile.TemporaryDirectory() as work:
            path = Path(work) / 'reviews.json'
            path.write_text(json.dumps({'schema_version': 1, 'scanner': 'trufflehog 3.99.7',
                                        'findings': [review]}))
            valid = deepcopy(finding)
            module.apply_reviews([valid], path, date(2026, 10, 26))
            self.assertEqual(valid['disposition'], 'reviewed_nonconfidential_dependency_content')
            for field in ['detector', 'candidate_sha256', 'file']:
                with self.subTest(field=field):
                    changed = deepcopy(finding)
                    target = changed['location'] if field == 'file' else changed
                    target[field] += '-changed'
                    module.apply_reviews([changed], path, date(2026, 10, 26))
                    self.assertEqual(changed['disposition'], 'requires_review')
            for day in [date(2026, 10, 25), date(2027, 1, 27)]:
                changed = deepcopy(finding)
                module.apply_reviews([changed], path, day)
                self.assertEqual(changed['disposition'], 'requires_review')

if __name__ == '__main__':
    unittest.main()
