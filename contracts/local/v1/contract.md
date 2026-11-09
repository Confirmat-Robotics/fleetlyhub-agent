# Registered local interface v1

Robot software talks to the enrolled Agent, normally at `127.0.0.1:7001`, using
UTF-8 newline-delimited JSON. Each line has exactly `protocol`, `topic`, `payload`;
`protocol` is `fleetly.local.v1`. The default maximum line size is 64 KiB.
Partial reads and multiple lines per read are normal TCP behavior. Serialize writes
from concurrent producers. No authentication credentials belong in these messages.

## Registration and independent owners

Send `agent/register` and wait for `agent/registered` before publishing data.
Default client `robot-adapter` owns robot telemetry/state/events/camera status and
task command results. Independent `service-bridge` owns service status/results.
The [route catalogue](routes.json) states directions, owners and payload schemas.
See both registration [examples](examples). Agent `settings.json` must allow every
claim; clients cannot grant themselves rights. Custom IDs require corresponding
local settings. One owner per interface; duplicate registrations do not evict it.
Only the final event/result segment supports `+`, not `#` or MQTT namespaces.

Disconnect releases only that client's routes. Reconnect, register again and send
current snapshots. Do not replay expired commands or old state after reconnect.
An Agent connection is not proof of robot, cloud or sensor health. The loopback
interface trusts local processes; do not expose it on the internet.

## JSON payloads

Schemas reject unknown fields. Omit unknown optional values instead of sending
`null`, NaN or infinity. Time fields are UTC Unix milliseconds; video uses its
separate monotonic microsecond clock. ROS simulation time is not UTC time.
Examples use fixed illustrative timestamps; create fresh timestamps at runtime.
String limits in Agent validation can be UTF-8 byte limits; the offline checker
also checks issue strings. Keep identifiers ASCII and messages bounded.

Never supply robot/tenant/user identity, cloud `session_id` or `seq`. Nested
`details` and command `result` must also omit identity/transport fields. Agent
adds cloud session/sequence; it alone owns MQTT topics, QoS, retention and keys.

### Telemetry

`telemetry`: `robot.telemetry.v1` contains `ts`, `battery_pct` (0–100), optional
`temperature_c` (-100–200), and `location`. `known=false` omits name/coordinates.
`known=true` requires a name or latitude/longitude together, in decimal degrees
(-90–90 / -180–180). No altitude, heading, speed or arbitrary sensor fields are
currently defined. Invalid/stale GPS should become unknown rather than a fake
position. Battery percentage is required; if it cannot be measured/mapped, do
not invent one. Decide the integration's missing-battery policy before publishing.
ROS BatteryState uses 0–1; convert valid percentage to Fleetly's 0–100.
Telemetry is latest-value data, not a durable measurement history.

### Operational state and task commands

`state`: `robot.state.v1` carries `mode`, `execution_state`, `available_commands`
and optional `fault_codes`. Execution states are idle, ready, running, paused,
finished, failed. Publish after registration, on change and at least every five
seconds. Advertise only implemented actions valid in the actual current state.

Agent forwards `commands/stop`, `commands/pause`, `commands/resume` to the owning
connection. STOP cancels the task; PAUSE preserves continuation; RESUME continues
only a paused task. There is no task START. Robot code validates expiry and state,
handles duplicates by command ID, and reports `command-results/<command_id>` on
that same registered connection. Results carry increasing `result_seq` and status
received/accepted/running/succeeded/rejected/failed/expired, optional reason/result.
Succeeded means execution was confirmed, not merely queued. Continue publishing
actual state; Agent does not derive it from results. Operational STOP is not a
safety-rated emergency stop. Do not enable unsupported remote actions.

### Issues and push notifications

`events/<category>`: `robot.event.v1` requires `event_id`, `ts`, `severity`, `source`.
Optional `message`, `details`, `active` describe the issue. Levels are info,
warning, error, critical. `active=true` reports active, false resolves it; absence
is a message without lifecycle state. Keep one ID during a fault and its updates,
use later timestamps, and allocate a new ID for a fresh occurrence. Repeated
identical ROS diagnostics should not create repeated issues. ROS STALE does not
mean critical. Keep details useful and bounded, without credentials/personal data.
Maximum issue payload is 32 KiB, details at most 32 top-level properties.

The Hub applies tenant push settings and issue transitions. No robot field requests
or guarantees a push; not every issue update generates one. Delivery/permissions
also depend on Android and connectivity. Images/attachments are not an issue API.
Only issue storage receipts, handled internally by Agent, confirm durable Hub
processing; they do not confirm a person saw a notification. After Agent accepts
an issue into its durable outbox it retries cloud delivery. If it sends
`event_outbox_unavailable`, preserve the rejected issue and retry after recovery.
Do not treat successful local socket writes as durable Hub acknowledgement.

### Services

`services/status`: a full snapshot, including stopped services, at changes and at
least every ten seconds. Maximum 64 unique `.service` names; status/substatus are
bounded lowercase state strings; `available_actions` is a unique subset of
start/stop/restart. Empty snapshot clears the inventory. Custom services can use
logical `.service` names without systemd but must preserve these semantics.

`services/control` carries the exact service, action, command ID and issue/expiry
times. Execute only explicit allowed actions, never arbitrary shell input.
Return `services/control-results/<command_id>` on its receiving connection,
with increasing result sequence and confirmed outcome. Observe expiry, duplicate
IDs and uncertain interrupted actions; do not execute blindly twice. Service
controls are independent of task STOP/PAUSE/RESUME and video publication.

The supplied Linux systemd bridge reads configured user/system units and controls
only allowlisted user units via D-Bus. It journals actions. Agent and bridge
themselves remain status-only. System-unit writes and a Windows systemd bridge
are not supplied. Website properties and mobile role authorization further limit
what is visible/actionable; the website does not issue robot/service controls.

### Camera and media control

`camera/status` reports unavailable/idle/publishing/error; error requires
`error_code`, other states omit it. This is not video data. Adapter availability
alone does not prove a current cloud publication.

Agent's `media/start` and `media/stop` use `fleetly.local-media-control.v2` with
publication ID and action. Start supplies expiry and a loopback input endpoint
using `fleetly.media.v1` / H.264. Producer connects to Media (normally port 7002),
waits for HELLO_ACK and supplies encoded frames. No reusable LiveKit secret or
participant token is passed to the source adapter. Match stop to its publication;
release the capture source when its Media connection closes. Do not allow an old
stop to terminate a replacement publication. Camera can be a separate helper;
only one producer connects to a publication at once.

### Supervision and errors

`supervision` has the existing aggregate-state schema. Its route/schema is
available, but the new tenant-wide Supervisor heartbeat and pause-ownership
workflow is not currently an implemented guarantee. Do not infer camera viewing
or a specific active operator from login/roles alone, and do not build a safety
claim on this route without an agreed implemented supervision policy.

`agent/error` reports registration, route/ownership, message/queue and outbox
failures. Rejected/absent clients do not accumulate remote commands. Implement
bounded queues/reconnect backoff and expose failures; do not spin or flood logs.
