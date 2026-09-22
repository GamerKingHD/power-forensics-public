interface Props {
  title: string
  cli: string
  description: string
}

/**
 * Honest placeholder: the page states plainly that the workflow is not wired
 * yet and points at the backend CLI verb that already implements it.
 */
export function PlaceholderPage({ title, cli, description }: Props) {
  return (
    <>
      <div className="page-title">
        <h1>{title}</h1>
        <span className="hint">Not implemented in milestone 1</span>
      </div>
      <div className="notice">
        {description}
        <div style={{ marginTop: 8 }}>
          The backend already implements this workflow:
          <pre className="mono-sm" style={{ margin: '6px 0 0' }}>
            {cli}
          </pre>
        </div>
      </div>
    </>
  )
}
