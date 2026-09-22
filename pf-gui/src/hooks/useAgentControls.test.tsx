import { act, renderHook } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'

const h = vi.hoisted(() => {
  class HoistedBridgeFailure extends Error {
    code: string
    detail?: string
    constructor(code: string, message: string, detail?: string) {
      super(message)
      this.code = code
      this.detail = detail
    }
  }
  return {
    BridgeFailure: HoistedBridgeFailure,
    start: vi.fn(),
    pause: vi.fn(),
    resume: vi.fn(),
    stop: vi.fn(),
    addMarker: vi.fn(),
  }
})

vi.mock('../api/bridge', () => ({
  BridgeFailure: h.BridgeFailure,
  isTauri: () => true,
  bridge: {
    start: h.start,
    pause: h.pause,
    resume: h.resume,
    stop: h.stop,
    addMarker: h.addMarker,
  },
}))

import { useAgentControls } from './useAgentControls'

describe('useAgentControls', () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  it('starts monitoring and reports success', async () => {
    h.start.mockResolvedValue({})
    const onChanged = vi.fn()
    const { result } = renderHook(() => useAgentControls(onChanged))
    await act(async () => {
      await result.current.start({ intervalMs: 1000, note: 'test' })
    })
    expect(h.start).toHaveBeenCalledWith({ intervalMs: 1000, note: 'test' })
    expect(result.current.error).toBeNull()
    expect(result.current.message).toMatch(/started/i)
    expect(result.current.busy).toBe(false)
    expect(onChanged).toHaveBeenCalled()
  })

  it('surfaces a structured error and clears busy state', async () => {
    h.start.mockRejectedValue(new h.BridgeFailure('agent-binary-missing', 'binary not found', 'detail'))
    const { result } = renderHook(() => useAgentControls())
    await act(async () => {
      await result.current.start({})
    })
    expect(result.current.error?.code).toBe('agent-binary-missing')
    expect(result.current.error?.detail).toBe('detail')
    expect(result.current.busy).toBe(false)
  })

  it('maps pause/resume/stop/marker to the bridge', async () => {
    h.pause.mockResolvedValue({})
    h.resume.mockResolvedValue({})
    h.stop.mockResolvedValue({})
    h.addMarker.mockResolvedValue({})
    const { result } = renderHook(() => useAgentControls())
    await act(async () => {
      await result.current.pause()
    })
    await act(async () => {
      await result.current.resume()
    })
    await act(async () => {
      await result.current.stop()
    })
    await act(async () => {
      await result.current.marker('lunch')
    })
    expect(h.pause).toHaveBeenCalled()
    expect(h.resume).toHaveBeenCalled()
    expect(h.stop).toHaveBeenCalled()
    expect(h.addMarker).toHaveBeenCalledWith('lunch')
  })

  it('exposes the pending action and rejects duplicate requests', async () => {
    let release!: () => void
    h.stop.mockReturnValue(
      new Promise<void>((resolve) => {
        release = resolve
      }),
    )
    const { result } = renderHook(() => useAgentControls())
    let first!: Promise<void>
    await act(async () => {
      first = result.current.stop()
      // A second request while the first is in flight must not reach the bridge.
      void result.current.stop()
    })
    expect(result.current.pending).toBe('stop')
    expect(result.current.busy).toBe(true)
    expect(h.stop).toHaveBeenCalledTimes(1)
    await act(async () => {
      release()
      await first
    })
    expect(result.current.pending).toBeNull()
    expect(result.current.busy).toBe(false)
  })
})
