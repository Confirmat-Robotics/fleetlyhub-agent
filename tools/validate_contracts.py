#!/usr/bin/env python3
"""Validate the supported local contract offline, including byte and route rules."""
import argparse
import json
from pathlib import Path

from jsonschema import Draft202012Validator
from jsonschema.exceptions import ValidationError

ROOT = Path(__file__).resolve().parents[1]
FORBIDDEN = {'robot_id', 'tenant_id', 'organization_id', 'org_id', 'user_id',
             'issued_by_user_id', 'email', 'role', 'topic', 'session_id', 'seq'}


def load(path):
    return json.loads(path.read_text(encoding='utf-8'), parse_constant=lambda _: (
        _ for _ in ()).throw(ValueError('Non-finite JSON number')))


def contract_root(root=ROOT):
    return root / 'contracts/local/v1'


def validator(root=ROOT):
    directory = contract_root(root)
    envelope = load(directory / 'schemas/envelope.schema.json')
    # Resolve only our explicit local payload files: never fetch network schemas.
    for branch in envelope['oneOf']:
        name = branch['properties']['payload']['$ref']
        if Path(name).name != name or not name.endswith('.schema.json'):
            raise ValueError('Unsafe schema reference')
        branch['properties']['payload'] = load(directory / 'schemas' / name)
    Draft202012Validator.check_schema(envelope)
    return Draft202012Validator(envelope)


def reject_identity(value):
    if isinstance(value, dict):
        if FORBIDDEN.intersection(value):
            raise ValueError('Agent-owned identity or transport field in domain data')
        for child in value.values():
            reject_identity(child)
    elif isinstance(value, list):
        for child in value:
            reject_identity(child)


def check(envelope, root=ROOT, compiled=None):
    (compiled or validator(root)).validate(envelope)
    wire = json.dumps(envelope, ensure_ascii=False, allow_nan=False,
                      separators=(',', ':')).encode('utf-8') + b'\n'
    if len(wire) > 65536:
        raise ValueError('Local envelope exceeds the default 64 KiB line limit')
    topic, payload = envelope['topic'], envelope['payload']
    byte_fields = []
    if topic == 'telemetry' and 'name' in payload['location']:
        byte_fields.append((payload['location']['name'], 128))
    if topic == 'state':
        byte_fields.append((payload['mode'], 64))
    if topic.startswith('command-results/') and 'reason' in payload:
        byte_fields.append((payload['reason'], 512))
    if any(len(value.encode('utf-8')) > limit for value, limit in byte_fields):
        raise ValueError('String exceeds its runtime UTF-8 byte limit')
    if topic.startswith('events/'):
        if len(json.dumps(payload, ensure_ascii=False, separators=(',', ':')).encode()) > 32768:
            raise ValueError('Event exceeds 32 KiB')
        reject_identity(payload.get('details', {}))
        for key, limit in [('event_id', 128), ('source', 128), ('message', 1024)]:
            if key in payload and len(payload[key].encode('utf-8')) > limit:
                raise ValueError('Event string exceeds its UTF-8 byte limit')
    if topic.startswith('command-results/'):
        reject_identity(payload.get('result', {}))
    if topic == 'services/status':
        names = [s['service'] for s in payload['services']]
        if len(names) != len(set(names)):
            raise ValueError('Duplicate service names')
        if len(json.dumps(payload, separators=(',', ':')).encode()) > 16000:
            raise ValueError('Service snapshot exceeds the local 16000-byte limit')
    if topic == 'agent/register':
        routes = load(contract_root(root) / 'routes.json')['routes']
        owner = payload['client_id']
        # The two documented default clients. Custom IDs need matching Agent settings.
        if owner in ('robot-adapter', 'service-bridge'):
            for field, direction in [('publish_routes', 'to-agent'), ('subscribe_routes', 'from-agent')]:
                allowed = {r['topic'] for r in routes if r['owner'] == owner and r['direction'] == direction}
                if not set(payload[field]) <= allowed:
                    raise ValueError('Route claim belongs to another interface or direction')
        if not payload['publish_routes'] and not payload['subscribe_routes']:
            raise ValueError('Registration needs at least one route')


def validate_package(root=ROOT):
    directory = contract_root(root)
    compiled = validator(root)
    for schema in (directory / 'schemas').glob('*.json'):
        Draft202012Validator.check_schema(load(schema))
    examples = sorted((directory / 'examples').glob('*.json'))
    if not examples:
        raise ValueError('Missing examples')
    for example in examples:
        check(load(example), root, compiled)
    for case in load(directory / 'invalid-examples.json'):
        try:
            check(case['envelope'], root, compiled)
        except (ValueError, ValidationError):
            pass
        else:
            raise ValueError('Invalid example accepted: ' + case['name'])
    return len(examples)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('message', nargs='?', type=Path, help='Optional local envelope JSON file')
    args = parser.parse_args()
    if args.message:
        check(load(args.message))
        print('Local message satisfies the supported contract.')
    else:
        print(f'Contract schemas and {validate_package()} valid examples passed; invalid examples rejected.')
