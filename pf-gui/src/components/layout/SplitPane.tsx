import { useCallback, useEffect, useRef, useState } from 'react'

interface Props {
  id: string
  /** Which child is the user-resizable evidence panel. */
  side: 'left' | 'right'
  children: [React.ReactNode, React.ReactNode]
  initial?: number
  min?: number
  max?: number
}

/**
 * A two-pane layout with one user-resizable panel. Only major panes (chart vs
 * evidence) are resizable; small cards are not. The width persists locally.
 */
export function SplitPane({ id, side, children, initial = 320, min = 200, max = 640 }: Props) {
  const storageKey = `pf.split.${id}`
  const [width, setWidth] = useState(() => {
    try {
      const saved = Number(window.localStorage.getItem(storageKey))
      return Number.isFinite(saved) && saved >= min ? Math.min(saved, max) : initial
    } catch {
      return initial
    }
  })
  const dragging = useRef(false)
  const hostRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    try {
      window.localStorage.setItem(storageKey, String(width))
    } catch {
      // Layout persistence is best-effort; never fail rendering over it.
    }
  }, [storageKey, width])

  const onMove = useCallback(
    (e: PointerEvent) => {
      const host = hostRef.current
      if (!dragging.current || !host) return
      const rect = host.getBoundingClientRect()
      const next =
        side === 'right' ? rect.right - e.clientX : e.clientX - rect.left
      setWidth(Math.max(min, Math.min(max, next)))
    },
    [side, min, max],
  )

  const stop = useCallback(() => {
    dragging.current = false
    document.body.style.cursor = ''
    document.body.style.userSelect = ''
  }, [])

  useEffect(() => {
    window.addEventListener('pointermove', onMove)
    window.addEventListener('pointerup', stop)
    return () => {
      window.removeEventListener('pointermove', onMove)
      window.removeEventListener('pointerup', stop)
    }
  }, [onMove, stop])

  const start = () => {
    dragging.current = true
    document.body.style.cursor = 'col-resize'
    document.body.style.userSelect = 'none'
  }

  const columns =
    side === 'right'
      ? `minmax(0, 1fr) 6px ${width}px`
      : `${width}px 6px minmax(0, 1fr)`

  return (
    <div className="split" ref={hostRef} style={{ gridTemplateColumns: columns }}>
      <div className="split-pane">{children[0]}</div>
      <div
        className="split-handle"
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize panel"
        onPointerDown={start}
      />
      <div className="split-pane">{children[1]}</div>
    </div>
  )
}
