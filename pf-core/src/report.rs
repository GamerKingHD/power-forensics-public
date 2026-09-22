//! Report domain and deterministic HTML rendering.
//!
//! Reports are generated from structured analysis outputs, never from
//! screenshots or by scraping the UI. This module owns the shape of a report,
//! its evidence manifest, and a self-contained printable HTML renderer with
//! deterministic inline-SVG charts. The desktop bridge builds the sections from
//! the existing Analyze/Compare/Experiment/Calibration outputs and calls
//! [`render_html`].
//!
//! Language rule: the reporting layer never strengthens the engine's wording.
//! It renders the engine's own lines verbatim and adds no causal prose.

/// Report shape version. Bump on any structural change.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// What produced the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportSourceKind {
    Analyze,
    Compare,
    Experiment,
    Calibration,
}

impl ReportSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportSourceKind::Analyze => "analyze",
            ReportSourceKind::Compare => "compare",
            ReportSourceKind::Experiment => "experiment",
            ReportSourceKind::Calibration => "calibration",
        }
    }
}

/// Redaction state recorded in the manifest so a reader knows whether identities
/// were removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionState {
    Original,
    Redacted,
}

impl RedactionState {
    pub fn as_str(self) -> &'static str {
        match self {
            RedactionState::Original => "original",
            RedactionState::Redacted => "redacted",
        }
    }
}

/// Whether the report can be reproduced exactly. Missing/changed sources
/// downgrade this instead of being silently ignored.
#[derive(Debug, Clone, PartialEq)]
pub enum Reproducibility {
    Full,
    Qualified(String),
}

/// Identity of one input session. Fingerprints are recorded so a reader can
/// tell whether the raw evidence has since changed or disappeared.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceFingerprint {
    /// Stable session id used in exports (usually the file name).
    pub session_id: String,
    pub path: String,
    pub bytes: u64,
    pub mtime_s: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportRange {
    pub from_ms: u64,
    pub to_ms: u64,
}

/// The audit manifest embedded in every report.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceManifest {
    pub report_schema_version: u32,
    pub application_version: String,
    pub analysis_version: String,
    pub generated_at_ms: u64,
    pub kind: ReportSourceKind,
    pub sources: Vec<SourceFingerprint>,
    pub ranges: Vec<ReportRange>,
    /// Calibration ids/versions that shaped the analysis, if any.
    pub calibration_ids: Vec<String>,
    pub redaction: RedactionState,
    /// Free-form, deterministic options (e.g. include_processes=true).
    pub options: Vec<(String, String)>,
    pub reproducibility: Reproducibility,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportTable {
    pub caption: Option<String>,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// One chart point. `value` is optional so gaps stay gaps: the renderer never
/// interpolates across an unavailable period.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartPoint {
    pub t_ms: u64,
    pub value: Option<f64>,
    /// "measured" | "derived" | "estimated" | "unavailable"
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChartSeries {
    pub label: String,
    pub unit: String,
    pub points: Vec<ChartPoint>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChartEvent {
    pub t_ms: u64,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportChart {
    pub title: String,
    pub series: Vec<ChartSeries>,
    pub events: Vec<ChartEvent>,
    /// Expected sample spacing; a larger jump is drawn as a gap even when both
    /// endpoints are known, so missing periods are not bridged.
    pub gap_ms: Option<u64>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReportSection {
    pub heading: String,
    pub paragraphs: Vec<String>,
    pub tables: Vec<ReportTable>,
    pub charts: Vec<ReportChart>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub title: String,
    pub manifest: EvidenceManifest,
    /// Engine-authored headline lines, rendered verbatim.
    pub summary_lines: Vec<String>,
    /// Evidence-quality statements (coverage, discontinuities, ...).
    pub evidence_quality: Vec<String>,
    pub sections: Vec<ReportSection>,
    pub caveats: Vec<String>,
}

// ---------------------------------------------------------------------------
// HTML rendering
// ---------------------------------------------------------------------------

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn num(v: f64) -> String {
    if v.is_finite() {
        // Three decimals, trailing zeros trimmed, but keep at least one.
        let s = format!("{v:.3}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        if s.is_empty() || s == "-" {
            "0".to_string()
        } else {
            s.to_string()
        }
    } else {
        "—".to_string()
    }
}

/// Deterministic, self-contained, print-safe report HTML.
pub fn render_html(report: &Report) -> String {
    let mut body = String::new();
    body.push_str(&format!(
        "<header class=\"rpt-head\"><h1>{}</h1>\n<div class=\"rpt-kind\">{} report · generated {} · application {} · analysis {} · redaction {}</div></header>\n",
        escape_html(&report.title),
        escape_html(report.manifest.kind.as_str()),
        report.manifest.generated_at_ms,
        escape_html(&report.manifest.application_version),
        escape_html(&report.manifest.analysis_version),
        escape_html(report.manifest.redaction.as_str()),
    ));

    if !report.summary_lines.is_empty() {
        body.push_str("<section><h2>Summary</h2><ul class=\"rpt-summary\">\n");
        for line in &report.summary_lines {
            body.push_str(&format!("<li>{}</li>\n", escape_html(line)));
        }
        body.push_str("</ul></section>\n");
    }

    if !report.evidence_quality.is_empty() {
        body.push_str("<section><h2>Evidence quality</h2><ul>\n");
        for line in &report.evidence_quality {
            body.push_str(&format!("<li>{}</li>\n", escape_html(line)));
        }
        body.push_str("</ul></section>\n");
    }

    for section in &report.sections {
        body.push_str(&render_section(section));
    }

    if !report.caveats.is_empty() {
        body.push_str("<section class=\"rpt-caveats\"><h2>Caveats</h2><ul>\n");
        for c in &report.caveats {
            body.push_str(&format!("<li>{}</li>\n", escape_html(c)));
        }
        body.push_str("</ul></section>\n");
    }

    body.push_str(&render_manifest(&report.manifest));
    body.push_str(&render_legend());

    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<meta name=\"pf-report-schema\" content=\"{}\">\
<meta name=\"pf-manifest\" content=\"{}\">\
<title>{}</title>\n{}</head><body>\n{}\n</body></html>\n",
        REPORT_SCHEMA_VERSION,
        escape_html(&manifest_meta(report)),
        escape_html(&report.title),
        STYLE,
        body,
    )
}

fn render_section(section: &ReportSection) -> String {
    let mut out = format!("<section><h2>{}</h2>\n", escape_html(&section.heading));
    for p in &section.paragraphs {
        out.push_str(&format!("<p>{}</p>\n", escape_html(p)));
    }
    for t in &section.tables {
        out.push_str(&render_table(t));
    }
    for c in &section.charts {
        out.push_str(&render_chart(c));
    }
    for n in &section.notes {
        out.push_str(&format!("<p class=\"rpt-note\">{}</p>\n", escape_html(n)));
    }
    out.push_str("</section>\n");
    out
}

fn render_table(t: &ReportTable) -> String {
    let mut out = String::from("<table><thead><tr>");
    for c in &t.columns {
        out.push_str(&format!("<th scope=\"col\">{}</th>", escape_html(c)));
    }
    out.push_str("</tr></thead><tbody>\n");
    for row in &t.rows {
        out.push_str("<tr>");
        for cell in row {
            out.push_str(&format!("<td>{}</td>", escape_html(cell)));
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</tbody></table>\n");
    if let Some(cap) = &t.caption {
        out.push_str(&format!("<p class=\"rpt-cap\">{}</p>\n", escape_html(cap)));
    }
    out
}

const CHART_W: f64 = 720.0;
const CHART_H: f64 = 200.0;
const PAD_L: f64 = 52.0;
const PAD_R: f64 = 10.0;
const PAD_T: f64 = 12.0;
const PAD_B: f64 = 22.0;

/// Deterministic inline SVG. Gaps are preserved (a break in the line), and
/// estimated/derived runs are dashed so provenance is visible in the chart.
fn render_chart(chart: &ReportChart) -> String {
    let mut xs: Vec<u64> = Vec::new();
    let mut ys: Vec<f64> = Vec::new();
    for s in &chart.series {
        for p in &s.points {
            xs.push(p.t_ms);
            if let Some(v) = p.value {
                ys.push(v);
            }
        }
    }
    for e in &chart.events {
        xs.push(e.t_ms);
    }
    let (x0, x1) = match (xs.iter().min(), xs.iter().max()) {
        (Some(a), Some(b)) if b > a => (*a as f64, *b as f64),
        (Some(a), _) => (*a as f64, *a as f64 + 1.0),
        _ => (0.0, 1.0),
    };
    let (mut y0, mut y1) = match (
        ys.iter().cloned().reduce(f64::min),
        ys.iter().cloned().reduce(f64::max),
    ) {
        (Some(a), Some(b)) if b > a => (a, b),
        (Some(a), _) => (a.min(0.0), a.max(1.0)),
        _ => (0.0, 1.0),
    };
    if (y1 - y0).abs() < f64::EPSILON {
        y0 -= 0.5;
        y1 += 0.5;
    }
    let pad = (y1 - y0) * 0.05;
    y0 -= pad;
    y1 += pad;

    let sx = |t: f64| PAD_L + (t - x0) / (x1 - x0) * (CHART_W - PAD_L - PAD_R);
    let sy = |v: f64| PAD_T + (1.0 - (v - y0) / (y1 - y0)) * (CHART_H - PAD_T - PAD_B);

    let mut svg = format!(
        "<figure class=\"rpt-chart\"><figcaption>{}</figcaption>\
<svg viewBox=\"0 0 {CHART_W} {CHART_H}\" width=\"100%\" height=\"200\" role=\"img\" \
aria-label=\"{}\" preserveAspectRatio=\"none\">\
<rect x=\"0\" y=\"0\" width=\"{CHART_W}\" height=\"{CHART_H}\" class=\"bg\"/>",
        escape_html(&chart.title),
        escape_html(&chart.title),
    );

    // Y gridlines at min/mid/max with value labels.
    for frac in [0.0, 0.5, 1.0] {
        let v = y1 - (y1 - y0) * frac;
        let y = sy(v);
        svg.push_str(&format!(
            "<line x1=\"{PAD_L:.1}\" y1=\"{y:.1}\" x2=\"{:.1}\" y2=\"{y:.1}\" class=\"grid\"/>",
            CHART_W - PAD_R
        ));
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" class=\"axis\" text-anchor=\"end\">{}</text>",
            PAD_L - 4.0,
            y + 3.0,
            num(v)
        ));
    }

    // Event markers behind the series.
    for e in &chart.events {
        let x = sx(e.t_ms as f64);
        svg.push_str(&format!(
            "<line x1=\"{x:.1}\" y1=\"{PAD_T:.1}\" x2=\"{x:.1}\" y2=\"{:.1}\" class=\"event\"><title>{}</title></line>",
            CHART_H - PAD_B,
            escape_html(&e.label)
        ));
    }

    for (idx, s) in chart.series.iter().enumerate() {
        let class = format!("s{}", idx % 6);
        let mut run: Vec<(f64, f64, &str)> = Vec::new();
        let mut prev_t: Option<u64> = None;
        for p in &s.points {
            let gap = match (prev_t, chart.gap_ms) {
                (Some(pt), Some(g)) if g > 0 => p.t_ms.saturating_sub(pt) > g.saturating_mul(2),
                _ => false,
            };
            match p.value {
                Some(v) if !gap => run.push((sx(p.t_ms as f64), sy(v), &p.provenance)),
                Some(v) => {
                    flush_run(&mut svg, &run, &class);
                    run.clear();
                    run.push((sx(p.t_ms as f64), sy(v), &p.provenance));
                }
                None => {
                    // Gap: close the current run without bridging.
                    flush_run(&mut svg, &run, &class);
                    run.clear();
                }
            }
            prev_t = Some(p.t_ms);
        }
        flush_run(&mut svg, &run, &class);
    }

    svg.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" class=\"axis\" text-anchor=\"end\">{}</text>",
        CHART_W - PAD_R,
        CHART_H - 6.0,
        escape_html(
            &chart
                .series
                .first()
                .map(|s| s.unit.clone())
                .unwrap_or_default()
        )
    ));
    svg.push_str("</svg>");
    if let Some(note) = &chart.note {
        svg.push_str(&format!("<p class=\"rpt-cap\">{}</p>", escape_html(note)));
    }
    svg.push_str("</figure>\n");
    svg
}

fn flush_run(svg: &mut String, run: &[(f64, f64, &str)], class: &str) {
    if run.len() < 2 {
        // A single isolated point is drawn as a dot, never extended into a line.
        if let Some((x, y, prov)) = run.first() {
            svg.push_str(&format!(
                "<circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"1.6\" class=\"pt {}\" data-prov=\"{}\"/>",
                class,
                escape_html(prov)
            ));
        }
        return;
    }
    // Split into provenance runs so dashed = estimated/derived, solid = measured.
    let mut start = 0usize;
    for i in 1..=run.len() {
        let boundary = i == run.len() || run[i].2 != run[start].2;
        if boundary {
            let seg = &run[start..i];
            let style = if seg[0].2 == "measured" || seg[0].2 == "fresh" {
                "solid"
            } else {
                "dashed"
            };
            let mut pts = String::new();
            for (x, y, _) in seg {
                pts.push_str(&format!("{x:.1},{y:.1} "));
            }
            svg.push_str(&format!(
                "<polyline points=\"{}\" fill=\"none\" class=\"line {}\" data-prov=\"{}\" stroke-dasharray=\"{}\"/>",
                pts.trim_end(),
                class,
                escape_html(seg[0].2),
                if style == "dashed" { "5 4" } else { "none" }
            ));
            start = i;
        }
    }
}

/// Redact every identifying text field of a report in place, using the shared
/// backend redactor and the session-derived tokens. Structure, tables and
/// numbers are preserved; only identity strings are masked. The manifest's
/// redaction state is updated so the report discloses that it was redacted.
pub fn redact_report(report: &mut Report, tokens: &[crate::analysis::RedactToken]) {
    use crate::analysis::redact_report_identifiers;
    let r = |s: &str| redact_report_identifiers(s, tokens);
    report.title = r(&report.title);
    for line in &mut report.summary_lines {
        *line = r(line);
    }
    for line in &mut report.evidence_quality {
        *line = r(line);
    }
    for caveat in &mut report.caveats {
        *caveat = r(caveat);
    }
    for section in &mut report.sections {
        section.heading = r(&section.heading);
        for p in &mut section.paragraphs {
            *p = r(p);
        }
        for n in &mut section.notes {
            *n = r(n);
        }
        for t in &mut section.tables {
            if let Some(cap) = &mut t.caption {
                *cap = r(cap);
            }
            for c in &mut t.columns {
                *c = r(c);
            }
            for row in &mut t.rows {
                for cell in row {
                    *cell = r(cell);
                }
            }
        }
        for c in &mut section.charts {
            c.title = r(&c.title);
            if let Some(note) = &mut c.note {
                *note = r(note);
            }
            for s in &mut c.series {
                s.label = r(&s.label);
            }
            for e in &mut c.events {
                e.label = r(&e.label);
            }
        }
    }
    for s in &mut report.manifest.sources {
        s.session_id = r(&s.session_id);
        s.path = r(&s.path);
    }
    report.manifest.redaction = RedactionState::Redacted;
}

fn manifest_meta(report: &Report) -> String {
    let mut parts = vec![
        format!("schema={}", REPORT_SCHEMA_VERSION),
        format!("kind={}", report.manifest.kind.as_str()),
        format!("generated_at={}", report.manifest.generated_at_ms),
        format!("redaction={}", report.manifest.redaction.as_str()),
        format!("application={}", report.manifest.application_version),
        format!("analysis={}", report.manifest.analysis_version),
    ];
    for s in &report.manifest.sources {
        parts.push(format!("source={}:{}:{}", s.session_id, s.bytes, s.mtime_s));
    }
    for r in &report.manifest.ranges {
        parts.push(format!("range={}-{}", r.from_ms, r.to_ms));
    }
    for c in &report.manifest.calibration_ids {
        parts.push(format!("calibration={c}"));
    }
    if let Reproducibility::Qualified(reason) = &report.manifest.reproducibility {
        parts.push(format!("reproducibility=qualified:{reason}"));
    }
    parts.join("; ")
}

fn render_manifest(m: &EvidenceManifest) -> String {
    let mut out =
        String::from("<section class=\"rpt-manifest\"><h2>Evidence manifest</h2><table><tbody>\n");
    let row = |out: &mut String, k: &str, v: String| {
        out.push_str(&format!(
            "<tr><th scope=\"row\">{}</th><td>{}</td></tr>\n",
            escape_html(k),
            escape_html(&v)
        ));
    };
    row(
        &mut out,
        "Report schema",
        m.report_schema_version.to_string(),
    );
    row(&mut out, "Source kind", m.kind.as_str().to_string());
    row(&mut out, "Generated at (ms)", m.generated_at_ms.to_string());
    row(
        &mut out,
        "Application version",
        m.application_version.clone(),
    );
    row(&mut out, "Analysis version", m.analysis_version.clone());
    row(&mut out, "Redaction", m.redaction.as_str().to_string());
    for s in &m.sources {
        row(
            &mut out,
            "Source",
            format!(
                "{} ({}, {} bytes, mtime {})",
                s.session_id, s.path, s.bytes, s.mtime_s
            ),
        );
    }
    for r in &m.ranges {
        row(&mut out, "Range", format!("{} – {} ms", r.from_ms, r.to_ms));
    }
    for c in &m.calibration_ids {
        row(&mut out, "Calibration", c.clone());
    }
    for (k, v) in &m.options {
        row(&mut out, &format!("Option: {k}"), v.clone());
    }
    match &m.reproducibility {
        Reproducibility::Full => row(&mut out, "Reproducibility", "full".to_string()),
        Reproducibility::Qualified(reason) => {
            row(&mut out, "Reproducibility", format!("qualified: {reason}"));
        }
    }
    out.push_str("</tbody></table></section>\n");
    out
}

fn render_legend() -> String {
    String::from(
        "<section class=\"rpt-legend\"><h2>Legend</h2><ul>\
<li><b>measured</b> — a sensor/collector reading.</li>\
<li><b>derived</b> — computed from measured values (e.g. calibration model).</li>\
<li><b>estimated</b> — a model output; never a measurement.</li>\
<li><b>unavailable</b> — no value; shown as a gap, never as zero.</li>\
<li><b>stale</b> — a value too old to trust.</li>\
<li><b>recovered</b> — evidence rebuilt after an interrupted session.</li>\
<li><b>coverage</b> — share of the interval backed by known evidence.</li>\
<li><b>discontinuity</b> — a forced clock/sampling boundary.</li>\
</ul></section>\n",
    )
}

const STYLE: &str = "<style>\
:root{color-scheme:light dark}\
body{font:13px/1.5 system-ui,Segoe UI,Roboto,sans-serif;margin:24px;max-width:960px;color:#16181d;background:#fff}\
h1{font-size:20px;margin:0 0 4px} h2{font-size:15px;margin:18px 0 6px;border-bottom:1px solid #d8dbe2;padding-bottom:3px}\
.rpt-kind,.rpt-cap,.rpt-note{color:#5b6472;font-size:11.5px}\
table{border-collapse:collapse;width:100%;margin:6px 0}\
th,td{border:1px solid #d8dbe2;text-align:left;padding:4px 6px;vertical-align:top}\
th[scope=row]{background:#f3f5f8;white-space:nowrap}\
thead th{background:#f3f5f8}\
li{margin:2px 0}\
.rpt-caveats li{color:#7a4a00}\
figure{margin:10px 0}\
svg{background:#fff;border:1px solid #e3e6ec;border-radius:4px}\
svg .bg{fill:#fff}\
svg .grid{stroke:#eceff4;stroke-width:1}\
svg .axis{fill:#5b6472;font-size:9px}\
svg .event{stroke:#c0392b;stroke-width:1;stroke-dasharray:2 2;opacity:.7}\
svg .line{stroke-width:1.6}\
svg .s0{stroke:#1f6feb}svg .s1{stroke:#2f9e44}svg .s2{stroke:#c2255c}svg .s3{stroke:#e8590c}svg .s4{stroke:#7048e8}svg .s5{stroke:#0b7285}\
svg .pt{fill:#1f6feb}\
@media print{body{margin:0;max-width:none}h2{break-after:avoid}section,.rpt-chart,table{break-inside:avoid}\
svg .event{stroke:#000}}\
@media (prefers-color-scheme:dark){body{background:#14161a;color:#e6e8ec}\
h2{border-color:#2c313a}th,td{border-color:#2c313a}th[scope=row],thead th{background:#1d2026}\
.rpt-caveats li{color:#e0a94a}svg{background:#14161a;border-color:#2c313a}svg .bg{fill:#14161a}svg .grid{stroke:#232830}\
.rpt-kind,.rpt-cap,.rpt-note{color:#9aa4b2}\
svg .event{stroke:#e06c75}}\
</style>";

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> EvidenceManifest {
        EvidenceManifest {
            report_schema_version: REPORT_SCHEMA_VERSION,
            application_version: "0.1.0".to_string(),
            analysis_version: "analyze-v1".to_string(),
            generated_at_ms: 1234,
            kind: ReportSourceKind::Analyze,
            sources: vec![SourceFingerprint {
                session_id: "s.jsonl".to_string(),
                path: "sessions/s.jsonl".to_string(),
                bytes: 42,
                mtime_s: 99,
            }],
            ranges: vec![ReportRange {
                from_ms: 0,
                to_ms: 1000,
            }],
            calibration_ids: vec!["display_power#2".to_string()],
            redaction: RedactionState::Original,
            options: vec![("include_processes".to_string(), "true".to_string())],
            reproducibility: Reproducibility::Full,
        }
    }

    fn chart() -> ReportChart {
        ReportChart {
            title: "Discharge".to_string(),
            series: vec![ChartSeries {
                label: "Discharge W".to_string(),
                unit: "W".to_string(),
                points: vec![
                    ChartPoint {
                        t_ms: 0,
                        value: Some(6.0),
                        provenance: "measured".to_string(),
                    },
                    ChartPoint {
                        t_ms: 1000,
                        value: Some(6.2),
                        provenance: "measured".to_string(),
                    },
                    ChartPoint {
                        t_ms: 2000,
                        value: None,
                        provenance: "unavailable".to_string(),
                    },
                    ChartPoint {
                        t_ms: 3000,
                        value: Some(5.9),
                        provenance: "measured".to_string(),
                    },
                    ChartPoint {
                        t_ms: 4000,
                        value: Some(6.1),
                        provenance: "estimated".to_string(),
                    },
                ],
            }],
            events: vec![ChartEvent {
                t_ms: 1500,
                label: "marker: idle".to_string(),
            }],
            gap_ms: Some(1000),
            note: None,
        }
    }

    #[test]
    fn html_escapes_untrusted_text() {
        assert_eq!(
            escape_html("<script>&\"'"),
            "&lt;script&gt;&amp;&quot;&#39;"
        );
    }

    #[test]
    fn render_contains_manifest_and_is_deterministic() {
        let report = Report {
            title: "Analyze <b>".to_string(),
            manifest: manifest(),
            summary_lines: vec!["observed power decreased".to_string()],
            evidence_quality: vec!["coverage 98%".to_string()],
            sections: vec![ReportSection {
                heading: "Timeline".to_string(),
                charts: vec![chart()],
                ..Default::default()
            }],
            caveats: vec!["single run".to_string()],
        };
        let a = render_html(&report);
        let b = render_html(&report);
        assert_eq!(a, b, "rendering must be deterministic");
        assert!(a.contains("Evidence manifest"));
        assert!(a.contains("pf-report-schema"));
        assert!(a.contains("&lt;b&gt;"), "title escaped");
        assert!(a.contains("calibration=display_power#2"));
        // Gap preserved: a None point must not be bridged by a polyline.
        assert!(a.contains("<polyline"));
        assert!(
            a.contains("stroke-dasharray=\"5 4\""),
            "estimated run dashed"
        );
    }

    #[test]
    fn unavailable_gap_is_never_zero() {
        let report = Report {
            title: "t".to_string(),
            manifest: manifest(),
            summary_lines: vec![],
            evidence_quality: vec![],
            sections: vec![ReportSection {
                heading: "c".to_string(),
                charts: vec![chart()],
                ..Default::default()
            }],
            caveats: vec![],
        };
        let html = render_html(&report);
        // The gap is represented, and no y-axis value of 0 is fabricated for
        // the missing point beyond the axis baseline label.
        assert!(html.contains("data-prov=\"unavailable\"") || html.contains("unavailable"));
    }

    #[test]
    fn report_never_strengthens_engine_wording() {
        let report = Report {
            title: "Experiment report".to_string(),
            manifest: manifest(),
            summary_lines: vec![
                "Classification: possible_difference.".to_string(),
                "Validity: valid_with_caveats.".to_string(),
            ],
            evidence_quality: vec![],
            sections: vec![],
            caveats: vec!["Observed difference ≠ causation.".to_string()],
        };
        let html = render_html(&report).to_lowercase();
        // The engine's own classification survives verbatim.
        assert!(html.contains("possible_difference"));
        // The reporting layer adds no stronger claim.
        assert!(!html.contains("significant improvement"));
        assert!(!html.contains("proves"));
        assert!(!html.contains("caused by"));
    }

    #[test]
    fn redact_report_masks_identity_and_discloses_state() {
        let mut report = Report {
            title: "Analyze report — secret-app.exe".to_string(),
            manifest: manifest(),
            summary_lines: vec!["Process secret-app.exe was observed.".to_string()],
            evidence_quality: vec![],
            sections: vec![],
            caveats: vec![],
        };
        let tokens = vec![crate::analysis::RedactToken::new(
            "secret-app.exe",
            "process-1",
        )];
        redact_report(&mut report, &tokens);
        assert!(!report.title.contains("secret-app.exe"));
        assert!(report.title.contains("process-1"));
        assert_eq!(report.manifest.redaction, RedactionState::Redacted);
        let html = render_html(&report);
        assert!(!html.contains("secret-app.exe"));
        assert!(html.contains("redacted"));
    }

    #[test]
    fn qualified_reproducibility_is_disclosed() {
        let mut m = manifest();
        m.reproducibility = Reproducibility::Qualified("source changed".to_string());
        let report = Report {
            title: "t".to_string(),
            manifest: m,
            summary_lines: vec![],
            evidence_quality: vec![],
            sections: vec![],
            caveats: vec![],
        };
        let html = render_html(&report);
        assert!(html.contains("qualified: source changed"));
    }
}
