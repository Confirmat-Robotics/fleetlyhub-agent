# Diagnostics, issues and phone notifications

Read [the shared instructions](README.md). Implement a diagnostic/logging module
inside the existing registered robot adapter. Use the
[event schema](../../contracts/local/v1/schemas/robot.event.v1.schema.json) and
[issue lifecycle examples](../../contracts/local/v1/examples/issue-active.json).
The module publishes `events/<category>`; it does not contact a push provider.

## Choose useful robot events

Inspect the existing ROS diagnostics, structured logs, fault codes and application
callbacks. Agree which conditions deserve a team-visible issue, severity mapping,
recovery rules, debounce and stale-source handling. Keep ordinary verbose logs
local. Do not upload every log line or every repeated diagnostics array.

For ROS 2, inspect `diagnostic_msgs/msg/DiagnosticArray` and each contained
`DiagnosticStatus`, plus the actual publisher QoS/topic names. Use a separate
subscriber/adapter rather than modifying diagnostic publishers. `DiagnosticStatus`
has OK/WARN/ERROR/STALE; Fleetly has info/warning/error/critical. A useful mapping
to review with the robot owner is:

| ROS condition | Proposed behavior |
| --- | --- |
| WARN | Open/update a warning issue. |
| ERROR | Open/update an error issue. |
| STALE | A stale-data condition, normally warning/error according to operational impact; never map numeric 3 directly to critical. |
| OK after an active condition | Resolve that condition using its existing event ID. |
| Repeated unchanged OK | No new issue. |
| Robot-specific critical condition | Map only an explicitly identified condition to critical. |

For `/rosout` or custom logs, inspect the actual message and level constants
rather than reusing diagnostic numeric levels. Prefer structured fault callbacks
when available. A publisher disappearing is not proof of recovery; handle its
freshness watchdog separately. Multiple diagnostics may describe independent
faults in one component, so define the condition key explicitly.

## Maintain a real issue lifecycle

Use a stable local condition key (for example component + fault code) to track
an occurrence. Allocate an ASCII event ID once when that occurrence begins;
keep it across message/severity updates and resolution. Use strictly later UTC
timestamps for changed versions of the same occurrence. If two transitions occur
within one millisecond, use a monotonic per-occurrence version timestamp within
the accepted clock tolerance. Preserve ordering; do not let concurrent callbacks
send resolution before activation. A later recurrence gets a new ID.

Persist occurrence identity if the condition must survive adapter restart; do
not restart with random IDs for faults already open. Reconcile current diagnostic
state after reconnect instead of replaying obsolete snapshots. For ambiguous
handoff/retry keep the original event ID/timestamp/payload. Explain any bounded
local buffering and overflow behavior; never silently claim guaranteed delivery.
Follow the shared guide's Agent/outbox error handling.

Use `active=true` for a condition, false for its confirmed recovery, or omit active
for a one-time message. Absence is not false. Do not require every informational
message to be resolved. Debounce noisy signals; identical periodic diagnostics
must not create new occurrences. Limit details to useful measurements/codes,
without credentials, private personal data or large log dumps. Do not add images,
attachments, human acknowledgement fields or robot-selected push requests.

Example first report:

```json
{"protocol":"fleetly.local.v1","topic":"events/diagnostics","payload":{"schema":"robot.event.v1","event_id":"motor-temperature-001","ts":1790000000000,"severity":"warning","source":"robot-diagnostics","active":true,"message":"Drive motor temperature is high","details":{"temperature_c":81}}}
```

Confirmed recovery of that occurrence:

```json
{"protocol":"fleetly.local.v1","topic":"events/diagnostics","payload":{"schema":"robot.event.v1","event_id":"motor-temperature-001","ts":1790000002000,"severity":"info","source":"robot-diagnostics","active":false,"message":"Drive motor temperature recovered","details":{"temperature_c":64}}}
```

## Push is a Hub decision

The Hub stores accepted issues and applies the tenant's Push notifications
settings, severity hierarchy, time windows and issue-transition rules. The robot
cannot demand or guarantee a push. An issue can appear in the app without a new
notification; resolution and ordinary updates do not each cause another push.
There is no notification-image API in this integration.

To test a notification, use the owner's TEST tenant, an authorized app with push
permission/registration and a newly triggered occurrence eligible under its push
policy. Observe foreground, background or closed-app behavior as requested.
Neither a local socket write nor a push-provider send result proves a person
saw the alert. The Agent's internal Hub-storage receipt is not human confirmation.

## Acceptance checks

- Inject WARN -> repeated WARN -> ERROR -> OK for one condition. Expect one ID,
  ordered changed versions, severity escalation and final active=false.
- Inject two simultaneous conditions, then a recurrence; verify isolation and
  new occurrence identity. Test restart, stale source and reconnect behavior.
- Test non-ASCII message byte limits, oversized details, queue overflow and
  `agent/error`. Validate every emitted envelope with the published validator.
- Confirm the issue lifecycle in the TEST app and one eligible notification with
  the human tester. Record policy/device state; do not label an unobserved push PASS.

ROS reference: [DiagnosticStatus](https://github.com/ros2/common_interfaces/blob/rolling/diagnostic_msgs/msg/DiagnosticStatus.msg).
Use installed definitions for the robot's ROS distribution.
