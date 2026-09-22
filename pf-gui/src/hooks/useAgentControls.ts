import { useCallback, useRef, useState } from 'react'
import { BridgeFailure, bridge } from '../api/bridge'

export interface StartOptions {
  intervalMs?: number
  collectors?: string
  preset?: string
  note?: string
}

export interface AgentControls {
  busy: boolean
  /** Which asynchronous action is in flight ("start" | "pause" | ...), if any. */
  pending: string | null
  error: BridgeFailure | null
  message: string | null
  start: (opts: StartOptions) => Promise<void>
  pause: () => Promise<void>
  resume: () => Promise<void>
  stop: () => Promise<void>
  marker: (text: string) => Promise<void>
  clear: () => void
}

/** Thin wrapper over the agent control commands; surfaces real errors and
 * exposes which action is pending so controls can disable precisely. */
export function useAgentControls(onChanged?: () => void): AgentControls {
  const [pending, setPending] = useState<string | null>(null)
  const [error, setError] = useState<BridgeFailure | null>(null)
  const [message, setMessage] = useState<string | null>(null)
  const busyRef = useRef(false)

  const run = useCallback(
    async (action: string, fn: () => Promise<unknown>, ok: string) => {
      // Reject a repeated request while one is still in flight instead of
      // firing a duplicate control command.
      if (busyRef.current) return
      busyRef.current = true
      setPending(action)
      setError(null)
      setMessage(null)
      try {
        await fn()
        setMessage(ok)
        onChanged?.()
      } catch (e) {
        setError(e instanceof BridgeFailure ? e : new BridgeFailure('unknown', String(e)))
      } finally {
        busyRef.current = false
        setPending(null)
      }
    },
    [onChanged],
  )

  return {
    busy: pending !== null,
    pending,
    error,
    message,
    start: (opts) => run('start', () => bridge.start(opts), 'Monitoring started.'),
    pause: () => run('pause', () => bridge.pause(), 'Recording paused.'),
    resume: () => run('resume', () => bridge.resume(), 'Recording resumed.'),
    stop: () => run('stop', () => bridge.stop(), 'Stop requested; session is finalizing.'),
    marker: (text) => run('marker', () => bridge.addMarker(text), 'Marker added to session.'),
    clear: () => {
      setError(null)
      setMessage(null)
    },
  }
}
