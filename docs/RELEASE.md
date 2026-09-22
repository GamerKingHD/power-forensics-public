# Release procedure

This is the exact, repeatable procedure for cutting a power-forensics Windows
release. It assumes Windows x86-64, the pinned Rust toolchain from
`rust-toolchain.toml` (1.98.1) and Node.js.

## Development model

This public repository is a **generated release surface**, not the development
repository. It receives deliberate, release-oriented commits produced by the
private canonical repository's export pipeline; private development history is
never mirrored. Do not develop against this repository — direct edits are
overwritten the next time a release is exported. Community issues and patches
are welcome and are reviewed, then applied in the canonical repository and
re-exported.

The release tooling below builds and packages the application from whatever
tree it is run in (public release surface or a private checkout).

The authoritative build is a single command:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/release-windows.ps1
```

It fails closed: any failed gate, missing sidecar, missing build output, or
failed signature aborts the release.

## 0. Prerequisites

- A Git repository with a remote (GitHub is the intended public host).
- Rust 1.98.1 with `rustfmt`, `clippy` and the `x86_64-pc-windows-msvc` target.
- Node.js (LTS) and npm; run `npm ci` in `pf-gui/` on a clean checkout.
- For a **signed** release only: an Authenticode code-signing certificate and
  `signtool.exe` (Windows SDK), supplied through environment variables (below).
  No certificate is required for local/unsigned builds.

## 1. Update the version (only if not already correct)

One canonical version exists across every workspace crate, `pf-gui/package.json`
and `tauri.conf.json`. Update all of them, then verify:

```powershell
cd pf-gui
npm run check-version
```

Do not maintain unrelated version strings.

## 2. Commit the source

```powershell
git add -A
git commit -m "release: power-forensics 0.1.0"
```

The release script refuses a dirty working tree.

## 3. Tag the release commit

```powershell
git tag -a v0.1.0 -m "power-forensics 0.1.0"
```

The tag must be exactly `v<version>`; `npm run release:check` verifies this.

## 4. Run the local release build (optional but recommended)

```powershell
powershell -ExecutionPolicy Bypass -File scripts/release-windows.ps1
```

This runs all gates, builds the engine, the privileged helper and the Tauri
GUI, optionally signs, and writes `dist-release/v0.1.0/`.

## 5. Push the tag and let CI build

```powershell
git push origin main
git push origin v0.1.0
```

The `Release` workflow (`.github/workflows/release.yml`) triggers on `v*` tags,
repeats every gate, builds the artifacts, signs when signing secrets are
available in the protected `release-signing` environment, and creates the
GitHub Release with the installer, portable ZIP, `SHA256SUMS.txt` and
`release-manifest.json`.

## 6. Verify the release evidence

Download the artifacts and check:

```powershell
Get-FileHash .\power-forensics-0.1.0-x64-setup.exe -Algorithm SHA256
Get-AuthenticodeSignature .\power-forensics-0.1.0-x64-setup.exe | Format-List Status,SignerCertificate,TimeStamperCertificate
```

- `SHA256SUMS.txt` must match the downloaded bytes.
- `release-manifest.json` must name the exact `gitCommit` and `gitTag`.
- The portable ZIP must contain `LICENSE`, `NOTICE`, `PRIVACY.md`, and `THIRD_PARTY_NOTICES.txt`.
- The installed bundle must carry the Apache-2.0 `LICENSE` and project `NOTICE` resources.
- For a signed build, every signature must be `Valid` with a timestamp.
- An unsigned build must be described as `UNSIGNED BUILD`; never as signed.

## 7. Smoke-test the downloaded artifacts

Follow the installer and portable QA steps in this document (sections 8 and 9)
using the files from the Release, not the local build.

## Signing environment variables

Signing is opt-in. When none of the certificate variables are set the script
produces an unsigned build and reports `UNSIGNED BUILD`.

### Signing policy

| Build | Signed? | Enforced by |
| --- | --- | --- |
| Developer validation build | unsigned allowed | default (`-SkipSign` or no cert) |
| CI packaging validation | unsigned allowed | `validate-release-package.yml` passes `-SkipSign` |
| Public release candidate | unsigned by default | `release-manifest.json` records `signed: false`; notes must say `UNSIGNED BUILD` |
| Production stable release | signing required | set `PF_SIGN_REQUIRED=1`; a missing certificate is a hard failure |

A build is never silently downgraded: if `PF_SIGN_REQUIRED=1` is set and no
certificate is configured, the release script fails before packaging. If a
certificate is configured, every distributed binary and the installer are
signed and the signatures (including an RFC3161 timestamp) are verified.

| Variable | Purpose |
| --- | --- |
| `PF_SIGN_CERT` | Path to a `.pfx` code-signing certificate |
| `PF_SIGN_CERT_PASSWORD` | Password for `PF_SIGN_CERT` |
| `PF_SIGN_CERT_SHA1` | Thumbprint of a certificate in the Windows store (alternative to `PF_SIGN_CERT`) |
| `PF_SIGN_TIMESTAMP_URL` | RFC3161 timestamp server (default `http://timestamp.digicert.com`) |
| `PF_SIGN_PUBLISHER` | Expected signer subject substring; verification fails on mismatch |
| `PF_SIGN_REQUIRED` | `1` makes a missing certificate a hard failure |
| `PF_SIGNTOOL` | Explicit path to `signtool.exe` if it is not on `PATH` |

Certificates and passwords must **never** be committed. In CI they are stored
as secrets on the protected `release-signing` environment.

## Versioning

The single source of truth is the workspace version (`Cargo.toml` crates,
`pf-gui/package.json`, `pf-gui/src-tauri/tauri.conf.json`); `npm run
check-version` compares them and fails on any disagreement.

This project does **not** use SemVer prerelease identifiers for its first
release line. `0.1.0` is the first pre-alpha/RC identifier, so the release tag
is exactly `v0.1.0`. Prerelease tags such as `v0.1.0-rc.1` are not used and
would fail exact-tag validation; if they are adopted later, every manifest must
carry the same prerelease string. Exact-tag validation is strict: the packaging
script requires `HEAD` to be at exactly `v<version>` and aborts otherwise.


## Runtime dependencies

`.cargo/config.toml` builds every Windows binary with
`-C target-feature=+crt-static`, so the MSVC C runtime is linked statically.
The distributed GUI, engine and privileged helper therefore do **not** require
the Visual C++ 2015-2022 redistributable, Visual Studio, or the Rust toolchain.
Verify after a build with:

```powershell
dumpbin /dependents target\release\power-forensics-gui.exe
dumpbin /dependents target\release\power-forensics.exe
dumpbin /dependents target\release\pf-elevated.exe
```

None may list `VCRUNTIME140.dll` or `MSVCP140.dll`. The Universal CRT
(`api-ms-win-crt-*`) is part of Windows 10/11.

## 8. Installer QA

1. Install to the default location.
2. Install again to a custom path containing spaces (and Unicode if practical).
3. Launch: the GUI must start and the status/diagnostics pages must show the
   bundled engine (no developer repository or PATH).
4. Record a short session, stop it, and open it in Analyze.
5. Confirm the installed bundle contains the Apache-2.0 `LICENSE` and `NOTICE` resources.
6. Uninstall and confirm that sessions/reports/experiments/calibrations in
   `%LOCALAPPDATA%\power-forensics\` are preserved.

## 9. Portable QA

1. Extract the ZIP to a normal path, a path with spaces, and a directory
   outside any repository.
2. Run `power-forensics-gui.exe`; confirm the GUI starts, the bundled engine is
   found, a recording starts and finalizes, and Analyze opens it.
3. Confirm it does not depend on any file outside the extracted folder and the
   per-user data directory, and that sessions are created next to the
   executable.

## 10. Release checklist

- [ ] Version consistent (`npm run check-version`).
- [ ] Working tree committed; tag `v0.1.0` created.
- [ ] `cargo fmt --all -- --check` clean.
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` clean.
- [ ] `cargo test --workspace --locked` passes.
- [ ] `npm audit --audit-level=high` passes.
- [ ] `npm run typecheck`, `npm test`, `npm run build`, `npm run check-version` pass.
- [ ] Release build produced installer + portable ZIP.
- [ ] Apache-2.0 `LICENSE` and GamerKingHD `NOTICE` are present in distributed packages.
- [ ] `release-manifest.json` records commit, tag and toolchain.
- [ ] `SHA256SUMS.txt` matches final artifact bytes.
- [ ] Signatures valid, or build explicitly labelled `UNSIGNED BUILD`.
- [ ] GitHub Release created from the tag with the four artifacts.
- [ ] Downloaded installer and portable ZIP smoke-tested.
