#!/usr/bin/env python3
"""Create a quarantined, history-free Git snapshot. Does NOT sanitize file contents."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess


def git(repo, *args):
    # Never inherit custom templates, hooks, filters, or identities from global config.
    env = {key: value for key, value in os.environ.items() if not key.startswith('GIT_')}
    env.update(GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull, GIT_NO_REPLACE_OBJECTS='1')
    return subprocess.check_output(['git', '-C', str(repo), '-c', 'core.hooksPath=' + os.devnull, *args], env=env)


def export(source, ref, output, evidence):
    source = Path(source).resolve()
    output = Path(output).absolute()
    evidence = Path(evidence).absolute()
    if output.exists() or output.is_symlink():
        raise ValueError('output must not exist')
    if evidence.exists() or evidence.is_symlink():
        raise ValueError('evidence must not exist')
    output = output.resolve()
    evidence = evidence.resolve()
    if evidence == output or output in evidence.parents:
        raise ValueError('private evidence must be outside the exported repository')
    commit = git(source, 'rev-parse', '--verify', ref + '^{commit}').decode().strip()
    entries = []
    for record in git(source, 'ls-tree', '-rz', '--full-tree', commit).split(b'\0'):
        if not record:
            continue
        metadata, raw_path = record.split(b'\t', 1)
        mode, kind, oid = metadata.decode().split()
        name = raw_path.decode('utf-8', errors='strict')
        path = PurePosixPath(name)
        if path.is_absolute() or any(part in {'.', '..'} or part.casefold() == '.git' for part in path.parts):
            raise ValueError('unsafe path in source tree')
        if kind != 'blob' or mode not in {'100644', '100755', '120000'}:
            raise ValueError('submodules and unsupported tree entries must be resolved before export')
        entries.append((name, mode, oid))
    output.mkdir(parents=False, exist_ok=False)
    try:
        manifest = []
        links = []
        # Materialize ordinary files first, then links, so writes never traverse links.
        for name, mode, oid in entries:
            data = git(source, 'cat-file', 'blob', oid)
            dest = output / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            if mode == '120000':
                links.append((dest, data.decode('utf-8', errors='strict')))
            else:
                dest.write_bytes(data)
                dest.chmod(0o755 if mode == '100755' else 0o644)
            manifest.append({'path': name, 'mode': mode, 'sha256': hashlib.sha256(data).hexdigest()})
        for dest, target in links:
            if os.path.isabs(target):
                raise ValueError('absolute symlinks are not exportable')
            dest.symlink_to(target)
        for dest, _ in links:
            resolved = dest.resolve()
            if resolved != output.resolve() and output.resolve() not in resolved.parents:
                raise ValueError('symlink escapes export directory')
        git(output, 'init', '--quiet', '--template=', '--initial-branch=main')
        # -f preserves the exact tracked snapshot even when its .gitignore matches files.
        git(output, 'add', '--force', '--all')
        git(output, '-c', 'user.name=Public Release', '-c', 'user.email=release@example.invalid',
            '-c', 'commit.gpgSign=false', 'commit', '--quiet', '--allow-empty', '-m', 'Initial source snapshot')
        exported = git(output, 'rev-parse', 'HEAD').decode().strip()
        assert git(output, 'rev-list', '--all', '--count').strip() == b'1'
        assert git(output, 'for-each-ref', '--format=%(refname)').strip() == b'refs/heads/main'
        assert git(output, 'remote').strip() == b''
        assert len(git(output, 'rev-list', '--parents', 'HEAD').split()) == 1
        # Match source tree bytes/modes exactly, including symlink content.
        source_tree = git(source, 'rev-parse', commit + '^{tree}').strip()
        assert git(output, 'rev-parse', 'HEAD^{tree}').strip() == source_tree
        report = {'status': 'quarantined-not-approved-for-publication', 'source_commit': commit,
                  'export_commit': exported, 'source_tree': source_tree.decode(),
                  'commit_count': 1, 'refs': ['refs/heads/main'], 'remotes': [],
                  'github_metadata_copied': False, 'file_content_sanitization': False, 'files': manifest}
        with evidence.open('x') as file:
            json.dump(report, file, indent=2)
            file.write('\n')
    except BaseException:
        shutil.rmtree(output)
        raise
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', required=True)
    parser.add_argument('--ref', required=True, help='Reviewed source commit; resolved once before copying')
    parser.add_argument('--output', required=True, help='New quarantine directory; parent must exist')
    parser.add_argument('--evidence', required=True, help='New PRIVATE JSON outside output')
    args = parser.parse_args()
    report = export(args.source, args.ref, args.output, args.evidence)
    print('Created quarantined one-commit snapshot: ' + report['export_commit'])
    print('File contents are unchanged and UNAPPROVED for publication; run release gates separately.')
