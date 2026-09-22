//! Regression tests for sample-timing evidence and suspend/gap detection.
//!
//! Two invariants are locked in here:
//!   1. A segment boundary is forced whenever EITHER clock (monotonic or
//!      wall) shows a gap, or the two diverge (suspend/resume, clock step).
//!      Energy is never integrated across it and the unobserved interval is
//!      reported, not silently bridged.
//!   2. Per-sample `t_start_ms`/`t_end_ms` survive JSONL -> SessionData ->
//!      archive v2 -> reload, and a missing window stays `None` (never 0),
//!      while the value provenance/absence around it is preserved.

use pf_core::archive::{load_archive_v2, migrate};
use pf_core::session::load_session;
use pf_core::stats::{
    DiscontinuityKind, discontinuities, integrate_segmented, integrate_segmented_clocks,
};
use pf_core::telemetry::Provenance;

// ---------------------------------------------------------------------------
// Both-clock segment boundaries
// ---------------------------------------------------------------------------

#[test]
fn monotonic_gap_forces_boundary_reports_unobserved_no_phantom_wh() {
    // Two 1 s windows around a 300 s monotonic gap; both clocks gap together.
    let t = vec![0.0, 1.0, 2.0, 302.0];
    let wall = vec![1000.0, 1001.0, 1002.0, 1302.0];
    let w = vec![Some(6.0), Some(6.0), Some(6.0), Some(6.0)];

    let d = discontinuities(&t, &wall, 5.0, 2.0);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].kind, DiscontinuityKind::SamplingGap);

    let seg = integrate_segmented_clocks(&t, &wall, &w, 5.0, 2.0);
    // Only the two observed 1 s windows contribute; the 300 s gap does not.
    assert!((seg.energy_wh - 6.0 * 2.0 / 3600.0).abs() < 1e-12);
    assert!((seg.covered_secs - 2.0).abs() < 1e-9);
    assert_eq!(seg.discontinuities, 1);
    assert!((seg.unobserved_secs - 300.0).abs() < 1e-9);
}

#[test]
fn wall_jump_forward_while_mono_continuous_forces_boundary() {
    // Mono ticks 1 s; wall jumps 600 s: NTP step, or resume with a sleeping
    // monotonic clock. Mono alone would look continuous.
    let t = vec![0.0, 1.0, 2.0, 3.0];
    let wall = vec![1000.0, 1001.0, 1601.0, 1602.0];
    let w = vec![Some(6.0), Some(6.0), Some(6.0), Some(6.0)];

    let d = discontinuities(&t, &wall, 5.0, 2.0);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].kind, DiscontinuityKind::WallAheadOfMono);

    let seg = integrate_segmented_clocks(&t, &wall, &w, 5.0, 2.0);
    assert_eq!(seg.discontinuities, 1);
    assert!((seg.unobserved_secs - 600.0).abs() < 1e-9);
    // The 600 s wall jump is never bridged as if it were a mono interval.
    assert!(seg.energy_wh <= 2.0 * 6.0 / 3600.0 + 1e-12);
}

#[test]
fn mono_jump_while_wall_continuous_forces_boundary() {
    // Wall stays on a 1 s cadence while mono jumps 600 s.
    let t = vec![0.0, 1.0, 602.0, 603.0];
    let wall = vec![1000.0, 1001.0, 1002.0, 1003.0];
    let w = vec![Some(6.0), Some(6.0), Some(6.0), Some(6.0)];

    let d = discontinuities(&t, &wall, 5.0, 2.0);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].kind, DiscontinuityKind::MonoAheadOfWall);
    assert_eq!(
        integrate_segmented_clocks(&t, &wall, &w, 5.0, 2.0).discontinuities,
        1
    );
}

#[test]
fn continuous_series_is_one_segment_matching_existing_behavior() {
    let t = vec![0.0, 1.0, 2.0, 3.0, 4.0];
    let wall = vec![1000.0, 1001.0, 1002.0, 1003.0, 1004.0];
    let w = vec![Some(6.0), Some(5.0), Some(7.0), Some(6.0), Some(6.0)];

    assert!(discontinuities(&t, &wall, 5.0, 2.0).is_empty());
    let both = integrate_segmented_clocks(&t, &wall, &w, 5.0, 2.0);
    assert_eq!(both, integrate_segmented(&t, &w, 5.0));
    assert_eq!(both.discontinuities, 0);
}

// ---------------------------------------------------------------------------
// t_start/t_end persistence
// ---------------------------------------------------------------------------

fn session_jsonl() -> String {
    // proc deliberately omits t_start_ms/t_end_ms to prove absence is kept.
    [
        "{\"type\":\"session_header\",\"wall_ms\":1000}",
        "{\"collector\":\"battery\",\"wall_ms\":1000,\"mono_ms\":0,\"t_start_ms\":100,\"t_end_ms\":200,\
         \"discharge_w\":{\"v\":6.0,\"p\":\"measured\"},\
         \"charge_w\":{\"v\":null,\"p\":\"unavailable\",\"k\":2,\"q\":4,\"reason\":\"not charging\"}}",
        "{\"collector\":\"cpu\",\"wall_ms\":1000,\"mono_ms\":0,\"t_start_ms\":101,\"t_end_ms\":201,\
         \"totals\":{\"utility\":{\"v\":5.0,\"p\":\"measured\"}}}",
        "{\"collector\":\"gpu\",\"wall_ms\":1000,\"mono_ms\":0,\"t_start_ms\":102,\"t_end_ms\":202,\
         \"adapters\":[{\"name\":\"AMD\",\"total_util_pct\":{\"v\":1.0,\"p\":\"derived\"}}]}",
        "{\"collector\":\"proc\",\"wall_ms\":1000,\"mono_ms\":0,\
         \"top\":[{\"pid\":1,\"name\":\"a.exe\",\"cpu_pct\":{\"v\":0.1,\"p\":\"derived\"}}]}",
        "{\"collector\":\"display\",\"wall_ms\":1000,\"mono_ms\":0,\"t_start_ms\":103,\"t_end_ms\":203,\
         \"displays\":[{\"brightness_pct\":{\"v\":40,\"p\":\"measured\"}}]}",
        "{\"collector\":\"net\",\"wall_ms\":1000,\"mono_ms\":0,\"t_start_ms\":104,\"t_end_ms\":204,\
         \"total_rx_bps\":{\"v\":1.0,\"p\":\"measured\"},\"total_tx_bps\":{\"v\":2.0,\"p\":\"measured\"}}",
        "{\"collector\":\"storage\",\"wall_ms\":1000,\"mono_ms\":0,\"t_start_ms\":105,\"t_end_ms\":205,\
         \"disks\":[{\"instance\":\"_Total\",\"total\":true,\"disk_time_pct\":{\"v\":3.0,\"p\":\"measured\"}}]}",
        "{\"collector\":\"usb\",\"wall_ms\":1000,\"mono_ms\":0,\"t_start_ms\":106,\"t_end_ms\":206,\
         \"groups\":[{\"class\":\"usb\",\"devices\":[{\"instance\":\"A\"}]}]}",
        "{\"collector\":\"self\",\"wall_ms\":1000,\"mono_ms\":0,\"t_start_ms\":107,\"t_end_ms\":207,\
         \"ws_mb\":{\"v\":1.0,\"p\":\"measured\"}}",
    ]
    .join("\n")
}

#[test]
fn timing_survives_jsonl_session_archive_reload() {
    let s = load_session("t", &session_jsonl()).unwrap();

    // JSONL -> SessionData: present windows retained, absent stays None.
    assert_eq!(
        (s.battery_extra[0].t_start_ms, s.battery_extra[0].t_end_ms),
        (Some(100), Some(200))
    );
    assert_eq!(
        (s.cpu[0].t_start_ms, s.cpu[0].t_end_ms),
        (Some(101), Some(201))
    );
    assert_eq!(
        (s.gpu[0].t_start_ms, s.gpu[0].t_end_ms),
        (Some(102), Some(202))
    );
    assert_eq!(
        (s.display[0].t_start_ms, s.display[0].t_end_ms),
        (Some(103), Some(203))
    );
    assert_eq!(
        (s.net[0].t_start_ms, s.net[0].t_end_ms),
        (Some(104), Some(204))
    );
    assert_eq!(
        (s.storage[0].t_start_ms, s.storage[0].t_end_ms),
        (Some(105), Some(205))
    );
    assert_eq!(
        (s.usb[0].t_start_ms, s.usb[0].t_end_ms),
        (Some(106), Some(206))
    );
    assert_eq!(
        (s.selfmon[0].t_start_ms, s.selfmon[0].t_end_ms),
        (Some(107), Some(207))
    );
    assert_eq!((s.procs[0].t_start_ms, s.procs[0].t_end_ms), (None, None));

    // Provenance/absence around the timing are untouched by the load.
    assert_eq!(s.battery[0].discharge.provenance(), Provenance::Measured);
    assert_eq!(s.battery[0].charge.provenance(), Provenance::Unavailable);
    assert_eq!(s.battery[0].charge.value.reason(), Some("not charging"));

    // Archive v2 -> reload: same timing, same absence, same provenance.
    let a = migrate("t", "t.jsonl", &s);
    let back = load_archive_v2("t", &a.manifest, &a.samples, &a.events).unwrap();

    assert_eq!(
        (
            back.battery_extra[0].t_start_ms,
            back.battery_extra[0].t_end_ms
        ),
        (Some(100), Some(200))
    );
    assert_eq!(
        (back.cpu[0].t_start_ms, back.cpu[0].t_end_ms),
        (Some(101), Some(201))
    );
    assert_eq!(
        (back.gpu[0].t_start_ms, back.gpu[0].t_end_ms),
        (Some(102), Some(202))
    );
    assert_eq!(
        (back.display[0].t_start_ms, back.display[0].t_end_ms),
        (Some(103), Some(203))
    );
    assert_eq!(
        (back.net[0].t_start_ms, back.net[0].t_end_ms),
        (Some(104), Some(204))
    );
    assert_eq!(
        (back.storage[0].t_start_ms, back.storage[0].t_end_ms),
        (Some(105), Some(205))
    );
    assert_eq!(
        (back.usb[0].t_start_ms, back.usb[0].t_end_ms),
        (Some(106), Some(206))
    );
    assert_eq!(
        (back.selfmon[0].t_start_ms, back.selfmon[0].t_end_ms),
        (Some(107), Some(207))
    );
    // Absent timing must reload as None, not a fabricated 0.
    assert_eq!(
        (back.procs[0].t_start_ms, back.procs[0].t_end_ms),
        (None, None)
    );

    assert_eq!(back.battery[0].discharge.provenance(), Provenance::Measured);
    assert_eq!(back.battery[0].charge.provenance(), Provenance::Unavailable);
    assert_eq!(back.battery[0].charge.value.reason(), Some("not charging"));
}

#[test]
fn absent_timing_is_not_fabricated_as_zero() {
    // A collector line with no t_start/t_end must expose None, never Some(0).
    let text = "{\"collector\":\"cpu\",\"wall_ms\":5,\"mono_ms\":0,\
                \"totals\":{\"utility\":{\"v\":5.0,\"p\":\"measured\"}}}\n";
    let s = load_session("t", text).unwrap();
    assert_eq!(s.cpu[0].t_start_ms, None);
    assert_eq!(s.cpu[0].t_end_ms, None);
}
