#!/usr/bin/env python3
"""Check public AI-guide links and JSON examples against the actual contracts."""
import argparse
import json
from pathlib import Path
import re
from urllib.parse import unquote, urlsplit

from validate_contracts import check, validator

ROOT = Path(__file__).resolve().parents[1]
GUIDES = ('README.md', 'camera.md', 'telemetry.md', 'diagnostics.md',
          'services.md', 'robot-controls.md')


def check_document(text, path, root, compiled):
    links, examples = 0, 0
    for destination in re.findall(r'\]\(([^)\s]+)\)', text):
        url = urlsplit(destination)
        if url.scheme in ('https', 'http'):
            continue  # External references are not fetched by offline checks.
        if url.scheme or url.netloc:
            raise ValueError('Unsupported guide link: ' + destination)
        if not url.path:
            continue
        target = (path.parent / unquote(url.path)).resolve()
        if not target.is_relative_to(root.resolve()) or not target.exists():
            raise ValueError('Missing/nonpublic guide link: ' + destination)
        links += 1
    for kind, body in re.findall(r'^```json([^\n]*)\n(.*?)\n```', text, re.M | re.S):
        value = json.loads(body, parse_constant=lambda _: (
            _ for _ in ()).throw(ValueError('Non-finite JSON')))
        if kind.strip() == 'service-bridge-config':
            if not isinstance(value, dict) or 'services' not in value:
                raise ValueError('Missing service configuration')
        elif not kind.strip():
            check(value, root, compiled)
        else:
            raise ValueError('Unknown JSON example kind')
        examples += 1
    return links, examples


def validate_guides(root=ROOT):
    compiled = validator(root)
    totals = [0, 0]
    for name in GUIDES:
        path = root / 'docs/ai' / name
        if not path.is_file():
            raise ValueError('Missing public AI guide: ' + name)
        text = path.read_text(encoding='utf-8')
        if not text.strip():
            raise ValueError('Empty public AI guide: ' + name)
        counts = check_document(text, path, root, compiled)
        totals = [a + b for a, b in zip(totals, counts)]
    if totals[1] < 9:
        raise ValueError('Missing expected integration examples')
    return {'guides': len(GUIDES), 'local_links': totals[0], 'json_examples': totals[1]}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=ROOT,
                        help='Root of a complete public source export/checkout')
    args = parser.parse_args()
    print('Public AI guides passed: ' + json.dumps(validate_guides(args.root)))
