#!/usr/bin/env python3
"""Inventory declared dependency licenses; this is not a license approval tool."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / 'docs/release/license-inventory.json'


def rust_inventory(manifest_path):
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--manifest-path', str(ROOT / manifest_path),
         '--locked', '--offline', '--format-version', '1'], cwd=ROOT))
    rust = []
    for package in metadata['packages']:
        manifest = Path(package['manifest_path'])
        source = package['source']
        if source is None:
            source = 'path:' + str(manifest.parent.relative_to(ROOT))
        license_file = package.get('license_file')
        if license_file:
            license_path = Path(license_file)
            if not license_path.is_absolute():
                license_path = manifest.parent / license_path
            # Do not leak machine-local registry or checkout paths.
            license_file = str(license_path.relative_to(manifest.parent))
        rust.append(dict(name=package['name'], version=package['version'], source=source,
                         declared_license=package.get('license'), license_file=license_file))
    return sorted(rust, key=lambda p: (p['name'], p['version'], p['source']))


def npm_inventory(lock_path):
    lock = json.loads((ROOT / lock_path).read_text())
    return [dict(path=path, version=p.get('version'), declared_license=p.get('license'),
                 dev=bool(p.get('dev')), optional=bool(p.get('optional')))
            for path, p in sorted(lock['packages'].items()) if path]


def inventory():
    rust = rust_inventory('Cargo.toml')
    ui = npm_inventory('ui/package-lock.json')
    docs_npm = npm_inventory('docs-site/package-lock.json')
    docs_rust = rust_inventory('docs-site/tools/schema-source/Cargo.toml')
    inputs = ['vendor/chalk-federated-auth/Cargo.toml', 'vendor/chalk-federated-auth/LICENSE',
              'vendor/chalk-federated-auth/PROVENANCE.md', 'proto/chalk/auth/LICENSE',
              'proto/chalk/auth/PROVENANCE.md', 'proto/chalk/utils/LICENSE',
              'proto/chalk/utils/PROVENANCE.md', 'LICENSE', 'Cargo.toml', 'Cargo.lock', 'ui/package.json', 'ui/package-lock.json',
              'vendor/reqsign-google/Cargo.toml', 'vendor/reqsign-google/LICENSE',
              'vendor/reqsign-google/NOTICE', 'docs-site/package.json',
              'docs-site/package-lock.json', 'docs-site/tools/schema-source/Cargo.toml',
              'docs-site/tools/schema-source/Cargo.lock']
    return dict(schema_version=2,
                scope='All packages returned by locked router/documentation Cargo metadata and UI/documentation npm lock package records; includes development/platform-specific packages. Does not cover container OS packages, copied source rights, or completeness of third-party notices.',
                approval='NOT APPROVED: declarations are evidence, not permission or completed notices.',
                input_sha256={p: hashlib.sha256((ROOT / p).read_bytes()).hexdigest() for p in inputs},
                rust=rust, ui=ui, documentation_npm=docs_npm, documentation_rust=docs_rust)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    rendered = json.dumps(inventory(), indent=2, sort_keys=True) + '\n'
    if args.check:
        if not OUTPUT.exists() or OUTPUT.read_text() != rendered:
            raise SystemExit('License inventory is stale; regenerate after fetching locked dependencies.')
        print('License declarations match locked inputs; redistribution remains unapproved.')
    else:
        OUTPUT.parent.mkdir(parents=True, exist_ok=True)
        OUTPUT.write_text(rendered)
        print('Wrote declaration inventory; this does not approve redistribution.')


if __name__ == '__main__':
    main()
