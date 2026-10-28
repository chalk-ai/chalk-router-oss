#!/usr/bin/env python3
"""Scan every image layer offline; retain only redacted evidence for review."""
import argparse
from datetime import date
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile


def redact_findings(records):
    findings = []
    for record in records:
        raw = [record.get('Raw', ''), record.get('RawV2', '')]
        metadata = record.get('SourceMetadata', {}).get('Data', {})
        location = {}
        for source in metadata.values():
            if isinstance(source, dict):
                for name in ['file', 'path', 'line', 'layer', 'image']:
                    if name in source:
                        location[name] = source[name]
        def scrub(value):
            if not isinstance(value, str):
                return value
            for candidate in raw:
                if candidate:
                    value = value.replace(candidate, '[REDACTED]')
            return value
        findings.append({'detector': scrub(record.get('DetectorName')),
                         'location': {name: scrub(value) for name, value in location.items()},
                         'candidate_sha256': hashlib.sha256(json.dumps(raw).encode()).hexdigest(),
                         'disposition': 'requires_review'})
    return findings


def apply_reviews(findings, review_path, today=None):
    """Only exact detector/path/candidate tuples can use a dated review."""
    today = today or date.today()
    reviews = json.loads(Path(review_path).read_text())
    if reviews.get('schema_version') != 1 or reviews.get('scanner') != 'trufflehog 3.99.7':
        raise ValueError('Unsupported finding review format or scanner version')
    for finding in findings:
        for review in reviews['findings']:
            if (review['detector'] == finding['detector']
                    and review['file'] == finding['location'].get('file')
                    and review['candidate_sha256'] == finding['candidate_sha256']
                    and date.fromisoformat(review['reviewed_on']) <= today
                    <= date.fromisoformat(review['review_by'])
                    and review.get('reason') and review.get('evidence')):
                finding['disposition'] = 'reviewed_nonconfidential_dependency_content'
                finding['review'] = {key: review[key] for key in
                                     ['reviewed_on', 'review_by', 'reason', 'evidence']}
                break


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True, help='Local Docker image; no registry pulls')
    parser.add_argument('--report', required=True)
    parser.add_argument('--scanner', required=True)
    args = parser.parse_args()
    image_id = subprocess.check_output(['docker', 'image', 'inspect', '--format', '{{.Id}}', args.image], text=True).strip()
    source_commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    with tempfile.TemporaryDirectory(prefix='router-image-scan-') as work:
        image_tar = Path(work) / 'image.tar'
        subprocess.run(['docker', 'save', '--output', str(image_tar), image_id], check=True)
        digest = hashlib.sha256()
        with image_tar.open('rb') as image_file:
            for block in iter(lambda: image_file.read(1024 * 1024), b''):
                digest.update(block)
        raw_path = Path(work) / 'findings.jsonl'
        with raw_path.open('wb') as output, (Path(work) / 'scanner.log').open('wb') as log:
            result = subprocess.run([args.scanner, 'docker', '--image', 'file://' + str(image_tar),
                                     '--no-verification', '--no-update', '--json'], stdout=output, stderr=log)
        records = [json.loads(line) for line in raw_path.read_text().splitlines() if line]
        findings = redact_findings(records)
        apply_reviews(findings, Path(__file__).with_name('reviewed-image-findings.json'))
        unresolved = any(f['disposition'] == 'requires_review' for f in findings)
        # The pinned Docker source logs some failures and continues with exit 0.
        # Require evidence of actual scanning and reject every logged error.
        log_entries = []
        invalid_log_lines = 0
        for line in (Path(work) / 'scanner.log').read_text().splitlines():
            try:
                log_entries.append(json.loads(line))
            except json.JSONDecodeError:
                invalid_log_lines += 1
        error_count = sum(entry.get('level') in ['error', 'fatal', 'panic'] for entry in log_entries)
        finished = [entry for entry in log_entries if entry.get('msg') == 'finished scanning']
        summary = finished[-1] if finished else {}
        incomplete = (invalid_log_lines or error_count or summary.get('bytes', 0) <= 0
                      or summary.get('chunks', 0) <= 0 or summary.get('trufflehog_version') != '3.99.7')
        report = {'schema_version': 1, 'source_commit': source_commit, 'image_id': image_id,
                  'image_label': args.image, 'image_tar_sha256': digest.hexdigest(),
                  'scanner': 'trufflehog 3.99.7', 'logged_errors': error_count,
                  'unparsed_log_lines': invalid_log_lines,
                  'scanned_bytes': summary.get('bytes', 0), 'scanned_chunks': summary.get('chunks', 0),
                  'verification': False, 'scanner_exit_code': result.returncode,
                  'findings': findings, 'status': ('requires_review' if unresolved else
                             'reviewed_findings' if findings else 'no_detector_findings')}
        if result.returncode or incomplete:
            report['status'] = 'scanner_failed'
        Path(args.report).parent.mkdir(parents=True, exist_ok=True)
        Path(args.report).write_text(json.dumps(report, indent=2) + '\n')
        print(f'Image scan: {len(findings)} findings; status={report["status"]}; image={image_id}')
        if result.returncode or incomplete or unresolved:
            raise SystemExit(1)


if __name__ == '__main__':
    main()
