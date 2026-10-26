#!/usr/bin/env python3
"""Verify the staged immutable overlay and enable the measured Q8 service."""
import hashlib
import json
from pathlib import Path
import re
import subprocess


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


root = Path('/opt/shield')
inputs = json.loads((root / 'standard/inputs.json').read_text())
overlay = json.loads((root / 'standard/overlay-files.json').read_text())
for name, digest in overlay.items():
    assert sha(name) == digest, 'overlay digest mismatch: ' + name
manifest_path = root / 'files.sha256.json'
assert sha(manifest_path) == inputs['baseSdkManifestSha256'], 'base SDK manifest mismatch'
manifest = json.loads(manifest_path.read_text())
# Existing SDK files outside the explicit overlay must remain byte-identical.
for name, digest in manifest.items():
    absolute = str(root / name)
    if absolute not in overlay:
        assert sha(absolute) == digest, 'unexpected SDK change: ' + name
decline_check = None
if 'sourceDeclines' in inputs:
    if json.loads((root / 'standard/source-declines/build-receipt.json').read_text()) != inputs['sourceDeclines']:
        raise ValueError('rejection hint provenance differs from the staged build receipt')
    decline_check = root / 'standard/source-declines/installed-check.json'
    subprocess.run(['/usr/bin/python3', '-I', str(root / 'standard/source_declines_package.py'),
                    '--installed', str(root / 'standard/source-declines'),
                    '--receipt', str(decline_check)], check=True)
config_path = Path('/shield/service.json')
config = json.loads(config_path.read_text())
assert config['sdk'] == str(root)
assert config['worker'] == {'host': '10.0.2.2', 'port': 9741}
config['enableQ8Experimental'] = True
config_path.write_text(json.dumps(config, indent=2) + '\n')
firewall_path = Path('/shield/guest-firewall.nft')
ports = re.findall(r'tcp dport \{ ([0-9, ]+) \}', firewall_path.read_text())
assert len(ports) == 1, 'ambiguous base attested-service ports'
template = (root / 'standard/guest-firewall.nft.in').read_text()
assert template.count('__ATTESTED_SERVICE_PORTS__') == 1
firewall_path.write_text(template.replace('__ATTESTED_SERVICE_PORTS__', ports[0]))
(root / 'standard/generated-files.json').write_text(json.dumps({
    str(path): sha(path) for path in (config_path, firewall_path)}, indent=2) + '\n')
provenance_path = root / 'source-provenance.json'
provenance = json.loads(provenance_path.read_text())
provenance['standardOpenclPackage'] = inputs
provenance_path.write_text(json.dumps(provenance, indent=2) + '\n')
for absolute in overlay:
    if absolute.startswith('/opt/shield/'):
        manifest[str(Path(absolute).relative_to(root))] = sha(absolute)
manifest['source-provenance.json'] = sha(provenance_path)
manifest['standard/inputs.json'] = sha(root / 'standard/inputs.json')
manifest['standard/overlay-files.json'] = sha(root / 'standard/overlay-files.json')
manifest['standard/generated-files.json'] = sha(root / 'standard/generated-files.json')
if decline_check is not None:
    manifest['standard/source-declines/installed-check.json'] = sha(decline_check)
manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + '\n')
print('Verified standard OpenCL overlay and native SDK; service Q8 route enabled')
