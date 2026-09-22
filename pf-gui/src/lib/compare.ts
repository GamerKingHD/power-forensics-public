// Presentation-only helpers for the Compare workspace. No forensic value is
// computed here: comparability levels, deltas and reliability all arrive from
// Rust. These functions map them to labels/colors and transform time axes.
//
// A/B colors are neutral by design: no green/red "better/worse" semantics.
// Warning colors are reserved for weak evidence and incomparability.

import type {
  ComparabilityLevel,
  ComparisonSide,
  Direction,
  ReliabilityLevel,
  TimelinePoint,
} from '../api/types'
import { displayUnit, formatNumber } from './format'

export const COLOR_A = '#4aa8ff'
export const COLOR_B = '#d29922'

export type AlignmentMode = 'start' | 'end' | 'normalized' | 'absolute'
export type TimelineView = 'stacked' | 'overlay'
export type XMode = 'wall' | 'elapsed' | 'normalized'

export const ALIGNMENTS: { key: AlignmentMode; label: string; hint: string }[] = [
  { key: 'start', label: 'Start-aligned', hint: 'Both sides begin at t=0 (relative alignment)' },
  { key: 'end', label: 'End-aligned', hint: 'Both sides end at t=0' },
  { key: 'normalized', label: 'Normalized 0–100%', hint: 'Progress through each range; changes interpretation' },
  { key: 'absolute', label: 'Wall clock', hint: 'Only meaningful when both are from related periods' },
]

export function alignmentXMode(mode: AlignmentMode): XMode {
  if (mode === 'absolute') return 'wall'
  if (mode === 'normalized') return 'normalized'
  return 'elapsed'
}

/** Presentation-only time transform. Never alters values, only the x clock. */
export function transformPoints(
  points: TimelinePoint[],
  side: ComparisonSide,
  mode: AlignmentMode,
): TimelinePoint[] {
  if (mode === 'absolute') return points
  const span = Math.max(1, side.toMs - side.fromMs)
  return points.map((p) => {
    let tMs: number
    switch (mode) {
      case 'start':
        tMs = p.tMs - side.fromMs
        break
      case 'end':
        tMs = p.tMs - side.toMs
        break
      case 'normalized':
        tMs = ((p.tMs - side.fromMs) / span) * 100_000
        break
      default:
        tMs = p.tMs
    }
    return { ...p, tMs }
  })
}

export function comparabilityLabel(level: ComparabilityLevel): string {
  switch (level) {
    case 'compatible':
      return 'Comparable'
    case 'compatible_with_caveats':
      return 'Comparable with caveats'
    case 'weak':
      return 'Weak comparison'
    default:
      return 'Not comparable'
  }
}

/** Badge class. Only weak/incomparable get a warning tint. */
export function comparabilityClass(level: ComparabilityLevel): string {
  switch (level) {
    case 'compatible':
      return 'available'
    case 'compatible_with_caveats':
      return 'derived'
    case 'weak':
      return 'degraded'
    default:
      return 'unavailable'
  }
}

export function directionLabel(direction: Direction): string {
  switch (direction) {
    case 'increased':
      return 'increased'
    case 'decreased':
      return 'decreased'
    case 'unchanged':
      return 'unchanged'
    case 'missing_a':
      return 'missing in A'
    case 'missing_b':
      return 'missing in B'
    default:
      return 'unavailable'
  }
}

export function reliabilityLabel(level: ReliabilityLevel): string {
  switch (level) {
    case 'distinguishable':
      return 'exceeds noise floor'
    case 'weak_evidence':
      return 'weak evidence'
    case 'within_noise':
      return 'within noise'
    default:
      return 'insufficient evidence'
  }
}

export function reliabilityClass(level: ReliabilityLevel): string {
  switch (level) {
    case 'distinguishable':
      return 'available'
    case 'weak_evidence':
      return 'derived'
    case 'within_noise':
      return 'degraded'
    default:
      return 'unavailable'
  }
}

const trim = (s: string) => s.replace(/\.0+$/, '').replace(/(\.\d*?)0+$/, '$1')

/** A signed absolute delta. Percent units are shown as percentage points. */
export function formatDeltaValue(delta: number, unit: string): string {
  const sign = delta > 0 ? '+' : ''
  if (unit === '%') return `${sign}${trim(delta.toFixed(1))} pp`
  const n = formatNumber(delta, unit)
  const u = displayUnit(unit)
  return `${sign}${n}${u ? ` ${u}` : ''}`
}

/**
 * Absolute delta plus a relative delta only when Rust supplied one. A missing
 * relative delta is shown explicitly as "n/a" rather than a fabricated 0%.
 */
export function formatDelta(
  delta: number | null,
  relative: number | null,
  unit: string,
): string {
  if (delta === null || delta === undefined) return '—'
  const base = formatDeltaValue(delta, unit)
  if (relative === null || relative === undefined) return base
  const pct = `${relative > 0 ? '+' : ''}${trim((relative * 100).toFixed(1))}%`
  return `${base} (${pct})`
}

export function formatValue(v: number | null, unit: string): string {
  if (v === null || v === undefined) return '—'
  const n = formatNumber(v, unit)
  const u = displayUnit(unit)
  return u ? `${n} ${u}` : n
}

export function pctText(v: number | null, digits = 1): string {
  return v === null || v === undefined ? 'unknown' : `${(v * 100).toFixed(digits)}%`
}


