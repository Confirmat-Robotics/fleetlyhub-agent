# AI-assisted robot integration

Give this document to your coding assistant together with the relevant guides
below and access to your robot project. These are ordinary Markdown instructions;
they need no particular AI vendor, IDE, agent framework or paid AI API.

Public source: [fleetlyhub-agent](https://github.com/Confirmat-Robotics/fleetlyhub-agent).
Read linked guides/contracts from the same source revision. If only this text is
available, obtain those public files before generating an adapter; do not guess
their contents. Keep a reference checkout separate from the customer's project.
Commands in these guides run from that checkout's root unless stated otherwise;
the generated robot package has its own documented working directory.

Start with this request, replacing the bracketed description:

```text
Integrate [my robot/project] with Fleetly Hub. Read docs/ai/README.md in the
fleetlyhub-agent repository and follow the guides for the features I request.
Inspect my existing software first. Preserve its local operation. Show the
mapping and unresolved choices before implementing robot-specific behavior.
Use the published contracts and examples; do not invent Fleetly fields or APIs.
Implement and test the adapter, then give me exact startup/configuration steps
and a report distinguishing automated checks from real robot/app observations.
```

## Choose the integration

| Robot feature | Guide |
| --- | --- |
| USB camera, existing encoder or ROS image source | [Camera](camera.md) |
| GPS, battery and operating status | [Telemetry](telemetry.md) |
| Diagnostics, logs, issues and phone notifications | [Diagnostics](diagnostics.md) |
| Existing systemd units or custom application services | [Services](services.md) |
| STOP and optional PAUSE/RESUME | [Robot controls](robot-controls.md) |

## Instructions to the coding assistant

### 1. Establish the actual environment

Read [compatibility](../../contracts/compatibility.json), the
[local contract](../../contracts/local/v1/contract.md),
[route catalogue](../../contracts/local/v1/routes.json) and relevant schemas.
The contracts take precedence over illustrative guide text and older examples
found elsewhere. An unadvertised field in runtime source is not a supported API.

Inspect the robot OS/architecture, installed Agent version, launch system,
existing adapter and selected camera/services. For ROS 2, inspect the installed
distribution, packages, actual topic types/names, publisher QoS, executor and
task-control APIs. Do not impose a different ROS distribution or replace its
navigation/behavior tree. Ask only for choices that cannot be established from
the project: requested features, source mappings, fault thresholds, permitted
actions and handling of missing data. Record these in the delivered configuration.

Use [installation](../install.md) or [source builds](../build.md) as appropriate.
Enrollment belongs to the operator. Do not request, read, print, copy or commit
private keys, credential directories, user passwords or cloud API secrets.
Use the enrolled Agent's configured environment; do not silently re-enroll it,
change its Hub or rewrite its existing settings. A build does not enroll a robot.

### 2. Preserve the process boundaries

```text
robot software -> one registered robot adapter -> Agent on loopback TCP 7001
systemd/custom services -> independent registered service bridge -> same Agent
camera/encoder -> framed H.264 -> Media on loopback TCP 7002, when authorized
```

Only the Agent owns robot credentials, cloud identity and MQTT. Only the Agent
starts/grants Media. An adapter needs no MQTT client, cloud SDK, web server or
LiveKit token. Keep these interfaces on loopback, in the same network namespace;
container localhost is not the host's localhost. Do not expose these ports to
work around a container configuration problem.

There is one active `robot-adapter` owner and one independent `service-bridge`
owner by default. Add telemetry/diagnostic/control modules to the existing robot
adapter, sharing its connection. Do not launch a second node claiming the same
owner. A ROS node may subscribe to several topics; those are not separate Agent
connections. Custom client IDs/routes require explicitly corresponding Agent
settings and a route-ownership design; do not change settings just to evade an
existing owner. [Registration examples](../../contracts/local/v1/examples/register-robot-adapter.json)
and [service registration](../../contracts/local/v1/examples/register-service-bridge.json)
are full envelopes; a registration may claim a smaller supported route subset.

### 3. Implement the common connection correctly

1. Connect to the configured loopback gateway, normally `127.0.0.1:7001`.
   Send `agent/register`; wait for the matching `agent/registered` before domain
   messages. Handle rejection without replacing another owner.
2. Use UTF-8 NDJSON: one complete envelope followed by a newline. TCP reads may
   split or combine lines. Bound the receive buffer/line length to the documented
   limit; use strict JSON without NaN/infinity. Serialize concurrent writes.
3. Keep a reader running while publishing, including for `agent/error`, task
   controls and media controls. Do not block a ROS executor on TCP/encoding.
   Use bounded queues and a worker/task; coalesce latest-value telemetry/state.
4. Send current snapshots after every successful registration. Reconnect with
   bounded backoff and transition-based logging; disconnect must not stop the
   robot's normal local functions. Do not replay old state or expired actions.
5. Use UTC Unix milliseconds for JSON timestamps and command expiry; use
   monotonic time for local freshness/backoff. ROS simulated time is not UTC.
   Keep clocks synchronized; detect invalid/rolled-back time rather than replay
   commands. Video capture has its separate monotonic microsecond clock.
6. Supply only domain data. Never add robot/tenant/user identity, cloud session,
   sequence, MQTT topic, QoS or retained flags, including inside nested details.
   Omit unknown optional fields; do not use null or manufacture measurements.
7. Surface queue/outbox/registration errors. A successful socket write is not a
   durable Hub receipt. Preserve rejected or ambiguously handed-off issues with
   their original identity/version under an explicit bounded retry policy.
   The Agent handles its own durable cloud outbox; there is no per-message local
   success acknowledgement to use as a promise of exactly-once storage.

### 4. Deliver an integration that can be maintained

Put robot-specific code/configuration in the robot project. Keep Fleetly source
unchanged unless an actual supported-interface defect is identified. Generate a
separate adapter/module with configurable topic names, mappings, freshness limits,
queue bounds and permitted actions. For ROS Python use the robot's installed
`rclpy`/message packages; declare dependencies and provide launch/parameter files
and the project's normal build/run commands. Do not prescribe a universal QoS.

Include mapping tests with fake sensors/task/service backends and a fake TCP
peer for registration, split/coalesced lines, reconnect, errors and backpressure.
Validate captured envelopes using:

```sh
python -m pip install -r tools/contract-requirements.txt
python tools/validate_contracts.py
python tools/validate_contracts.py captured-envelope.json
python tools/validate_ai_guides.py
```

Then exercise the real packaged Agent on a test robot/simulator, followed by the
requested TEST-app observations in each guide. Mocked tests do not establish
camera compatibility, push delivery, actual task execution or systemd behavior.
Actuation tests need a simulator/disposable task or an operator-approved test
setup. Leave real camera access, hardware controls and secrets with the operator.

Deliver the changed files, mapping/configuration, dependency/install commands,
startup and restart/rollback instructions, tests/results and remaining missing
inputs. Use generic paths/placeholders, not the assistant's workstation paths.
Ordinary robot operation must remain possible with the adapter/Agent stopped.
