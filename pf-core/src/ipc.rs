//! IPC codecs for the legacy file protocol and the live named-pipe transport.
//!
//! Layout under `<sessions>/ipc/`:
//!   `state.json`               — latest snapshot written by `ipc serve`
//!   `commands/<cmd>.json`      — one file per command written by `ipc send`
//!
//! Commands: `status` | `snapshot` | `start` | `stop` | `marker`.
//! All helpers here are pure (paths + JSON strings); `main.rs` does the IO.

use crate::session::SessionData;
use crate::telemetry::escape_json;

pub fn state_path(sessions_dir: &str) -> String {
    format!(
        "{}/ipc/state.json",
        sessions_dir.trim_end_matches(['/', '\\'])
    )
}

pub fn commands_dir(sessions_dir: &str) -> String {
    format!(
        "{}/ipc/commands",
        sessions_dir.trim_end_matches(['/', '\\'])
    )
}

pub fn command_path(sessions_dir: &str, cmd: &str) -> String {
    format!("{}/{}.json", commands_dir(sessions_dir), cmd)
}

pub fn valid_commands() -> &'static [&'static str] {
    &["status", "snapshot", "start", "stop", "marker"]
}

pub fn is_valid_command(cmd: &str) -> bool {
    valid_commands().contains(&cmd)
}

/// Encode a command file body. Pure; caller writes it to `command_path()`.
pub fn encode_command(cmd: &str, arg: Option<&str>, wall_ms: u64) -> Result<String, String> {
    if !is_valid_command(cmd) {
        return Err(format!(
            "unknown command: {cmd} (status|snapshot|start|stop|marker)"
        ));
    }
    let arg_json = match arg {
        Some(a) => format!("\"{}\"", escape_json(a)),
        None => "null".to_string(),
    };
    Ok(format!(
        "{{\"cmd\":\"{}\",\"arg\":{arg_json},\"wall_ms\":{wall_ms}}}",
        escape_json(cmd)
    ))
}

/// Decode a command file body into (cmd, arg).
pub fn decode_command(text: &str) -> Result<(String, Option<String>), String> {
    let v = crate::json::parse(text).map_err(|e| format!("bad command JSON: {e}"))?;
    let cmd = v
        .get("cmd")
        .and_then(|c| c.as_str())
        .ok_or("command JSON lacks string \"cmd\"".to_string())?;
    if !is_valid_command(cmd) {
        return Err(format!("unknown command: {cmd}"));
    }
    let arg = v.get("arg").and_then(|a| a.as_str()).map(|s| s.to_string());
    Ok((cmd.to_string(), arg))
}

/// Build `state.json` from a loaded session. Current watts = last discharge
/// value, else last charge value (negative sign kept out; caller reads both).
/// Recent timeline = last 30 discharge watts (null when unavailable).
pub fn state_from_session(s: &SessionData) -> String {
    let last_b = s.battery.last();
    let watts = last_b.and_then(|b| b.discharge.val());
    let charge_w = last_b.and_then(|b| b.charge.val());
    let pct = last_b.and_then(|b| b.pct.val());
    let remaining_wh = last_b.and_then(|b| b.remaining_wh.val());
    let num = |o: Option<f64>| o.map(|v| format!("{v:.3}")).unwrap_or("null".to_string());
    let recent: Vec<String> = s
        .battery
        .iter()
        .rev()
        .take(30)
        .rev()
        .map(|b| {
            b.discharge
                .val()
                .map(|v| format!("{v:.3}"))
                .unwrap_or("null".to_string())
        })
        .collect();
    let footer_json = match &s.footer {
        Some(f) => crate::json::to_json(f),
        None => "null".to_string(),
    };
    format!(
        "{{\"label\":\"{}\",\"samples\":{},\"watts\":{},\"charge_w\":{},\
         \"battery_pct\":{},\"remaining_wh\":{},\"events\":{},\
         \"recent_watts\":[{}],\"footer\":{footer_json}}}",
        escape_json(&s.label),
        s.battery.len(),
        num(watts),
        num(charge_w),
        num(pct),
        num(remaining_wh),
        s.events.len(),
        recent.join(","),
    )
}

// ============================ named-pipe IPC v1 ============================
// Pure codec + framing only; the Windows pipe handles live in
// `pf-agent::ipc_server`. The file-protocol helpers above remain available as
// a compatibility fallback.

/// Named-pipe IPC protocol version. Requests and responses carry it;
/// mismatches are rejected, never coerced.
pub const IPC_VERSION: u32 = 1;

/// Largest single framed message accepted (8 MiB). Larger length prefixes
/// are rejected as malformed; the connection is never trusted blindly.
pub const IPC_MAX_FRAME: usize = 8 * 1024 * 1024;

/// Pipe name for a user: `\\.\pipe\power-forensics-<sanitized>`.
/// Sanitizes to ASCII alnum plus `-_`, truncates to 32 chars,
/// empty (or fully stripped) input maps to "default".
pub fn pipe_name(user: &str) -> String {
    let kept: String = user
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    let name = if kept.is_empty() {
        "default".to_string()
    } else {
        kept
    };
    format!("\\\\.\\pipe\\power-forensics-{name}")
}

/// Verbs the pipe protocol accepts. This is the exact contract: every verb
/// here is implemented by `Service::handle` on the daemon. `start` is
/// deliberately absent: one agent process owns exactly one monitoring run,
/// which begins at process start, so a runtime `start` transition would be a
/// fake capability. Phantom stubs (`diagnostics`, `experiment_*`) were
/// removed rather than advertised.
pub fn valid_verbs() -> &'static [&'static str] {
    &[
        "status",
        "snapshot",
        "stop",
        "pause",
        "resume",
        "marker",
        "recent_timeline",
        "events",
        "session_list",
    ]
}

pub fn is_valid_verb(cmd: &str) -> bool {
    valid_verbs().contains(&cmd)
}

/// Encode a pipe request. `arg_json` is an already-encoded JSON fragment
/// (e.g. `"text"` or `{"k":1}`); `None` encodes null. The command is not
/// validated here — the server rejects unknown verbs on decode.
pub fn encode_request(cmd: &str, arg_json: Option<&str>) -> String {
    let arg = arg_json.unwrap_or("null");
    format!(
        "{{\"v\":{IPC_VERSION},\"cmd\":\"{}\",\"arg\":{arg},\"tool_version\":\"{}\"}}",
        escape_json(cmd),
        env!("CARGO_PKG_VERSION"),
    )
}

/// Decode a pipe request into (verb, arg). The arg is returned as canonical
/// JSON text (`None` for null/missing). Rejects bad JSON, wrong versions,
/// missing verbs, and unknown verbs — never panics on malformed input.
pub fn decode_request(text: &str) -> Result<(String, Option<String>), String> {
    let v = crate::json::parse(text).map_err(|e| format!("bad request JSON: {e}"))?;
    let ver = v
        .get("v")
        .and_then(|n| n.num())
        .ok_or("request JSON lacks numeric \"v\"".to_string())?;
    if ver != IPC_VERSION as f64 {
        return Err(format!(
            "unsupported IPC version: {ver} (need {})",
            IPC_VERSION
        ));
    }
    let cmd = v
        .get("cmd")
        .and_then(|c| c.as_str())
        .ok_or("request JSON lacks string \"cmd\"".to_string())?;
    if !is_valid_verb(cmd) {
        return Err(format!("unknown verb: {cmd}"));
    }
    let arg = match v.get("arg") {
        None | Some(crate::json::JVal::Null) => None,
        Some(a) => Some(crate::json::to_json(a)),
    };
    Ok((cmd.to_string(), arg))
}

/// Encode a success response wrapping an already-encoded JSON `data` fragment.
pub fn encode_response_ok(data_json: &str) -> String {
    format!("{{\"ok\":true,\"v\":{IPC_VERSION},\"data\":{data_json}}}")
}

/// Encode a failure response. The message is string-escaped, never embedded raw.
pub fn encode_response_err(msg: &str) -> String {
    format!(
        "{{\"ok\":false,\"v\":{IPC_VERSION},\"error\":\"{}\"}}",
        escape_json(msg)
    )
}

/// Decode a pipe response: `Ok(data_json)` on success (canonical JSON text,
/// `"null"` when absent), `Err(message)` on failure or malformed input.
pub fn decode_response(text: &str) -> Result<String, String> {
    let v = crate::json::parse(text).map_err(|e| format!("bad response JSON: {e}"))?;
    let ok = v
        .get("ok")
        .and_then(|b| b.as_bool())
        .ok_or("response JSON lacks boolean \"ok\"".to_string())?;
    if ok {
        match v.get("data") {
            None | Some(crate::json::JVal::Null) => Ok("null".to_string()),
            Some(d) => Ok(crate::json::to_json(d)),
        }
    } else {
        Err(v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("agent error")
            .to_string())
    }
}

/// Frame a message: u32 little-endian length prefix + UTF-8 bytes.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Try to extract one framed message from a byte buffer.
/// `Ok(None)` means incomplete (need more bytes); `Err` means malformed
/// (oversize length or non-UTF-8 payload). Never panics.
pub fn try_deframe(buf: &[u8]) -> Result<Option<(String, usize)>, String> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > IPC_MAX_FRAME {
        return Err(format!(
            "frame too large: {len} bytes (max {IPC_MAX_FRAME})"
        ));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let text =
        std::str::from_utf8(&buf[4..4 + len]).map_err(|e| format!("frame is not UTF-8: {e}"))?;
    Ok(Some((text.to_string(), 4 + len)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{ClockStamp, Telemetry};

    #[test]
    fn command_roundtrip() {
        let enc = encode_command("marker", Some("lunch break"), 123).unwrap();
        let (cmd, arg) = decode_command(&enc).unwrap();
        assert_eq!(cmd, "marker");
        assert_eq!(arg.as_deref(), Some("lunch break"));
        let enc2 = encode_command("status", None, 1).unwrap();
        let (cmd2, arg2) = decode_command(&enc2).unwrap();
        assert_eq!(cmd2, "status");
        assert!(arg2.is_none());
    }

    #[test]
    fn rejects_unknown_command() {
        assert!(encode_command("reboot", None, 1).is_err());
        assert!(decode_command("{\"cmd\":\"reboot\"}").is_err());
        assert!(decode_command("not json").is_err());
    }

    #[test]
    fn paths_nest_under_sessions_dir() {
        assert_eq!(state_path("sessions"), "sessions/ipc/state.json");
        assert_eq!(commands_dir("sessions"), "sessions/ipc/commands");
        assert_eq!(
            command_path("sessions", "stop"),
            "sessions/ipc/commands/stop.json"
        );
    }

    #[test]
    fn state_has_watts_and_recent() {
        let stamp = ClockStamp {
            wall_millis: 1,
            mono_millis: 0,
        };
        let mut s = SessionData {
            label: "demo".to_string(),
            ..Default::default()
        };
        s.battery.push(crate::session::BatteryPoint {
            t: 0.0,
            discharge: Telemetry::measured(6.5, "battery", stamp),
            pct: Telemetry::measured(55.0, "battery", stamp),
            ..Default::default()
        });
        let j = state_from_session(&s);
        assert!(j.contains("\"watts\":6.500"), "{j}");
        assert!(j.contains("recent_watts"), "{j}");
        let v = crate::json::parse(&j).unwrap();
        assert_eq!(v.get("samples").and_then(|n| n.num()), Some(1.0));
    }

    #[test]
    fn pipe_name_sanitizes() {
        assert_eq!(pipe_name("alice"), "\\\\.\\pipe\\power-forensics-alice");
        assert_eq!(pipe_name(""), "\\\\.\\pipe\\power-forensics-default");
        assert_eq!(pipe_name("..."), "\\\\.\\pipe\\power-forensics-default");
        assert_eq!(
            pipe_name("a/b\\c:d e"),
            "\\\\.\\pipe\\power-forensics-abcde"
        );
        assert_eq!(pipe_name("a-b_c1"), "\\\\.\\pipe\\power-forensics-a-b_c1");
        let long = "x".repeat(64);
        let named = pipe_name(&long);
        assert_eq!(
            named,
            format!("\\\\.\\pipe\\power-forensics-{}", "x".repeat(32))
        );
    }

    #[test]
    fn pipe_verbs_are_all_implemented_and_no_stubs() {
        // Exact contract: 9 verbs, all served by `Service::handle`.
        assert_eq!(valid_verbs().len(), 9);
        for v in [
            "status",
            "snapshot",
            "stop",
            "pause",
            "resume",
            "marker",
            "recent_timeline",
            "events",
            "session_list",
        ] {
            assert!(is_valid_verb(v), "missing {v}");
        }
        // Phantom capabilities are gone, including the fake runtime `start`.
        for gone in [
            "start",
            "diagnostics",
            "experiment_start",
            "experiment_status",
            "reboot",
        ] {
            assert!(!is_valid_verb(gone), "must not be advertised: {gone}");
        }
    }

    #[test]
    fn request_roundtrip() {
        let enc = encode_request("snapshot", None);
        let v = crate::json::parse(&enc).unwrap();
        assert_eq!(v.get("v").and_then(|n| n.num()), Some(1.0));
        assert_eq!(v.get("cmd").and_then(|c| c.as_str()), Some("snapshot"));
        assert_eq!(
            v.get("tool_version").and_then(|t| t.as_str()),
            Some(env!("CARGO_PKG_VERSION"))
        );
        let (cmd, arg) = decode_request(&enc).unwrap();
        assert_eq!(cmd, "snapshot");
        assert!(arg.is_none());
        let enc2 = encode_request("marker", Some("\"lunch\""));
        let (cmd2, arg2) = decode_request(&enc2).unwrap();
        assert_eq!(cmd2, "marker");
        assert_eq!(arg2.as_deref(), Some("\"lunch\""));
    }

    #[test]
    fn request_rejects_bad_version_and_verb() {
        assert!(decode_request("not json").is_err());
        assert!(decode_request("{\"v\":1}").is_err());
        assert!(decode_request("{\"v\":2,\"cmd\":\"status\"}").is_err());
        assert!(decode_request("{\"v\":1,\"cmd\":\"reboot\"}").is_err());
        assert!(decode_request("{\"v\":1,\"cmd\":\"status\"}").is_ok());
    }

    #[test]
    fn response_roundtrip() {
        let ok = encode_response_ok("{\"watts\":6.5}");
        assert_eq!(decode_response(&ok).unwrap(), "{\"watts\":6.5}");
        let err = encode_response_err("boom \"quoted\"");
        let e = decode_response(&err).unwrap_err();
        assert!(e.contains("boom"), "{e}");
        assert!(decode_response("{\"ok\":true}").unwrap() == "null");
        assert!(decode_response("garbage").is_err());
        assert!(decode_response("{\"v\":1}").is_err());
    }

    #[test]
    fn framing_roundtrip_and_partial() {
        let f = frame("hello".as_bytes());
        assert_eq!(&f[..4], &[5, 0, 0, 0]);
        assert_eq!(try_deframe(&f).unwrap(), Some(("hello".to_string(), 9)));
        assert_eq!(try_deframe(&f[..3]).unwrap(), None);
        assert_eq!(try_deframe(&f[..6]).unwrap(), None);
        let mut two = frame("a".as_bytes());
        two.extend_from_slice(&frame("bc".as_bytes()));
        let (first, used) = try_deframe(&two).unwrap().unwrap();
        assert_eq!((first.as_str(), used), ("a", 5));
        let (second, _) = try_deframe(&two[used..]).unwrap().unwrap();
        assert_eq!(second, "bc");
    }

    #[test]
    fn framing_rejects_oversize_and_bad_utf8() {
        let mut big = (IPC_MAX_FRAME as u32 + 1).to_le_bytes().to_vec();
        big.extend_from_slice(&[0u8; 4]);
        assert!(try_deframe(&big).is_err());
        let mut bad = 2u32.to_le_bytes().to_vec();
        bad.extend_from_slice(&[0xFF, 0xFF]);
        assert!(try_deframe(&bad).is_err());
    }
}
