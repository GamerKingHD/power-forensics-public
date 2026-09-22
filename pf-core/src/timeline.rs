//! Pure ASCII power timeline over `SessionData`.
//!
//! Per-second (per-bucket) discharge watts with event markers plus an
//! interval summary. No IO here; `main.rs` loads the session and prints.

use crate::session::SessionData;
use crate::stats;

/// Summary of discharge watts over `[start_s, end_s]`.
#[derive(Debug, Clone, PartialEq)]
pub struct IntervalSummary {
    pub start_s: f64,
    pub end_s: f64,
    pub samples: usize,
    pub median_w: Option<f64>,
    pub min_w: Option<f64>,
    pub max_w: Option<f64>,
    /// Events whose mono time falls inside the interval.
    pub events: usize,
}

/// Summarize discharge watts in `[start_s, end_s]` (mono seconds).
pub fn summarize_interval(s: &SessionData, start_s: f64, end_s: f64) -> IntervalSummary {
    let mut vals: Vec<f64> = s
        .battery
        .iter()
        .filter(|b| b.t >= start_s && b.t <= end_s)
        .filter_map(|b| b.discharge.val())
        .collect();
    vals.sort_by(|a, b| a.total_cmp(b));
    let events = s
        .events
        .iter()
        .filter(|e| {
            let t = e.mono_ms as f64 / 1000.0;
            t >= start_s && t <= end_s
        })
        .count();
    IntervalSummary {
        start_s,
        end_s,
        samples: vals.len(),
        median_w: stats::median(&vals),
        min_w: vals.first().copied(),
        max_w: vals.last().copied(),
        events,
    }
}

/// Render the whole session as `width` columns of `#` bars scaled to the
/// session max, with a `*` event-marker row and a one-line interval summary.
/// `width` is clamped to 8..=120.
pub fn render_ascii(s: &SessionData, width: usize) -> String {
    let width = width.clamp(8, 120);
    let mut o = String::new();
    if s.battery.is_empty() {
        return format!("TIMELINE {}: [no battery samples]\n", s.label);
    }
    let t0 = s.battery.first().map(|b| b.t).unwrap_or(0.0);
    let t1 = s.battery.last().map(|b| b.t).unwrap_or(0.0);
    let span = (t1 - t0).max(1e-9);
    // Bucket means (None when the bucket has no known watts).
    let mut buckets: Vec<Option<f64>> = vec![None; width];
    let mut counts = vec![0usize; width];
    let mut acc = vec![0.0f64; width];
    for b in &s.battery {
        if let Some(w) = b.discharge.val() {
            let i = (((b.t - t0) / span * width as f64).floor() as usize).min(width - 1);
            acc[i] += w;
            counts[i] += 1;
        }
    }
    for i in 0..width {
        if counts[i] > 0 {
            buckets[i] = Some(acc[i] / counts[i] as f64);
        }
    }
    let max_w = buckets
        .iter()
        .flatten()
        .fold(0.0f64, |a, &v| a.max(v))
        .max(1e-9);
    o.push_str(&format!(
        "TIMELINE {}  t={t0:.0}s..{t1:.0}s  max={max_w:.2}W\n",
        s.label
    ));
    let mut bar = String::from("W ");
    for b in &buckets {
        let h = match b {
            Some(w) => (w / max_w * 8.0).round() as usize,
            None => 0,
        };
        bar.push(match h {
            0 => '.',
            1 => '_',
            2 => '-',
            3 => '=',
            4 => '+',
            5 => '*',
            6 => '#',
            7 => '#',
            _ => '@',
        });
    }
    o.push_str(&bar);
    o.push('\n');
    // Event markers: `*` in buckets containing an event, else space.
    let mut marks = vec![' '; width];
    for e in &s.events {
        let t = e.mono_ms as f64 / 1000.0;
        if t >= t0 && t <= t1 {
            let i = (((t - t0) / span * width as f64).floor() as usize).min(width - 1);
            marks[i] = '*';
        }
    }
    o.push_str(&format!("E {}", marks.iter().collect::<String>()));
    o.push('\n');
    let sum = summarize_interval(s, t0, t1);
    let f = |v: Option<f64>| {
        v.map(|x| format!("{x:.2}W"))
            .unwrap_or_else(|| "n/a".to_string())
    };
    o.push_str(&format!(
        "summary: n={} median={} min={} max={} events={}\n",
        sum.samples,
        f(sum.median_w),
        f(sum.min_w),
        f(sum.max_w),
        sum.events
    ));
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{ClockStamp, Telemetry};

    fn stamp(ms: u64) -> ClockStamp {
        ClockStamp {
            wall_millis: ms,
            mono_millis: ms,
        }
    }

    fn session() -> SessionData {
        let mut s = SessionData {
            label: "t".to_string(),
            ..Default::default()
        };
        for i in 0..10 {
            s.battery.push(crate::session::BatteryPoint {
                t: i as f64,
                discharge: Telemetry::measured(5.0 + i as f64, "battery", stamp(i * 1000)),
                ..Default::default()
            });
        }
        s.events.push(crate::session::EventRec {
            wall_ms: 5000,
            mono_ms: 5000,
            kind: "marker".to_string(),
            detail: "x".to_string(),
        });
        s
    }

    #[test]
    fn summary_clips_to_interval() {
        let s = session();
        let full = summarize_interval(&s, 0.0, 9.0);
        assert_eq!(full.samples, 10);
        assert_eq!(full.median_w, Some(9.5));
        assert_eq!(full.min_w, Some(5.0));
        assert_eq!(full.max_w, Some(14.0));
        assert_eq!(full.events, 1);
        let part = summarize_interval(&s, 0.0, 4.0);
        assert_eq!(part.samples, 5);
        assert_eq!(part.events, 0);
        let empty = summarize_interval(&SessionData::default(), 0.0, 1.0);
        assert_eq!(empty.samples, 0);
        assert_eq!(empty.median_w, None);
    }

    #[test]
    fn ascii_has_bars_markers_summary() {
        let t = render_ascii(&session(), 20);
        assert!(t.contains("TIMELINE t"), "{t}");
        assert!(t.contains("summary: n=10"), "{t}");
        assert!(t.contains('*'), "{t}"); // event marker or tall bar
        assert!(t.lines().count() >= 4, "{t}");
        let none = render_ascii(&SessionData::default(), 20);
        assert!(none.contains("no battery samples"), "{none}");
    }
}
