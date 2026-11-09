# Expose selected robot services

Read [the shared instructions](README.md) and [service operation](../services.md).
Choose the supplied Linux systemd bridge for existing units, or implement a
custom service backend. Neither requires ROS. Keep one `service-bridge` owner;
do not run the stock and custom bridges against the same interface simultaneously.
Service actions are independent of [task STOP/PAUSE/RESUME](robot-controls.md).

## Existing systemd services: configure the supplied bridge

Inspect actual units and decide which should be visible and which actions are
allowed. Run the bridge as the account owning the relevant systemd user manager.
`systemctl --user` refers to that account; adding sudo selects a different manager
and is not a solution for a missing user bus. The bridge reads configured system
units but only controls explicitly allowed user units through D-Bus.

The following configuration uses an illustrative user unit. Replace the name
with an actual unit and choose only the actions the owner intends to expose:

```json service-bridge-config
{
  "agent_address": "127.0.0.1:7001",
  "poll_ms": 1000,
  "heartbeat_ms": 5000,
  "timeout_ms": 2000,
  "services": [{"name": "robot-navigation.service", "bus": "user"}],
  "controls": {"robot-navigation.service": ["start", "stop", "restart"]}
}
```

Keep unrelated existing configuration and remove `controls` for status-only use.
Never expose Agent or bridge shutdown/restart as remotely controllable services;
also protect custom unit names/aliases supervising these processes. Preserve the
bridge's persistent journal and account across upgrades. It already implements
registration, D-Bus observation, bounded reconnect and journaled control execution;
do not generate a replacement just to wrap `systemctl` in a shell.

Start with read-only inspection (reads units, does not start/stop them), then run
the configured bridge alongside the enrolled Agent:

```sh
service-bridge --config /absolute/path/services.json --once
service-bridge --config /absolute/path/services.json
```

If boot startup is wanted, generate a systemd user unit with an absolute config
path and the correct account/environment. Explain `systemctl --user daemon-reload`,
enable/start/status/journal commands. For headless boot, the owner's user manager
needs lingering, enabled once with `sudo loginctl enable-linger USERNAME`.
Enabling a unit alone does not start a user manager at machine boot. Install no
root credentials in the unit and do not change Agent enrollment ownership.

## Custom services: implement the same local contract

Use this route when services are application functions or the host is Windows.
The supplied systemd bridge is Linux-only; no Windows Service Control Manager
backend is included. Any generated Windows/backend adapter must still enforce
the explicit local allowlist and confirm actual execution under its chosen account.

Use [service registration](../../contracts/local/v1/examples/register-service-bridge.json),
[status](../../contracts/local/v1/schemas/robot.services-status.v1.schema.json),
[controls](../../contracts/local/v1/schemas/robot.service-control.v1.schema.json)
and [results](../../contracts/local/v1/schemas/robot.service-control-result.v1.schema.json).
Names are unique full `.service` names even for logical application services.
They are identifiers mapped to reviewed callbacks, never shell commands.

Publish complete inventories, including inactive/unavailable services, after
registration, on change and at least every ten seconds. Report observed status
and permitted `available_actions`; do not equate process existence with healthy
operation. A full empty array clears the inventory. Only start/stop/restart are
defined actions, maximum 64 services; do not add arbitrary remote parameters.

```json
{"protocol":"fleetly.local.v1","topic":"services/status","payload":{"schema":"robot.services-status.v1","ts":1790000000000,"services":[{"service":"inspection.service","status":"inactive","substatus":"dead","available_actions":["start"]}]}}
```

On `services/control`, verify exact service/action, issue/expiry times and command
ID before invoking the backend. Use a bounded persistent execution journal where
needed: identical IDs return recorded results without redispatch; conflicting IDs
are rejected. An uncertain interrupted operation requires reconciliation, not a
blind second start/restart. Keep observation/heartbeat responsive during work.

Return correlated `services/control-results/<command_id>` on the receiving
connection, with increasing result sequence and actual outcome. A backend request
being queued is not success. Reconnect sends fresh inventory and does not replay
expired controls. Reuse the stock bridge's implementation as a behavior reference,
without copying its Linux dependencies into an unrelated custom backend.

## App authorization and acceptance

The owner enables Services status in robot properties. The Hub authorizes mobile
users; Viewers see status, authorized Supervisors can use enabled controls. Website
administration is not a robot/service command client. Do not implement user roles
or credentials in the robot bridge, but still enforce its own service/action allowlist.

- First test a disposable unit/fake backend. Confirm full inventory, stopped and
  missing units, status-only services, and per-service action restrictions.
- Test start/stop/restart through the TEST app; check correlated results and the
  actual backend state, not only button appearance or an acknowledgement of receipt.
- Test expired/conflicting/duplicate IDs, backend timeout, bridge restart during
  action, unavailable D-Bus and Agent reconnect. No duplicate side effect is allowed.
- Stop the robot adapter and confirm service reporting remains independent.
  A stale/disconnected bridge must not keep controls falsely available.
- Confirm Viewers cannot control services, Agent/bridge controls are absent, and
  no unrelated robot services, credentials or startup files were changed.
