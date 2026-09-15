# Security Policy

## Supported Versions

Security fixes are applied to the latest release on the `main` branch. Older versions may not receive fixes.

## Reporting a Vulnerability

Do not open a public issue for a suspected vulnerability. Report it privately through the repository's GitHub security advisory workflow, or contact the maintainer through the email address listed in the GitHub profile.

Include:

- A clear description of the impact
- Steps or a minimal proof of concept
- Affected version, platform, and configuration
- Any suggested mitigation

Please allow time for investigation before public disclosure. Do not include real credentials, private data, or destructive payloads in the report.

## Scope Notes

Portzilla coordinates local process leases and command hooks. It is not a sandbox, firewall, process supervisor, or cross-machine security boundary. Reports involving data loss, command execution, state-file corruption, bypasses of the kill guard, or leakage of raw command data are especially important.
