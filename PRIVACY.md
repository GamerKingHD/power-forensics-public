# Privacy and local data handling

power-forensics is a local analysis tool. It has **no cloud service, no
account, and no telemetry upload**. Nothing it records leaves the machine
unless you explicitly export, copy, or attach a file yourself.

## What is recorded

While a session is running, the bundled engine samples the local machine and
writes one append-only JSONL file. A recording can contain:

- battery discharge/charge power and energy, remaining capacity, cycle and
  health counters;
- CPU, GPU, display, network, storage, USB and system power/utilization
  readings, each tagged with how it was obtained (measured, derived,
  estimated, unavailable);
- the active power scheme, foreground process identity, and a bounded list of
  processes observed during sampling (process name, PID, and activity);
- session markers and lifecycle events you create;
- both wall-clock and monotonic timestamps.

Some values are firmware- or hardware-dependent and may be absent; absent
values are recorded as unavailable with a reason, never as zero.

## Where it is stored

- **Installed application:** `%LOCALAPPDATA%\power-forensics\`
  - `sessions\` - recordings (`.jsonl`), the derived `.pf-gui-index.json`
    cache, `experiments\`, `reports\`, and `pf-gui-settings.json`
  - `logs\` - the bounded `pf-gui.log` diagnostic log
- **Portable package:** the same subfolders next to `power-forensics-gui.exe`
  (portable mode is enabled by the bundled `power-forensics.portable` marker).
  Extract to a writable folder.
- You can override the recording directory with the `PF_SESSIONS_DIR`
  environment variable, or point report exports elsewhere in Settings.

The application never writes recordings into `Program Files`. Uninstalling
does **not** delete sessions, reports, experiments or calibrations; only the
application binaries are removed. Deleting your data is always a separate,
explicit action.

## Sensitive content

Process and application identifiers, window/foreground application names, and
system configuration may appear in a recording. HTML reports, CSV and JSON
exports preserve the same identifiers unless you enable **redaction** for that
report/export. Redaction is opt-in per artifact and is recorded in the export
metadata.

Recordings are ordinary files: anyone with access to the storage location can
read them. Treat them as you would any system diagnostic capture.

## Network

The application makes no outbound network connections for collection. The
Windows installer may download the Microsoft WebView2 runtime if it is not
already present (a Microsoft-hosted bootstrapper); that request is made by the
installer, not by the application.
