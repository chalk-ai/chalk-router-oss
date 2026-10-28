#!/usr/bin/env python3
"""Render security-relevant chart variants; requires helm and mikefarah/yq."""
import json
import subprocess


def render(*settings):
    args = ['helm', 'template', 'security-test', 'charts/chalk-router']
    for setting in settings:
        args += ['--set', setting]
    raw = subprocess.check_output(args, text=True)
    converted = subprocess.check_output(['yq', '-o=json', '-I=0', '.'], input=raw, text=True)
    return [json.loads(line) for line in converted.splitlines() if line.strip()]


for mode in ['environment', 'cluster']:
    docs = render(f'backend.tenancyMode={mode}', 'backend.metadataApiEndpoint=http://metadata.invalid:443')
    assert not any(d['kind'] == 'Ingress' for d in docs)
    workloads = [d for d in docs if d['kind'] in ['Deployment', 'Job']]
    assert len(workloads) == 3
    for doc in workloads:
        spec = doc['spec']['template']['spec']
        assert spec['automountServiceAccountToken'] is False
        security = spec['securityContext']
        assert security['runAsNonRoot'] is True
        assert security['runAsUser'] == security['runAsGroup'] == security['fsGroup'] == 10001
        assert security['seccompProfile']['type'] == 'RuntimeDefault'
        volumes = {v['name']: v for v in spec['volumes']}
        for container in spec['containers']:
            security = container['securityContext']
            assert security['allowPrivilegeEscalation'] is False
            assert security['readOnlyRootFilesystem'] is True
            assert security['capabilities']['drop'] == ['ALL']
            for mount in container.get('volumeMounts', []):
                volume = volumes[mount['name']]
                assert 'hostPath' not in volume
                if 'emptyDir' in volume:
                    assert volume['emptyDir']['sizeLimit']

for public in ['false', 'true']:
    docs = render('ingress.uiEnabled=true', 'ingress.apiEnabled=true', f'ingress.apiFunnelEnabled={public}')
    ingresses = {d['metadata']['name']: d for d in docs if d['kind'] == 'Ingress'}
    assert len(ingresses) == 2
    assert ingresses['chalk-router']['metadata']['annotations']['tailscale.com/funnel'] == public
    assert ingresses['chalk-router-ui']['metadata']['annotations']['tailscale.com/funnel'] == 'false'
invalid = subprocess.run(['helm', 'template', 'test', 'charts/chalk-router', '--set', 'ingress.apiFunnelEnabled=true'], capture_output=True)
assert invalid.returncode != 0 and b'requires ingress.apiEnabled' in invalid.stderr
print('Default and cluster containment; bounded mounts; private/public ingress opt-ins verified')

for doc in render('backend.automountServiceAccountToken=true'):
    if doc['kind'] in ['Deployment', 'Job']:
        enabled = doc['kind'] == 'Deployment' and doc['metadata']['name'] == 'chalk-router'
        assert doc['spec']['template']['spec']['automountServiceAccountToken'] is enabled
print('Backend-only service-account token opt-in verified')
