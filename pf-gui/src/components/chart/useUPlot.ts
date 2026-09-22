import { useEffect, useRef } from 'react'
import type { RefObject } from 'react'
import uPlot from 'uplot'

/**
 * Shared uPlot lifecycle owner.
 *
 * The recurring desktop defect class was charts that never drew on their first
 * mount: the instance was constructed while the WebView container had zero (or
 * stale) layout, so uPlot skipped every draw (`fullWidCss === 0`) even though
 * data and scales were valid. Recreating the component happened to fix it.
 *
 * The invariant here is explicit and deterministic:
 *   data available + container measured non-zero -> the plot renders.
 *
 * Rules enforced for every chart:
 * - an instance is only constructed once the host has a real measured width;
 * - if layout arrives after data, the ResizeObserver constructs it;
 * - data changes never require a remount: `setData` is applied in place;
 * - a resize only calls `setSize`, never destroys the instance;
 * - the caller's scale hook is re-applied after every construction/data change
 *   so a valid viewport is never lost to an auto-range.
 */
export interface UPlotHandle {
  hostRef: RefObject<HTMLDivElement | null>
  plotRef: RefObject<uPlot | null>
}

export interface ScaleRange {
  min: number
  max: number
}

/**
 * Explicit x-scale for a fixed wall-clock window (ms). Returns null rather than
 * a NaN/Infinity range: handing uPlot an invalid scale poisons it and the
 * series silently stops rendering, which is the defect this guards against.
 */
export function windowScale(range: [number, number] | null | undefined): ScaleRange | null {
  if (!range) return null
  const [a, b] = range
  if (!Number.isFinite(a) || !Number.isFinite(b)) return null
  const lo = Math.min(a, b)
  const hi = Math.max(a, b)
  if (hi - lo < 1e-6) return null
  return { min: lo / 1000, max: hi / 1000 }
}

/**
 * Data-fitted x-scale. Returns null for empty or non-finite data instead of a
 * degenerate range. A single timestamp still gets a small pad so it is
 * visible rather than collapsed to a zero-width plot.
 */
export function dataScale(data: uPlot.AlignedData): ScaleRange | null {
  const xs = data[0]
  if (!xs || xs.length === 0) return null
  const lo = xs[0]
  const hi = xs[xs.length - 1]
  if (!Number.isFinite(lo) || !Number.isFinite(hi)) return null
  const pad = Math.max(hi - lo, 1) * 0.02
  return { min: lo - pad, max: hi + pad }
}

/** Resolve the scale a chart should show: explicit window, else data extent. */
export function resolveScale(
  range: [number, number] | null | undefined,
  data: uPlot.AlignedData,
): ScaleRange | null {
  return windowScale(range) ?? dataScale(data)
}

interface Params {
  data: uPlot.AlignedData
  height: number
  makeOptions: (width: number, height: number) => uPlot.Options
  /**
   * Re-assert the x (and any other) scale. Must never pass NaN/Infinity to
   * uPlot. Called after construction and after each data replacement.
   */
  applyScale?: (plot: uPlot, data: uPlot.AlignedData) => void
  /** Options inputs that require a new instance. Never include `data`. */
  recreateDeps: readonly unknown[]
  /** Development-only label used by the plot state inspector. */
  debugLabel?: string
}

interface PlotState {
  label: string
  width: number
  height: number
  dataLen: number
  status: number
  xMin: number | null
  xMax: number | null
}

/**
 * Development-only registry of live plot instances. `window.__pfCharts` is a
 * compact committed snapshot; `window.__pfPlotObjects` keeps the instances so
 * the inspector can read the real uPlot state. Neither exists in production.
 */
function registerPlot(plot: uPlot, label: string | undefined): void {
  if (!import.meta.env.DEV || !label) return
  const w = window as unknown as { __pfPlotObjects?: Record<string, uPlot> }
  w.__pfPlotObjects ??= {}
  w.__pfPlotObjects[label] = plot
}

function recordState(plot: uPlot, label: string | undefined): void {
  if (!import.meta.env.DEV || !label) return
  const w = window as unknown as { __pfCharts?: Record<string, PlotState> }
  w.__pfCharts ??= {}
  const x = plot.scales.x
  w.__pfCharts[label] = {
    label,
    width: plot.width,
    height: plot.height,
    dataLen: plot.data[0]?.length ?? 0,
    status: plot.status,
    xMin: typeof x.min === 'number' && Number.isFinite(x.min) ? x.min : null,
    xMax: typeof x.max === 'number' && Number.isFinite(x.max) ? x.max : null,
  }
}

export function useUPlot({
  data,
  height,
  makeOptions,
  applyScale,
  recreateDeps,
  debugLabel,
}: Params): UPlotHandle {
  const hostRef = useRef<HTMLDivElement | null>(null)
  const plotRef = useRef<uPlot | null>(null)
  const dataRef = useRef(data)
  dataRef.current = data
  const makeRef = useRef(makeOptions)
  makeRef.current = makeOptions
  const scaleRef = useRef(applyScale)
  scaleRef.current = applyScale
  const labelRef = useRef(debugLabel)
  labelRef.current = debugLabel

  // Construct/resize. Recreated only when geometry-affecting inputs change.
  useEffect(() => {
    const host = hostRef.current
    if (!host) return
    let disposed = false

    const build = () => {
      const width = host.clientWidth
      if (width <= 0 || height <= 0 || plotRef.current) return
      const plot = new uPlot(makeRef.current(width, height), dataRef.current, host)
      plotRef.current = plot
      scaleRef.current?.(plot, dataRef.current)
      registerPlot(plot, labelRef.current)
      // Record after each real draw (uPlot commits asynchronously), so the
      // inspector shows committed scale/status rather than the pre-commit state.
      if (import.meta.env.DEV && labelRef.current) {
        plot.hooks.draw = [
          ...(plot.hooks.draw ?? []),
          () => recordState(plot, labelRef.current),
        ]
      }
      recordState(plot, labelRef.current)
    }

    build()
    const ro = new ResizeObserver(() => {
      if (disposed) return
      const plot = plotRef.current
      if (!plot) {
        build()
        return
      }
      const width = host.clientWidth
      if (width > 0 && (plot.width !== width || plot.height !== height)) {
        plot.setSize({ width, height })
        recordState(plot, labelRef.current)
      }
    })
    ro.observe(host)
    return () => {
      disposed = true
      ro.disconnect()
      plotRef.current?.destroy()
      plotRef.current = null
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [height, ...recreateDeps])

  // Apply data in place. If layout was unavailable at build time, construct now.
  useEffect(() => {
    const host = hostRef.current
    let plot = plotRef.current
    if (!plot && host) {
      const width = host.clientWidth
      if (width > 0 && height > 0) {
        plot = new uPlot(makeRef.current(width, height), data, host)
        plotRef.current = plot
      }
    }
    if (!plot) return
    plot.setData(data)
    scaleRef.current?.(plot, data)
    recordState(plot, labelRef.current)
  }, [data, height])

  return { hostRef, plotRef }
}
