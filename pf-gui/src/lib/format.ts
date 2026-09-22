import type { EvidenceValue, Provenance, Quality } from '../api/types'

const trimZeros = (s: string) => s.replace(/\.0+$/, '').replace(/(\.\d*?)0+$/, '$1')

export function formatBps(v: number): string {
  const a = Math.abs(v)
  if (a >= 1024 * 1024 * 1024) return `${trimZeros((v / (1024 * 1024 * 1024)).toFixed(2))} GiB/s`
  if (a >= 1024 * 1024) return `${trimZeros((v / (1024 * 1024)).toFixed(2))} MiB/s`
  if (a >= 1024) return `${trimZeros((v / 1024).toFixed(1))} KiB/s`
  return `${trimZeros(v.toFixed(0))} B/s`
}

/** Format a raw reading by unit. Never receives an unavailable value. */
export function formatNumber(value: number, unit: string): string {
  switch (unit) {
    case '%':
      return trimZeros(value.toFixed(Math.abs(value) < 10 ? 1 : 0))
    case 'W':
    case 'Wh':
      return trimZeros(value.toFixed(2))
    case 'MB':
      return trimZeros(value.toFixed(0))
    case 'ratio':
      return trimZeros((value * 100).toFixed(0))
    case 'B/s':
      return formatBps(value)
    case 'bool':
      return value >= 0.5 ? 'yes' : 'no'
    default:
      return trimZeros(value.toFixed(2))
  }
}

export function displayUnit(unit: string): string {
  switch (unit) {
    case 'ratio':
      return '%'
    case 'B/s':
      return ''
    case 'bool':
      return ''
    default:
      return unit
  }
}

/** "13.8 W" or "—" plus reason via the evidence component. */
export function formatEvidence(ev: EvidenceValue | null | undefined): string {
  if (!ev || ev.value === null || ev.value === undefined) return '—'
  const n = formatNumber(ev.value, ev.unit)
  const u = displayUnit(ev.unit)
  return u ? `${n} ${u}` : n
}

export function provenanceLabel(p: Provenance): string {
  switch (p) {
    case 'measured':
      return 'Measured'
    case 'derived':
      return 'Derived'
    case 'estimated':
      return 'Estimated'
    case 'unavailable':
      return 'Unavailable'
  }
}

export function qualityLabel(q: Quality): string {
  switch (q) {
    case 'fresh':
      return 'Fresh'
    case 'repeated':
      return 'Repeated'
    case 'stale':
      return 'Stale'
    case 'error':
      return 'Error'
    case 'unknown':
      return 'Unknown'
  }
}

export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return '0s'
  const total = Math.floor(ms / 1000)
  const h = Math.floor(total / 3600)
  const m = Math.floor((total % 3600) / 60)
  const s = total % 60
  if (h > 0) return `${h}h ${m.toString().padStart(2, '0')}m`
  if (m > 0) return `${m}m ${s.toString().padStart(2, '0')}s`
  return `${s}s`
}

export function formatClock(ms: number): string {
  if (!ms) return '—'
  const d = new Date(ms)
  return d.toLocaleTimeString(undefined, { hour12: false })
}

/** Compact, subtle age of a reading. Empty string when age is unknown. */
export function formatAge(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms) || ms < 0) return ''
  if (ms < 1000) return `${Math.round(ms)} ms ago`
  if (ms < 60_000) return `${trimZeros((ms / 1000).toFixed(1))} s ago`
  const s = Math.floor(ms / 1000)
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ${s % 60}s ago`
  const h = Math.floor(m / 60)
  return `${h}h ${m % 60}m ago`
}

export function formatDateTime(ms: number): string {
  if (!ms) return '—'
  const d = new Date(ms)
  return `${d.toLocaleDateString()} ${d.toLocaleTimeString(undefined, { hour12: false })}`
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KiB', 'MiB', 'GiB']
  let v = bytes
  let i = 0
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i += 1
  }
  return `${trimZeros(v.toFixed(i === 0 ? 0 : 1))} ${units[i]}`
}

export function humanHz(hz: number | null | undefined): string {
  if (hz === null || hz === undefined || !Number.isFinite(hz) || hz <= 0) return '—'
  return `${trimZeros(hz.toFixed(2))} Hz`
}
