# GPS, battery and robot status

Read [the shared instructions](README.md) first. Extend the existing robot-adapter
connection; do not create a second telemetry owner. Authoritative definitions:
[telemetry schema](../../contracts/local/v1/schemas/robot.telemetry.v1.schema.json),
[state schema](../../contracts/local/v1/schemas/robot.state.v1.schema.json),
[GPS example](../../contracts/local/v1/examples/telemetry.json) and
[no-fix example](../../contracts/local/v1/examples/telemetry-no-fix.json).

## Discover and agree the mappings

Identify battery percentage, optional temperature, position source and actual
task/mode feedback. Record units, data validity and stale-source timeouts. Fleetly
telemetry is a current snapshot, not a historical sensor-stream recorder. Default
Agent publication is approximately once per second; do not forward every high-rate
GPS/IMU callback as a separate queued message.

| Source | Mapping |
| --- | --- |
| Valid battery fraction, such as ROS `BatteryState.percentage` | Multiply by 100 after checking finite 0..1 input; Fleetly expects 0..100. |
| Battery percent already in 0..100 | Preserve units; do not multiply again. |
| Missing/NaN/stale battery | Battery is required by this contract. Withhold telemetry and report the mapping/source problem through diagnostics; ask the owner for a real alternative if none exists. Do not report a fabricated 100%. |
| Fresh valid WGS84 GPS fix, such as ROS `NavSatFix` | Decimal latitude/longitude together, with `location.known=true`. |
| No fix, invalid coordinates or stale GPS | `location: {"known": false}`; omit the old name/coordinates. |
| Configured location name without GPS | A genuine configured place name can be sent with known=true; do not invent coordinates. |
| Optional temperature | Convert to Celsius; omit unknown/non-finite/out-of-contract values. |

Do not infer GPS validity just from nonzero coordinates: zero latitude/longitude
can be real. Check fix status, freshness and numeric ranges. NavSatFix altitude,
covariance, heading, speed and local-map x/y have no corresponding telemetry
fields here. Local x/y needs an explicitly supplied georeferencing transform;
never label meters as latitude/longitude. Do not add fields to bypass the schema.

## ROS 2 implementation instructions

Inspect actual sources with `ros2 topic list -t`, `ros2 topic info --verbose`
and `ros2 interface show` for the installed message types. Match publisher QoS;
sensor publishers may use best-effort delivery. Keep subscribers observational.
Use a timer to build a coherent latest snapshot and a separate bounded transport
worker. Track each input's local arrival time; do not refresh freshness merely
by resending an old sample. Handle ROS `/use_sim_time` separately from the UTC
timestamp required on the Fleetly envelope.

Do not turn a diagnostic severity into an execution state. Publish `state` from
the robot's actual task/mode API, initially, on change and at least every five
seconds. Available task actions must match [robot controls](robot-controls.md).
If task-state input becomes stale, withdraw advertised actions; do not fabricate
running/paused or keep extending the last state as if it were observed.

Example complete telemetry envelope (timestamps are illustrative, not live data):

```json
{"protocol":"fleetly.local.v1","topic":"telemetry","payload":{"schema":"robot.telemetry.v1","ts":1790000000000,"battery_pct":72.5,"temperature_c":21.0,"location":{"known":true,"lat":51.2,"lon":5.5}}}
```

Unknown GPS with a still-valid battery:

```json
{"protocol":"fleetly.local.v1","topic":"telemetry","payload":{"schema":"robot.telemetry.v1","ts":1790000001000,"battery_pct":72.5,"location":{"known":false}}}
```

## Acceptance checks

- Test battery fractions 0, 0.725 and 1; reject missing/NaN/out-of-range values.
  Test optional temperature omission without suppressing otherwise valid telemetry.
- Test real zero GPS coordinates, no-fix, stale fix, invalid ranges and ROS clock
  reset. Never retain stale coordinates under known=true.
- Validate emitted envelopes; verify partial TCP reads/reconnect and latest-value
  coalescing without blocking robot callbacks.
- On a test robot, confirm the app position/battery/mode agrees with the injected
  readings; remove a sensor source and verify the agreed missing-data behavior.
  Agent Online alone does not prove the telemetry source is healthy.

ROS message references: [BatteryState](https://github.com/ros2/common_interfaces/blob/rolling/sensor_msgs/msg/BatteryState.msg),
[NavSatFix](https://github.com/ros2/common_interfaces/blob/rolling/sensor_msgs/msg/NavSatFix.msg).
Inspect the definitions installed on the robot; these links are references, not
a requirement to upgrade to ROS Rolling.
