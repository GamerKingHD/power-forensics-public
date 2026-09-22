# Security Policy

## Supported versions

power-forensics is currently in pre-alpha. Security fixes are applied to the latest public release and the current default branch; older pre-alpha builds are not maintained as separate supported lines.

## Reporting a vulnerability

Please do **not** open a public issue for a vulnerability that could expose local data, cross the desktop application's trust boundary, abuse the elevated helper, or enable arbitrary code execution.

Use GitHub's private vulnerability reporting / Security Advisory flow for this repository when it is available. If that control is not visible, open a public issue containing **no exploit details** and request a private reporting channel. Include the following only in the private report:

- the affected version or commit;
- Windows version and relevant hardware/driver context;
- a minimal reproduction;
- the security impact;
- logs or captures with personal identifiers removed where practical.

Ordinary bugs, telemetry inaccuracies, unsupported hardware, and feature requests can use public GitHub issues.

## Security model

power-forensics is a local desktop application. Its main security boundaries are:

- the Tauri/WebView frontend to Rust command bridge;
- the per-user named-pipe IPC used by the background agent;
- the optional elevated helper;
- local session, report, settings, and log files;
- the release/signing pipeline.

The normal application does not require administrator rights. Privileged collection is isolated in the optional helper and must fail closed to unavailable evidence rather than silently broadening privileges.

See [PRIVACY.md](PRIVACY.md) for data-handling details.
