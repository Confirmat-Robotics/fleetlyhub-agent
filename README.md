# Fleetly Hub Agent

Robot-side connectivity for Fleetly Hub: the Rust Agent, Go Media forwarder,
camera helper and Linux service bridge. All Fleetly-authored code in this
repository is Apache-2.0. Dependencies retain their own licences.

The robot retains its local software and ownership. The Agent can be stopped,
removed or replaced. Only functions explicitly enabled by the robot owner are
available remotely. Fleetly operational controls are not safety-rated stops.

## Get started

- [Install and enroll](docs/install.md).
- [Build from this public source](docs/build.md), without private GitHub access.
- [Local interface and Media contracts](contracts/README.md).
- [AI-assisted integration: start here](docs/ai/README.md), with camera, telemetry,
  diagnostics/ROS 2, services and task-control guides.
- [Camera helper and existing video sources](docs/camera.md).
- [Linux service bridge](docs/services.md).
- [Security and support](SECURITY.md), [third-party notices](THIRD_PARTY_NOTICES.md).

Your robot software connects locally to the Agent on TCP 7001. Encoded camera
video uses the separate Media input on TCP 7002. Only the Agent owns robot
credentials and the cloud MQTT connection; it starts Media for an authorized
publication. Integrations do not need LiveKit secrets or MQTT certificates.

The source contains the same runtime components used by Fleetly. It contains no
Hub server implementation, deployment credentials or APT publishing access.
Release metadata identifies the public source tree; official package release
and source synchronization are separate operations. Contract metadata states
component/protocol compatibility. Do not use unadvertised runtime fields as a
supported API. The AI guides reference these contracts and include integration
acceptance checks; they do not replace testing on your own robot.
