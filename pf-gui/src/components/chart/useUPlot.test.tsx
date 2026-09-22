import { render, screen, act } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type uPlot from 'uplot'
import { dataScale, resolveScale, useUPlot, windowScale } from './useUPlot'

// A minimal uPlot stand-in: enough state to prove the lifecycle guarantees
// (construct only when sized, apply data in place, never receive NaN).
const h = vi.hoisted(() => {
  const instances: any[] = []
  const observers: Array<() => void> = []
  class FakePlot {
    opts: any
    data: uPlot.AlignedData
    host: HTMLElement
    width: number
    height: number
    status = 1
    scales = { x: { min: null as number | null, max: null as number | null } }
    destroyed = false
    setDataCalls = 0
    scaleCalls: Array<{ min: number; max: number }> = []
    sizeCalls = 0
    constructor(opts: any, data: uPlot.AlignedData, host: HTMLElement) {
      this.opts = opts
      this.data = data
      this.host = host
      this.width = opts.width
      this.height = opts.height
      instances.push(this)
    }
    setData(d: uPlot.AlignedData) {
      this.data = d
      this.setDataCalls++
    }
    setScale(_key: string, o: { min: number; max: number }) {
      this.scales.x = { min: o.min, max: o.max }
      this.scaleCalls.push(o)
    }
    setSize({ width, height }: { width: number; height: number }) {
      this.width = width
      this.height = height
      this.sizeCalls++
    }
    redraw() {}
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

function Harness({
  data,
  height = 100,
  label = 'a',
  range,
}: {
  data: uPlot.AlignedData
  height?: number
  label?: string
  range?: [number, number] | null
}) {
  const { hostRef } = useUPlot({
    data,
    height,
    makeOptions: (width, hgt) => ({ width, height: hgt, series: [{}, {}] }) as unknown as uPlot.Options,
    applyScale: (plot, d) => {
      const scale = resolveScale(range, d)
      if (scale) plot.setScale('x', scale)
    },
    recreateDeps: [height, label],
  })
  return <div data-testid="host" ref={hostRef} />
}

const D1: uPlot.AlignedData = [[1, 2, 3], [10, 20, 30]]
const D2: uPlot.AlignedData = [[1, 2, 3, 4], [10, 20, 30, 40]]

function setWidth(el: HTMLElement, width: number) {
  Object.defineProperty(el, 'clientWidth', { configurable: true, value: width })
}

describe('chart scale helpers', () => {
  it('never produces a NaN/Infinity window scale', () => {
    expect(windowScale(null)).toBeNull()
    expect(windowScale([NaN, 100])).toBeNull()
    expect(windowScale([Infinity, 100])).toBeNull()
    expect(windowScale([100, 100])).toBeNull()
    // Reversed input is normalized, not handed to uPlot as min > max.
    expect(windowScale([3000, 1000])).toEqual({ min: 1, max: 3 })
  })

  it('returns no scale for empty data instead of a degenerate range', () => {
    expect(dataScale([[], []])).toBeNull()
    expect(dataScale([[NaN, NaN], [1, 2]])).toBeNull()
  })

  it('pads a single timestamp so it is visible rather than collapsed', () => {
    const s = dataScale([[5], [1]])
    expect(s).not.toBeNull()
    expect(s!.min).toBeLessThan(5)
    expect(s!.max).toBeGreaterThan(5)
    expect(Number.isFinite(s!.min) && Number.isFinite(s!.max)).toBe(true)
  })
})

describe('useUPlot lifecycle', () => {
  beforeEach(() => {
    h.instances.length = 0
    h.observers.length = 0
    vi.stubGlobal('ResizeObserver', h.FakeResizeObserver)
  })

  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('does not construct before layout, then constructs once sized', () => {
    render(<Harness data={D1} />)
    const host = screen.getByTestId('host')
    // jsdom reports zero width: constructing here is what produced invisible
    // charts. The instance must not exist yet.
    expect(h.instances).toHaveLength(0)

    setWidth(host, 800)
    act(() => h.observers[h.observers.length - 1]())
    expect(h.instances).toHaveLength(1)
    expect(h.instances[0].opts.width).toBe(800)
    // A valid scale was applied, and nothing NaN ever reached setScale.
    expect(h.instances[0].scaleCalls.length).toBeGreaterThan(0)
    expect(h.instances[0].scaleCalls.every((s: { min: number; max: number }) => Number.isFinite(s.min) && Number.isFinite(s.max))).toBe(true)
  })

  it('can construct from a data change once layout is available', () => {
    const view = render(<Harness data={[[], []]} />)
    const host = screen.getByTestId('host')
    setWidth(host, 640)
    // No ResizeObserver callback fired; arriving data alone must render.
    view.rerender(<Harness data={D1} />)
    expect(h.instances).toHaveLength(1)
  })

  it('replaces data in place without recreating the instance', () => {
    const view = render(<Harness data={D1} />)
    const host = screen.getByTestId('host')
    setWidth(host, 800)
    act(() => h.observers[h.observers.length - 1]())
    expect(h.instances).toHaveLength(1)

    view.rerender(<Harness data={D2} />)
    expect(h.instances).toHaveLength(1)
    expect(h.instances[0].setDataCalls).toBeGreaterThan(0)
    expect(h.instances[0].data).toBe(D2)
  })

  it('resizes without destroying the instance', () => {
    render(<Harness data={D1} />)
    const host = screen.getByTestId('host')
    setWidth(host, 800)
    act(() => h.observers[h.observers.length - 1]())
    setWidth(host, 420)
    act(() => h.observers[h.observers.length - 1]())
    expect(h.instances).toHaveLength(1)
    expect(h.instances[0].width).toBe(420)
  })

  it('recreates the instance only when options require it', () => {
    const view = render(<Harness data={D1} height={100} />)
    const host = screen.getByTestId('host')
    setWidth(host, 800)
    act(() => h.observers[h.observers.length - 1]())
    view.rerender(<Harness data={D1} height={150} />)
    expect(h.instances).toHaveLength(2)
    expect(h.instances[0].destroyed).toBe(true)
  })

  it('destroys the instance on unmount', () => {
    const view = render(<Harness data={D1} />)
    const host = screen.getByTestId('host')
    setWidth(host, 800)
    act(() => h.observers[h.observers.length - 1]())
    view.unmount()
    expect(h.instances[0].destroyed).toBe(true)
  })
})
