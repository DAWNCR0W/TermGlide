# Security policy

## Supported versions

The `1.0.x` line is the only supported release line. Pre-1.0 development snapshots are not
security-supported.

## Reporting a vulnerability

Use the repository's
[private GitHub Security Advisory flow](https://github.com/DAWNCR0W/TermGlide/security/advisories/new)
to report a vulnerability. Do not open a public issue for an undisclosed security problem. Include
the affected version, platform, browser version, reproduction steps, impact, and any relevant
sanitized logs.

Do not include passwords, cookies, tokens, normal Chrome profile data, or other secrets in a report.

## Security boundary

TermGlide launches a local Chrome or Chromium process with a unique temporary profile and controls
it through an ephemeral loopback CDP endpoint. It does not reuse the user's normal browser profile,
import credentials, or accept remote CDP endpoints. The temporary process and profile must be
cleaned up on success, failure, cancellation, and terminal restoration paths.

Chrome remains responsible for web-content parsing, scripting, networking, sandboxing, and browser
security updates. Users should keep the selected browser patched.
