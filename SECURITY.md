# Security policy

ARC Live handles local TLS session keys and short-lived game credentials in
memory. Please do not publish diagnostic archives, keylog files, access tokens,
or security reports in a public issue.

Report a vulnerability through GitHub's private vulnerability reporting for
this repository. Include the affected version, reproduction steps, and the
expected impact. We will acknowledge a complete report as soon as practical.

Supported versions are the latest stable release and the latest beta release.
Update manifests are signed with the ARC Live Ed25519 release key and every
installer is verified against the signed SHA-256 value before launch.
