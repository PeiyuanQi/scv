# Security Policy

SCV v0.x receives security fixes on the latest released minor version.

Please report suspected vulnerabilities privately through GitHub's **Report a
vulnerability** feature for this repository. Do not open a public issue with
unreleased exploit details. Include the affected version, operating system,
reproduction, impact, and any suggested mitigation.

SCV is a local developer tool, not an OS sandbox. Approved shell commands and
nested agents run with the invoking user's permissions. Read
[`docs/security.md`](docs/security.md) before using SCV with untrusted code.
