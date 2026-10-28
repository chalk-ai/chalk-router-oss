#!/usr/bin/env python3
"""Probe the built server from a separate process, with only synthetic credentials."""
import argparse
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    inventory = json.loads((ROOT / 'docs-site/public/api-inventory.json').read_text())
    paths = [r['path'] for r in inventory['rpcs'] if r['path'].startswith('/chalk.router.v1.')]
    assert len(paths) == 47 and len(set(paths)) == len(paths), 'Review admin inventory changes'
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    key = secrets.token_hex(32)
    # Do not inherit provider, database, proxy, JWT, telemetry or dotenv settings.
    env = {'PATH': os.environ['PATH'], 'PORT': str(port), 'CHALK_ENVIRONMENT_ID': 'synthetic-release-check',
           'ROUTER_ADMIN_KEY': key, 'RUST_LOG': 'error'}
    origin = f'http://127.0.0.1:{port}'
    with tempfile.TemporaryDirectory(prefix='router-admin-check-') as directory:
        with open(Path(directory) / 'server.log', 'wb') as log:
            process = subprocess.Popen([binary], cwd=directory, env=env, stdout=log, stderr=log)
            try:
                opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                for _ in range(200):
                    if process.poll() is not None:
                        raise RuntimeError('Synthetic router exited before health became ready')
                    try:
                        with opener.open(origin + '/health', timeout=1) as response:
                            if response.status == 200:
                                break
                    except (urllib.error.URLError, TimeoutError):
                        time.sleep(.1)
                else:
                    raise RuntimeError('Synthetic router health timeout')
                for path in paths + ['/chalk.router.v1.UnknownService/UnknownMethod']:
                    for protocol, content_type in [('--http1.1', 'application/grpc-web+proto'),
                                                   ('--http2-prior-knowledge', 'application/grpc')]:
                        result = subprocess.run(['curl', '--noproxy', '*', '--silent', '--show-error',
                            '--max-time', '5', protocol, '-o', os.devnull, '-w', '%{http_code}',
                            '-H', f'Content-Type: {content_type}', '-X', 'POST',
                            '--data-binary', '@-', origin + path], input=b'\0\0\0\0\0',
                            capture_output=True, check=True)
                        assert result.stdout == b'401', (path, protocol, result.stdout.decode())
                for path in ['/v1/admin/unknown', '/v1/debug/unknown', '/v1/models']:
                    try:
                        opener.open(origin + path, timeout=5)
                        raise AssertionError(f'Anonymous request accepted: {path}')
                    except urllib.error.HTTPError as error:
                        assert error.code == 401, (path, error.code)
                request = urllib.request.Request(origin + '/v1/models', headers={'Authorization': 'Bearer ' + key})
                with opener.open(request, timeout=5) as response:
                    assert response.status == 200
                print(json.dumps({'admin_rpcs': len(paths), 'anonymous_rpc_probes': (len(paths)+1)*2,
                    'protocols': ['HTTP/1.1 gRPC-Web', 'HTTP/2 gRPC'], 'denied_status': 401,
                    'legacy_admin_debug_paths_denied': True, 'authenticated_model_listing': 200}))
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


if __name__ == '__main__':
    main()
