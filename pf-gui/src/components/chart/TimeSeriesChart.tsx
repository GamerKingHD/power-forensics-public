import { useEffect, useMemo, useRef } from 'react'
import uPlot from 'uplot'
import type { Provenance, Quality, TimelinePoint } from '../../api/types'
import { displayUnit, formatNumber } from '../../lib/format'
import { useUPlot, resolveScale } from './useUPlot'

export interface CursorInfo {
  tMs: number
  value: number | null
  provenance: Provenance
  quality: Quality
  index: number
}

interface Props {
  points: TimelinePoint[]
  unit: string
  label: string
  height?: number
  /** Optional fixed x-range in wall ms (e.g. a 5-minute quick window). */
  range?: [number, number] | null
  onCursor?: (info: CursorInfo | null) => void
}

const AXIS_COLOR = '#7d8a9a'
const GRID_COLOR = 'rgba(51, 65, 85, 0.55)'
const LINE_COLOR = '#4aa8ff'

function buildData(points: TimelinePoint[]): { data: uPlot.AlignedData; points: TimelinePoint[] } {
  const sorted = [...points].filter((p) => p.tMs > 0).sort((a, b) => a.tMs - b.tMs)
  const xs: number[] = []
  const ys: (number | null)[] = []
  const kept: TimelinePoint[] = []
  let last = -1
  for (const p of sorted) {
    if (p.tMs === last) continue
    last = p.tMs
    xs.push(p.tMs / 1000)
    ys.push(p.value)
    kept.push(p)
  }
  return { data: [xs, ys], points: kept }
}

/**
 * uPlot wrapper. Kept behind this component so the charting library can be
 * replaced without touching pages. Gaps stay gaps (`spanGaps: false`).
 */
export function TimeSeriesChart({ points, unit, label, height = 220, range, onCursor }: Props) {
  const tipRef = useRef<HTMLDivElement | null>(null)
  const { data, points: kept } = useMemo(() => buildData(points), [points])
  const keptRef = useRef(kept)
  keptRef.current = kept
  const cursorRef = useRef(onCursor)
  cursorRef.current = onCursor
  const metaRef = useRef({ unit, label })
  metaRef.current = { unit, label }

  const { hostRef, plotRef } = useUPlot({
    data,
    height,
    debugLabel: `timeseries:${label}`,
    makeOptions: (width, h) => ({
      width,
      height: h,
      padding: [8, 10, 0, 0],
      cursor: {
        drag: { x: true, y: false, setScale: true },
        points: { size: 6 },
      },
      scales: { x: { time: true }, y: { auto: true } },
      axes: [
        {
          stroke: AXIS_COLOR,
          grid: { stroke: GRID_COLOR, width: 1 },
          ticks: { stroke: GRID_COLOR, width: 1 },
          font: '11px Cascadia Mono, monospace',
          // Short spans otherwise emit dozens of sub-second labels that overlap.
          space: 90,
        },
        {
          stroke: AXIS_COLOR,
          grid: { stroke: GRID_COLOR, width: 1 },
          ticks: { stroke: GRID_COLOR, width: 1 },
          font: '11px Cascadia Mono, monospace',
          size: 52,
        },
      ],
      series: [
        {},
        {
          label,
          stroke: LINE_COLOR,
          width: 1.5,
          spanGaps: false,
          // A series with fewer than a few distinct timestamps has no segment
          // to stroke; without markers it would render completely blank.
          points: { show: (u) => (u.data[0]?.length ?? 0) < 3, size: 6 },
        },
      ],
      legend: { show: false },
      hooks: {
        setCursor: [
          (u) => {
            const idx = u.cursor.idx
            const left = u.cursor.left ?? -1
            const top = u.cursor.top ?? -1
            const p = idx == null ? undefined : keptRef.current[idx]
            cursorRef.current?.(
              p
                ? { tMs: p.tMs, value: p.value, provenance: p.provenance, quality: p.quality, index: idx ?? 0 }
                : null,
            )
            const tip = tipRef.current
            if (!tip) return
            if (!p || left < 0) {
              tip.style.display = 'none'
              return
            }
            const m = metaRef.current
            const stamp = new Date(p.tMs).toLocaleTimeString(undefined, { hour12: false })
            const value =
              p.value == null
                ? '— unavailable'
                : `${formatNumber(p.value, m.unit)}${displayUnit(m.unit) ? ` ${displayUnit(m.unit)}` : ''}`
            tip.textContent = `${stamp} · ${m.label}: ${value} · ${p.provenance}/${p.quality}`
            tip.style.display = 'block'
            tip.style.left = `${Math.min(left + 12, (u.width ?? 0) - 300)}px`
            tip.style.top = `${Math.max(top - 30, 4)}px`
          },
        ],
      },
    }),
    applyScale: (plot, d) => {
      // Never hand uPlot a NaN scale: `setScale('x', {min: NaN, max: NaN})`
      // poisons the scale and the series silently stops rendering.
      const scale = resolveScale(range, d)
      if (scale) plot.setScale('x', scale)
    },
    recreateDeps: [height, label],
  })

  // A range-only change (data unchanged) still needs the explicit scale.
  useEffect(() => {
    const plot = plotRef.current
    if (!plot) return
    const scale = resolveScale(range, data)
    if (scale) plot.setScale('x', scale)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [range])

  const empty = data[0].length === 0
  const hasValue = Array.from(data[1] as ArrayLike<number | null>).some((v) => v !== null)
  const allUnavailable = !empty && !hasValue
  const overlay = empty
    ? 'No recorded data in this window'
    : allUnavailable
      ? 'Every sample in this window is unavailable — gaps are not zero'
      : null
  return (
    <div className="chart-wrap" style={{ height }}>
      <div ref={hostRef} style={{ width: '100%', height, overflow: 'hidden' }} />
      <div ref={tipRef} className="chart-tooltip" style={{ display: 'none' }} />
      {overlay ? (
        <div className="empty" style={{ position: 'absolute', inset: 0, display: 'grid', placeItems: 'center' }}>
          {overlay}
        </div>
      ) : null}
    </div>
  )
}
