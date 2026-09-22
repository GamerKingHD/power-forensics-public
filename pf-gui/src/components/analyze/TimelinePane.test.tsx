import { render, act } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type uPlot from 'uplot'
import type { TimelinePoint } from '../../api/types'

// Fake uPlot that records the exact redraw contract. The historical bug was
// `plot.redraw()` (rebuildPaths=true), which re-applies the plot's *current*
// x scale. On first mount uPlot commits asynchronously, so the committed scale
// is still null; the mount-time redraw then overwrote the pending data-fitted
// range with null and the series never drew. TimelinePane must repaint only.
const h = vi.hoisted(() => {
  const instances: any[] = []
  const observers: Array<() => void> = []
  class FakePlot {
    opts: any
    data: uPlot.AlignedData
    width: number
    height: number
    status = 1
    scales = { x: { min: null as number | null, max: null as number | null }, y: { min: null, max: null } }
    hooks: Record<string, Array<() => void>> = {}
    destroyed = false
    redrawArgs: unknown[] = []
    scaleCalls: Array<{ min: number; max: number }> = []
    constructor(opts: any, data: uPlot.AlignedData) {
      this.opts = opts
      this.data = data
      this.width = opts.width
      this.height = opts.height
      instances.push(this)
    }
    setData(d: uPlot.AlignedData) {
      this.data = d
    }
    setScale(_key: string, o: { min: number; max: number }) {
      this.scales.x = { min: o.min, max: o.max }
      this.scaleCalls.push(o)
    }
    setSize() {}
    redraw(rebuildPaths?: boolean) {
      this.redrawArgs.push(rebuildPaths)
    }
    destroy() {
      this.destroyed = true
    }
  }
  class FakeResizeObserver {
    cb: () => void
    constructor(cb: () => void) {
      this.cb = cb
      observers.push(cb)
    }
    observe() {}
    unobserve() {}
    disconnect() {}
  }
  return { instances, observers, FakePlot, FakeResizeObserver }
})

vi.mock('uplot', () => ({ default: h.FakePlot }))

import { TimelinePane } from './TimelinePane'

const POINTS: TimelinePoint[] = [
  { tMs: 1_000_000, value: 10, provenance: 'measured', quality: 'fresh' },
  { tMs: 1_060_000, value: 12, provenance: 'measured', quality: 'fresh' },
  { tMs: 1_120_000, value: 11, provenance: 'measured', quality: 'fresh' },
]

function setWidth(el: HTMLElement, width: number) {
  Object.defineProperty(el, 'clientWidth', { configurable: true, value: width })
}

describe('TimelinePane redraw contract', () => {
  beforeEach(() => {
    h.instances.length = 0
    h.observers.length = 0
    vi.stubGlobal('ResizeObserver', h.FakeResizeObserver)
  })
  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('never re-applies a stale/null scale via redraw after mount', () => {
    const view = render(
      <TimelinePane
        points={POINTS}
        unit="W"
        label="Power"
        viewport={null}
        selection={null}
      />,
    )
    const host = view.container.querySelector('.analyze-pane > div') as HTMLElement
    setWidth(host, 800)
    act(() => h.observers[h.observers.length - 1]())
    expect(h.instances.length).toBe(1)

    // A selection change triggers a repaint on a live pane.
    view.rerender(
      <TimelinePane
        points={POINTS}
        unit="W"
        label="Power"
        viewport={null}
        selection={[1_000_000, 1_060_000]}
      />,
    )
    act(() => h.observers[h.observers.length - 1]())

    expect(h.instances[0].redrawArgs.length).toBeGreaterThan(0)
    // rebuildPaths must always be false: a true value clobbers the x scale.
    expect(h.instances[0].redrawArgs.every((a: unknown) => a === false)).toBe(true)
    // And no scale provided to uPlot is ever null/NaN.
    expect(
      h.instances[0].scaleCalls.every(
        (s: { min: number; max: number }) => Number.isFinite(s.min) && Number.isFinite(s.max),
      ),
    ).toBe(true)
  })
})
