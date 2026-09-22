# Contributing to power-forensics

power-forensics is a Windows-first, pre-alpha power-forensics tool. Contributions
are welcome, but changes must preserve the project's evidence model: missing
telemetry stays unavailable, estimates stay labelled estimated, and correlation
must not be presented as causation.

## Before opening a pull request

Use Windows x86-64 with Rust 1.98.1 and Node.js 22 or newer.

Run the same gates used by CI:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked

cd pf-gui
npm ci
npm audit --audit-level=high
npm run typecheck
npm test
npm run build
npm run check-version
```

Keep pull requests focused. Do not commit build output, session captures, signing
certificates, credentials, machine-specific logs, or other generated local data.

## Telemetry and analysis changes

When adding or changing evidence:

- preserve provenance as measured, derived, estimated, or unavailable;
- preserve acquisition timing and quality/freshness semantics;
- do not silently replace missing values with zero;
- document hardware, firmware, driver, privilege, and platform limitations;
- add regression tests for parsing, analysis, IPC, or collection behavior where
  the change can be exercised deterministically.

## Security issues

Do not publish exploit details in a normal issue. Follow [SECURITY.md](SECURITY.md).

## License

Unless explicitly stated otherwise, contributions intentionally submitted for
inclusion in power-forensics are accepted under the
[Apache License 2.0](LICENSE), consistent with Section 5 of that license.

The project attribution is recorded in [NOTICE](NOTICE). Contributions must not
remove applicable copyright, license, or attribution notices.
