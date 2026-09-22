// Presentation-only configuration for the Analyze workspace. Track choice and
// colors live here; every forensic value still comes from Rust.

export interface TrackDef {
  key: string
  label: string
  seriesKey: string
  unit: string
  color: string
  defaultEnabled: boolean
}

/**
 * Unit-homogeneous tracks: each pane charts exactly one unit so incompatible
 * metrics are never silently placed on a shared axis.
 */
export const TRACKS: TrackDef[] = [
  {
    key: 'power',
    label: 'Power (battery discharge)',
    seriesKey: 'battery_discharge_w',
    unit: 'W',
    color: '#4aa8ff',
    defaultEnabled: true,
  },
  {
    key: 'cpu',
    label: 'CPU utility',
    seriesKey: 'cpu_utility_pct',
    unit: '%',
    color: '#3fb950',
    defaultEnabled: true,
  },
  {
    key: 'cpu_pkg',
    label: 'CPU package (derived)',
    seriesKey: 'cpu_pkg_derived_w',
    unit: 'W',
    color: '#7ee787',
    defaultEnabled: false,
  },
  {
    key: 'gpu',
    label: 'GPU utilization',
    seriesKey: 'gpu_util_pct',
    unit: '%',
    color: '#d29922',
    defaultEnabled: true,
  },
  {
    key: 'display',
    label: 'Display brightness',
    seriesKey: 'display_brightness_pct',
    unit: '%',
    color: '#b58be0',
    defaultEnabled: true,
  },
  {
    key: 'network',
    label: 'Network RX',
    seriesKey: 'net_rx_bps',
    unit: 'B/s',
    color: '#39c5cf',
    defaultEnabled: true,
  },
  {
    key: 'storage',
    label: 'Disk activity',
    seriesKey: 'storage_disk_time_pct',
    unit: '%',
    color: '#db6d28',
    defaultEnabled: true,
  },
]

export function trackByKey(key: string): TrackDef | undefined {
  return TRACKS.find((t) => t.key === key)
}

export type QuickWindow = '1m' | '5m' | '15m' | 'all'

export const QUICK_WINDOWS: { key: QuickWindow; label: string; ms: number | null }[] = [
  { key: '1m', label: '1m', ms: 60_000 },
  { key: '5m', label: '5m', ms: 5 * 60_000 },
  { key: '15m', label: '15m', ms: 15 * 60_000 },
  { key: 'all', label: 'Full session', ms: null },
]

/**
 * Preserve a Compare-selected range when navigating into Analyze. Uses the
 * same sessionStorage slot Analyze already restores, so timestamps the user
 * selected are never re-entered by hand.
 */
export function seedAnalyzeRange(path: string, fromMs: number, toMs: number): void {
  try {
    sessionStorage.setItem(
      `pf.analyze.${path}`,
      JSON.stringify({ viewport: [fromMs, toMs], selection: [fromMs, toMs], pid: null }),
    )
  } catch {
    // Best-effort; navigation must not fail over persistence.
  }
}

/** Observational wording for a change direction; never causal. */
export function directionLabel(direction: string): string {
  switch (direction) {
    case 'increased':
      return 'increased'
    case 'decreased':
      return 'decreased'
    case 'appeared':
      return 'appeared'
    default:
      return direction
  }
}

export function strengthLabel(strength: string): string {
  switch (strength) {
    case 'strong':
      return 'strong'
    case 'moderate':
      return 'moderate'
    case 'weak':
      return 'weak'
    case 'negligible':
      return 'negligible'
    default:
      return 'insufficient evidence'
  }
}
