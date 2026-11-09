# Installation and enrollment

Linux users can install the official package from the signed public APT source:

```sh
curl -fsSLo /tmp/fleetlyhub-archive-keyring.asc https://apt.fleetlyhub.eu/fleetlyhub-archive-keyring.asc
gpg --show-keys --with-fingerprint /tmp/fleetlyhub-archive-keyring.asc
# Verify fingerprint 0C87DB14A08882B4697E0A7AFD01E23E3D80E352 before proceeding.
sudo install -m 0644 /tmp/fleetlyhub-archive-keyring.asc /usr/share/keyrings/fleetlyhub-archive-keyring.asc
echo 'deb [signed-by=/usr/share/keyrings/fleetlyhub-archive-keyring.asc] https://apt.fleetlyhub.eu stable main' | sudo tee /etc/apt/sources.list.d/fleetlyhub.list
sudo apt update
sudo apt install fleetlyhub-agent
```

Use the official website's current installation instructions to verify the key
fingerprint and repository setup before installation. A public verification key
does not permit package publication. Packages support Linux amd64/arm64; native
Windows x64 can build the public source with the included launcher. Public source
synchronization does not itself publish official release ZIPs or APT versions.

Create your robot on `https://fleetlyhub.eu`, then enroll and run under the same
account and persistent state directory:

```sh
fleetlyhub-agent enroll --state-dir /absolute/path/to/agent-state
fleetlyhub-agent --state-dir /absolute/path/to/agent-state
```

Packaged Linux commands normally use `/var/lib/fleetly-agent`; access may require
the configured deployment account or sudo. Keep its ownership consistent. Source
builds default to `.local/enrolled` relative to the working directory; choose an
absolute persistent directory instead. Never commit this runtime directory.

Approve the displayed code and fingerprint in the intended robot's website setup.
The default enrollment endpoint is `https://api.fleetlyhub.eu:8443`. Public HTTPS
uses system trust; no private CA or manually supplied robot key is needed.
Credentials are generated/stored locally. To change an existing installation,
revoke it through Set up Agent again, then enroll with `--replace` only if previous
local credentials remain. Updating software does not require re-enrollment.

Agent starts Media on demand. It must be in the current PATH or explicitly
configured. Camera helper runs separately, waits for authorized Media, then opens
the selected camera. Installation does not enable/start any systemd service.
Use optional [user service examples](../examples/systemd-user) with the same
deployment account/state directory. Review unit paths before enabling them.
`loginctl enable-linger USER` lets that account's user services run without an
interactive login. `systemctl --user` runs as that user, not through sudo.

Back up credentials privately. Stop the Agent before changing binaries/settings,
retain credentials and journals, and restart your own process supervisor. Do not
share an enrollment directory between robots or simultaneously running Agents.
