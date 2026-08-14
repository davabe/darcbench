# Security policy

## Reporting a vulnerability

**Email `security@getdarc.com`.** Please do not open a public issue.

Include: what you found, how to reproduce it, the affected version, and the
impact as you see it. A proof of concept helps enormously.

**Response targets:** acknowledgement within 3 working days; an initial
assessment within 10; a fix or a documented mitigation for critical issues
within 30. We will keep you updated if any of that slips.

We will credit you in the advisory unless you prefer otherwise. There is no bug
bounty at this stage; we will say so plainly rather than imply one.

## Supported versions

Pre-1.0, only the latest release is supported. After 1.0, the current major and
the previous minor.

## Threat model

The full analysis is in [docs/THREAT-MODEL.md](docs/THREAT-MODEL.md), including
mitigations, tests and accepted residual risks. Read it before reporting — some
things that look like vulnerabilities are documented, deliberate positions.

## In scope

- Remote code execution, command injection, path traversal.
- Authentication or authorisation bypass on the agent API.
- CSRF against mutating endpoints.
- XSS in the dashboard or in generated reports.
- Signature forgery, or any way to make a tampered bundle validate.
- Leaking identifying data that should be redacted.
- Any way to make the agent modify configuration outside its state directory.
- Any way to make the agent generate traffic to a third party.
- Privilege escalation via the agent.

## Known and accepted

These are documented positions, not unreported bugs:

- **An operator can fabricate results on a machine they control.** Unfixable
  without hardware attestation, which DARCBench will not require. Handled by
  classification: a locally-signed bundle never exceeds `SelfReported`.
- **The access token appears in shell scrollback**, and in a reverse proxy's
  access log if one is used. Inherent to a printable bootstrap URL; mitigated by
  per-start generated tokens.
- **Benchmarking degrades the machine being benchmarked.** That is what a
  benchmark does. It is made explicit and requires acknowledgement.
- **The shipped scoring model is uncalibrated.** Stated everywhere, enforced by a
  test.

## Security properties we commit to

Regressions in any of these are treated as security bugs:

1. No code path from an HTTP request to a shell, a filesystem path or a command
   line.
2. No unauthenticated dashboard on any interface.
3. Mutating requests require the `Authorization` header, never a cookie alone.
4. No cloud metadata endpoint is ever queried.
5. No web server, panel or firewall configuration is ever modified.
6. No raw block device is ever written.
7. No user-supplied URL or hostname is ever used as a load-generation target.
8. Identifying values redact by default.
9. The agent signing key is created at mode 0600 and never logged.
10. `unsafe` code is forbidden across the workspace.
