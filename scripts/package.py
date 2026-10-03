#!/usr/bin/env python3
"""Package native executables with an installer; no runtime Python dependency."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import tarfile
import zipfile

parser = argparse.ArgumentParser()
parser.add_argument('--platform', choices=['windows', 'macos', 'linux'], required=True)
parser.add_argument('--arch', choices=['x86_64', 'aarch64'], required=True)
parser.add_argument('--target')
parser.add_argument('--binary', type=Path)
args = parser.parse_args()
if not args.binary and not args.target:
    parser.error('specify --target or --binary')
root = Path(__file__).resolve().parent.parent
name = f'digital-kvm-{args.platform}-{args.arch}'
stage = root / 'dist' / name
stage.mkdir(parents=True, exist_ok=True)
executable = 'digital-kvm.exe' if args.platform == 'windows' else 'digital-kvm'
binary = args.binary or root / 'target' / args.target / 'release' / executable
shutil.copy2(binary, stage / executable)
if args.platform != 'windows':
    (stage / executable).chmod(0o755)
for filename in ['README.md', 'LICENSE', 'THIRD_PARTY_NOTICES.md']:
    shutil.copy2(root / filename, stage / filename)
shutil.copy2(root / 'examples' / f'{args.platform}.json', stage / 'config.json')
scripts = [f'install-{args.platform}', f'uninstall-{args.platform}']
if args.platform == 'windows':
    scripts.append('stop-windows')
suffix = '.ps1' if args.platform == 'windows' else '.sh'
for script in scripts:
    destination = stage / (script + suffix)
    shutil.copy2(root / 'scripts' / destination.name, destination)
    destination.chmod(0o755)
shutil.copytree(root / 'docs', stage / 'docs', dirs_exist_ok=True)
metadata = {'version': '0.1.0', 'platform': args.platform, 'architecture': args.arch,
            'target': args.target, 'commit': os.environ.get('GITHUB_SHA', 'local'),
            'executable_sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
(stage / 'build-info.json').write_text(json.dumps(metadata, indent=2) + '\n', encoding='utf-8')
output = root / 'dist' / 'packages'
output.mkdir(parents=True, exist_ok=True)
if args.platform == 'windows':
    archive = output / (name + '.zip')
    with zipfile.ZipFile(archive, 'w', compression=zipfile.ZIP_DEFLATED) as bundle:
        for path in sorted(stage.rglob('*')):
            if path.is_file():
                bundle.write(path, Path(name) / path.relative_to(stage))
else:
    archive = output / (name + '.tar.gz')
    with tarfile.open(archive, 'w:gz') as bundle:
        bundle.add(stage, arcname=name)
print(archive)
