import { useEffect, useState } from 'react'
import { bridge } from '../api/bridge'
import type { AppSettings } from '../api/types'
import { usePolling } from '../hooks/usePolling'

function Row({ k, children }: { k: string; children: React.ReactNode }) {
  return (
    <>
      <div className="k">{k}</div>
      <div className="v">{children}</div>
    </>
  )
}

export function SettingsPage({ onNotice }: { onNotice: (msg: string | null) => void }) {
  const app = usePolling(bridge.appStatus, 30000)
  const [settings, setSettings] = useState<AppSettings | null>(null)
  const [busy, setBusy] = useState(false)
  const s = app.data

  useEffect(() => {
    bridge
      .getSettings()
      .then(setSettings)
      .catch(() => onNotice('Could not load settings; using defaults.'))
  }, [onNotice])

  const patch = (p: Partial<AppSettings>) => setSettings((cur) => (cur ? { ...cur, ...p } : cur))

  const save = async () => {
    if (!settings) return
    setBusy(true)
    try {
      const saved = await bridge.saveSettings(settings)
      setSettings(saved)
      onNotice('Settings saved.')
    } catch (e) {
      const err = e as { code?: string; message?: string }
      onNotice(`${err.code ?? 'error'}: ${err.message ?? 'settings rejected'}`)
    } finally {
      setBusy(false)
    }
  }

  return (
    <>
      <div className="page-title">
        <h1>Settings</h1>
        <span className="hint">Only settings backed by real behavior are shown.</span>
        <span className="spacer" style={{ flex: 1 }} />
        <button onClick={save} disabled={busy || !settings} className="primary">
          {busy ? 'Saving…' : 'Save'}
        </button>
      </div>

      <section className="panel">
        <header>Preferences</header>
        <div className="content">
          {settings ? (
            <>
              <div className="section-label">Monitoring</div>
              <div className="kv">
                <Row k="Default preset">
                  <input
                    value={settings.defaultPreset ?? ''}
                    placeholder="(agent default)"
                    onChange={(e) => patch({ defaultPreset: e.target.value || null })}
                  />
                </Row>
                <Row k="Default collectors">
                  <input
                    value={settings.defaultCollectors ?? ''}
                    placeholder="(all available)"
                    onChange={(e) => patch({ defaultCollectors: e.target.value || null })}
                  />
                </Row>
                <Row k="Live chart interval">
                  <input
                    type="number"
                    min={250}
                    max={60000}
                    step={250}
                    value={settings.chartIntervalMs}
                    onChange={(e) => patch({ chartIntervalMs: Number(e.target.value) })}
                  />{' '}
                  <span className="muted mono-sm">ms (250–60000)</span>
                </Row>
              </div>

              <div className="section-label">Appearance</div>
              <div className="kv">
                <Row k="Theme">
                  <select
                    value={settings.theme}
                    onChange={(e) => patch({ theme: e.target.value as AppSettings['theme'] })}
                  >
                    <option value="system">System</option>
                    <option value="light">Light</option>
                    <option value="dark">Dark</option>
                  </select>
                </Row>
              </div>

              <div className="section-label">Reports / export</div>
              <div className="kv">
                <Row k="Export directory">
                  <input
                    value={settings.reportExportDir ?? ''}
                    placeholder="(sessions/reports)"
                    style={{ width: '100%' }}
                    onChange={(e) => patch({ reportExportDir: e.target.value || null })}
                  />
                </Row>
                <Row k="Redaction default">
                  <label>
                    <input
                      type="checkbox"
                      checked={settings.redactionDefault}
                      onChange={(e) => patch({ redactionDefault: e.target.checked })}
                    />{' '}
                    pre-select redaction on new reports/exports
                  </label>
                </Row>
              </div>

              <p className="muted mono-sm" style={{ marginTop: 12 }}>
                Default preset/collectors apply the next time monitoring is started from the UI. The export directory is
                validated when saved and takes effect for new reports/exports.
              </p>
            </>
          ) : (
            <div className="empty">Loading settings…</div>
          )}
        </div>
      </section>

      <section className="panel">
        <header>
          Environment
          <span className="spacer" />
          <span className="muted inline-note">read-only runtime values</span>
        </header>
        <div className="content">
          <div className="kv">
            <Row k="Session directory">{s?.sessionsDir ?? '—'}</Row>
            <Row k="Agent pipe">{s?.agentPipe ?? '—'}</Row>
            <Row k="Agent binary">
              {s?.agentBinary ?? <span className="muted">not found — build the workspace</span>}
            </Row>
            <Row k="Platform">{s?.platform ?? '—'}</Row>
            <Row k="Elevation">{s?.elevated ? 'Administrator' : 'Standard user'}</Row>
            <Row k="GUI version">{s?.guiVersion ?? '—'}</Row>
            <Row k="Tool version">{s?.toolVersion ?? '—'}</Row>
          </div>
        </div>
      </section>
    </>
  )
}
