# Robot services

Use the bundled `service-bridge` on Linux to observe selected systemd services.
It connects to the Agent's independent service interface, not to MQTT. Configure
exact names and actions in `services.json`; see the example files. Start under
the deployment user owning the relevant systemd user manager:

```sh
service-bridge --config /absolute/path/services.json
```

User-unit start/stop/restart requires explicit allowlisting. System units are
read-only. Agent and bridge cannot be controlled remotely. The bridge uses D-Bus,
not arbitrary shell commands, and journals actions to avoid repeating them after
retries/crashes. Retain its state directory across upgrades. Camera and robot
telemetry remain separate processes. Do not run a second service-interface owner.

For custom services, implement the same registered service interface and
`services/status`, `services/control`, `services/control-results/<id>` messages.
Names still end in `.service`; they can identify application functions instead
of systemd units. Advertise only permitted actions, enforce expiry/idempotency,
and confirm actual completion. See [the local contract](../contracts/local/v1/contract.md).

Enable service visibility/control in the robot's website properties. Authorized
Supervisors use enabled actions in the app; Viewers have status-only access.
Stale/offline reports disable controls. The website itself does not issue these
actions. Service stop is maintenance, not an emergency or task STOP command.
