# Third-party software

Apache-2.0 applies to Fleetly-authored source in this repository. It does not
relicense dependency code, toolchains or separately installed camera utilities.
Retain the upstream copyright/licence notices when redistributing their code or
binaries. Direct and transitive dependencies are locked in Agent Cargo.lock,
Media go.mod/go.sum and service-bridge go.mod/go.sum. Build SBOMs accompany CI
artifacts. Inspect the actual dependency versions/licences for your distribution.

Agent uses Rust crates including rustls, reqwest, Tokio, serde and cryptographic
libraries. Media uses the LiveKit Go SDK and Pion; service bridge uses godbus and
golang.org/x/sys. Their upstream source/licence files remain authoritative.
FFmpeg/GStreamer/Python/systemd are external runtime dependencies, not source
whose licence can be replaced by Fleetly. FFmpeg licensing depends on its build.

This repository does not vendor dependency source. Cargo/Go download dependencies
with their upstream licence files. A distributor combining third-party material
must include applicable texts/notices; an SBOM is not a substitute for them.
