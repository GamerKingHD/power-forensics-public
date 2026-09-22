# power-forensics <version>

<one-paragraph summary of what changed in this release>

## Core capabilities

- Live Windows power telemetry: battery discharge/charge power and energy,
  CPU/GPU, display, network, storage, USB and system readings, each tagged
  measured / derived / estimated / unavailable.
- Provenance-aware evidence: every reading carries how it was obtained, its
  quality, and dual wall/monotonic timestamps.
- Sessions: append-only recordings with a durability contract and recovery of
  interrupted recordings.
- Analyze: bounded range analysis (energy, changes, domains, processes,
  correlations) over a session timeline.
- Compare: session-vs-session and range-vs-range comparison with per-metric
  comparability, reliability and caveats.
- Experiments: repeated-trial A/B workflows with run-level statistics and
  confounder checks.
- Display-power Calibration: fit and validate a calibration against reference
  values; calibrated values stay labelled estimated.
- HTML reports (self-contained) and CSV/JSON export with optional redaction.

## Important limitations

- Windows x64 only.
- Some telemetry is hardware- and firmware-dependent; unsupported sensors are
  reported unavailable, never faked.
- Calibrated values remain **Estimated**; calibration never promotes an
  estimate to a measurement.
- The GUI does not automatically request elevation; some privileged fields
  stay unavailable until the tool is run elevated.
- No cloud service or account is required, and nothing is uploaded.
- Microsoft WebView2 is required; the installer installs it if missing.

## Installation

Download `power-forensics-<version>-x64-setup.exe` and run it. Or download
`power-forensics-<version>-x64-portable.zip` and run
`power-forensics-gui.exe` from the extracted folder (no installation).

Verify downloads against `SHA256SUMS.txt`.

<if unsigned>
**This build is unsigned.** Windows SmartScreen may show a warning because the
publisher is not yet reputation-verified. Verify the SHA-256 before running.
</if>

<if signed>
This build is Authenticode-signed and RFC3161-timestamped. Verify with
`Get-AuthenticodeSignature`.
</if>

## License and attribution

power-forensics is licensed under Apache-2.0. Redistributions and derivative
works must preserve the applicable license, copyright, and attribution notices,
including the GamerKingHD attribution in `NOTICE`, as required by the license.
