import { useState } from 'react'
import { bridge } from '../../api/bridge'
import type { BuiltReport, ReportOptions, ReportRequest } from '../../api/types'

export function Modal({
  title,
  onClose,
  children,
  wide,
}: {
  title: string
  onClose: () => void
  children: React.ReactNode
  wide?: boolean
}) {
  return (
    <div
      className="overlay"
      role="presentation"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose()
      }}
    >
      <div className="modal" role="dialog" aria-modal="true" aria-label={title} style={wide ? { maxWidth: 1100 } : undefined}>
        <header className="modal-head">
          <h2>{title}</h2>
          <span className="spacer" style={{ flex: 1 }} />
          <button onClick={onClose} aria-label="Close">
            Close
          </button>
        </header>
        <div className="modal-body">{children}</div>
      </div>
    </div>
  )
}

function defaults(redact: boolean): ReportOptions {
  return {
    includeTimeline: true,
    includeProcesses: true,
    includeSecondary: true,
    detailedCaveats: true,
    redact,
  }
}

/**
 * Compact report/export controls for a workspace. Reports are generated in
 * Rust from structured analysis outputs; the preview is the exact HTML that
 * would be saved. Nothing is scraped from the rendered page.
 */
export function ReportExportBar({
  request,
  sessionPath,
  defaultRedact = false,
  onNotice,
}: {
  request: Omit<ReportRequest, 'options'>
  sessionPath?: string | null
  defaultRedact?: boolean
  onNotice: (msg: string | null) => void
}) {
  const [options, setOptions] = useState<ReportOptions>(() => defaults(defaultRedact))
  const [open, setOpen] = useState(false)
  const [preview, setPreview] = useState<BuiltReport | null>(null)
  const [busy, setBusy] = useState(false)
  const [replace, setReplace] = useState(false)

  const fullRequest: ReportRequest = { ...request, options }

  const withBusy = async (fn: () => Promise<void>) => {
    setBusy(true)
    try {
      await fn()
    } catch (e) {
      const err = e as { message?: string; code?: string }
      onNotice(`${err.code ?? 'error'}: ${err.message ?? 'export failed'}`)
    } finally {
      setBusy(false)
    }
  }

  const doPreview = () =>
    withBusy(async () => {
      const built = await bridge.generateReport(fullRequest)
      setPreview(built)
    })

  const doSaveReport = () =>
    withBusy(async () => {
      const saved = await bridge.saveReport(fullRequest)
      onNotice(`Report saved: ${saved.path}`)
    })

  const doExportJson = () =>
    withBusy(async () => {
      const path = await bridge.exportAnalysisJson(fullRequest)
      onNotice(`JSON export saved: ${path}`)
    })

  const doExportCsv = () =>
    withBusy(async () => {
      if (!sessionPath) return
      const path = await bridge.exportSessionTidy(sessionPath, options.redact, replace)
      onNotice(`Tidy CSV saved: ${path}`)
    })

  const toggle = (k: keyof ReportOptions) => setOptions((o) => ({ ...o, [k]: !o[k] }))

  return (
    <div className="report-export">
      <div className="report-export-actions">
        <button onClick={doPreview} disabled={busy} title="Preview the report before exporting">
          Report preview
        </button>
        <button onClick={doSaveReport} disabled={busy} title="Save a self-contained HTML report">
          Save HTML
        </button>
        <button onClick={doExportJson} disabled={busy} title="Versioned machine-readable JSON">
          Export JSON
        </button>
        {sessionPath ? (
          <button onClick={doExportCsv} disabled={busy} title="Tidy long-format CSV of the session">
            Export tidy CSV
          </button>
        ) : null}
        <button onClick={() => setOpen((v) => !v)} aria-expanded={open} title="Report options">
          Options {open ? '▾' : '▸'}
        </button>
      </div>
      {open ? (
        <div className="report-export-options">
          <label>
            <input type="checkbox" checked={options.includeTimeline} onChange={() => toggle('includeTimeline')} /> timeline
          </label>
          <label>
            <input type="checkbox" checked={options.includeProcesses} onChange={() => toggle('includeProcesses')} /> processes
          </label>
          <label>
            <input type="checkbox" checked={options.includeSecondary} onChange={() => toggle('includeSecondary')} /> secondary metrics
          </label>
          <label>
            <input type="checkbox" checked={options.detailedCaveats} onChange={() => toggle('detailedCaveats')} /> detailed caveats
          </label>
          <label>
            <input type="checkbox" checked={options.redact} onChange={() => toggle('redact')} /> redact identities
          </label>
          {sessionPath ? (
            <label>
              <input type="checkbox" checked={replace} onChange={() => setReplace((v) => !v)} /> replace existing CSV
            </label>
          ) : null}
        </div>
      ) : null}
      {preview ? (
        <Modal
          title={preview.title}
          wide
          onClose={() => setPreview(null)}
        >
          <div className="report-preview-meta mono-sm">
            {preview.manifest.kind} · schema v{preview.manifest.reportSchemaVersion} · redaction{' '}
            {preview.manifest.redaction} · analysis {preview.manifest.analysisVersion} · app{' '}
            {preview.manifest.applicationVersion} · {preview.suggestedFileName}
          </div>
          <iframe
            className="report-preview"
            title="Report preview"
            sandbox=""
            srcDoc={preview.html}
          />
        </Modal>
      ) : null}
    </div>
  )
}
