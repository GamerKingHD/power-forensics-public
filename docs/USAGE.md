# power-forensics — user guide

This is the practical operator guide. It is deliberately short and technical.
For architecture, storage format, and analysis internals see the top-level
`README.md`.

## What it measures

power-forensics records system-wide energy telemetry on Windows: battery
discharge/charge power and energy, CPU/GPU utilization and package power,
display brightness, network throughput, storage activity, process activity,
power scheme and foreground process. It is not a hardware-monitor clone and it
does not invent values.

The fundamental question it answers: **where did the watts go?**

## Evidence model

Every reading is tagged with how it was obtained. The tag is always text, never
only a color:

- **measured** — a collector read the value directly from a sensor/API.
- **derived** — computed from recorded evidence (for example package power
  cross-checks).
- **estimated** — model or calibration based. Calibration never promotes an
  estimate to a measurement.
- **unavailable** — not observed. The UI shows a dash plus the reason; it never
  substitutes `0`.

Correlation is reported with a confidence and the supporting evidence. It is
never upgraded to causation.

## Start your first recording

1. Open **Overview** and find the **Active session** panel (below the metric
   cards).
2. Optionally type a session label, choose an interval and a preset, then click
   **Start monitoring**.
3. The top bar changes to **Recording** with an elapsed timer; the status bar
   shows the agent state and collector counts.
4. Add **markers** (for example "started video call") from Overview or Live
   Monitor. Pause/Resume from Overview. **Stop session** finalizes the recording
   (a footer is written).

The recording is owned by the background agent. Closing the GUI does **not**
stop an active recording; reopen the GUI and it rediscovers the running agent
and session.

## Analyze a session

Open **Analyze** → pick a recording explicitly (Analyze never silently opens the
newest one). The synchronized timeline shows unit-homogeneous tracks. Drag to
select an interval, wheel to zoom, shift-drag to pan, double-click to reset.
Selecting an interval runs the range analysis: energy, quality/coverage,
subsystem changes, process evidence and correlations. **Compare this range**
seeds Compare with the exact timestamps.

## Compare sessions

Open **Compare**, choose side A and side B (whole session or a range), then
**Compare**. Each metric returns a comparability verdict and a reason. Deltas
are shown with reliability and caveats; raw Wh is only comparable for similar
durations, otherwise average power and per-hour energy are emphasized. **Analyze
A/B** reopens the same range in Analyze.

## Run an experiment

Open **Experiments** → **New experiment**. Define the question, primary metric
and groups, then run a guided sequence: the app tells you which group to set,
lets you confirm the treatment, waits out settle, measures, and lets you
Accept or Repeat each run. Ordering (interleaved or seeded-random) is chosen
once and persisted. An interrupted experiment is never auto-resumed; it is
reviewed or resumed explicitly.

## Calibration

Open **Calibration** → **New calibration**. Provide reference values from
external equipment (or clearly labelled manual/synthetic reference data) and
the recorded dataset. The fit, validation and revision history are shown; you
can switch the active revision and Analyze against calibrated evidence.
Calibration never changes raw evidence, and out-of-domain extrapolation is
warned about.

## Reports and export

From Analyze, Compare, Experiments and Calibration use the report bar to preview
or save a self-contained HTML report (print styles included; no external
resources) and to export:

- tidy CSV (long/long-format, Unicode-safe, explicit missing values),
- analysis/compare/experiment/calibration JSON (schema-tagged, with provenance,
  staleness, reasons and redaction state).

Redaction is opt-in per report/export. A saved report remains understandable
without the running application.

## Limitations

- The **Live Monitor** window buttons reflect history accumulated while the page
  stays open; the Rust live tail is byte-bounded, so the newest seconds arrive
  each poll and are accumulated client-side.
- Elevation-gated fields (some storage/USB/OS power detail) are unavailable
  without the elevated helper; the Diagnostics page states where elevation would
  help.
- Absolute battery power is only as good as the firmware's reported sensor.
  Calibration is the supported way to correct it, and it is labelled.
- On AC an empty battery-discharge series is a legitimate "not discharging"
  state, not missing data.
