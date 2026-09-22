//! Compare workspace bridge: adapts the pure `pf_core::compare` engine onto
//! wire DTOs and attaches the authoritative session metadata (label, note,
//! recovered/incomplete state) that the pure range model cannot carry.
//!
//! No forensic calculation happens here. Comparability, deltas, reliability,
//! distributions, process matching and caveats all come from `pf-core`; this
//! module only loads the bounded regions and maps the result.

use std::path::Path;

use pf_core::compare as core;
use pf_core::json::parse;
use pf_core::session::SessionData;

use crate::dto::{
    CategoricalDifferenceDto, CaveatDto, CollectorSampleCount, ComparabilityDto,
    ComparabilityFindingDto, ComparisonAnalysisDto, ComparisonContextDto, ComparisonSideDto,
    CorrelationComparisonDto, DistributionComparisonDto, DistributionSummaryDto,
    DomainComparisonDto, DomainMetricDto, EnergyComparisonDto, MetricComparisonDto,
    ProcessComparisonDto, ProcessDifferenceDto, QualityComparisonDto, RangeEnergyDto,
    RangeQualityDto, RankedDifferenceDto, ReliabilityDto,
};
use crate::evidence;
use crate::sessions;

/// Load one side bounded to its selected interval and run the core comparison.
#[allow(clippy::too_many_arguments)]
pub fn compare(
    path_a: &Path,
    from_a: Option<u64>,
    to_a: Option<u64>,
    path_b: &Path,
    from_b: Option<u64>,
    to_b: Option<u64>,
) -> Result<ComparisonAnalysisDto, crate::dto::BridgeError> {
    let interval_a = sessions::session_interval_ms(path_a)?;
    let interval_b = sessions::session_interval_ms(path_b)?;
    let meta_a = side_meta(path_a)?;
    let meta_b = side_meta(path_b)?;

    let sa = load_side(path_a, from_a, to_a)?;
    let sb = load_side(path_b, from_b, to_b)?;

    // Raw options preserve the distinction between "whole session" (both
    // bounds absent) and a one-sided bound that extends to the span edge.
    let input_a = core::ComparisonInput {
        session: &sa,
        interval_ms: interval_a,
        from_ms: from_a,
        to_ms: to_a,
    };
    let input_b = core::ComparisonInput {
        session: &sb,
        interval_ms: interval_b,
        from_ms: from_b,
        to_ms: to_b,
    };

    let analysis = core::compare_ranges(input_a, input_b);
    Ok(map(analysis, &meta_a, &meta_b))
}

/// Number of records in a bounded region. `from`/`to` of 0 are unbounded on
/// that side. A whole-session side reads to EOF so the footer is available.
fn load_side(
    path: &Path,
    from: Option<u64>,
    to: Option<u64>,
) -> Result<SessionData, crate::dto::BridgeError> {
    sessions::load_region_data(path, from.unwrap_or(0), to.unwrap_or(0))
}

struct SideMeta {
    path: String,
    label: String,
    note: String,
    status: String,
    recovered: bool,
}

fn side_meta(path: &Path) -> Result<SideMeta, crate::dto::BridgeError> {
    let (head, tail, _size) = sessions::read_head_tail(path)?;
    let header = sessions::header_json(&head);
    let footer = sessions::footer_line(&tail).and_then(|l| parse(&l).ok());
    let fsum = evidence::footer_summary(footer.as_ref().and_then(|v| v.get("summary")));
    let recovered = fsum.recovered
        || footer
            .as_ref()
            .and_then(|v| v.get("recovered"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let note = header
        .as_ref()
        .and_then(|v| v.get("note"))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    Ok(SideMeta {
        path: path.to_string_lossy().to_string(),
        label,
        note,
        status: if footer.is_some() { "ok" } else { "incomplete" }.to_string(),
        recovered,
    })
}

fn map(
    analysis: core::ComparisonAnalysis,
    meta_a: &SideMeta,
    meta_b: &SideMeta,
) -> ComparisonAnalysisDto {
    let mut caveats: Vec<CaveatDto> = analysis
        .caveats
        .iter()
        .map(|c| CaveatDto {
            scope: c.scope.to_string(),
            severity: c.severity.to_string(),
            message: c.message.clone(),
        })
        .collect();
    let mut comparability = ComparabilityDto {
        overall: analysis.comparability.overall.as_str().to_string(),
        findings: analysis
            .comparability
            .findings
            .iter()
            .map(|f| ComparabilityFindingDto {
                metric: f.metric.map(|m| m.to_string()),
                level: f.level.as_str().to_string(),
                reason: f.reason.clone(),
            })
            .collect(),
    };

    for (side, meta) in [("A", meta_a), ("B", meta_b)] {
        if meta.status != "ok" {
            caveats.push(CaveatDto {
                scope: "comparison".to_string(),
                severity: "warning".to_string(),
                message: format!(
                    "Session {side} has no footer (incomplete recording); its range is what could be parsed."
                ),
            });
        }
        if meta.recovered {
            caveats.push(CaveatDto {
                scope: "coverage".to_string(),
                severity: "warning".to_string(),
                message: format!(
                    "Session {side} was recovered; the recording does not contain a clean shutdown footer."
                ),
            });
            let already = comparability
                .findings
                .iter()
                .any(|f| f.reason.contains("recovered"));
            if !already {
                comparability.findings.push(ComparabilityFindingDto {
                    metric: None,
                    level: "compatible_with_caveats".to_string(),
                    reason: format!("session {side} was recovered from a torn recording"),
                });
            }
        }
    }
    // Session metadata the pure range model cannot carry (recovered/incomplete)
    // can only increase the overall verdict, never hide a raised finding.
    let rank = |level: &str| match level {
        "not_comparable" => 3,
        "weak" => 2,
        "compatible_with_caveats" => 1,
        _ => 0,
    };
    let mut overall = rank(&comparability.overall);
    for f in &comparability.findings {
        overall = overall.max(rank(&f.level));
    }
    comparability.overall = match overall {
        3 => "not_comparable",
        2 => "weak",
        1 => "compatible_with_caveats",
        _ => "compatible",
    }
    .to_string();

    ComparisonAnalysisDto {
        a: side_dto(analysis.a, meta_a),
        b: side_dto(analysis.b, meta_b),
        comparability,
        quality: QualityComparisonDto {
            coverage_a: analysis.quality.coverage_a,
            coverage_b: analysis.quality.coverage_b,
            discontinuities_a: analysis.quality.discontinuities_a,
            discontinuities_b: analysis.quality.discontinuities_b,
            timeouts_a: analysis.quality.timeouts_a,
            timeouts_b: analysis.quality.timeouts_b,
            stale_collectors_a: analysis.quality.stale_collectors_a,
            stale_collectors_b: analysis.quality.stale_collectors_b,
            weakest_side: analysis.quality.weakest_side.to_string(),
            note: analysis.quality.note,
        },
        energy: EnergyComparisonDto {
            total_wh_a: analysis.energy.total_wh_a,
            total_wh_b: analysis.energy.total_wh_b,
            avg_power_w_a: analysis.energy.avg_power_w_a,
            avg_power_w_b: analysis.energy.avg_power_w_b,
            normalized_wh_a: analysis.energy.normalized_wh_a,
            normalized_wh_b: analysis.energy.normalized_wh_b,
            duration_ratio: analysis.energy.duration_ratio,
            durations_similar: analysis.energy.durations_similar,
            preferred_basis: analysis.energy.preferred_basis.to_string(),
            note: analysis.energy.note,
        },
        headline_metrics: analysis
            .headline_metrics
            .into_iter()
            .map(|m| MetricComparisonDto {
                metric: m.metric.to_string(),
                label: m.label.to_string(),
                domain: m.domain.to_string(),
                unit: m.unit.to_string(),
                a: m.a,
                b: m.b,
                absolute_delta: m.absolute_delta,
                relative_delta: m.relative_delta,
                direction: m.direction.to_string(),
                sample_count_a: m.sample_count_a,
                sample_count_b: m.sample_count_b,
                coverage_a: m.coverage_a,
                coverage_b: m.coverage_b,
                provenance_a: m.provenance_a.to_string(),
                provenance_b: m.provenance_b.to_string(),
                comparability: m.comparability.as_str().to_string(),
                comparability_reason: m.comparability_reason,
                evidence: m.evidence,
                reliability: ReliabilityDto {
                    level: m.reliability.level.to_string(),
                    note: m.reliability.note,
                    noise_floor: m.reliability.noise_floor,
                    effect_size: m.reliability.effect_size,
                },
            })
            .collect(),
        ranked_changes: analysis
            .ranked_changes
            .into_iter()
            .map(|r| RankedDifferenceDto {
                metric: r.metric.to_string(),
                label: r.label.to_string(),
                domain: r.domain.to_string(),
                unit: r.unit.to_string(),
                delta: r.delta,
                relative_delta: r.relative_delta,
                direction: r.direction.to_string(),
                relevance: r.relevance,
                comparability: r.comparability.as_str().to_string(),
                reliability: r.reliability.to_string(),
                basis: r.basis,
            })
            .collect(),
        domains: analysis
            .domains
            .into_iter()
            .map(|d| DomainComparisonDto {
                domain: d.domain.to_string(),
                available_a: d.available_a,
                available_b: d.available_b,
                unavailable_reason: d.unavailable_reason,
                metrics: d
                    .metrics
                    .into_iter()
                    .map(|m| DomainMetricDto {
                        metric: m.metric.to_string(),
                        label: m.label.to_string(),
                        unit: m.unit.to_string(),
                        a_median: m.a_median,
                        b_median: m.b_median,
                        delta: m.delta,
                        known_a: m.known_a,
                        known_b: m.known_b,
                    })
                    .collect(),
            })
            .collect(),
        processes: ProcessComparisonDto {
            a_ticks: analysis.processes.a_ticks,
            b_ticks: analysis.processes.b_ticks,
            a_incomplete: analysis.processes.a_incomplete,
            b_incomplete: analysis.processes.b_incomplete,
            a_total_threads: analysis.processes.a_total_threads,
            b_total_threads: analysis.processes.b_total_threads,
            incomplete: analysis.processes.incomplete,
            rows: analysis
                .processes
                .rows
                .into_iter()
                .map(|p| ProcessDifferenceDto {
                    identity: p.identity,
                    display_name: p.display_name,
                    a_cpu_median_pct: p.a_cpu_median_pct,
                    b_cpu_median_pct: p.b_cpu_median_pct,
                    delta: p.delta,
                    presence: p.presence.to_string(),
                    a_presence: p.a_presence,
                    b_presence: p.b_presence,
                    a_ticks: p.a_ticks,
                    b_ticks: p.b_ticks,
                    a_ambiguous: p.a_ambiguous,
                    b_ambiguous: p.b_ambiguous,
                    note: p.note,
                })
                .collect(),
        },
        categorical_differences: analysis
            .categorical_differences
            .into_iter()
            .map(|c| CategoricalDifferenceDto {
                key: c.key.to_string(),
                label: c.label.to_string(),
                a: c.a,
                b: c.b,
                state: c.state.to_string(),
                note: c.note,
            })
            .collect(),
        distributions: analysis
            .distributions
            .into_iter()
            .map(|d| DistributionComparisonDto {
                metric: d.metric.to_string(),
                label: d.label.to_string(),
                unit: d.unit.to_string(),
                a: dist(d.a),
                b: dist(d.b),
            })
            .collect(),
        correlations: analysis
            .correlations
            .into_iter()
            .map(|c| CorrelationComparisonDto {
                x: c.x.to_string(),
                y: c.y.to_string(),
                label: c.label,
                r_a: c.r_a,
                r_b: c.r_b,
                n_a: c.n_a,
                n_b: c.n_b,
                effective_n_a: c.effective_n_a,
                effective_n_b: c.effective_n_b,
                comparable: c.comparable,
                note: c.note,
            })
            .collect(),
        caveats,
    }
}

fn dist(d: core::DistributionSummary) -> DistributionSummaryDto {
    DistributionSummaryDto {
        n: d.n,
        known: d.known,
        min: d.min,
        p10: d.p10,
        median: d.median,
        p90: d.p90,
        max: d.max,
        spread: d.spread,
    }
}

fn side_dto(side: core::ComparisonSide, meta: &SideMeta) -> ComparisonSideDto {
    ComparisonSideDto {
        path: meta.path.clone(),
        label: meta.label.clone(),
        note: meta.note.clone(),
        status: meta.status.clone(),
        recovered: meta.recovered,
        whole_session: side.whole_session,
        from_ms: side.from_ms,
        to_ms: side.to_ms,
        duration_s: side.duration_s,
        interval_ms: side.interval_ms,
        coverage: side.coverage,
        energy: RangeEnergyDto {
            discharge_wh: side.energy.discharge_wh,
            charge_wh: side.energy.charge_wh,
            covered_s: side.energy.covered_s,
            unknown_s: side.energy.unknown_s,
            unobserved_s: side.energy.unobserved_s,
            discontinuities: side.energy.discontinuities,
            crosses_discontinuity: side.energy.crosses_discontinuity,
            discharge_present: side.energy.discharge_present,
            charge_present: side.energy.charge_present,
        },
        quality: RangeQualityDto {
            span_s: side.quality.span_s,
            covered_s: side.quality.covered_s,
            unknown_s: side.quality.unknown_s,
            unobserved_s: side.quality.unobserved_s,
            coverage: side.quality.coverage,
            discontinuities: side.quality.discontinuities,
            timeouts: side.quality.timeouts,
            recoveries: side.quality.recoveries,
            errors: side.quality.errors,
            events: side.quality.events,
            markers: side.quality.markers,
            samples: side.quality.samples,
            collector_counts: side
                .quality
                .collector_counts
                .into_iter()
                .map(|(name, samples)| CollectorSampleCount {
                    name: name.to_string(),
                    samples: samples as u64,
                })
                .collect(),
            stale_collectors: side
                .quality
                .stale_collectors
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
        },
        context: ComparisonContextDto {
            power_scheme: side.context.power_scheme,
            power_source: side.context.power_source,
            refresh_hz: side.context.refresh_hz,
            display_count: side.context.display_count,
            gpu_adapters: side.context.gpu_adapters,
            effective_cadence_ms: side.context.effective_cadence_ms,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::compare;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pf-gui-compare-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_session(path: &std::path::Path, base: f64) {
        let mut text = String::from(
            "{\"type\":\"session_header\",\"wall_ms\":1000,\"interval_ms\":1000,\"note\":\"cmp\"}\n",
        );
        for i in 0..60u64 {
            let v = base + (i % 5) as f64 * 0.3;
            text.push_str(&format!(
                "{{\"collector\":\"battery\",\"wall_ms\":{},\"mono_ms\":{},\
                 \"discharge_w\":{{\"v\":{v},\"p\":\"measured\"}},\"charge_pct\":{{\"v\":50,\"p\":\"measured\"}}}}\n",
                1000 + i * 1000,
                i * 1000
            ));
            text.push_str(&format!(
                "{{\"collector\":\"cpu\",\"wall_ms\":{},\"mono_ms\":{},\
                 \"totals\":{{\"utility\":{{\"v\":10,\"p\":\"measured\"}}}}}}\n",
                1000 + i * 1000,
                i * 1000
            ));
        }
        text.push_str("{\"type\":\"session_footer\",\"wall_ms\":61000,\"summary\":{\"samples\":60,\"discharge_wh\":0.1,\"discharge_coverage_pct\":99.0,\"recovered\":false}}\n");
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn whole_session_comparison_is_typed_and_normalized() {
        let dir = temp_dir("whole");
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        write_session(&a, 6.0);
        write_session(&b, 4.0);
        let out = compare(&a, None, None, &b, None, None).unwrap();
        assert!(out.a.whole_session && out.b.whole_session);
        assert_eq!(out.a.label, "a.jsonl");
        let power = out
            .headline_metrics
            .iter()
            .find(|m| m.metric == "battery_discharge_w")
            .unwrap();
        assert!(power.absolute_delta.unwrap() < 0.0);
        assert_eq!(power.comparability, "compatible");
        assert!(!out.ranked_changes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn range_comparison_keeps_requested_bounds() {
        let dir = temp_dir("range");
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        write_session(&a, 6.0);
        write_session(&b, 5.0);
        let out = compare(
            &a,
            Some(11_000),
            Some(31_000),
            &b,
            Some(11_000),
            Some(31_000),
        )
        .unwrap();
        assert!(!out.a.whole_session);
        assert_eq!(out.a.from_ms, 11_000);
        assert_eq!(out.a.to_ms, 31_000);
        assert!((out.a.duration_s - 20.0).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real-session timing harness. Run explicitly:
    /// `cargo test -p pf-gui --lib perf_real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perf_real_sessions() {
        let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../sessions"));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("no sessions dir at {}", dir.display());
            return;
        };
        let mut files: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
            .collect();
        files.sort();
        if files.len() < 2 {
            eprintln!("need at least two sessions; found {}", files.len());
            return;
        }
        let a = &files[0];
        let b = &files[1];
        let t = std::time::Instant::now();
        let out = compare(a, None, None, b, None, None).unwrap();
        println!(
            "whole-session compare: {:?} (headline={}, ranked={}, domains={}, processes={}, caveats={})",
            t.elapsed(),
            out.headline_metrics.len(),
            out.ranked_changes.len(),
            out.domains.len(),
            out.processes.rows.len(),
            out.caveats.len(),
        );
        // A 5-minute window on each side, then switch B to exercise caching.
        let from = out.a.from_ms;
        let to = from + 5 * 60_000;
        let t = std::time::Instant::now();
        let r = compare(a, Some(from), Some(to), b, Some(from), Some(to)).unwrap();
        println!(
            "range compare (5m each): {:?} (headline={}, ranked={})",
            t.elapsed(),
            r.headline_metrics.len(),
            r.ranked_changes.len()
        );
        let t = std::time::Instant::now();
        let _ = compare(a, Some(from), Some(to), b, None, None).unwrap();
        println!("switch B (whole session): {:?}", t.elapsed());
    }

    #[test]
    fn recovered_metadata_becomes_a_caveat() {
        let dir = temp_dir("recovered");
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        write_session(&a, 6.0);
        let text = std::fs::read_to_string(&a)
            .unwrap()
            .replace("\"recovered\":false", "\"recovered\":true")
            .replace(
                "{\"type\":\"session_footer\"",
                "{\"type\":\"session_footer\",\"recovered\":true",
            );
        std::fs::write(&b, text).unwrap();
        let out = compare(&a, None, None, &b, None, None).unwrap();
        assert!(out.b.recovered);
        assert!(
            out.caveats.iter().any(|c| c.message.contains("recovered")),
            "{:?}",
            out.caveats
        );
    }
}
