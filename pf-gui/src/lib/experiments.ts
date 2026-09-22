// Presentation-only configuration for the Experiments workspace. Ordering and
// randomization are deterministic and persisted; all statistics come from Rust.

import type {
  ConfounderState,
  ExperimentClassification,
  ExperimentValidity,
  RunGroup,
  TreatmentDirection,
} from '../api/types'

export interface ExperimentMetric {
  key: string
  label: string
  unit: string
  defaultDirection: TreatmentDirection
}

/** Curated primary/secondary metrics, mirroring the Rust catalogue. */
export const EXPERIMENT_METRICS: ExperimentMetric[] = [
  { key: 'battery_discharge_w', label: 'Battery discharge power', unit: 'W', defaultDirection: 'lower' },
  { key: 'battery_pct', label: 'Battery charge', unit: '%', defaultDirection: 'neutral' },
  { key: 'cpu_utility_pct', label: 'CPU utility', unit: '%', defaultDirection: 'lower' },
  { key: 'cpu_pkg_power_w', label: 'CPU package power (measured)', unit: 'W', defaultDirection: 'lower' },
  { key: 'cpu_pkg_derived_w', label: 'CPU package power (derived)', unit: 'W', defaultDirection: 'lower' },
  { key: 'cpu_c3_pct', label: 'Processor C3', unit: '%', defaultDirection: 'neutral' },
  { key: 'gpu_util_pct', label: 'GPU utilization', unit: '%', defaultDirection: 'lower' },
  { key: 'display_brightness_pct', label: 'Display brightness', unit: '%', defaultDirection: 'lower' },
  { key: 'net_rx_bps', label: 'Network RX', unit: 'B/s', defaultDirection: 'neutral' },
  { key: 'net_tx_bps', label: 'Network TX', unit: 'B/s', defaultDirection: 'neutral' },
  { key: 'storage_disk_time_pct', label: 'Disk activity', unit: '%', defaultDirection: 'lower' },
  { key: 'storage_read_bps', label: 'Disk read', unit: 'B/s', defaultDirection: 'neutral' },
  { key: 'storage_write_bps', label: 'Disk write', unit: 'B/s', defaultDirection: 'neutral' },
]

export function metricByKey(key: string): ExperimentMetric | undefined {
  return EXPERIMENT_METRICS.find((m) => m.key === key)
}

export interface PlannedRun {
  group: RunGroup
  pairId: number | null
}

function lcg(seed: number): () => number {
  let s = seed >>> 0
  return () => {
    // Numerical Recipes LCG; deterministic across runs and platforms.
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0
    return s
  }
}

/**
 * Build the guided run sequence. Default interleaves baseline and treatment
 * (A B A B ...) to reduce temporal drift; optional randomization shuffles the
 * sequence with a persisted seed so it is never regenerated halfway through.
 */
export function planRuns(
  reps: number,
  randomized: boolean,
  seed: number,
  paired: boolean,
): PlannedRun[] {
  const n = Math.max(1, Math.floor(reps))
  const seq: PlannedRun[] = []
  for (let i = 1; i <= n; i += 1) {
    seq.push({ group: 'baseline', pairId: paired ? i : null })
    seq.push({ group: 'treatment', pairId: paired ? i : null })
  }
  if (!randomized) return seq
  const rand = lcg(seed || 1)
  for (let i = seq.length - 1; i > 0; i -= 1) {
    const j = rand() % (i + 1)
    const tmp = seq[i]
    seq[i] = seq[j]
    seq[j] = tmp
  }
  return seq
}

export function groupLabel(group: RunGroup): string {
  return group === 'baseline' ? 'Baseline' : 'Treatment'
}

export function classificationLabel(c: ExperimentClassification): string {
  switch (c) {
    case 'consistent_difference':
      return 'Consistent measurable difference'
    case 'possible_difference':
      return 'Possible difference, weak evidence'
    case 'within_noise':
      return 'Within observed run-to-run noise'
    case 'no_difference_detected':
      return 'No measurable difference detected'
    case 'confounded':
      return 'Confounded'
    default:
      return 'Insufficient evidence'
  }
}

/** Warning tint only for confounded/insufficient; a real difference is neutral. */
export function classificationClass(c: ExperimentClassification): string {
  switch (c) {
    case 'confounded':
      return 'error'
    case 'insufficient':
    case 'within_noise':
      return 'degraded'
    case 'possible_difference':
      return 'derived'
    default:
      return 'available'
  }
}

export function validityLabel(v: ExperimentValidity): string {
  switch (v) {
    case 'valid':
      return 'Valid'
    case 'valid_with_caveats':
      return 'Valid with caveats'
    case 'weak':
      return 'Weak'
    case 'confounded':
      return 'Confounded'
    default:
      return 'Insufficient'
  }
}

export function validityClass(v: ExperimentValidity): string {
  switch (v) {
    case 'valid':
      return 'available'
    case 'valid_with_caveats':
      return 'derived'
    case 'weak':
      return 'degraded'
    default:
      return 'error'
  }
}

export function confounderClass(state: ConfounderState): string {
  switch (state) {
    case 'controlled':
      return 'available'
    case 'changed':
      return 'degraded'
    default:
      return 'muted'
  }
}

export function statusClass(status: string): string {
  switch (status) {
    case 'complete':
      return 'available'
    case 'running':
      return 'recording'
    case 'needs_attention':
    case 'interrupted':
      return 'degraded'
    case 'missing':
      return 'error'
    default:
      return 'muted'
  }
}

export function directionLabel(d: TreatmentDirection): string {
  switch (d) {
    case 'lower':
      return 'lower is the desired direction'
    case 'higher':
      return 'higher is the desired direction'
    default:
      return 'informational only'
  }
}

export function formatSeconds(s: number): string {
  if (!Number.isFinite(s) || s < 0) return '—'
  const m = Math.floor(s / 60)
  const sec = Math.floor(s % 60)
  return `${String(m).padStart(2, '0')}:${String(sec).padStart(2, '0')}`
}
