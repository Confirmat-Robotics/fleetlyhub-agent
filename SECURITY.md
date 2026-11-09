# Security and support

Use this repository's Security tab → Report a vulnerability for a private report.
If unavailable, use https://fleetlyhub.eu/contact and request a private security
conversation without exploit details in the initial message. Do not publish
credentials, robot identifiers or private diagnostics in a public issue.

Use the current supported official release. Security support initially covers
the latest stable Agent/Media combination, not arbitrary historical/custom builds
or customer ROS/OEM software. Source synchronization is not a release approval.
SBOMs/checksums help identify components; they do not establish absence of flaws.

Keep local interfaces on loopback. Never share enrollment directories, private
keys, participant tokens or service-control journals. Expose only explicit
services/actions. Robot-local safety functions remain the manufacturer's concern;
network/app controls are not a safety-rated emergency stop.
