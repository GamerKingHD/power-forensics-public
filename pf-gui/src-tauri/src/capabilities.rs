//! Structured capability classification for the Diagnostics page.
//!
//! The inventory JSON comes from the agent (`capabilities_json`). The
//! AVAILABLE / DEGRADED / UNAVAILABLE rule is the same honest rule the CLI
//! uses: a field declared `unavailable` is unavailable regardless of
//! privilege; an admin-gated field is unavailable until the GUI is elevated;
//! a collector with a mix is degraded. Hardware-absence reasons are carried
//! through verbatim instead of being flattened to "failed".

use pf_core::json::parse;

use crate::dto::{BridgeError, CapabilityField, CapabilityReport, CollectorCapability};

pub fn classify(json: &str, elevated: bool) -> Result<CapabilityReport, BridgeError> {
    let v = parse(json).map_err(|e| {
        BridgeError::new(
            "analysis-unavailable",
            "capability inventory is not valid JSON",
        )
        .with_detail(e)
    })?;
    let collectors = v.get("collectors").and_then(|c| c.arr()).ok_or_else(|| {
        BridgeError::new(
            "analysis-unavailable",
            "capability inventory lacks \"collectors\"",
        )
    })?;

    let mut report = CapabilityReport {
        elevated,
        ..Default::default()
    };
    for c in collectors {
        let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let fields = c.get("fields").and_then(|f| f.arr()).unwrap_or(&[]);
        let mut available = 0usize;
        let mut unavailable = 0usize;
        let mut out_fields = Vec::with_capacity(fields.len());
        for f in fields {
            let fname = f.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let unit = f.get("unit").and_then(|n| n.as_str()).unwrap_or("");
            let source = f.get("source").and_then(|n| n.as_str()).unwrap_or("");
            let notes = f.get("notes").and_then(|n| n.as_str()).unwrap_or("");
            let prov = f.get("provenance").and_then(|n| n.as_str()).unwrap_or("");
            let admin = f
                .get("requires_admin")
                .and_then(|n| n.as_bool())
                .unwrap_or(false);
            let interval_ms = f
                .get("interval_ms")
                .and_then(|n| n.num())
                .unwrap_or(0.0)
                .max(0.0) as u64;

            let (state, reason, helps) = if prov == "unavailable" {
                ("unavailable", notes.to_string(), admin)
            } else if admin && !elevated {
                (
                    "unavailable",
                    "requires administrator privileges".to_string(),
                    true,
                )
            } else {
                ("available", String::new(), false)
            };
            if state == "available" {
                available += 1;
            } else {
                unavailable += 1;
            }
            out_fields.push(CapabilityField {
                name: fname.to_string(),
                unit: unit.to_string(),
                source: source.to_string(),
                provenance: prov.to_string(),
                interval_ms,
                requires_admin: admin,
                notes: notes.to_string(),
                state: state.to_string(),
                reason,
                elevation_would_help: helps,
            });
        }
        let state = if unavailable == 0 {
            report.available += 1;
            "available"
        } else if available == 0 {
            report.unavailable += 1;
            "unavailable"
        } else {
            report.degraded += 1;
            "degraded"
        };
        report.collectors.push(CollectorCapability {
            name: name.to_string(),
            state: state.to_string(),
            available,
            unavailable,
            fields: out_fields,
        });
    }
    Ok(report)
}

/// Convenience: the live inventory, classified for the current process.
pub fn current_report(elevated: bool) -> Result<CapabilityReport, BridgeError> {
    classify(&pf_agent::monitor::capabilities_json(), elevated)
}
