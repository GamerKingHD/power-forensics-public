//! Schema completeness gate.
//!
//! Every field a collector advertises via `capabilities()` MUST be
//! classified as exactly one of:
//!   * analytically retained (survives JSONL -> SessionData -> archive ->
//!     reload) — listed in `RETAINED_FIELDS` with its SessionData home,
//!   * presentation-only retained (kept in SessionData, not archived) —
//!     also in `RETAINED_FIELDS`, documented as presentation-only,
//!   * deliberately excluded — listed in `EXCLUDED_FIELDS` with a reason.
//!
//! A new collector field fails this test until someone classifies it.

use pf_collectors::battery::BatteryCollector;
use pf_collectors::collector::Collector;
use pf_collectors::cpu::CpuCollector;
use pf_collectors::display::DisplayCollector;
use pf_collectors::gpu::GpuCollector;
use pf_collectors::network::NetCollector;
use pf_collectors::proc::ProcCollector;
use pf_collectors::selfmon::SelfCollector;
use pf_collectors::storage::StorageCollector;
use pf_collectors::system::OsPowerCollector;
use pf_collectors::usb::UsbCollector;
use pf_core::archive::{STATIC_METRICS_PUB, load_archive_v2, migrate};
use pf_core::session::load_session;
use pf_core::telemetry::FieldMeta;

/// advertised field name -> where it lives in SessionData (or
/// "presentation-only: ..." when retained but not archived).
fn retained(name: &str) -> Option<&'static str> {
    for (prefix, home) in RETAINED_FIELDS {
        if *prefix == name {
            return Some(home);
        }
    }
    None
}

fn excluded_reason(name: &str) -> Option<&'static str> {
    EXCLUDED_FIELDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, r)| *r)
}

fn all_capabilities() -> Vec<FieldMeta> {
    let mut v = Vec::new();
    v.extend(CpuCollector::new().capabilities());
    v.extend(GpuCollector::new().capabilities());
    v.extend(DisplayCollector::new().capabilities());
    v.extend(NetCollector::new().capabilities());
    v.extend(StorageCollector::new().capabilities());
    v.extend(UsbCollector::new().capabilities());
    v.extend(SelfCollector::new().capabilities());
    v.extend(ProcCollector::new().capabilities());
    v.extend(BatteryCollector::new().capabilities());
    v.extend(OsPowerCollector::new().capabilities());
    v
}

#[test]
fn every_advertised_field_is_classified() {
    for meta in all_capabilities() {
        let name = meta.name;
        let r = retained(name);
        let e = excluded_reason(name);
        assert!(
            r.is_some() ^ e.is_some(),
            "field {name} must be in exactly one of RETAINED_FIELDS or EXCLUDED_FIELDS (retained={r:?}, excluded={e:?})"
        );
    }
}

#[test]
fn excluded_fields_carry_documented_reasons() {
    for (name, reason) in EXCLUDED_FIELDS {
        assert!(
            !reason.trim().is_empty(),
            "excluded field {name} has no reason"
        );
        // An exclusion must actually be advertised by a collector.
        assert!(
            all_capabilities().iter().any(|m| m.name == *name),
            "EXCLUDED_FIELDS entry {name} is not advertised by any collector (stale table)"
        );
    }
}

#[test]
fn retained_fields_are_advertised() {
    let caps = all_capabilities();
    for (name, home) in RETAINED_FIELDS {
        assert!(
            !home.trim().is_empty(),
            "retained field {name} has no SessionData home"
        );
        assert!(
            caps.iter().any(|m| m.name == *name),
            "RETAINED_FIELDS entry {name} is not advertised by any collector (stale table)"
        );
    }
}

#[test]
fn unknown_field_would_fail() {
    // Positive control: the classifier rejects an unclassified name, so a
    // newly advertised collector field cannot slip through.
    assert!(retained("brand.new.field").is_none());
    assert!(excluded_reason("brand.new.field").is_none());
}

/// Every advertised field name, as a flat list (used to prove the tables
/// cover the complete inventory).
#[test]
fn inventory_has_no_duplicate_names() {
    let mut seen = std::collections::HashSet::new();
    for m in all_capabilities() {
        assert!(seen.insert(m.name), "duplicate advertised name {}", m.name);
    }
    assert!(
        seen.len() >= 80,
        "unexpectedly small inventory: {}",
        seen.len()
    );
}

/// Static archive metrics must never collide with dynamic ids.
#[test]
fn static_metric_ids_are_dense_and_ordered() {
    for (i, (collector, metric, unit)) in STATIC_METRICS_PUB.iter().enumerate() {
        assert!(!collector.is_empty() && !metric.is_empty() && !unit.is_empty());
        assert!(i < 1000, "static id {i} overlaps the dynamic range");
    }
}

// ---------------------------------------------------------------------------
// Semantic gate: discover ACTUAL emitted field paths and classify each.
// ---------------------------------------------------------------------------

use pf_core::json::{JVal, parse};
use std::collections::BTreeSet;

/// Envelope keys present on every sample line and handled by the loader's
/// common path (identity + timing), not per-metric evidence.
const SAMPLE_ENVELOPE: &[&str] = &["collector", "wall_ms", "mono_ms", "t_start_ms", "t_end_ms"];

/// Recursively collect every scalar/telemetry field path actually emitted by
/// the representative serialization. Array indices normalize to `[*]`; a
/// telemetry envelope `{v,p,q,...}` is a single field (its metadata is
/// governed by the same classification and checked separately).
fn emitted_paths() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in realistic_jsonl().lines() {
        let Ok(v) = parse(line) else { continue };
        let Some(collector) = v.get("collector").and_then(|c| c.as_str()) else {
            continue;
        };
        if let JVal::Obj(pairs) = &v {
            for (k, val) in pairs {
                if SAMPLE_ENVELOPE.contains(&k.as_str()) {
                    continue;
                }
                walk(val, &format!("{collector}.{k}"), &mut out);
            }
        }
    }
    out
}

fn walk(v: &JVal, path: &str, out: &mut BTreeSet<String>) {
    match v {
        JVal::Arr(items) => {
            for it in items {
                walk(it, &format!("{path}[*]"), out);
            }
        }
        JVal::Obj(pairs) => {
            if pairs.iter().any(|(k, _)| k == "v") {
                out.insert(path.to_string());
                return;
            }
            for (k, val) in pairs {
                walk(val, &format!("{path}.{k}"), out);
            }
        }
        _ => {
            out.insert(path.to_string());
        }
    }
}

/// Does an emitted path match a declared pattern? `[*]` matches an array
/// wildcard segment; all other segments match literally.
fn path_matches(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('.').collect();
    let q: Vec<&str> = path.split('.').collect();
    p.len() == q.len()
        && p.iter()
            .zip(&q)
            .all(|(a, b)| *a == "[*]" || *b == "[*]" || a == b)
}

#[test]
fn every_emitted_field_is_classified_at_the_path_level() {
    let unclassified: Vec<String> = emitted_paths()
        .into_iter()
        .filter(|p| !SEMANTIC_FIELDS.iter().any(|(pat, _)| path_matches(pat, p)))
        .collect();
    assert!(
        unclassified.is_empty(),
        "emitted fields are not classified in SEMANTIC_FIELDS (add a retained home or an explicit exclusion): {unclassified:#?}"
    );
}

#[test]
fn emitted_paths_are_wildcarded_not_device_named() {
    let paths = emitted_paths();
    assert!(paths.contains("display.displays[*].width"), "{paths:#?}");
    assert!(paths.contains("cpu.cores[*].utility"), "{paths:#?}");
    assert!(
        paths.contains("gpu.adapters[*].total_util_pct"),
        "{paths:#?}"
    );
    assert!(paths.contains("net.adapters[*].alias"), "{paths:#?}");
    assert!(paths.contains("proc.top[*].pid"), "{paths:#?}");
    assert!(paths.contains("usb.groups[*].class"), "{paths:#?}");
    // Runtime device names never leak into the discovered paths.
    assert!(
        !paths
            .iter()
            .any(|p| p.contains("PANEL") || p.contains("MediaTek"))
    );
}

/// Deliberate regression tripwire: an unexpected emitted field must make the
/// gate FAIL until it is retained+mapped or explicitly excluded. This proves
/// the discovery is live rather than a comparison of two hand-written lists.
#[test]
fn unexpected_emitted_field_is_rejected_by_the_gate() {
    let clean = realistic_jsonl();
    assert!(
        classify_emitted(&clean).is_empty(),
        "fixture must be fully classified to start"
    );
    let injected = clean.replacen(
        "\"collector\":\"cpu\"",
        "\"collector\":\"cpu\",\"brand_new_sensor\":{\"v\":1.0,\"p\":\"measured\"}",
        1,
    );
    let missing = classify_emitted(&injected);
    assert_eq!(
        missing,
        vec!["cpu.brand_new_sensor".to_string()],
        "a new emitted field must be reported as unclassified"
    );
}

/// Return the emitted paths not covered by SEMANTIC_FIELDS.
fn classify_emitted(text: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let Ok(v) = parse(line) else { continue };
        let Some(collector) = v.get("collector").and_then(|c| c.as_str()) else {
            continue;
        };
        if let JVal::Obj(pairs) = &v {
            for (k, val) in pairs {
                if SAMPLE_ENVELOPE.contains(&k.as_str()) {
                    continue;
                }
                walk(val, &format!("{collector}.{k}"), &mut out);
            }
        }
    }
    out.into_iter()
        .filter(|p| !SEMANTIC_FIELDS.iter().any(|(pat, _)| path_matches(pat, p)))
        .collect()
}

/// Every `adv:` mapping must name an advertised field classified as retained,
/// and every `excl:` mapping must name an advertised field explicitly
/// excluded. This ties the emitted-path discovery to the advertised schema.
#[test]
fn semantic_paths_agree_with_advertised_classification() {
    let caps = all_capabilities();
    for (pattern, class) in SEMANTIC_FIELDS {
        if let Some(adv) = class.strip_prefix("adv:") {
            let meta = caps.iter().find(|m| m.name == adv).unwrap_or_else(|| {
                panic!("SEMANTIC_FIELDS {pattern} -> advertised {adv} is not advertised")
            });
            assert!(
                retained(meta.name).is_some(),
                "semantic path {pattern} -> advertised {adv} must be retained"
            );
        } else if let Some(adv) = class.strip_prefix("excl:") {
            assert!(
                caps.iter().any(|m| m.name == adv),
                "semantic path {pattern} -> excluded {adv} is not advertised"
            );
            assert!(
                excluded_reason(adv).is_some(),
                "semantic path {pattern} -> {adv} is not in EXCLUDED_FIELDS"
            );
        } else if let Some(home) = class.strip_prefix("home:") {
            assert!(
                !home.trim().is_empty(),
                "semantic path {pattern} has an empty SessionData home"
            );
        } else if let Some(why) = class.strip_prefix("drop:") {
            assert!(
                why.trim().len() > 5,
                "semantic path {pattern} must document why dropping is safe"
            );
        } else {
            panic!("SEMANTIC_FIELDS {pattern} has no classification: {class}");
        }
    }
}

/// Emitted JSON path (arrays wildcarded as `[*]`) -> classification:
///   `adv:<field>`  retained, and `<field>` is an advertised FieldMeta
///   `excl:<field>` emitted value is deliberately unsupported; `<field>` is
///                  an advertised field listed in EXCLUDED_FIELDS
///   `home:<path>`  retained in SessionData at `<path>`, not a per-metric
///                  advertised field (identity/annotation/error text)
///   `drop:<why>`   emitted for render/wire only and deliberately NOT
///                  persisted, with the reason it is safe (derivable, or the
///                  advertised counterpart is explicitly excluded)
/// Discovery walks the real serialization, so a new emitted field fails the
/// gate until it is classified here.
const SEMANTIC_FIELDS: &[(&str, &str)] = &[
    // battery
    ("battery.ac", "adv:battery.ac_connected"),
    ("battery.charge_w", "home:BatteryPoint.charge"),
    ("battery.discharge_w", "home:BatteryPoint.discharge"),
    (
        "battery.capabilities",
        "drop:wire-only capability mask, not evidence",
    ),
    ("battery.battery_index", "adv:battery.index_count"),
    ("battery.battery_count", "adv:battery.index_count"),
    (
        "battery.batteries[*].index",
        "home:BatteryMeta.batteries[].index",
    ),
    (
        "battery.batteries[*].designed_mwh",
        "adv:battery.design_mwh",
    ),
    (
        "battery.batteries[*].full_mwh",
        "adv:battery.full_charge_mwh",
    ),
    (
        "battery.batteries[*].cycle_count",
        "adv:battery.cycle_count",
    ),
    ("battery.batteries[*].chemistry", "adv:battery.chemistry"),
    ("battery.batteries[*].device_name", "adv:battery.identity"),
    ("battery.batteries[*].unique_id", "adv:battery.identity"),
    ("battery.charge_pct", "adv:battery.charge_percent"),
    ("battery.rate_mw", "adv:battery.rate_mw"),
    ("battery.remaining_mwh", "adv:battery.remaining_mwh"),
    ("battery.full_charge_mwh", "adv:battery.full_charge_mwh"),
    ("battery.design_mwh", "adv:battery.design_mwh"),
    ("battery.technology", "adv:battery.technology"),
    ("battery.cycle_count", "adv:battery.cycle_count"),
    ("battery.chemistry", "adv:battery.chemistry"),
    ("battery.device_name", "adv:battery.identity"),
    ("battery.mfg_name", "adv:battery.identity"),
    ("battery.mfg_date", "adv:battery.identity"),
    ("battery.unique_id", "adv:battery.identity"),
    ("battery.health", "adv:battery.health"),
    ("battery.temperature_raw", "adv:battery.temperature_raw"),
    (
        "battery.present",
        "drop:presence is derivable from the measured battery_count and the per-field Unavailable reasons; not retained separately",
    ),
    // cpu
    ("cpu.totals.utility", "adv:cpu.total.utility"),
    ("cpu.totals.c3_pct", "adv:cpu.total.c3_pct"),
    ("cpu.totals.idle_breaks", "adv:cpu.total.idle_breaks"),
    ("cpu.totals.ctx_switches", "adv:cpu.total.ctx_switches"),
    ("cpu.totals.proc_queue", "adv:cpu.total.proc_queue"),
    ("cpu.totals.priv_utility", "adv:cpu.total.priv_utility"),
    ("cpu.totals.performance", "adv:cpu.total.performance"),
    ("cpu.totals.of_max_freq", "adv:cpu.total.of_max_freq"),
    ("cpu.totals.freq_mhz", "adv:cpu.total.freq_mhz"),
    ("cpu.totals.idle_pct", "adv:cpu.total.idle_pct"),
    ("cpu.totals.c1_pct", "adv:cpu.total.c1_pct"),
    ("cpu.totals.c2_pct", "adv:cpu.total.c2_pct"),
    ("cpu.totals.c1_trans", "adv:cpu.total.c1_trans"),
    ("cpu.totals.c2_trans", "adv:cpu.total.c2_trans"),
    ("cpu.totals.c3_trans", "adv:cpu.total.c3_trans"),
    ("cpu.totals.interrupts", "adv:cpu.total.interrupts"),
    ("cpu.totals.dpc_rate", "adv:cpu.total.dpc_rate"),
    ("cpu.totals.dpc_pct", "adv:cpu.total.dpc_pct"),
    ("cpu.totals.interrupt_pct", "adv:cpu.total.interrupt_pct"),
    ("cpu.totals.user_time", "adv:cpu.total.user_time"),
    ("cpu.totals.processor_time", "adv:cpu.total.processor_time"),
    (
        "cpu.totals.perf_limit_flags",
        "adv:cpu.total.perf_limit_flags",
    ),
    ("cpu.totals.perf_limit_pct", "adv:cpu.total.perf_limit_pct"),
    ("cpu.cores[*].inst", "home:CpuPoint.cores[].instance"),
    ("cpu.cores[*].utility", "adv:cpu.core.utility"),
    ("cpu.cores[*].performance", "adv:cpu.core.performance"),
    ("cpu.cores[*].freq_mhz", "adv:cpu.core.freq_mhz"),
    ("cpu.cores[*].parking", "adv:cpu.core.parking"),
    (
        "cpu.energy.instance",
        "drop:RAPL instance label is diagnostic, not persisted",
    ),
    ("cpu.energy.pkg_power_w", "adv:cpu.energy.pkg_power_w"),
    ("cpu.energy.pkg_derived_w", "adv:cpu.energy.pkg_derived_w"),
    // display
    ("display.displays[*].name", "adv:display.monitors"),
    ("display.displays[*].primary", "adv:display.monitors"),
    ("display.displays[*].width", "adv:display.mode"),
    ("display.displays[*].height", "adv:display.mode"),
    ("display.displays[*].freq_hz", "adv:display.mode"),
    ("display.displays[*].bpp", "adv:display.mode"),
    (
        "display.displays[*].brightness_pct",
        "adv:display.brightness_pct",
    ),
    (
        "display.unmatched_sensors[*].instance",
        "home:DisplayPoint.unmatched_sensors",
    ),
    (
        "display.unmatched_sensors[*].brightness",
        "home:DisplayPoint.unmatched_sensors",
    ),
    ("display.hdr_targets[*].adapter", "adv:display.hdr"),
    ("display.hdr_targets[*].target", "adv:display.hdr"),
    ("display.hdr_targets[*].supported", "adv:display.hdr"),
    ("display.hdr_targets[*].enabled", "adv:display.hdr"),
    ("display.hdr_error", "home:DisplayPoint.hdr_error"),
    ("display.changes", "adv:display.changes"),
    // gpu
    ("gpu.stale", "adv:gpu.adapter.awake"),
    (
        "gpu.adapters[*].index",
        "drop:adapter index not retained; name is the identity",
    ),
    ("gpu.adapters[*].name", "adv:gpu.adapter.inventory"),
    (
        "gpu.adapters[*].vendor",
        "drop:adapter vendor not retained (name + discrete retained)",
    ),
    (
        "gpu.adapters[*].vendor_id",
        "drop:adapter PCI vendor id not retained; adapter name is the identity key",
    ),
    (
        "gpu.adapters[*].device_id",
        "drop:adapter PCI device id not retained; adapter name is the identity key",
    ),
    (
        "gpu.adapters[*].dedicated_mb",
        "drop:static dedicated VRAM total not retained; per-tick mem_dedicated_mb usage is",
    ),
    (
        "gpu.adapters[*].luid",
        "drop:runtime adapter LUID not retained; adapter name is the stable identity",
    ),
    ("gpu.adapters[*].discrete", "adv:gpu.adapter.inventory"),
    (
        "gpu.adapters[*].total_util_pct",
        "adv:gpu.adapter.util_total_pct",
    ),
    (
        "gpu.adapters[*].util_3d",
        "adv:gpu.adapter.util_by_class_pct",
    ),
    (
        "gpu.adapters[*].util_compute",
        "adv:gpu.adapter.util_by_class_pct",
    ),
    (
        "gpu.adapters[*].util_decode",
        "adv:gpu.adapter.util_by_class_pct",
    ),
    (
        "gpu.adapters[*].util_codec",
        "adv:gpu.adapter.util_by_class_pct",
    ),
    (
        "gpu.adapters[*].util_copy",
        "adv:gpu.adapter.util_by_class_pct",
    ),
    (
        "gpu.adapters[*].util_other",
        "adv:gpu.adapter.util_by_class_pct",
    ),
    (
        "gpu.adapters[*].engines_active",
        "home:GpuAdapterPoint.engines_active",
    ),
    (
        "gpu.adapters[*].engines_seen",
        "home:GpuAdapterPoint.engines_seen",
    ),
    ("gpu.adapters[*].mem_dedicated_mb", "adv:gpu.adapter.mem_mb"),
    ("gpu.adapters[*].mem_shared_mb", "adv:gpu.adapter.mem_mb"),
    (
        "gpu.adapters[*].top_pids[*].pid",
        "adv:gpu.adapter.top_pids",
    ),
    (
        "gpu.adapters[*].top_pids[*].util_pct",
        "adv:gpu.adapter.top_pids",
    ),
    ("gpu.adapters[*].awake.active", "adv:gpu.adapter.awake"),
    ("gpu.adapters[*].awake.confidence", "adv:gpu.adapter.awake"),
    ("gpu.adapters[*].awake.evidence", "adv:gpu.adapter.awake"),
    // net
    ("net.adapters[*].alias", "adv:net.adapter.inventory"),
    ("net.adapters[*].descr", "adv:net.adapter.inventory"),
    (
        "net.adapters[*].guid",
        "drop:adapter guid not retained; alias is the identity key",
    ),
    ("net.adapters[*].class", "adv:net.adapter.inventory"),
    ("net.adapters[*].iftype", "adv:net.adapter.inventory"),
    ("net.adapters[*].oper", "home:NetAdapterRec.oper"),
    (
        "net.adapters[*].oper_name",
        "drop:human oper label, derivable from oper",
    ),
    ("net.adapters[*].media", "home:NetAdapterRec.media"),
    ("net.adapters[*].admin_up", "home:NetAdapterRec.admin_up"),
    ("net.adapters[*].tx_mbps", "home:NetAdapterRec.tx_mbps"),
    ("net.adapters[*].rx_mbps", "home:NetAdapterRec.rx_mbps"),
    ("net.adapters[*].in_octets", "home:NetAdapterRec.in_octets"),
    (
        "net.adapters[*].out_octets",
        "home:NetAdapterRec.out_octets",
    ),
    ("net.throughput[*].instance", "adv:net.throughput_bps"),
    ("net.throughput[*].rx_bps", "adv:net.throughput_bps"),
    ("net.throughput[*].tx_bps", "adv:net.throughput_bps"),
    ("net.wifi[*].descr", "adv:net.wifi"),
    ("net.wifi[*].state", "home:WifiRec.state"),
    ("net.wifi[*].ssid", "adv:net.wifi"),
    ("net.wifi[*].signal_pct", "adv:net.wifi"),
    ("net.total_rx_bps", "adv:net.totals_bps"),
    ("net.total_tx_bps", "adv:net.totals_bps"),
    ("net.changes", "adv:net.changes"),
    ("net.topology_stale", "adv:net.topology_stale"),
    ("net.topology_error", "adv:net.topology_stale"),
    // os power
    ("os_power.active_scheme", "adv:os.active_power_scheme"),
    ("os_power.scheme_name", "adv:os.active_power_scheme"),
    (
        "os_power.policy_snapshot_wall_ms",
        "home:Policy.policy_snapshot_wall_ms",
    ),
    ("os_power.timer_resolution_ms", "adv:os.timer_resolution_ms"),
    ("os_power.cpu_min_pct.ac", "adv:os.cpu_min_max_pct"),
    ("os_power.cpu_min_pct.dc", "adv:os.cpu_min_max_pct"),
    ("os_power.cpu_max_pct.ac", "adv:os.cpu_min_max_pct"),
    ("os_power.cpu_max_pct.dc", "adv:os.cpu_min_max_pct"),
    ("os_power.epp.ac", "adv:os.epp"),
    ("os_power.epp.dc", "adv:os.epp"),
    ("os_power.display_timeout_s.ac", "adv:os.display_timeout_s"),
    ("os_power.display_timeout_s.dc", "adv:os.display_timeout_s"),
    ("os_power.brightness_pct.ac", "adv:os.brightness_policy_pct"),
    ("os_power.brightness_pct.dc", "adv:os.brightness_policy_pct"),
    ("os_power.sleep_timeout_s.ac", "adv:os.sleep_timeout_s"),
    ("os_power.sleep_timeout_s.dc", "adv:os.sleep_timeout_s"),
    (
        "os_power.hibernate_timeout_s.ac",
        "adv:os.hibernate_timeout_s",
    ),
    (
        "os_power.hibernate_timeout_s.dc",
        "adv:os.hibernate_timeout_s",
    ),
    ("os_power.low_batt_pct.ac", "adv:os.battery_levels_pct"),
    ("os_power.low_batt_pct.dc", "adv:os.battery_levels_pct"),
    ("os_power.crit_batt_pct.ac", "adv:os.battery_levels_pct"),
    ("os_power.crit_batt_pct.dc", "adv:os.battery_levels_pct"),
    // storage (only the `_Total` aggregate is persisted)
    (
        "storage.disks[*].instance",
        "drop:per-disk identity; only the _Total aggregate is persisted",
    ),
    (
        "storage.disks[*].total",
        "home:StoragePoint (total-disk selector)",
    ),
    (
        "storage.disks[*].disk_time_pct",
        "adv:storage.disk.util_pct",
    ),
    (
        "storage.disks[*].idle_pct",
        "drop:per-disk idle not retained (disk_time_pct is the busyness signal)",
    ),
    (
        "storage.disks[*].avg_queue",
        "excl:storage.disk.queue_latency",
    ),
    (
        "storage.disks[*].cur_queue",
        "excl:storage.disk.queue_latency",
    ),
    ("storage.disks[*].reads_s", "excl:storage.disk.iops"),
    ("storage.disks[*].writes_s", "excl:storage.disk.iops"),
    (
        "storage.disks[*].read_bps",
        "adv:storage.disk.throughput_bps",
    ),
    (
        "storage.disks[*].write_bps",
        "adv:storage.disk.throughput_bps",
    ),
    (
        "storage.disks[*].read_lat_ms",
        "excl:storage.disk.queue_latency",
    ),
    (
        "storage.disks[*].write_lat_ms",
        "excl:storage.disk.queue_latency",
    ),
    // proc
    (
        "proc.total_procs",
        "drop:total process count not retained (proc.list excluded)",
    ),
    ("proc.total_threads", "home:ProcPoint.total_threads"),
    ("proc.inaccessible", "home:ProcPoint.inaccessible"),
    ("proc.truncated", "home:ProcPoint.truncated"),
    ("proc.top[*].pid", "adv:proc.top.identity"),
    ("proc.top[*].ppid", "adv:proc.top.identity"),
    ("proc.top[*].name", "adv:proc.top.identity"),
    ("proc.top[*].start_unix_ms", "adv:proc.top.identity"),
    ("proc.top[*].exit_unix_ms", "adv:proc.top.identity"),
    ("proc.top[*].cpu_pct", "adv:proc.top.cpu_pct"),
    ("proc.top[*].threads", "excl:proc.top.handles_threads"),
    (
        "proc.top[*].accessible",
        "drop:per-process accessibility is summarized by the retained aggregate proc.inaccessible; not duplicated per entry",
    ),
    (
        "proc.top[*].cpu_100ns_total",
        "drop:raw cumulative CPU counter; cpu_pct is the retained derived rate",
    ),
    ("proc.top[*].ws_mb", "excl:proc.top.mem_mb"),
    ("proc.top[*].priv_mb", "excl:proc.top.mem_mb"),
    ("proc.top[*].io_r_bps", "excl:proc.top.io_bps"),
    ("proc.top[*].io_w_bps", "excl:proc.top.io_bps"),
    (
        "proc.top[*].io_r_total",
        "drop:raw cumulative IO counter; per-process rates are excluded under proc.top.io_bps",
    ),
    (
        "proc.top[*].io_w_total",
        "drop:raw cumulative IO counter; per-process rates are excluded under proc.top.io_bps",
    ),
    ("proc.top[*].handles", "excl:proc.top.handles_threads"),
    // self
    ("self.cpu_pct", "adv:self.cpu_pct"),
    ("self.ws_mb", "adv:self.mem_mb"),
    ("self.priv_mb", "adv:self.mem_mb"),
    ("self.io_read_b", "adv:self.io_bytes"),
    ("self.io_write_b", "adv:self.io_bytes"),
    ("self.threads", "adv:self.threads"),
    ("self.ctx_switches_s", "adv:self.ctx_switches_s"),
    // usb
    ("usb.groups[*].class", "home:UsbClassCount.class"),
    ("usb.groups[*].count", "home:UsbClassCount.count"),
    (
        "usb.groups[*].devices[*].instance",
        "drop:per-device identity not retained (usb.devices excluded)",
    ),
    (
        "usb.groups[*].devices[*].name",
        "drop:per-device name not retained (usb.devices excluded)",
    ),
    (
        "usb.groups[*].devices[*].started",
        "drop:per-device started not retained (usb.devices excluded)",
    ),
    ("usb.power_devices[*]", "adv:usb.power_devices"),
    (
        "usb.power_error",
        "drop:error text is surfaced via session events, not persisted per-tick",
    ),
    ("usb.changes", "adv:usb.changes"),
];

/// A loader that preserves the number but drops its uncertainty is still
/// lossy. For representative retained telemetry (including wildcard-indexed
/// dynamic fields and an unavailable-with-reason field), the serialized
/// evidence metadata must survive JSONL -> SessionData.
#[test]
fn emitted_evidence_metadata_survives_loading() {
    use pf_core::telemetry::{Provenance, UnavailKind};
    let s = load_session("meta", &realistic_jsonl()).expect("loads");
    // Measured with explicit quality.
    assert_eq!(s.battery_extra[0].ac.provenance(), Provenance::Measured);
    assert_eq!(
        s.battery_extra[0].ac.quality,
        pf_core::telemetry::SampleQuality::Fresh
    );
    // Derived aggregate.
    assert_eq!(s.battery_extra[0].health.provenance(), Provenance::Derived);
    // Estimated energy.
    assert_eq!(s.cpu[0].pkg_power_w.provenance(), Provenance::Estimated);
    // Dynamic (wildcard) field: a specific adapter's class utilization.
    assert_eq!(
        s.gpu[0].adapters[0].util_3d.provenance(),
        Provenance::Derived
    );
    // Unavailable WITH a reason and kind: must not degrade to a bare zero.
    let b = &s.display[0].displays[1].brightness;
    assert_eq!(b.provenance(), Provenance::Unavailable);
    assert_eq!(b.unavail_kind, Some(UnavailKind::Unsupported));
    assert_eq!(b.value.reason(), Some("no WMI sensor matched"));
    // Presentation-only error text is retained, not fabricated.
    assert!(s.display[0].hdr_error.is_none());
    // Staleness on a topology record.
    assert!(!s.net[0].topology_stale);
}

/// Home validation: representative serialized evidence must actually arrive
/// in the SessionData destination claimed by SEMANTIC_FIELDS (not merely be a
/// non-empty string). Each check names a semantic path and asserts the loaded
/// value is present and correct in that home.
#[test]
fn retained_emitted_evidence_reaches_its_home() {
    let s = load_session("home", &realistic_jsonl()).expect("loads");
    // home:BatteryExtraPoint.ac
    assert_eq!(s.battery_extra[0].ac.val(), Some(0.0));
    // home:BatteryMeta.batteries[].index
    assert_eq!(s.battery_meta.batteries[0].index, 0);
    // home:CpuPoint.cores[].instance
    assert_eq!(s.cpu[0].cores[0].instance, "0,0");
    // home:DisplayPoint.unmatched_sensors
    assert_eq!(s.display[0].unmatched_sensors[0].0, "DISPLAY\\ZZZ");
    assert_eq!(s.display[0].unmatched_sensors[0].1, Some(20.0));
    // home:GpuAdapterPoint.engines_active
    assert_eq!(s.gpu[0].adapters[0].engines_active.val(), Some(1.0));
    // home:NetAdapterRec.oper / media / admin_up
    assert_eq!(s.net[0].adapters[0].oper, Some(1.0));
    assert_eq!(s.net[0].adapters[0].media, Some(1.0));
    assert!(s.net[0].adapters[0].admin_up);
    // home:WifiRec.state
    assert_eq!(s.net[0].wifi[0].state, Some(1.0));
    // home:Policy.policy_snapshot_wall_ms
    assert_eq!(s.policy.policy_snapshot_wall_ms, Some(1000));
    // home:UsbClassCount.class / count
    assert_eq!(s.usb[0].classes[1].class, "gaming");
    assert_eq!(s.usb[0].classes[1].count, 1);
    // home:BatteryPoint.discharge (present and labeled derived in this fixture)
    assert_eq!(
        s.battery[0].discharge.provenance(),
        pf_core::telemetry::Provenance::Derived
    );
    // home:ProcPoint coverage qualifiers reach their typed destinations and
    // keep unknown/incomplete distinct from complete.
    assert_eq!(s.procs[0].total_threads, Some(400));
    assert_eq!(s.procs[0].inaccessible, Some(0));
    assert_eq!(s.procs[0].truncated, Some(false));
    assert_eq!(s.procs[3].inaccessible, Some(2));
    assert_eq!(s.procs[3].truncated, Some(true));
    assert_eq!(s.procs[3].total_threads, Some(401));
    assert_eq!(s.process_coverage_incomplete(), Some(true));
}

/// Targeted regression: the fields the acceptance audit found escaping
/// (battery.present, proc.total_threads, proc.inaccessible, proc.truncated)
/// plus the nested GPU adapter identity fields that the follow-up serialized
/// comparison found escaping must be represented in the representative
/// fixture and classified, so they cannot silently disappear from the gate
/// again. `storage.stale` was dead capability and is asserted removed at the
/// producer instead.
#[test]
fn audit_escaped_fields_are_now_covered() {
    let paths = emitted_paths();
    for p in [
        "battery.present",
        "proc.total_threads",
        "proc.inaccessible",
        "proc.truncated",
        "gpu.adapters[*].vendor_id",
        "gpu.adapters[*].device_id",
        "gpu.adapters[*].dedicated_mb",
        "gpu.adapters[*].luid",
    ] {
        assert!(paths.contains(p), "fixture must emit {p} (got {paths:#?})");
        assert!(
            SEMANTIC_FIELDS.iter().any(|(pat, _)| path_matches(pat, p)),
            "{p} must be classified in SEMANTIC_FIELDS"
        );
    }
    // `storage.stale` is no longer a production path at all.
    assert!(
        !paths.iter().any(|p| p == "storage.stale"),
        "dead storage.stale must not be emitted/represented"
    );
}

#[test]
fn classification_tables_have_no_duplicate_or_overlapping_entries() {
    let mut seen = std::collections::HashSet::new();
    for (p, _) in SEMANTIC_FIELDS {
        assert!(seen.insert(*p), "duplicate semantic path {p}");
    }
    let mut retained_names = std::collections::HashSet::new();
    for (n, _) in RETAINED_FIELDS {
        assert!(
            retained_names.insert(*n),
            "duplicate RETAINED_FIELDS entry {n}"
        );
    }
    for (n, _) in EXCLUDED_FIELDS {
        assert!(
            !retained_names.contains(*n),
            "field {n} is declared both retained and excluded"
        );
    }
}

#[test]
fn every_advertised_sampled_field_is_represented_by_an_emitted_path() {
    // Ties the discovered serialization to the advertised schema: a field a
    // collector advertises as sampled must either appear as an emitted path
    // (adv:) or be a deliberately unsupported advertised field (excl:).
    let mut failures = Vec::new();
    for m in all_capabilities() {
        if m.interval_ms == 0 {
            continue; // informational / event-derived, not a per-tick sample
        }
        // Event-derived advertised fields (e.g. storage.watchdog) are emitted
        // through the session event stream, not as a per-tick JSON metric.
        let home = retained(m.name).unwrap_or("");
        if home.contains("events") {
            continue;
        }
        let as_adv = format!("adv:{}", m.name);
        let covered = SEMANTIC_FIELDS.iter().any(|(_, c)| *c == as_adv)
            || EXCLUDED_FIELDS.iter().any(|(n, _)| *n == m.name);
        if !covered {
            failures.push(m.name);
        }
    }
    assert!(
        failures.is_empty(),
        "advertised sampled fields with no emitted-path classification: {failures:#?}"
    );
}

// ---------------------------------------------------------------------------
// Classification tables (the authoritative inventory).
// ---------------------------------------------------------------------------

const RETAINED_FIELDS: &[(&str, &str)] = &[
    // battery
    ("battery.index_count", "BatteryMeta.battery_index/count"),
    ("battery.charge_percent", "BatteryPoint.pct"),
    ("battery.ac_connected", "BatteryExtraPoint.ac"),
    ("battery.rate_mw", "BatteryExtraPoint.rate_mw"),
    ("battery.remaining_mwh", "BatteryPoint.remaining_wh"),
    ("battery.full_charge_mwh", "BatteryMeta.full_mwh"),
    ("battery.design_mwh", "BatteryMeta.design_mwh"),
    ("battery.chemistry", "BatteryMeta.batteries[].chemistry"),
    ("battery.technology", "BatteryMeta.batteries[].technology"),
    ("battery.cycle_count", "BatteryMeta.batteries[].cycle_count"),
    ("battery.health", "BatteryExtraPoint.health"),
    (
        "battery.temperature_raw",
        "BatteryExtraPoint.temperature_raw",
    ),
    (
        "battery.identity",
        "BatteryMeta.batteries[].device_name/unique_id/mfg_name/mfg_date",
    ),
    // cpu totals (dedicated fields)
    ("cpu.total.utility", "CpuPoint.utility"),
    ("cpu.total.c3_pct", "CpuPoint.c3_pct"),
    ("cpu.total.idle_breaks", "CpuPoint.idle_breaks"),
    ("cpu.total.ctx_switches", "CpuPoint.ctx_switches"),
    ("cpu.total.proc_queue", "CpuPoint.totals[proc_queue]"),
    ("cpu.total.priv_utility", "CpuPoint.totals[priv_utility]"),
    ("cpu.total.performance", "CpuPoint.totals[performance]"),
    ("cpu.total.of_max_freq", "CpuPoint.totals[of_max_freq]"),
    ("cpu.total.freq_mhz", "CpuPoint.totals[freq_mhz]"),
    ("cpu.total.idle_pct", "CpuPoint.totals[idle_pct]"),
    ("cpu.total.c1_pct", "CpuPoint.totals[c1_pct]"),
    ("cpu.total.c2_pct", "CpuPoint.totals[c2_pct]"),
    ("cpu.total.c1_trans", "CpuPoint.totals[c1_trans]"),
    ("cpu.total.c2_trans", "CpuPoint.totals[c2_trans]"),
    ("cpu.total.c3_trans", "CpuPoint.totals[c3_trans]"),
    ("cpu.total.interrupts", "CpuPoint.totals[interrupts]"),
    ("cpu.total.dpc_rate", "CpuPoint.totals[dpc_rate]"),
    ("cpu.total.dpc_pct", "CpuPoint.totals[dpc_pct]"),
    ("cpu.total.interrupt_pct", "CpuPoint.totals[interrupt_pct]"),
    ("cpu.total.user_time", "CpuPoint.totals[user_time]"),
    (
        "cpu.total.processor_time",
        "CpuPoint.totals[processor_time]",
    ),
    (
        "cpu.total.perf_limit_flags",
        "CpuPoint.totals[perf_limit_flags]",
    ),
    (
        "cpu.total.perf_limit_pct",
        "CpuPoint.totals[perf_limit_pct]",
    ),
    ("cpu.core.utility", "CpuPoint.cores[].utility"),
    ("cpu.core.performance", "CpuPoint.cores[].performance"),
    ("cpu.core.freq_mhz", "CpuPoint.cores[].freq_mhz"),
    ("cpu.core.parking", "CpuPoint.cores[].parking"),
    ("cpu.energy.pkg_power_w", "CpuPoint.pkg_power_w"),
    ("cpu.energy.pkg_derived_w", "CpuPoint.pkg_derived_w"),
    // gpu
    (
        "gpu.adapter.inventory",
        "GpuAdapterPoint.name/discrete (gpu_adapters table)",
    ),
    ("gpu.adapter.util_total_pct", "GpuAdapterPoint.total_util"),
    (
        "gpu.adapter.util_by_class_pct",
        "GpuAdapterPoint.util_3d/compute/decode/codec/copy/other",
    ),
    (
        "gpu.adapter.mem_mb",
        "GpuAdapterPoint.mem_dedicated_mb/mem_shared_mb",
    ),
    ("gpu.adapter.top_pids", "GpuAdapterPoint.top_pids"),
    (
        "gpu.adapter.awake",
        "GpuPoint.awake.active + GpuPoint.stale",
    ),
    // display
    (
        "display.mode",
        "DisplayEntry width/height/freq_hz/bpp/primary",
    ),
    ("display.brightness_pct", "DisplayEntry.brightness"),
    ("display.monitors", "DisplayPoint.displays"),
    ("display.hdr", "DisplayPoint.hdr_targets"),
    (
        "display.changes",
        "presentation-only: DisplayPoint.changes (also session events)",
    ),
    // usb
    ("usb.power_devices", "UsbPoint.power_devices"),
    (
        "usb.changes",
        "presentation-only: UsbPoint.changes (also session events)",
    ),
    // net
    (
        "net.adapter.inventory",
        "NetAdapterRec (net_adapters identity table)",
    ),
    ("net.throughput_bps", "NetPoint.throughput"),
    ("net.totals_bps", "NetPoint.total_rx/total_tx"),
    (
        "net.topology_stale",
        "NetPoint.topology_stale (also archive net.topology_stale)",
    ),
    ("net.wifi", "WifiRec (ssid in wifi_identity table)"),
    (
        "net.changes",
        "presentation-only: NetPoint.changes (also session events)",
    ),
    // storage
    ("storage.disk.util_pct", "StoragePoint.disk_time"),
    (
        "storage.disk.throughput_bps",
        "StoragePoint.read_bps/write_bps",
    ),
    ("storage.watchdog", "session events (storage kind)"),
    // self
    ("self.cpu_pct", "SelfPoint.cpu_pct"),
    ("self.mem_mb", "SelfPoint.ws_mb/priv_mb"),
    ("self.io_bytes", "SelfPoint.io_read_b/io_write_b"),
    ("self.threads", "SelfPoint.threads"),
    ("self.ctx_switches_s", "SelfPoint.ctx_switches"),
    // proc
    (
        "proc.top.identity",
        "ProcEntry pid/ppid/name/start_unix_ms/exit_unix_ms",
    ),
    ("proc.top.cpu_pct", "ProcEntry.cpu"),
    // os power
    ("os.active_power_scheme", "Policy.scheme_guid/scheme_name"),
    ("os.cpu_min_max_pct", "Policy.cpu_min/max_ac/dc"),
    ("os.epp", "Policy.epp_ac/dc"),
    ("os.display_timeout_s", "Policy.display_timeout_ac/dc"),
    ("os.brightness_policy_pct", "Policy.brightness_ac/dc"),
    ("os.sleep_timeout_s", "Policy.sleep_timeout_ac/dc"),
    ("os.hibernate_timeout_s", "Policy.hibernate_timeout_ac/dc"),
    ("os.battery_levels_pct", "Policy.low_batt/crit_batt_ac/dc"),
    ("os.timer_resolution_ms", "Policy.timer_resolution"),
];

const EXCLUDED_FIELDS: &[(&str, &str)] = &[
    (
        "gpu.power_w",
        "no vendor GPU power API linked on this build; advertised Unavailable",
    ),
    ("gpu.clock_mhz", "vendor API absent; advertised Unavailable"),
    (
        "gpu.temperature_c",
        "vendor API absent; advertised Unavailable",
    ),
    (
        "gpu.pcie_state",
        "no generic PCIe link-state API; advertised Unavailable",
    ),
    (
        "display.power_on",
        "panel power needs WMI eventing/ETW; advertised Unavailable",
    ),
    (
        "display.power_w",
        "needs a per-machine brightness calibration experiment; Unavailable",
    ),
    (
        "usb.devices",
        "per-device identity/started not retained; per-class counts retained instead",
    ),
    (
        "usb.camera_mic_in_use",
        "active streaming needs ETW; advertised Unavailable",
    ),
    (
        "usb.selective_suspend",
        "USB power-framework query not implemented; advertised Unavailable",
    ),
    (
        "usb.storage_throughput",
        "mass storage appears as PhysicalDisk in the storage collector",
    ),
    (
        "net.proc_bps",
        "per-process network needs ETW (admin); advertised Unavailable",
    ),
    (
        "net.adapter_power",
        "NDIS power state is driver-dependent; advertised Unavailable",
    ),
    (
        "storage.disk.iops",
        "per-tick IOPS not retained; throughput + busyness retained",
    ),
    (
        "storage.disk.queue_latency",
        "queue depth / latency not retained",
    ),
    (
        "storage.nvme_power",
        "NVMe IOCTL needs admin (ACCESS_DENIED); advertised Unavailable",
    ),
    (
        "storage.temperature_c",
        "no SMART instance for this NVMe drive; advertised Unavailable",
    ),
    (
        "storage.proc_io",
        "per-process disk attribution needs ETW; proc.io_bps covers all IO",
    ),
    (
        "proc.list",
        "raw total-process count not retained (top-N retained; thread total retained as proc.total_threads)",
    ),
    (
        "proc.top.mem_mb",
        "per-process memory/io/handles/threads not retained (cpu only)",
    ),
    (
        "proc.top.io_bps",
        "per-process IO rates not retained (cpu only)",
    ),
    (
        "proc.top.handles_threads",
        "per-process handles/threads not retained (cpu only)",
    ),
    (
        "proc.top.energy_impact",
        "Energy Estimation Engine needs admin; advertised Unavailable",
    ),
    (
        "proc.top.wakeups",
        "context-switch attribution needs ETW; advertised Unavailable",
    ),
    (
        "proc.top.net_bps",
        "per-process network needs ETW; advertised Unavailable",
    ),
    (
        "os.power_requests",
        "powercfg /requests needs elevation; advertised Unavailable",
    ),
    (
        "os.energy_saver",
        "no Win32 query API; advertised Unavailable",
    ),
    (
        "os.session_events",
        "lock/unlock + sleep transitions need ETW; advertised Unavailable",
    ),
];

// ---------------------------------------------------------------------------
// Realistic round-trip: full supported field set, not just retained fields.
// ---------------------------------------------------------------------------

fn realistic_jsonl() -> String {
    let mut lines = vec![
        "{\"type\":\"session_header\",\"wall_ms\":1000,\"tool\":\"t\",\"version\":\"0.1.0\",\"format\":\"pf-jsonl-2\",\"interval_ms\":1000}".to_string(),
    ];
    for i in 0..6 {
        let t = i * 1000;
        let w = 1000 + t;
        let hi = i >= 3;
        let ac = if hi { "true" } else { "false" };
        let dw = if hi { -11400 } else { -5700 };
        lines.push(format!(
            "{{\"collector\":\"battery\",\"wall_ms\":{w},\"mono_ms\":{t},\"t_start_ms\":5,\"t_end_ms\":95,\
             \"battery_index\":{{\"v\":0,\"p\":\"measured\"}},\"battery_count\":{{\"v\":1,\"p\":\"measured\"}},\"present\":true,\
             \"batteries\":[{{\"index\":0,\"designed_mwh\":{{\"v\":42000,\"p\":\"measured\"}},\"full_mwh\":{{\"v\":38700,\"p\":\"measured\"}},\
             \"cycle_count\":{{\"v\":123,\"p\":\"measured\"}},\"chemistry\":{{\"v\":\"LION\",\"p\":\"measured\"}},\
             \"device_name\":{{\"v\":\"BAT0\",\"p\":\"measured\"}},\"unique_id\":{{\"v\":\"UID0\",\"p\":\"measured\"}}}}],\
             \"ac\":{{\"v\":{ac},\"p\":\"measured\"}},\"charge_pct\":{{\"v\":55,\"p\":\"measured\"}},\
             \"rate_mw\":{{\"v\":{dw},\"p\":\"measured\"}},\"remaining_mwh\":{{\"v\":29000,\"p\":\"measured\"}},\
             \"full_charge_mwh\":{{\"v\":38700,\"p\":\"measured\"}},\"design_mwh\":{{\"v\":42000,\"p\":\"measured\"}},\
             \"discharge_w\":{{\"v\":{},\"p\":\"derived\"}},\"charge_w\":{{\"v\":null,\"p\":\"unavailable\",\"k\":2,\"q\":4,\"reason\":\"not charging\"}},\
             \"health\":{{\"v\":0.92,\"p\":\"derived\"}},\"temperature_raw\":{{\"v\":2982,\"p\":\"measured\"}},\
             \"chemistry\":{{\"v\":\"LION\",\"p\":\"measured\"}},\"technology\":{{\"v\":1,\"p\":\"measured\"}},\
             \"capabilities\":{{\"v\":\"0x1\",\"p\":\"measured\"}},\"cycle_count\":{{\"v\":123,\"p\":\"measured\"}},\
             \"device_name\":{{\"v\":\"BAT0\",\"p\":\"measured\"}},\"mfg_name\":{{\"v\":\"ASUS\",\"p\":\"measured\"}},\
             \"unique_id\":{{\"v\":\"UID0\",\"p\":\"measured\"}},\"mfg_date\":{{\"v\":\"2024-01-02\",\"p\":\"measured\"}}}}",
            if hi { 11.4 } else { 5.7 }
        ));
        let (u, c3, pkg, pp) = if hi {
            (18.0, 40.0, 4.2, 4.3)
        } else {
            (4.0, 92.0, 1.1, 1.2)
        };
        lines.push(format!(
            "{{\"collector\":\"cpu\",\"wall_ms\":{w},\"mono_ms\":{t},\"t_start_ms\":5,\"t_end_ms\":95,\
             \"totals\":{{\"utility\":{{\"v\":{u},\"p\":\"measured\",\"q\":0}},\"c3_pct\":{{\"v\":{c3},\"p\":\"measured\",\"q\":0}},\
             \"idle_breaks\":{{\"v\":1000,\"p\":\"measured\",\"q\":0}},\"ctx_switches\":{{\"v\":5000,\"p\":\"measured\",\"q\":0}},\
             \"idle_pct\":{{\"v\":80,\"p\":\"measured\",\"q\":0}},\"c1_pct\":{{\"v\":5,\"p\":\"measured\",\"q\":0}},\
             \"c2_pct\":{{\"v\":4,\"p\":\"measured\",\"q\":0}},\"c1_trans\":{{\"v\":300,\"p\":\"measured\",\"q\":0}},\
             \"c2_trans\":{{\"v\":40,\"p\":\"measured\",\"q\":0}},\"c3_trans\":{{\"v\":3,\"p\":\"measured\",\"q\":0}},\
             \"interrupts\":{{\"v\":900,\"p\":\"measured\",\"q\":0}},\"dpc_rate\":{{\"v\":120,\"p\":\"measured\",\"q\":0}},\
             \"dpc_pct\":{{\"v\":0.5,\"p\":\"measured\",\"q\":0}},\"interrupt_pct\":{{\"v\":0.4,\"p\":\"measured\",\"q\":0}},\
             \"priv_utility\":{{\"v\":1.1,\"p\":\"measured\",\"q\":0}},\"performance\":{{\"v\":115,\"p\":\"measured\",\"q\":0}},\
             \"of_max_freq\":{{\"v\":90,\"p\":\"measured\",\"q\":0}},\"freq_mhz\":{{\"v\":2300,\"p\":\"measured\",\"q\":0}},\
             \"user_time\":{{\"v\":2.0,\"p\":\"measured\",\"q\":0}},\"processor_time\":{{\"v\":3.0,\"p\":\"measured\",\"q\":0}},\
             \"perf_limit_flags\":{{\"v\":0,\"p\":\"measured\",\"q\":0}},\"perf_limit_pct\":{{\"v\":100,\"p\":\"measured\",\"q\":0}},\
             \"proc_queue\":{{\"v\":0,\"p\":\"measured\",\"q\":0}}}},\
             \"cores\":[{{\"inst\":\"0,0\",\"utility\":{{\"v\":4,\"p\":\"measured\"}},\"performance\":{{\"v\":110,\"p\":\"measured\"}},\
             \"freq_mhz\":{{\"v\":2300,\"p\":\"measured\"}},\"parking\":{{\"v\":0,\"p\":\"measured\"}}}}],\
             \"energy\":{{\"instance\":\"rapl_package0_pkg\",\"pkg_power_w\":{{\"v\":{pp},\"p\":\"estimated\"}},\
             \"pkg_derived_w\":{{\"v\":{pkg},\"p\":\"estimated\"}}}}}}"
        ));
        let gu = if hi { 6.0 } else { 0.0 };
        let stale = if hi { "true" } else { "false" };
        lines.push(format!(
            "{{\"collector\":\"gpu\",\"wall_ms\":{w},\"mono_ms\":{t},\"t_start_ms\":5,\"t_end_ms\":95,\"stale\":{stale},\
             \"adapters\":[{{\"index\":0,\"name\":\"dGPU\",\"vendor\":\"NVIDIA\",\"vendor_id\":\"0x10DE\",\"device_id\":\"0x1234\",\"dedicated_mb\":4096,\"luid\":\"0x0/0x1\",\"discrete\":true,\
             \"total_util_pct\":{{\"v\":{gu},\"p\":\"derived\",\"q\":0}},\"util_3d\":{{\"v\":{gu},\"p\":\"derived\",\"q\":0}},\
             \"util_compute\":{{\"v\":0.2,\"p\":\"derived\",\"q\":0}},\"util_decode\":{{\"v\":0.1,\"p\":\"derived\",\"q\":0}},\
             \"util_codec\":{{\"v\":0.0,\"p\":\"derived\",\"q\":0}},\"util_copy\":{{\"v\":0.0,\"p\":\"derived\",\"q\":0}},\
             \"util_other\":{{\"v\":0.0,\"p\":\"derived\",\"q\":0}},\"engines_active\":{{\"v\":1,\"p\":\"derived\",\"q\":0}},\
             \"engines_seen\":{{\"v\":3,\"p\":\"derived\",\"q\":0}},\"mem_dedicated_mb\":{{\"v\":300,\"p\":\"derived\",\"q\":0}},\
             \"mem_shared_mb\":{{\"v\":50,\"p\":\"derived\",\"q\":0}},\"top_pids\":[{{\"pid\":200,\"util_pct\":{{\"v\":1.2,\"p\":\"derived\",\"q\":0}}}}],\
             \"awake\":{{\"active\":{},\"confidence\":\"high\",\"evidence\":\"engine activity\"}}}}]}}",
            if hi { "true" } else { "false" }
        ));
        let procs = if hi {
            "[{\"pid\":100,\"ppid\":1,\"name\":\"idle.exe\",\"threads\":8,\"accessible\":true,\
              \"start_unix_ms\":null,\"exit_unix_ms\":4321,\"cpu_pct\":{\"v\":0.1,\"p\":\"derived\",\"q\":0},\
              \"cpu_100ns_total\":{\"v\":1000,\"p\":\"measured\",\"q\":0},\"ws_mb\":{\"v\":10.0,\"p\":\"measured\",\"q\":0},\
              \"priv_mb\":{\"v\":8.0,\"p\":\"measured\",\"q\":0},\"io_r_bps\":{\"v\":1.0,\"p\":\"derived\",\"q\":0},\
              \"io_w_bps\":{\"v\":2.0,\"p\":\"derived\",\"q\":0},\"io_r_total\":{\"v\":100,\"p\":\"measured\",\"q\":0},\
              \"io_w_total\":{\"v\":200,\"p\":\"measured\",\"q\":0},\"handles\":100},\
             {\"pid\":200,\"ppid\":1,\"name\":\"game.exe\",\"threads\":20,\"accessible\":true,\
              \"start_unix_ms\":2000,\"exit_unix_ms\":null,\"cpu_pct\":{\"v\":12.0,\"p\":\"derived\",\"q\":0},\
              \"cpu_100ns_total\":{\"v\":2000,\"p\":\"measured\",\"q\":0},\"ws_mb\":{\"v\":300.0,\"p\":\"measured\",\"q\":0},\
              \"priv_mb\":{\"v\":250.0,\"p\":\"measured\",\"q\":0},\"io_r_bps\":{\"v\":3.0,\"p\":\"derived\",\"q\":0},\
              \"io_w_bps\":{\"v\":4.0,\"p\":\"derived\",\"q\":0},\"io_r_total\":{\"v\":300,\"p\":\"measured\",\"q\":0},\
              \"io_w_total\":{\"v\":400,\"p\":\"measured\",\"q\":0},\"handles\":150}]"
        } else {
            "[{\"pid\":100,\"ppid\":1,\"name\":\"idle.exe\",\"threads\":8,\"accessible\":true,\
              \"start_unix_ms\":null,\"exit_unix_ms\":null,\"cpu_pct\":{\"v\":0.1,\"p\":\"derived\",\"q\":0},\
              \"cpu_100ns_total\":{\"v\":1000,\"p\":\"measured\",\"q\":0},\"ws_mb\":{\"v\":10.0,\"p\":\"measured\",\"q\":0},\
              \"priv_mb\":{\"v\":8.0,\"p\":\"measured\",\"q\":0},\"io_r_bps\":{\"v\":1.0,\"p\":\"derived\",\"q\":0},\
              \"io_w_bps\":{\"v\":2.0,\"p\":\"derived\",\"q\":0},\"io_r_total\":{\"v\":100,\"p\":\"measured\",\"q\":0},\
              \"io_w_total\":{\"v\":200,\"p\":\"measured\",\"q\":0},\"handles\":100}]"
        };
        lines.push(format!(
            "{{\"collector\":\"proc\",\"wall_ms\":{w},\"mono_ms\":{t},\"total_procs\":150,\
             \"total_threads\":{},\"inaccessible\":{},\"truncated\":{},\"top\":{procs}}}",
            if hi { 401 } else { 400 },
            if hi { 2 } else { 0 },
            if hi { "true" } else { "false" },
        ));
        lines.push(format!(
            "{{\"collector\":\"display\",\"wall_ms\":{w},\"mono_ms\":{t},\"t_start_ms\":5,\"t_end_ms\":95,\
             \"displays\":[{{\"name\":\"PANEL\",\"primary\":true,\"width\":1920,\"height\":1200,\"freq_hz\":60,\"bpp\":32,\
             \"brightness_pct\":{{\"v\":40,\"p\":\"measured\",\"q\":0}}}},\
             {{\"name\":\"EXT\",\"primary\":false,\"width\":2560,\"height\":1440,\"freq_hz\":144,\"bpp\":32,\
             \"brightness_pct\":{{\"v\":null,\"p\":\"unavailable\",\"k\":0,\"q\":4,\"reason\":\"no WMI sensor matched\"}}}}],\
             \"unmatched_sensors\":[{{\"instance\":\"DISPLAY\\\\ZZZ\",\"brightness\":20}}],\
             \"hdr_targets\":[{{\"adapter\":\"1/0\",\"target\":0,\"supported\":true,\"enabled\":false}}],\
             \"hdr_error\":null,\"changes\":[]}}"
        ));
        lines.push(format!(
            "{{\"collector\":\"net\",\"wall_ms\":{w},\"mono_ms\":{t},\"t_start_ms\":5,\"t_end_ms\":95,\
             \"adapters\":[{{\"alias\":\"WiFi\",\"descr\":\"MediaTek\",\"guid\":\"00\",\"class\":\"wifi\",\"iftype\":\"wifi\",\
             \"oper\":1,\"oper_name\":\"Up\",\"media\":\"Connected\",\"admin_up\":true,\"tx_mbps\":100.0,\"rx_mbps\":100.0,\
             \"in_octets\":1000,\"out_octets\":2000}}],\
             \"throughput\":[{{\"instance\":\"WiFi\",\"rx_bps\":{{\"v\":100,\"p\":\"measured\",\"q\":0}},\"tx_bps\":{{\"v\":50,\"p\":\"measured\",\"q\":0}}}}],\
             \"wifi\":[{{\"descr\":\"MediaTek\",\"state\":\"connected\",\"ssid\":\"Home\",\"signal_pct\":{{\"v\":80,\"p\":\"measured\"}}}}],\
             \"total_rx_bps\":{{\"v\":100,\"p\":\"derived\",\"q\":0}},\"total_tx_bps\":{{\"v\":50,\"p\":\"derived\",\"q\":0}},\
             \"topology_stale\":false,\"topology_error\":null,\"changes\":[]}}"
         ));
        lines.push(format!(
             "{{\"collector\":\"storage\",\"wall_ms\":{w},\"mono_ms\":{t},\"t_start_ms\":5,\"t_end_ms\":95,\
              \"disks\":[{{\"instance\":\"_Total\",\"total\":true,\"disk_time_pct\":{{\"v\":2.0,\"p\":\"measured\",\"q\":0}},\
              \"idle_pct\":{{\"v\":98.0,\"p\":\"measured\",\"q\":0}},\"avg_queue\":{{\"v\":0.1,\"p\":\"measured\",\"q\":0}},\
              \"cur_queue\":{{\"v\":0.0,\"p\":\"measured\",\"q\":0}},\"reads_s\":{{\"v\":1.0,\"p\":\"measured\",\"q\":0}},\
              \"writes_s\":{{\"v\":2.0,\"p\":\"measured\",\"q\":0}},\"read_bps\":{{\"v\":1000,\"p\":\"measured\",\"q\":0}},\
              \"write_bps\":{{\"v\":2000,\"p\":\"measured\",\"q\":0}},\"read_lat_ms\":{{\"v\":0.5,\"p\":\"derived\",\"q\":0}},\
              \"write_lat_ms\":{{\"v\":0.7,\"p\":\"derived\",\"q\":0}}}}]}}"
         ));
        lines.push(format!(
             "{{\"collector\":\"usb\",\"wall_ms\":{w},\"mono_ms\":{t},\"t_start_ms\":5,\"t_end_ms\":95,\
             \"groups\":[{{\"class\":\"usb\",\"devices\":[{{\"instance\":\"USB\\\\A\",\"name\":\"Stick\",\"started\":true}}],\
             \"count\":{{\"v\":1,\"p\":\"derived\",\"q\":0}}}},\
             {{\"class\":\"gaming\",\"devices\":[{{\"instance\":\"USB\\\\G\",\"name\":\"Pad\",\"started\":true}}],\
             \"count\":{{\"v\":1,\"p\":\"measured\",\"q\":0}}}}],\
             \"power_devices\":[\"SPPSERVICE4\"],\"power_error\":null,\"changes\":[]}}"
        ));
        lines.push(format!(
            "{{\"collector\":\"self\",\"wall_ms\":{w},\"mono_ms\":{t},\"t_start_ms\":5,\"t_end_ms\":95,\
             \"cpu_pct\":{{\"v\":1.5,\"p\":\"measured\",\"q\":0}},\"ws_mb\":{{\"v\":40,\"p\":\"measured\",\"q\":0}},\
             \"priv_mb\":{{\"v\":30,\"p\":\"measured\",\"q\":0}},\"io_read_b\":{{\"v\":4096,\"p\":\"derived\",\"q\":0}},\
             \"io_write_b\":{{\"v\":8192,\"p\":\"derived\",\"q\":0}},\"threads\":{{\"v\":9,\"p\":\"measured\",\"q\":0}},\
             \"ctx_switches_s\":{{\"v\":5000,\"p\":\"estimated\",\"q\":0}}}}"
        ));
    }
    lines.push(
        "{\"collector\":\"os_power\",\"wall_ms\":1000,\"mono_ms\":0,\"t_start_ms\":1,\"t_end_ms\":9,\
         \"active_scheme\":{\"v\":\"8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c\",\"p\":\"measured\"},\"scheme_name\":\"High performance\",\
         \"timer_resolution_ms\":{\"v\":1.0,\"p\":\"measured\"},\"policy_snapshot_wall_ms\":1000,\
         \"cpu_min_pct\":{\"ac\":{\"v\":0,\"p\":\"measured\"},\"dc\":{\"v\":5,\"p\":\"measured\"}},\
         \"cpu_max_pct\":{\"ac\":{\"v\":100,\"p\":\"measured\"},\"dc\":{\"v\":100,\"p\":\"measured\"}},\
         \"epp\":{\"ac\":{\"v\":null,\"p\":\"unavailable\",\"k\":0,\"q\":4,\"reason\":\"setting not present in active scheme\"},\"dc\":{\"v\":null,\"p\":\"unavailable\",\"k\":0,\"q\":4,\"reason\":\"setting not present in active scheme\"}},\
         \"display_timeout_s\":{\"ac\":{\"v\":300,\"p\":\"measured\"},\"dc\":{\"v\":60,\"p\":\"measured\"}},\
         \"brightness_pct\":{\"ac\":{\"v\":40,\"p\":\"measured\"},\"dc\":{\"v\":40,\"p\":\"measured\"}},\
         \"sleep_timeout_s\":{\"ac\":{\"v\":0,\"p\":\"measured\"},\"dc\":{\"v\":900,\"p\":\"measured\"}},\
         \"hibernate_timeout_s\":{\"ac\":{\"v\":0,\"p\":\"measured\"},\"dc\":{\"v\":1800,\"p\":\"measured\"}},\
         \"low_batt_pct\":{\"ac\":{\"v\":20,\"p\":\"measured\"},\"dc\":{\"v\":20,\"p\":\"measured\"}},\
         \"crit_batt_pct\":{\"ac\":{\"v\":5,\"p\":\"measured\"},\"dc\":{\"v\":5,\"p\":\"measured\"}}}"
            .to_string(),
    );
    lines.push(
        "{\"type\":\"event\",\"wall_ms\":1001,\"mono_ms\":1,\"kind\":\"marker\",\"detail\":\"x\"}"
            .to_string(),
    );
    lines.push(
        "{\"type\":\"session_footer\",\"wall_ms\":7000,\"summary\":{\"discharge_wh\":0.05}}"
            .to_string(),
    );
    lines.join("\n")
}

#[test]
fn full_field_set_round_trips_through_archive() {
    let text = realistic_jsonl();
    let s = load_session("full", &text).expect("jsonl loads");

    // Retention checks before archiving.
    assert_eq!(s.battery_extra.len(), 6);
    assert_eq!(s.battery_extra[0].ac.val(), Some(0.0));
    assert_eq!(s.battery_extra[0].rate_mw.val(), Some(-5700.0));
    assert_eq!(
        s.battery_extra[0].health.provenance(),
        pf_core::telemetry::Provenance::Derived
    );
    assert_eq!(s.battery_extra[0].temperature_raw.val(), Some(2982.0));
    assert_eq!(
        (s.battery_extra[0].t_start_ms, s.battery_extra[0].t_end_ms),
        (Some(5), Some(95))
    );
    // Timing is retained for every series the collector reported it for,
    // and absence stays None (the proc line above omits t_start/t_end).
    assert_eq!(
        (s.gpu[0].t_start_ms, s.gpu[0].t_end_ms),
        (Some(5), Some(95))
    );
    assert_eq!(
        (s.display[0].t_start_ms, s.display[0].t_end_ms),
        (Some(5), Some(95))
    );
    assert_eq!(
        (s.net[0].t_start_ms, s.net[0].t_end_ms),
        (Some(5), Some(95))
    );
    assert_eq!(
        (s.usb[0].t_start_ms, s.usb[0].t_end_ms),
        (Some(5), Some(95))
    );
    assert_eq!(
        (s.selfmon[0].t_start_ms, s.selfmon[0].t_end_ms),
        (Some(5), Some(95))
    );
    assert_eq!((s.procs[0].t_start_ms, s.procs[0].t_end_ms), (None, None));
    assert_eq!(s.cpu[0].pkg_power_w.val(), Some(1.2));
    assert_eq!(s.cpu[0].ctx_switches.val(), Some(5000.0));
    assert_eq!(s.cpu[0].cores.len(), 1);
    assert_eq!(s.cpu[0].cores[0].instance, "0,0");
    assert_eq!(s.cpu[0].totals.len(), 19);
    assert!(s.cpu[0].totals.iter().any(|(k, _)| k == "proc_queue"));
    assert!(!s.gpu[0].stale && s.gpu[3].stale);
    assert_eq!(s.gpu[0].adapters[0].util_3d.val(), Some(0.0));
    assert_eq!(s.gpu[0].adapters[0].mem_shared_mb.val(), Some(50.0));
    assert!(s.gpu[3].adapters[0].awake.as_ref().unwrap().active);
    assert_eq!(s.gpu[0].adapters[0].top_pids[0].0, 200);
    assert_eq!(s.display[0].displays.len(), 2);
    assert_eq!(s.display[0].displays[1].name, "EXT");
    assert!(s.display[0].displays[1].brightness.val().is_none());
    assert_eq!(s.display[0].hdr_targets[0].adapter, "1/0");
    assert_eq!(s.net[0].adapters[0].alias, "WiFi");
    assert_eq!(s.net[0].wifi[0].ssid, "Home");
    assert_eq!(s.net[0].throughput[0].0, "WiFi");
    assert_eq!(s.usb[0].classes.len(), 2);
    assert_eq!(s.usb[0].classes[1].class, "gaming");
    assert_eq!(
        s.usb[0].classes[1].evidence.provenance(),
        pf_core::telemetry::Provenance::Measured
    );
    assert_eq!(s.usb[0].power_devices, vec!["SPPSERVICE4".to_string()]);
    assert_eq!(s.selfmon[0].priv_mb.val(), Some(30.0));
    assert_eq!(s.selfmon[0].io_read_b.val(), Some(4096.0));
    assert_eq!(s.selfmon[0].ctx_switches.val(), Some(5000.0));
    // AC vs DC become distinguishable.
    assert_eq!(s.policy.cpu_min_ac.val(), Some(0.0));
    assert_eq!(s.policy.cpu_min_dc.val(), Some(5.0));
    assert_eq!(s.policy.hibernate_timeout_dc.val(), Some(1800.0));
    assert_eq!(
        s.policy.scheme_guid.as_deref(),
        Some("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c")
    );
    assert_eq!(s.policy.policy_snapshot_wall_ms, Some(1000));
    assert_eq!(s.procs[3].top[0].exit_unix_ms, Some(4321));
    // Process coverage qualifiers are retained from the emitted paths.
    assert_eq!(
        (
            s.procs[0].total_threads,
            s.procs[0].inaccessible,
            s.procs[0].truncated
        ),
        (Some(400), Some(0), Some(false))
    );
    assert_eq!(
        (
            s.procs[3].total_threads,
            s.procs[3].inaccessible,
            s.procs[3].truncated
        ),
        (Some(401), Some(2), Some(true))
    );

    // Archive -> reload.
    let a = migrate("full", "full.jsonl", &s);
    assert!(a.manifest.contains("\"net_adapters\""));
    assert!(a.manifest.contains("\"wifi_identity\""));
    assert!(a.manifest.contains("\"scheme_guid\""));
    let back = load_archive_v2("full", &a.manifest, &a.samples, &a.events).unwrap();

    // Analytical equivalence for the retained/archived set.
    assert_eq!(back.battery_extra.len(), 6);
    let teq = |x: &pf_core::telemetry::Telemetry<f64>, y: &pf_core::telemetry::Telemetry<f64>| {
        assert_eq!(x.val(), y.val());
        assert_eq!(x.provenance(), y.provenance());
        assert_eq!(x.unavail_kind, y.unavail_kind);
        assert_eq!(x.quality, y.quality);
        assert_eq!(x.source, y.source);
        if x.val().is_none() {
            assert_eq!(x.value.reason(), y.value.reason());
        }
    };
    teq(&s.battery_extra[0].ac, &back.battery_extra[0].ac);
    teq(&s.battery_extra[0].rate_mw, &back.battery_extra[0].rate_mw);
    teq(&s.battery_extra[0].health, &back.battery_extra[0].health);
    assert_eq!(
        (s.battery_extra[0].t_start_ms, s.battery_extra[0].t_end_ms),
        (
            back.battery_extra[0].t_start_ms,
            back.battery_extra[0].t_end_ms
        )
    );
    // Present timing round-trips for every series, and an absent window
    // (proc emits none here) reloads as None, never as 0.
    for (x, y) in [
        (s.gpu[0].t_start_ms, back.gpu[0].t_start_ms),
        (s.gpu[0].t_end_ms, back.gpu[0].t_end_ms),
        (s.display[0].t_start_ms, back.display[0].t_start_ms),
        (s.net[0].t_start_ms, back.net[0].t_start_ms),
        (s.usb[0].t_start_ms, back.usb[0].t_start_ms),
        (s.selfmon[0].t_start_ms, back.selfmon[0].t_start_ms),
        (s.procs[0].t_start_ms, back.procs[0].t_start_ms),
    ] {
        assert_eq!(x, y);
    }
    assert_eq!(
        (back.procs[0].t_start_ms, back.procs[0].t_end_ms),
        (None, None)
    );
    // Incomplete process observation survives the archive: a truncated /
    // inaccessible tick must not reload as a complete one.
    assert_eq!(
        (
            back.procs[0].total_threads,
            back.procs[0].inaccessible,
            back.procs[0].truncated
        ),
        (Some(400), Some(0), Some(false))
    );
    assert_eq!(
        (
            back.procs[3].total_threads,
            back.procs[3].inaccessible,
            back.procs[3].truncated
        ),
        (Some(401), Some(2), Some(true))
    );
    assert_eq!(back.process_coverage_incomplete(), Some(true));
    teq(&s.cpu[0].pkg_power_w, &back.cpu[0].pkg_power_w);
    teq(&s.cpu[0].ctx_switches, &back.cpu[0].ctx_switches);
    assert_eq!(s.cpu[0].cores.len(), back.cpu[0].cores.len());
    assert_eq!(
        s.cpu[0].cores[0].freq_mhz.val(),
        back.cpu[0].cores[0].freq_mhz.val()
    );
    assert_eq!(s.cpu[0].totals.len(), back.cpu[0].totals.len());
    for ((k1, v1), (k2, v2)) in s.cpu[0].totals.iter().zip(&back.cpu[0].totals) {
        assert_eq!(k1, k2);
        teq(v1, v2);
    }
    assert!(!back.gpu[0].stale && back.gpu[3].stale);
    teq(
        &s.gpu[0].adapters[0].util_3d,
        &back.gpu[0].adapters[0].util_3d,
    );
    teq(
        &s.gpu[0].adapters[0].mem_shared_mb,
        &back.gpu[0].adapters[0].mem_shared_mb,
    );
    assert_eq!(
        s.gpu[3].adapters[0].awake.as_ref().unwrap().active,
        back.gpu[3].adapters[0].awake.as_ref().unwrap().active
    );
    assert_eq!(
        s.gpu[0].adapters[0].top_pids.len(),
        back.gpu[0].adapters[0].top_pids.len()
    );
    assert_eq!(back.display[0].displays.len(), 2);
    assert_eq!(back.display[0].displays[1].name, "EXT");
    assert_eq!(
        back.display[0].displays[1].brightness.provenance(),
        pf_core::telemetry::Provenance::Unavailable
    );
    assert!(!back.display[0].hdr_targets[0].enabled);
    assert_eq!(back.net[0].adapters[0].descr, "MediaTek");
    assert_eq!(back.net[0].wifi[0].ssid, "Home");
    teq(
        &s.net[0].wifi[0].signal_pct,
        &back.net[0].wifi[0].signal_pct,
    );
    assert_eq!(back.usb[0].classes[1].class, "gaming");
    assert_eq!(
        back.usb[0].classes[1].evidence.provenance(),
        pf_core::telemetry::Provenance::Measured
    );
    assert_eq!(back.usb[0].power_devices, vec!["SPPSERVICE4".to_string()]);
    teq(&s.selfmon[0].io_read_b, &back.selfmon[0].io_read_b);
    teq(&s.selfmon[0].ctx_switches, &back.selfmon[0].ctx_switches);
    assert_eq!(back.policy.cpu_min_ac.val(), Some(0.0));
    assert_eq!(back.policy.cpu_min_dc.val(), Some(5.0));
    assert_eq!(back.policy.hibernate_timeout_dc.val(), Some(1800.0));
    assert_eq!(
        back.policy.scheme_guid.as_deref(),
        Some("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c")
    );
    assert_eq!(back.procs[3].top[0].exit_unix_ms, Some(4321));
    assert_eq!(
        back.battery_meta.batteries[0].temperature_raw,
        s.battery_meta.batteries[0].temperature_raw
    );
}
