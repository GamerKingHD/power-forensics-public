import { useEffect, useMemo, useRef } from 'react'
import uPlot from 'uplot'
import type { TimelineEvent, TimelinePoint } from '../../api/types'
import { displayUnit, formatNumber } from '../../lib/format'
import { useUPlot, resolveScale } from '../chart/useUPlot'

const AXIS_COLOR = '#7d8a9a'
const GRID_COLOR = 'rgba(51, 65, 85, 0.55)'

const EVENT_COLOR: Record<string, string> = {
  marker: 'rgba(74, 168, 255, 0.85)',
  warning: 'rgba(210, 153, 34, 0.85)',
  error: 'rgba(248, 81, 73, 0.85)',
  info: 'rgba(110, 118, 129, 0.7)',
}

export interface TimelinePaneProps {
  points: TimelinePoint[]
  unit: string
  label: string
  height?: number
  viewport: [number, number] | null
  selection: [number, number] | null
  events?: TimelineEvent[]
  color?: string
  /** Explicit navigation bounds (whole session); falls back to data extent. */
  bounds?: [number, number] | null
  showAxis?: boolean
  /** Disable interaction (e.g. a decorative overview). */
  interactive?: boolean
  /**
   * Optional second series drawn on the same axis (Compare). Additive only:
   * Analyze passes nothing and the pane is unchanged. Callers must only
   * overlay metrics with the same unit and semantic definition.
   */
  overlayPoints?: TimelinePoint[]
  overlayLabel?: string
  overlayColor?: string
  /**
   * Presentation-only x-axis clock. "wall" is the Analyze default. Compare
   * uses "elapsed" (start/end aligned) and "normalized" (0-100%).
   */
  xMode?: 'wall' | 'elapsed' | 'normalized'
  onViewport?: (v: [number, number] | null) => void
  onSelect?: (s: [number, number] | null) => void
  onCursor?: (tMs: number | null) => void
}

function elapsedLabel(seconds: number): string {
  const s = Math.max(0, Math.round(seconds))
  const m = Math.floor(s / 60)
  const sec = s % 60
  if (m >= 60) return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}`
  return `${m}:${String(sec).padStart(2, '0')}`
}

function buildData(
  points: TimelinePoint[],
  overlayPoints?: TimelinePoint[],
): {
  data: uPlot.AlignedData
  kept: TimelinePoint[]
} {
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
  if (overlayPoints && overlayPoints.length) {
    // Union the two timestamp sets; unmatched points become real gaps, never
    // interpolated or carried forward.
    const aMap = new Map<number, number | null>()
    const bMap = new Map<number, number | null>()
    for (const p of sorted) aMap.set(p.tMs, p.value)
    for (const p of overlayPoints) if (p.tMs > 0) bMap.set(p.tMs, p.value)
    const all = [...new Set([...aMap.keys(), ...bMap.keys()])].sort((a, b) => a - b)
    const aVals = all.map((t) => (aMap.has(t) ? (aMap.get(t) as number | null) : null))
    const bVals = all.map((t) => (bMap.has(t) ? (bMap.get(t) as number | null) : null))
    return { data: [all.map((t) => t / 1000), aVals, bVals], kept }
  }
  return { data: [xs, ys], kept }
}

/**
 * One controlled uPlot pane. It owns no range state: viewport, selection and
 * cursor all come from the Analyze page, so every pane is synchronized by
 * construction. Interactions (wheel zoom, drag pan/select) only report.
 */
export function TimelinePane({
  points,
  unit,
  label,
  height = 120,
  viewport,
  selection,
  events = [],
  color = '#4aa8ff',
  bounds,
  showAxis = true,
  interactive = true,
  overlayPoints,
  overlayLabel = 'B',
  overlayColor = '#d29922',
  xMode = 'wall',
  onViewport,
  onSelect,
  onCursor,
}: TimelinePaneProps) {
  const overlayRef = useRef<HTMLDivElement | null>(null)
  const tipRef = useRef<HTMLDivElement | null>(null)
  const hasOverlay = Boolean(overlayPoints && overlayPoints.length)
  const { data, kept } = useMemo(
    () => buildData(points, overlayPoints),
    [points, overlayPoints],
  )
  const keptRef = useRef(kept)
  keptRef.current = kept
  const selectionRef = useRef(selection)
  selectionRef.current = selection
  const cursorRef = useRef<number | null>(null)
  const eventsRef = useRef(events)
  eventsRef.current = events
  const metaRef = useRef({ unit, label })
  metaRef.current = { unit, label }
  const dataBounds = useMemo<[number, number] | null>(() => {
    if (kept.length === 0) return null
    return [kept[0].tMs, kept[kept.length - 1].tMs]
  }, [kept])
  const boundsRef = useRef<[number, number] | null>(null)
  boundsRef.current = bounds ?? dataBounds
  const cbRef = useRef({ onViewport, onSelect, onCursor })
  cbRef.current = { onViewport, onSelect, onCursor }
  const viewportRef = useRef(viewport)
  viewportRef.current = viewport

  const { hostRef, plotRef } = useUPlot({
    data,
    height,
    debugLabel: `timeline:${label}`,
    makeOptions: (width, h) => ({
      width,
      height: h,
      padding: [6, 8, 0, 0],
      cursor: { drag: { x: false, y: false, setScale: false }, points: { show: false } },
      scales: { x: { time: xMode === 'wall' }, y: { auto: true } },
      axes: [
        {
          show: showAxis,
          stroke: AXIS_COLOR,
          grid: { stroke: GRID_COLOR, width: 1 },
          ticks: { stroke: GRID_COLOR, width: 1 },
          font: '11px Cascadia Mono, monospace',
          size: xMode === 'wall' ? 26 : 40,
          values:
            xMode === 'wall'
              ? undefined
              : (_u: uPlot, splits: number[]) =>
                  splits.map((v) =>
                    xMode === 'normalized' ? `${Math.round(v)}%` : elapsedLabel(v),
                  ),
        },
        {
          show: showAxis,
          stroke: AXIS_COLOR,
          grid: { stroke: GRID_COLOR, width: 1 },
          ticks: { stroke: GRID_COLOR, width: 1 },
          font: '11px Cascadia Mono, monospace',
          size: 56,
        },
      ],
      series: [
        {},
        {
          label,
          stroke: color,
          width: 1.4,
          spanGaps: false,
          // A 1-2 point window has no segment to stroke; show the points so a
          // valid tiny series never renders as an empty axis.
          points: { show: (u) => (u.data[0]?.length ?? 0) < 3, size: 4 },
        },
        ...(hasOverlay
          ? [
              {
                label: overlayLabel,
                stroke: overlayColor,
                width: 1.4,
                dash: [5, 4],
                spanGaps: false,
                points: { show: false },
              },
            ]
          : []),
      ],
      legend: { show: false },
      plugins: [
        {
          hooks: {
            draw: [
              (u: uPlot) => {
                const ctx = u.ctx
                ctx.save()
                const b = u.bbox
                // Exact event positions: never bucketed or averaged.
                for (const e of eventsRef.current) {
                  const x = u.valToPos(e.wallMs / 1000, 'x', true)
                  if (!Number.isFinite(x) || x < b.left || x > b.left + b.width) continue
                  ctx.strokeStyle = EVENT_COLOR[e.severity] ?? EVENT_COLOR.info
                  ctx.lineWidth = 1
                  ctx.beginPath()
                  ctx.moveTo(x, b.top)
                  ctx.lineTo(x, b.top + b.height)
                  ctx.stroke()
                }
                const sel = selectionRef.current
                if (sel) {
                  const x0 = u.valToPos(Math.min(sel[0], sel[1]) / 1000, 'x', true)
                  const x1 = u.valToPos(Math.max(sel[0], sel[1]) / 1000, 'x', true)
                  const l = Math.max(x0, b.left)
                  const r = Math.min(x1, b.left + b.width)
                  if (r > l) {
                    ctx.fillStyle = 'rgba(74, 168, 255, 0.13)'
                    ctx.fillRect(l, b.top, r - l, b.height)
                    ctx.strokeStyle = 'rgba(74, 168, 255, 0.75)'
                    ctx.beginPath()
                    ctx.moveTo(l, b.top)
                    ctx.lineTo(l, b.top + b.height)
                    ctx.moveTo(r, b.top)
                    ctx.lineTo(r, b.top + b.height)
                    ctx.stroke()
                  }
                }
                if (cursorRef.current != null) {
                  const x = u.valToPos(cursorRef.current / 1000, 'x', true)
                  if (x >= b.left && x <= b.left + b.width) {
                    ctx.strokeStyle = 'rgba(230, 237, 243, 0.35)'
                    ctx.setLineDash([3, 3])
                    ctx.beginPath()
                    ctx.moveTo(x, b.top)
                    ctx.lineTo(x, b.top + b.height)
                    ctx.stroke()
                    ctx.setLineDash([])
                  }
                }
                ctx.restore()
              },
            ],
          },
        },
      ],
    }),
    applyScale: (plot, d) => {
      // Passing NaN here poisons the scale and the tracks render empty with no
      // axis. Without an explicit viewport, fit the data (or session) bounds.
      const scale = resolveScale(viewport, d)
      if (scale) plot.setScale('x', scale)
    },
    recreateDeps: [height, label, showAxis, hasOverlay, overlayLabel, overlayColor, xMode],
  })

  // Viewport changes that do not touch data still need an explicit scale.
  useEffect(() => {
    const plot = plotRef.current
    if (!plot) return
    const scale = resolveScale(viewport, data)
    if (scale) plot.setScale('x', scale)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [viewport])

  useEffect(() => {
    plotRef.current?.redraw(false)
  }, [selection, events])

  const msAt = (clientX: number): number | null => {
    const plot = plotRef.current
    const overlay = overlayRef.current
    if (!plot || !overlay) return null
    const rect = overlay.getBoundingClientRect()
    const x = clientX - rect.left
    const v = plot.posToVal(x, 'x')
    return Number.isFinite(v) ? v * 1000 : null
  }

  useEffect(() => {
    const overlay = overlayRef.current
    // Do not capture the plot here: it may be constructed later once the pane
    // has a measured width. Handlers resolve the current instance each event.
    if (!overlay || !interactive) return
    let mode: 'none' | 'select' | 'pan' = 'none'
    let startMs = 0
    let startViewport: [number, number] | null = null
    let startClientX = 0
    let moved = false

    const currentViewport = (): [number, number] | null => {
      const b = boundsRef.current
      return viewportRef.current ?? b
    }
    const clampViewport = (v: [number, number]): [number, number] => {
      const b = boundsRef.current
      if (!b) return v
      const full = b[1] - b[0]
      let span = Math.min(Math.max(v[1] - v[0], Math.max(1000, full / 2000)), full)
      if (span >= full) return [b[0], b[1]]
      let from = v[0]
      if (from < b[0]) from = b[0]
      if (from + span > b[1]) from = b[1] - span
      return [from, from + span]
    }

    const onWheel = (e: WheelEvent) => {
      const b = boundsRef.current
      const cur = currentViewport()
      if (!b || !cur) return
      e.preventDefault()
      const at = msAt(e.clientX)
      if (at == null) return
      const factor = e.deltaY > 0 ? 1.3 : 1 / 1.3
      const target = clampViewport([at - ((at - cur[0]) * factor), at + ((cur[1] - at) * factor)])
      cbRef.current.onViewport?.(target)
    }
    const onPointerDown = (e: PointerEvent) => {
      if (e.button !== 0 && e.button !== 1) return
      const at = msAt(e.clientX)
      if (at == null) return
      startMs = at
      startClientX = e.clientX
      startViewport = currentViewport()
      moved = false
      mode = e.shiftKey || e.button === 1 ? 'pan' : 'select'
      overlay.setPointerCapture(e.pointerId)
      cbRef.current.onCursor?.(at)
    }
    const onPointerMove = (e: PointerEvent) => {
      const at = msAt(e.clientX)
      if (at == null) return
      cbRef.current.onCursor?.(at)
      cursorRef.current = at
      plotRef.current?.redraw(false)
      const tip = tipRef.current
      if (tip) {
        const p = nearest(keptRef.current, at)
        if (p) {
          const m = metaRef.current
          const stamp = new Date(p.tMs).toLocaleTimeString(undefined, { hour12: false })
          const value =
            p.value == null
              ? '— unavailable'
              : `${formatNumber(p.value, m.unit)}${displayUnit(m.unit) ? ` ${displayUnit(m.unit)}` : ''}`
          tip.textContent = `${stamp} · ${m.label}: ${value} · ${p.provenance}/${p.quality}`
          tip.style.display = 'block'
          const rect = overlay.getBoundingClientRect()
          tip.style.left = `${Math.min(e.clientX - rect.left + 12, (plotRef.current?.width ?? 0) - 300)}px`
          tip.style.top = `4px`
        }
      }
      if (mode === 'none') return
      if (Math.abs(e.clientX - startClientX) > 3) moved = true
      if (!moved) return
      if (mode === 'select') {
        selectionRef.current = [Math.min(startMs, at), Math.max(startMs, at)]
        plotRef.current?.redraw(false)
      } else if (startViewport) {
        const span = startViewport[1] - startViewport[0]
        const width = overlay.clientWidth || 1
        const dms = ((e.clientX - startClientX) / width) * span
        cbRef.current.onViewport?.(clampViewport([startViewport[0] - dms, startViewport[1] - dms]))
      }
    }
    const finish = (e: PointerEvent) => {
      if (mode === 'select') {
        const at = msAt(e.clientX)
        if (moved && at != null) {
          const sel: [number, number] = [Math.min(startMs, at), Math.max(startMs, at)]
          selectionRef.current = sel
          cbRef.current.onSelect?.(sel)
        } else {
          selectionRef.current = null
          cbRef.current.onSelect?.(null)
        }
        plotRef.current?.redraw(false)
      }
      mode = 'none'
      try {
        overlay.releasePointerCapture(e.pointerId)
      } catch {
        // capture may already be gone
      }
    }
    const onLeave = () => {
      const tip = tipRef.current
      if (tip) tip.style.display = 'none'
      cursorRef.current = null
      plotRef.current?.redraw(false)
      cbRef.current.onCursor?.(null)
    }
    const onDblClick = () => {
      cbRef.current.onViewport?.(null)
    }
    overlay.addEventListener('wheel', onWheel, { passive: false })
    overlay.addEventListener('pointerdown', onPointerDown)
    overlay.addEventListener('pointermove', onPointerMove)
    overlay.addEventListener('pointerup', finish)
    overlay.addEventListener('pointercancel', finish)
    overlay.addEventListener('pointerleave', onLeave)
    overlay.addEventListener('dblclick', onDblClick)
    return () => {
      overlay.removeEventListener('wheel', onWheel)
      overlay.removeEventListener('pointerdown', onPointerDown)
      overlay.removeEventListener('pointermove', onPointerMove)
      overlay.removeEventListener('pointerup', finish)
      overlay.removeEventListener('pointercancel', finish)
      overlay.removeEventListener('pointerleave', onLeave)
      overlay.removeEventListener('dblclick', onDblClick)
    }
  }, [interactive, data])

  const empty = data[0].length === 0
  const hasValue = (data[1] as ArrayLike<number | null>).length
    ? Array.from(data[1] as ArrayLike<number | null>).some((v) => v !== null)
    : false
  const allUnavailable = !empty && !hasValue
  return (
    <div className="chart-wrap analyze-pane" style={{ height }}>
      <div ref={hostRef} style={{ width: '100%', height }} />
      <div
        ref={overlayRef}
        className="analyze-overlay"
        style={{ height, cursor: interactive ? 'crosshair' : 'default' }}
      />
      <div ref={tipRef} className="chart-tooltip" style={{ display: 'none' }} />
      {empty ? (
        <div className="analyze-empty">No recorded data in this window</div>
      ) : allUnavailable ? (
        <div className="analyze-empty">Every sample in this window is unavailable — gaps are not zero</div>
      ) : null}
    </div>
  )
}

function nearest(points: TimelinePoint[], tMs: number): TimelinePoint | null {
  if (points.length === 0) return null
  let lo = 0
  let hi = points.length - 1
  while (lo < hi) {
    const mid = (lo + hi) >> 1
    if (points[mid].tMs < tMs) lo = mid + 1
    else hi = mid
  }
  const a = points[lo]
  const b = points[Math.max(0, lo - 1)]
  return Math.abs(a.tMs - tMs) <= Math.abs(b.tMs - tMs) ? a : b
}
