# Build from source

Clone this repository. No other private repository, GitHub token, Fleetly account
or server installation is needed to compile it. Package registries/toolchain
downloads need normal internet access; builds do not enroll a robot.

Use the Rust version in `tools/rust-version`, Go 1.26.0, Python 3.11+ and a
native C toolchain. Linux amd64/arm64 and Windows x64 are the current targets.
The service bridge is Linux-only. Camera runtime needs FFmpeg; Linux V4L2 tools
and Windows GStreamer hardware capture are described in the camera documentation.

```sh
rustup toolchain install "$(cat tools/rust-version)" --profile minimal --component clippy --component rustfmt
rustup default "$(cat tools/rust-version)"
python -m pip install -r tools/contract-requirements.txt
python tools/validate_contracts.py
python tools/build.py --check
python tools/build.py
```

In PowerShell select Rust using `(Get-Content tools/rust-version).Trim()`; the
same Python commands work. `--check` runs Rust/Go checks, contract fixtures and
camera unit tests; it does not operate your camera or systemd units. Output goes
under `dist/`; Windows builds have no service-bridge executable. Source builds
use the ordinary per-process Agent defaults, not machine package paths.

Run the Agent from `dist` with an explicit persistent `--state-dir`; place Media
on the current process PATH or configure its absolute path in `settings.json`.
See installation instructions. Build checks are not real robot/hardware tests.

CI runs native Linux amd64/arm64 and Windows x64 builds and retains short-lived
SBOM evidence without signing keys or production access. These jobs do not
distribute binaries or publish official packages; that remains the separate
release process, including dependency notices and release checks.
