#!/usr/bin/env python3
"""Build/check public robot components without private source or credentials."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def run(args, directory=ROOT):
    subprocess.run(args, cwd=directory, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    bridge = os.name != 'nt'
    if args.check:
        run([sys.executable, 'tools/validate_contracts.py'])
        run([sys.executable, 'tools/validate_ai_guides.py'])
        run(['cargo', 'fmt', '--check'], ROOT / 'agent')
        run(['cargo', 'clippy', '--locked', '--all-targets', '--all-features', '--', '-D', 'warnings'], ROOT / 'agent')
        run(['cargo', 'test', '--locked'], ROOT / 'agent')
        for component in ['media'] + (['service-bridge'] if bridge else []):
            directory = ROOT / component
            formatted = subprocess.check_output(['gofmt', '-l', *[p.name for p in directory.glob('*.go')]], cwd=directory, text=True)
            if formatted.strip():
                raise SystemExit('Go files need formatting: ' + component)
            run(['go', 'mod', 'verify'], directory)
            run(['go', 'vet', '-mod=readonly', './...'], directory)
            run(['go', 'test', '-mod=readonly', './...'], directory)
        run([sys.executable, '-m', 'unittest', 'discover', '-s', 'media/example-camera-adapter', '-p', 'test_stream_webcam.py'])
        return
    output = ROOT / 'dist'
    output.mkdir(exist_ok=True)
    run(['cargo', 'build', '--release', '--locked'], ROOT / 'agent')
    extension = '.exe' if os.name == 'nt' else ''
    shutil.copy2(ROOT / f'agent/target/release/fleetlyhub-agent{extension}', output)
    for component, name in [('media', 'fleetly-media')] + ([('service-bridge', 'service-bridge')] if bridge else []):
        run(['go', 'build', '-mod=readonly', '-trimpath', '-o', str(output / (name + extension)), '.'], ROOT / component)
    shutil.copy2(ROOT / 'media/example-camera-adapter/stream_webcam.py', output / 'fleetlyhub-camera.py')
    shutil.copy2(ROOT / 'tools/run-agent.ps1', output)
    shutil.copy2(ROOT / 'LICENSE', output)
    shutil.copy2(ROOT / 'NOTICE', output)
    shutil.copy2(ROOT / 'THIRD_PARTY_NOTICES.md', output)
    print('Built public robot software in dist; no enrollment or deployment performed.')


if __name__ == '__main__':
    main()
