import { Modal } from './report/ReportExport'

const TERMS: { term: string; meaning: string }[] = [
  { term: 'Measured', meaning: 'A value reported by a sensor or collector. The only class that is a direct observation.' },
  { term: 'Derived', meaning: 'Computed from measured values (for example an energy integral or a calibration model).' },
  {
    term: 'Estimated',
    meaning: 'A model output. A calibrated value is still an estimate; calibration never promotes it to a measurement.',
  },
  { term: 'Unavailable', meaning: 'No value exists. It is shown as a gap or empty cell, never as zero.' },
  { term: 'Stale', meaning: 'A value too old to trust; it is not used as fresh evidence.' },
  { term: 'Recovered', meaning: 'Evidence rebuilt after an interrupted recording; the footer was reconstructed.' },
  { term: 'Coverage', meaning: 'The share of an interval backed by known, contiguous evidence.' },
  { term: 'Discontinuity', meaning: 'A forced clock or sampling boundary; energy across it is qualified.' },
]

/** Compact built-in explanation of the evidence model. No onboarding flow. */
export function EvidenceHelp({ onClose }: { onClose: () => void }) {
  return (
    <Modal title="Evidence model" onClose={onClose}>
      <p className="muted">
        Every reading carries its provenance. The GUI never collapses these classes into a bare number.
      </p>
      <table className="data-table">
        <thead>
          <tr>
            <th scope="col">Term</th>
            <th scope="col">Meaning</th>
          </tr>
        </thead>
        <tbody>
          {TERMS.map((t) => (
            <tr key={t.term}>
              <td>{t.term}</td>
              <td>{t.meaning}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </Modal>
  )
}
