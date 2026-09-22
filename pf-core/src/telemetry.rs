//! Core telemetry primitives.
//!
//! Every value in the system carries provenance: measured, derived,
//! estimated, or unavailable. Nothing is ever silently faked.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How a value came to exist. Stored alongside every field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Measured,
    Derived,
    Estimated,
    Unavailable,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Measured => "measured",
            Provenance::Derived => "derived",
            Provenance::Estimated => "estimated",
            Provenance::Unavailable => "unavailable",
        }
    }
}

/// A single telemetry value with enforced provenance.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading<T> {
    Measured(T),
    Derived(T),
    Estimated(T),
    Unavailable(&'static str),
}

impl<T> Reading<T> {
    pub fn provenance(&self) -> Provenance {
        match self {
            Reading::Measured(_) => Provenance::Measured,
            Reading::Derived(_) => Provenance::Derived,
            Reading::Estimated(_) => Provenance::Estimated,
            Reading::Unavailable(_) => Provenance::Unavailable,
        }
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            Reading::Measured(v) | Reading::Derived(v) | Reading::Estimated(v) => Some(v),
            Reading::Unavailable(_) => None,
        }
    }

    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Reading::Unavailable(r) => Some(r),
            _ => None,
        }
    }
}

/// Dual-clock timestamp. `wall_millis` (UTC, for correlation and display)
/// and `mono_millis` (monotonic ms since program start, for intervals and
/// energy integration). Wall time is never used for durations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClockStamp {
    pub wall_millis: u64,
    pub mono_millis: u64,
}

#[derive(Debug)]
pub struct Clock {
    base: Instant,
}

impl Clock {
    pub fn new() -> Self {
        Clock {
            base: Instant::now(),
        }
    }

    pub fn stamp(&self) -> ClockStamp {
        let wall_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        ClockStamp {
            wall_millis,
            mono_millis: self.base.elapsed().as_millis() as u64,
        }
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    /// Session monotonic base shared with scheduler workers so every
    /// timestamp in the session uses one clock.
    pub fn mono_base(&self) -> Instant {
        self.base
    }
}

/// Static provenance metadata for one telemetry field.
#[derive(Debug, Clone)]
pub struct FieldMeta {
    pub name: &'static str,
    pub unit: &'static str,
    pub source: &'static str,
    pub provenance: Provenance,
    pub interval_ms: u64,
    pub requires_admin: bool,
    pub notes: &'static str,
}

/// Why a value is unavailable. These states are NOT equivalent: a
/// permanently unsupported sensor must not be retried/polled like a
/// transient error, and a stale value must not feed fresh-sample math.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailKind {
    /// Sensor/setting absent on this hardware or scheme.
    Unsupported = 0,
    /// Read failed; may succeed later.
    TransientError = 1,
    /// No sample taken (warmup, cadence skip) or no value by state
    /// (e.g. discharge watts while charging — see reason text).
    NotSampled = 2,
    /// Value too old to trust.
    Stale = 3,
}

impl UnavailKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UnavailKind::Unsupported => "unsupported",
            UnavailKind::TransientError => "transient",
            UnavailKind::NotSampled => "not-sampled",
            UnavailKind::Stale => "stale",
        }
    }

    /// Unknown wire codes map to NotSampled (neutral): never promote an
    /// unknown absence into an error, and never into a value.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => UnavailKind::Unsupported,
            1 => UnavailKind::TransientError,
            2 => UnavailKind::NotSampled,
            3 => UnavailKind::Stale,
            _ => UnavailKind::NotSampled,
        }
    }
}

/// Freshness of a sample. Repeated identical readings do NOT automatically
/// mean stable hardware (see §19); quality travels separately from value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleQuality {
    Fresh = 0,
    RepeatedCached = 1,
    Stale = 2,
    Error = 3,
    /// Unknown (e.g. pre-quality files, or absence without further info).
    Unknown = 4,
}

impl SampleQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            SampleQuality::Fresh => "fresh",
            SampleQuality::RepeatedCached => "repeated",
            SampleQuality::Stale => "stale",
            SampleQuality::Error => "error",
            SampleQuality::Unknown => "unknown",
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => SampleQuality::Fresh,
            1 => SampleQuality::RepeatedCached,
            2 => SampleQuality::Stale,
            3 => SampleQuality::Error,
            _ => SampleQuality::Unknown,
        }
    }
}

/// Quality implied by an absence kind (used when emitting Unavailable).
pub fn quality_for_kind(kind: UnavailKind) -> SampleQuality {
    match kind {
        UnavailKind::TransientError => SampleQuality::Error,
        UnavailKind::Stale => SampleQuality::Stale,
        _ => SampleQuality::Unknown,
    }
}

/// A single telemetry reading with full evidence: value + provenance +
/// source + dual timestamps + quality + structured absence kind.
/// This is the unit that must survive collection → session → archive →
/// reload → analysis → export without loss.
#[derive(Debug, Clone)]
pub struct Telemetry<T> {
    pub value: Reading<T>,
    pub source: &'static str,
    pub stamp: ClockStamp,
    pub quality: SampleQuality,
    pub unavail_kind: Option<UnavailKind>,
}

impl<T> Telemetry<T> {
    pub fn measured(value: T, source: &'static str, stamp: ClockStamp) -> Self {
        Telemetry {
            value: Reading::Measured(value),
            source,
            stamp,
            quality: SampleQuality::Fresh,
            unavail_kind: None,
        }
    }

    pub fn derived(value: T, source: &'static str, stamp: ClockStamp) -> Self {
        Telemetry {
            value: Reading::Derived(value),
            source,
            stamp,
            quality: SampleQuality::Fresh,
            unavail_kind: None,
        }
    }

    pub fn estimated(value: T, source: &'static str, stamp: ClockStamp) -> Self {
        Telemetry {
            value: Reading::Estimated(value),
            source,
            stamp,
            quality: SampleQuality::Fresh,
            unavail_kind: None,
        }
    }

    pub fn unavailable(
        kind: UnavailKind,
        reason: &'static str,
        source: &'static str,
        stamp: ClockStamp,
    ) -> Self {
        Telemetry {
            value: Reading::Unavailable(reason),
            source,
            stamp,
            quality: quality_for_kind(kind),
            unavail_kind: Some(kind),
        }
    }

    pub fn provenance(&self) -> Provenance {
        self.value.provenance()
    }

    pub fn val(&self) -> Option<T>
    where
        T: Copy,
    {
        self.value.value().copied()
    }
}

impl<T> Default for Telemetry<T> {
    fn default() -> Self {
        Telemetry {
            value: Reading::Unavailable("default-constructed"),
            source: "",
            stamp: ClockStamp::default(),
            quality: SampleQuality::Unknown,
            unavail_kind: Some(UnavailKind::NotSampled),
        }
    }
}

/// Canonical JSON envelope writer: {"v","p","q"} when present,
/// plus "k" (absence kind) and "reason" when unavailable.
/// Present values always carry Fresh quality; absent values carry
/// quality_for_kind(kind). All collectors route readings through here
/// so the wire format cannot drift per collector.
pub fn json_num(
    v: Option<f64>,
    prov: Provenance,
    kind: UnavailKind,
    reason: &'static str,
) -> String {
    match v {
        Some(x) => format!("{{\"v\":{x},\"p\":\"{}\",\"q\":0}}", prov.as_str()),
        None => format!(
            "{{\"v\":null,\"p\":\"unavailable\",\"k\":{},\"q\":{},\"reason\":\"{}\"}}",
            kind as u8,
            quality_for_kind(kind) as u8,
            escape_json(reason)
        ),
    }
}

/// Canonical JSON envelope writer for string readings (same shape).
pub fn json_str(
    v: Option<&str>,
    prov: Provenance,
    kind: UnavailKind,
    reason: &'static str,
) -> String {
    match v {
        Some(x) => format!(
            "{{\"v\":\"{}\",\"p\":\"{}\",\"q\":0}}",
            escape_json(x),
            prov.as_str()
        ),
        None => format!(
            "{{\"v\":null,\"p\":\"unavailable\",\"k\":{},\"q\":{},\"reason\":\"{}\"}}",
            kind as u8,
            quality_for_kind(kind) as u8,
            escape_json(reason)
        ),
    }
}
pub fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_carries_reason_and_no_value() {
        let r: Reading<f64> = Reading::Unavailable("no sensor");
        assert_eq!(r.provenance(), Provenance::Unavailable);
        assert!(r.value().is_none());
        assert_eq!(r.reason(), Some("no sensor"));
    }

    #[test]
    fn each_kind_reports_its_provenance() {
        assert_eq!(Reading::Measured(1.0).provenance(), Provenance::Measured);
        assert_eq!(Reading::Derived(1.0).provenance(), Provenance::Derived);
        assert_eq!(Reading::Estimated(1.0).provenance(), Provenance::Estimated);
    }

    #[test]
    fn mono_clock_is_monotonic() {
        let c = Clock::new();
        let a = c.stamp();
        let b = c.stamp();
        assert!(b.mono_millis >= a.mono_millis);
        assert!(a.wall_millis > 0);
    }

    #[test]
    fn escape_handles_control_chars() {
        assert_eq!(escape_json("a\"b\\c\n"), "a\\\"b\\\\c\\n");
    }

    #[test]
    fn unavail_kind_codec() {
        assert_eq!(UnavailKind::from_u8(0), UnavailKind::Unsupported);
        assert_eq!(UnavailKind::from_u8(3), UnavailKind::Stale);
        // Unknown wire codes stay neutral, never promoted.
        assert_eq!(UnavailKind::from_u8(9), UnavailKind::NotSampled);
        assert_eq!(SampleQuality::from_u8(9), SampleQuality::Unknown);
        assert_eq!(
            quality_for_kind(UnavailKind::TransientError),
            SampleQuality::Error
        );
        assert_eq!(quality_for_kind(UnavailKind::Stale), SampleQuality::Stale);
        assert_eq!(
            quality_for_kind(UnavailKind::Unsupported),
            SampleQuality::Unknown
        );
    }

    #[test]
    fn telemetry_envelope() {
        let stamp = ClockStamp {
            wall_millis: 7,
            mono_millis: 3,
        };
        let m = Telemetry::measured(6.4, "test-src", stamp);
        assert_eq!(m.provenance(), Provenance::Measured);
        assert_eq!(m.val(), Some(6.4));
        assert_eq!(m.quality, SampleQuality::Fresh);
        assert_eq!(m.unavail_kind, None);
        let u: Telemetry<f64> =
            Telemetry::unavailable(UnavailKind::Unsupported, "no sensor", "test-src", stamp);
        assert_eq!(u.provenance(), Provenance::Unavailable);
        assert_eq!(u.val(), None);
        assert_eq!(u.unavail_kind, Some(UnavailKind::Unsupported));
    }

    #[test]
    fn json_envelope_roundtrip_shape() {
        let s = json_num(
            Some(1.5),
            Provenance::Measured,
            UnavailKind::NotSampled,
            "n/a",
        );
        assert!(
            s.contains("\"v\":1.5") && s.contains("\"p\":\"measured\"") && s.contains("\"q\":0")
        );
        assert!(!s.contains("\"k\"")); // kind classifies absence only
        let s = json_num(
            None,
            Provenance::Unavailable,
            UnavailKind::Unsupported,
            "no sensor",
        );
        assert!(
            s.contains("\"v\":null")
                && s.contains("\"k\":0")
                && s.contains("\"q\":4")
                && s.contains("no sensor")
        );
        let s = json_str(
            Some("a\"b"),
            Provenance::Measured,
            UnavailKind::NotSampled,
            "",
        );
        assert!(s.contains("a\\\"b"));
    }
}
