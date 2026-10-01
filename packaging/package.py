#!/usr/bin/env python3
"""Native portable builds; source collection uses an explicit allowlist."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parent.parent


def sources():
    for name in ('Cargo.toml', 'Cargo.lock', 'README.md', 'LICENSE', '.gitignore', '.gitattributes'):
        yield ROOT / name
    for name in ('src', 'vendor', 'packaging', '.github', 'tests'):
        for path in sorted((ROOT / name).rglob('*')):
            if path.is_file() and '__pycache__' not in path.parts:
                yield path


def source_zip(destination):
    with zipfile.ZipFile(destination, 'w', zipfile.ZIP_DEFLATED, strict_timestamps=False) as archive:
        for path in sources():
            archive.write(path, Path('StickMix-source') / path.relative_to(ROOT))


def smoke(folder):
    extension = '.exe' if platform.system() == 'Windows' else ''
    binary = str((folder / ('stickmix' + extension)).resolve())
    subprocess.run([binary, '--help'], check=True, capture_output=True)
    # Device discovery only; this cannot format, copy to, or eject a USB.
    subprocess.run([binary, 'drives'], check=True, capture_output=True)
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        source = root / 'music'
        source.mkdir()
        shutil.copy2(ROOT / 'tests/fixtures/tone.mp3', source / 'tone.mp3')
        import os
        environment = dict(os.environ, STICKMIX_CACHE_DIR=str(root / 'cache'))
        command = [binary, 'prepare', str(source), '--output', str(root / 'USB'), '--name', 'Test playlist']
        subprocess.run(command, env=environment, check=True, capture_output=True)
        subprocess.run(command, env=environment, check=True, capture_output=True)
        exported = list((root / 'USB/Contents').rglob('*.mp3'))
        assert len(exported) == 1
        assert hashlib.sha256(exported[0].read_bytes()).digest() == hashlib.sha256((source/'tone.mp3').read_bytes()).digest()
        assert (root/'USB/PIONEER/rekordbox/export.pdb').stat().st_size >= 4096
        for suffix in ('DAT', 'EXT'):
            assert list((root/'USB/PIONEER/USBANLZ').rglob('*.'+suffix))
    print('Native smoke test passed: discovery, analysis, Pioneer export, byte-exact audio, resume.')


def package(system, arch):
    native = {'Windows':'windows', 'Darwin':'macos', 'Linux':'linux'}[platform.system()]
    assert native == system, 'Run on the native operating system; do not relabel binaries.'
    folder = ROOT/'dist'/f'StickMix-{system}-{arch}'
    if folder.exists():
        shutil.rmtree(folder)
    folder.mkdir(parents=True)
    extension = '.exe' if system == 'windows' else ''
    shutil.copy2(ROOT/'target/release'/('stickmix'+extension), folder/('stickmix'+extension))
    launcher = 'Start StickMix.cmd' if system == 'windows' else 'Start StickMix.command'
    shutil.copy2(ROOT/'packaging'/launcher,folder/launcher)
    if system != 'windows':
        (folder/launcher).chmod(0o755)
    for name in ('LICENSE','README.md'):
        shutil.copy2(ROOT/name,folder/name)
    source_dir=folder/'Sources'
    source_dir.mkdir()
    source_zip(source_dir/'StickMix-source.zip')
    dependency_dir=ROOT/'.build/dependency-source'
    if not dependency_dir.is_dir():
        raise SystemExit('First run cargo vendor --locked --versioned-dirs .build/dependency-source')
    with zipfile.ZipFile(source_dir/'Dependency-sources.zip','w',zipfile.ZIP_DEFLATED,strict_timestamps=False) as archive:
        for path in sorted(dependency_dir.rglob('*')):
            if path.is_file():
                archive.write(path,path.relative_to(dependency_dir))
    metadata=json.loads(subprocess.check_output(['cargo','metadata','--locked','--offline','--format-version','1'],cwd=ROOT))
    licenses=folder/'Licenses'
    licenses.mkdir()
    inventory=[]
    for crate in metadata['packages']:
        inventory.append(f"{crate['name']} {crate['version']}: {crate.get('license') or 'see included license'}")
        for path in Path(crate['manifest_path']).parent.iterdir():
            if path.is_file() and path.name.upper().startswith(('LICENSE','COPYING','NOTICE','COPYRIGHT')):
                destination=licenses/f"{crate['name']}-{crate['version']}"
                destination.mkdir(exist_ok=True)
                shutil.copy2(path,destination/path.name)
    (licenses/'DEPENDENCIES.txt').write_text('\n'.join(inventory)+'\n',encoding='utf-8')
    smoke(folder)
    destination=ROOT/'dist'/(folder.name+'.zip')
    with zipfile.ZipFile(destination,'w',zipfile.ZIP_DEFLATED,strict_timestamps=False) as archive:
        for path in sorted(folder.rglob('*')):
            if path.is_file():
                archive.write(path,path.relative_to(ROOT/'dist'))
    print(destination)


if __name__ == '__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('--platform',choices=('linux','windows','macos'))
    parser.add_argument('--arch',choices=('x64','arm64'))
    parser.add_argument('--source-only',action='store_true')
    args=parser.parse_args()
    if args.source_only:
        (ROOT/'dist').mkdir(exist_ok=True)
        source_zip(ROOT/'dist/StickMix-source.zip')
    elif args.platform and args.arch:
        package(args.platform,args.arch)
    else:
        parser.error('--platform and --arch are required')
