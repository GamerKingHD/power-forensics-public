import type { ProcessAnalysis } from '../../api/types'

function pct(v: number | null): string {
  return v === null || v === undefined ? '—' : `${v.toFixed(1)}%`
}

/**
 * Process evidence for the selected interval. Enumeration completeness is
 * evidence in its own right: an incomplete list must never look exhaustive.
 */
export function ProcessPanel({
  processes,
  selectedPid,
  onSelect,
}: {
  processes: ProcessAnalysis | null
  selectedPid: number | null
  onSelect: (pid: number | null) => void
}) {
  if (!processes) {
    return (
      <section className="panel">
        <header>Processes</header>
        <div className="content">
          <div className="empty">No range selected.</div>
        </div>
      </section>
    )
  }
  const incomplete = processes.incomplete === true
  return (
    <section className="panel">
      <header>
        Process evidence <span className="hint">{processes.ticks} snapshot(s) in range</span>
        {selectedPid !== null ? (
          <button style={{ marginLeft: 'auto' }} onClick={() => onSelect(null)}>
            Clear process
          </button>
        ) : null}
      </header>
      <div className="content flush">
        {incomplete ? (
          <div className="quality-warning">
            Process evidence incomplete
            {processes.inaccessible
              ? ` — ${processes.inaccessible} process(es) inaccessible`
              : ''}
            {processes.truncated ? ' — list was truncated' : ''}. The list below is not exhaustive.
          </div>
        ) : processes.incomplete === null ? (
          <div className="notice-inline">
            Process enumeration completeness was not recorded for this interval.
          </div>
        ) : null}
        <div className="proc-meta mono-sm muted">
          {processes.totalThreads !== null ? `${processes.totalThreads} threads system-wide · ` : ''}
          {processes.inaccessible !== null ? `${processes.inaccessible} inaccessible · ` : ''}
          {processes.truncated !== null ? `truncated: ${processes.truncated ? 'yes' : 'no'}` : ''}
        </div>
        <div className="proc-table">
          {processes.entries.length ? (
            processes.entries.map((p) => (
              <button
                key={p.pid}
                className={`proc-row${selectedPid === p.pid ? ' active' : ''}`}
                onClick={() => onSelect(selectedPid === p.pid ? null : p.pid)}
                title={`${p.name} (pid ${p.pid}, parent ${p.ppid}); present in ${p.presence}/${p.ticks} snapshots`}
              >
                <span className="mono-sm" style={{ minWidth: 58 }}>
                  {p.pid}
                </span>
                <span style={{ flex: 1, textAlign: 'left' }}>{p.name}</span>
                <span className="mono-sm muted" style={{ minWidth: 70 }}>
                  med {pct(p.cpuMedianPct)}
                </span>
                <span className="mono-sm muted" style={{ minWidth: 64 }}>
                  max {pct(p.cpuMaxPct)}
                </span>
                <span className="mono-sm muted" style={{ minWidth: 78 }}>
                  {p.presence}/{p.ticks} ticks
                </span>
              </button>
            ))
          ) : (
            <div className="empty">No process snapshot falls inside the selected interval.</div>
          )}
        </div>
      </div>
    </section>
  )
}
