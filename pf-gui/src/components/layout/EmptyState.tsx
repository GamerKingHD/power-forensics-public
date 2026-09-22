import type { ReactNode } from 'react'

export interface EmptyStateLine {
  k: string
  v: ReactNode
}

/**
 * Deliberate, text-first empty state used across the workbench. No
 * illustrations: the state, the explanation, the next action and any
 * supporting facts are all legible without decoding.
 */
export function EmptyState({
  title,
  description,
  actions,
  details,
  centered = false,
}: {
  title: string
  description: ReactNode
  actions?: ReactNode
  details?: EmptyStateLine[]
  centered?: boolean
}) {
  return (
    <div className={`empty-state${centered ? ' centered' : ''}`}>
      <div className="es-title">{title}</div>
      <div className="es-desc">{description}</div>
      {actions ? <div className="es-actions">{actions}</div> : null}
      {details && details.length ? (
        <div className="es-details">
          {details.map((d) => (
            <div className="es-line" key={d.k}>
              <span className="k">{d.k}</span>
              <span className="v">{d.v}</span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  )
}
