import { useCallback, useEffect, useRef, useState } from 'react'
import { BridgeFailure } from '../api/bridge'

export interface AsyncState<T> {
  data: T | null
  error: BridgeFailure | null
  loading: boolean
}

function toFailure(e: unknown): BridgeFailure {
  if (e instanceof BridgeFailure) return e
  if (e instanceof Error) return new BridgeFailure('unknown', e.message)
  return new BridgeFailure('unknown', String(e))
}

/**
 * Poll a bridge function on a bounded interval. Uses a timeout chain rather
 * than setInterval so slow calls never overlap, and refreshes are throttled to
 * the cadence the backend can actually produce.
 */
export function usePolling<T>(
  fetcher: () => Promise<T>,
  intervalMs: number,
  enabled = true,
): AsyncState<T> & { refresh: () => void } {
  const [state, setState] = useState<AsyncState<T>>({ data: null, error: null, loading: true })
  const fetcherRef = useRef(fetcher)
  fetcherRef.current = fetcher
  const [nonce, setNonce] = useState(0)

  const refresh = useCallback(() => setNonce((n) => n + 1), [])

  useEffect(() => {
    if (!enabled) return
    let cancelled = false
    let timer: ReturnType<typeof setTimeout> | undefined
    const tick = async () => {
      try {
        const data = await fetcherRef.current()
        if (!cancelled) setState({ data, error: null, loading: false })
      } catch (e) {
        if (!cancelled) setState((prev) => ({ ...prev, error: toFailure(e), loading: false }))
      }
      if (!cancelled) timer = setTimeout(tick, intervalMs)
    }
    void tick()
    return () => {
      cancelled = true
      if (timer) clearTimeout(timer)
    }
  }, [intervalMs, enabled, nonce])

  return { ...state, refresh }
}
