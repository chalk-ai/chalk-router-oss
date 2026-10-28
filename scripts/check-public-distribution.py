#!/usr/bin/env python3
"""Prevent private deployment assets from returning to the public source tree.

This is a packaging guard, not a secret/confidential-content scanner.
"""
import fnmatch
import subprocess

forbidden = ['.buildkite/*', '.env.enc', '.env.*.enc', '.sops.yaml', 'kapp',
             'k8s/seed/*', '.github/workflows/sync-buildkite.yml']
paths = subprocess.check_output(['git', 'ls-files', '-z']).decode().split('\0')
found = [p for p in paths if p and any(fnmatch.fnmatchcase(p, pattern) for pattern in forbidden)]
if found:
    raise SystemExit('Private deployment assets are excluded from this distribution: ' + ', '.join(found))
print('Private deployment asset exclusion check passed; confidential-content/release scans remain separate')
