import { useState } from 'react'
import { bridge } from '../api/bridge'
import type { CollectorCapability } from '../api/types'
import { usePolling } from '../hooks/usePolling'

function stateBadge(state: string): string {
  return state === 'available' ? 'available' : state === 'degraded' ? 'degraded' : 'error'
}

function CollectorCard({ c }: { c: CollectorCapability }) {
  const [open, setOpen] = useState(c.state !== 'available')
  return (
    <section className="panel">
      <header>
        <button
          onClick={() => setOpen((v) => !v)}
          style={{ background: 'transparent', border: 'none', padding: 0, color: 'inherit', cursor: 'pointer' }}
        >
          {open ? '▾' : '▸'} {c.name}
        </button>
        <span className={`badge ${stateBadge(c.state)}`}>{c.state}</span>
        <span className="spacer" />
        <span className="muted" style={{ textTransform: 'none', letterSpacing: 0 }}>
          {c.available} available · {c.unavailable} unavailable
        </span>
      </header>
      {open ? (
        <div className="content flush">
          <div className="list">
            {c.fields.map((f) => (
              <div className="row" key={f.name} title={f.notes || f.reason}>
                <span className={`dot ${f.state === 'available' ? 'ok' : 'error'}`} aria-hidden />
                <span style={{ minWidth: 220 }} className="name">
                  {f.name}
                </span>
                <span className={`badge ${f.state === 'available' ? 'available' : 'error'}`}>{f.state}</span>
                <span className="muted mono-sm">{f.unit || '—'}</span>
                <span className="muted mono-sm" style={{ minWidth: 120 }}>
                  [{f.source || 'none'}]
                </span>
                <span className="muted mono-sm">{f.provenance}</span>
                {f.intervalMs ? <span className="muted mono-sm">{f.intervalMs} ms</span> : null}
                {f.state === 'unavailable' ? (
                  <span className="mono-sm" style={{ marginLeft: 'auto' }}>
                    {f.reason || f.notes}
                    {f.elevationWouldHelp ? <span className="badge warn">elevation helps</span> : null}
                  </span>
                ) : null}
              </div>
            ))}
          </div>
        </div>
      ) : null}
    </section>
  )
}

function AppLayerPanel() {
  const diag = usePolling(bridge.diagnostics, 10000)
  const d = diag.data
  if (!d) return null
  return (
    <section className="panel">
      <header>
        Application layer <span className="hint">support/debugging state; no secrets</span>
      </header>
      <div className="content">
        <div className="kv">
          <div className="k">Version</div>
          <div className="v mono-sm">
            GUI {d.guiVersion} · tool {d.toolVersion}
          </div>
          <div className="k">Session directory</div>
          <div className="v mono-sm">{d.sessionsDir}</div>
          <div className="k">Agent pipe</div>
          <div className="v mono-sm">{d.agentPipe}</div>
          <div className="k">Session index</div>
          <div className="v mono-sm">
            {d.indexEntries} entries · schema v{d.indexSchema}
          </div>
          <div className="k">Active calibrations</div>
          <div className="v mono-sm">
            {d.calibrationActiveIds.length ? d.calibrationActiveIds.join(', ') : 'none'}
          </div>
          <div className="k">Experiment store</div>
          <div className="v mono-sm">
            {d.experimentCount} record(s) · {d.experimentStoreDir}
          </div>
          <div className="k">Report directory</div>
          <div className="v mono-sm">{d.reportDir}</div>
          <div className="k">Settings file</div>
          <div className="v mono-sm">{d.settingsFile}</div>
          <div className="k">Application log</div>
          <div className="v mono-sm">
            {d.logDir}{' '}
            <button onClick={() => void bridge.openLogDir()} style={{ marginLeft: 8 }}>
              Open log directory
            </button>
          </div>
          <div className="k">Recent bridge errors</div>
          <div className="v mono-sm">
            {d.recentErrors.length ? (
              <ul style={{ margin: 0, paddingLeft: 16 }}>
                {d.recentErrors.slice(-5).map((e, i) => (
                  <li key={i}>{e}</li>
                ))}
              </ul>
            ) : (
              'none'
            )}
          </div>
        </div>
      </div>
    </section>
  )
}

export function DiagnosticsPage() {
  const caps = usePolling(bridge.capabilities, 30000)

  if (caps.error) {
    return (
      <>
        <div className="page-title">
          <h1>Diagnostics</h1>
          <span className="hint">Collector and capability inventory</span>
        </div>
        <div className="error-box">
          <div className="code">{caps.error.code}</div>
          <div>{caps.error.message}</div>
          {caps.error.detail ? (
            <details>
              <summary>Technical detail</summary>
              <pre className="mono-sm">{caps.error.detail}</pre>
            </details>
          ) : null}
        </div>
      </>
    )
  }

  const report = caps.data
  return (
    <>
      <div className="page-title">
        <h1>Diagnostics</h1>
        <span className="hint">
          AVAILABLE / DEGRADED / UNAVAILABLE with the actual reason, including whether elevation would help.
        </span>
      </div>

      {report ? (
        <>
          <div className="toolbar">
            <span className={`badge ${report.elevated ? 'available' : 'degraded'}`}>
              {report.elevated ? 'elevated' : 'standard user'}
            </span>
            <span className="muted mono-sm">
              {report.available} available · {report.degraded} degraded · {report.unavailable} unavailable
            </span>
            <span className="spacer" />
            <button onClick={caps.refresh}>Refresh</button>
          </div>

          {report.degraded || report.unavailable ? (
            <div className="notice">
              {report.elevated
                ? 'Some collectors are unavailable because the hardware or provider does not support them. Elevation will not change that.'
                : 'Some collectors are unavailable. Elevation would unlock admin-gated fields; permanently-absent sensors stay unavailable either way.'}
            </div>
          ) : null}

          <AppLayerPanel />

          <div className="columns" style={{ gap: 10 }}>
            {report.collectors.map((c) => (
              <CollectorCard key={c.name} c={c} />
            ))}
          </div>
        </>
      ) : (
        <div className="empty">Loading capability inventory…</div>
      )}
    </>
  )
}
