# Connect STOP and PAUSE/RESUME to robot tasks

Read [the shared instructions](README.md). This guide connects the robot's own
task API to Fleetly's registered robot-adapter interface. Keep the robot's existing
navigation/mission/behavior-tree implementation. The Agent forwards controls;
it does not implement another task state machine or decide that a task succeeded.

Definitions: [commands](../../contracts/local/v1/schemas/robot.command.v1.schema.json),
[results](../../contracts/local/v1/schemas/robot.command-result.v1.schema.json),
[state](../../contracts/local/v1/schemas/robot.state.v1.schema.json) and
[registration](../../contracts/local/v1/examples/register-robot-adapter.json).

## Discover the real task API first

- STOP cancels the current task. It does not merely pause motion, stop the
  camera process or stop a systemd unit.
- PAUSE preserves task continuation. RESUME continues that paused task; it must
  not create a new goal after cancellation. There is no operational START route.
- Robot properties select STOP only or STOP and PAUSE/RESUME. This permission is
  separate from the adapter's advertised actions and actual backend support.

Identify cancellation, pause/resume and authoritative task-state feedback in the
existing robot project. For ROS actions, inspect the action server, goal identity,
cancel/result semantics and any existing pause/resume service or behavior tree.
Do not assume a generic navigation action supplies pause/resume, implement pause
by publishing zero velocity, or send a replacement goal just to simulate resume.
Ask for the intended behavior if that API is missing; support STOP-only meanwhile.

## Implement bounded, correlated execution

Receive commands on the registered robot connection. Verify route/schema,
command ID, issued/expiry times, parameters and actual task preconditions.
Maintain an idempotency record: identical redelivery must not repeat the action;
the same ID with conflicting contents is rejected. Preserve enough backend/journal
state to reconcile restart uncertainty. Do not execute an action queued past expiry.
Once an accepted operation has started, report its actual outcome; command expiry
is not permission to invent success or silently undo a task action.

Dispatch asynchronously so TCP reads, ROS state feedback, service reporting and
camera operation remain responsive. Return results on
`command-results/<command_id>` with increasing `result_seq` for that command.
Use accepted/running for real intermediate stages; succeeded requires confirmed
completion, rejected for invalid preconditions and failed for execution failure.
Do not claim success as soon as a ROS service request/goal cancellation is sent.

Example received pause request (illustrative timestamps):

```json
{"protocol":"fleetly.local.v1","topic":"commands/pause","payload":{"schema":"robot.command.v1","command_id":"command-001","issued_at":1790000000000,"expires_at":1790000030000,"parameters":{}}}
```

Example result after the task API confirms pausing:

```json
{"protocol":"fleetly.local.v1","topic":"command-results/command-001","payload":{"schema":"robot.command-result.v1","ts":1790000001000,"result_seq":1,"status":"succeeded"}}
```

Do not store remote commands for blind replay on another connection/process.
The Agent accepts results only for commands delivered to that receiving owner.
After connection loss, reconcile actual task state; do not invent an independent
cloud command/result exchange or reuse a prior owner's result channel.

## Report state from observation

Publish state after registration, on changes and at least every five seconds.
Execution states are idle, ready, running, paused, finished and failed. Use the
real mode; fault codes are bounded identifiers defined by the contract, not log
dumps. Available actions reflect both implementation and current conditions:

| Actual state | Typical extended controls |
| --- | --- |
| Running task | stop, pause |
| Paused task | stop, resume |
| Cancelled/no resumable task | no resume; normally no task actions |
| Unknown/stale task feedback | withdraw actions; expose the source problem |

STOP-only adapters advertise only stop when applicable. Robot properties and
roles can further restrict controls; never bypass them with extra routes.

```json
{"protocol":"fleetly.local.v1","topic":"state","payload":{"schema":"robot.state.v1","ts":1790000001000,"mode":"autonomous","execution_state":"paused","available_commands":["stop","resume"],"fault_codes":[]}}
```

If feedback is lost, remove advertised actions rather than refreshing stale
running/paused evidence. Do not add an undocumented unknown execution enum.
If no truthful execution-state snapshot can be formed, stop claiming fresh state
and report the source failure; recovery must query the backend before enabling
actions again. Result success and state observation are separate evidence.

Camera publication and service actions remain independent. Do not synthesize
STOP when a supervisor/app disconnects. The existing `supervision` schema is not
a guarantee of the future tenant-wide supervisor-heartbeat/pause-owner policy;
do not invent exclusive ownership or claim safety/compliance from login state.
Fleetly task controls are not a safety-rated emergency-stop system.

## Acceptance checks

- With a fake task API, test RUNNING -> PAUSED -> RUNNING, then PAUSED -> STOP
  -> IDLE and reject subsequent RESUME. STOP must really cancel the task.
- Test STOP-only capability, invalid/stale state, expired/conflicting/duplicate
  commands, backend rejection/timeout and restart uncertainty. Correlate every
  reported result with the command actually received.
- Validate envelopes; test that long-running control work does not block state,
  diagnostics, camera or transport heartbeat/reconnect behavior.
- In the TEST app, confirm Supervisor controls match robot properties and current
  state, Viewer controls are absent, and real test-backend state changes agree
  with reported results. Use a simulator or an operator-approved task setup.
