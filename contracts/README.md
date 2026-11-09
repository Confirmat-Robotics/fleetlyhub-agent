# Fleetly robot integration contracts

This package describes the supported customer interface to the Fleetly Hub Agent
and Media. It is usable offline and needs no Hub source code, cloud credentials
or AI-provider-specific tools. Read [compatibility.json](compatibility.json) for
the matching component/protocol versions.

- [Registered local JSON interface](local/v1/contract.md),
  [route catalogue](local/v1/routes.json), [schemas](local/v1/schemas),
  [complete envelopes](local/v1/examples).
- [Local H.264 media interface](media/v1/contract.md).

The JSON examples describe local domain data, not MQTT messages. The Agent owns
robot identity, certificate authentication, cloud sessions and sequence counters.
Do not copy cloud envelopes into a robot adapter.

Install `tools/contract-requirements.txt` and run
`python tools/validate_contracts.py`. Add a JSON filename to check one envelope.
Schema/byte validation does not establish authorization, command execution,
delivery, camera hardware compatibility or a human seeing a push notification.

This is the first supported public contract subset. Unadvertised legacy runtime
fields are not part of the customer API. Existing protocol/schema names remain
unchanged. Client-visible breaking changes require an explicit version and
coordinated compatibility decision; editing prose never silently changes runtime.
