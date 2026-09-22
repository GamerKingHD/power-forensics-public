//! Named-pipe IPC v1 server + client.
//!
//! Transport: `\\.\pipe\power-forensics-<user>`, byte stream, one u32-LE
//! framed UTF-8 JSON request answered by one framed JSON response per
//! connection. The codec (verbs, versions, framing) is pure and lives in
//! `pf_core::ipc`; this module owns the kernel32 handles. Malformed input
//! yields an error response, never a crash; absent pipes degrade to an
//! `Err` string ("unsupported" off Windows, "unavailable" without a server).
//!
//! Security model: the pipe is created with an explicit security descriptor
//! whose DACL grants `GENERIC_ALL` to exactly one principal — the current
//! process token's user SID (`GetTokenInformation(TokenUser)`) — and nothing
//! to anyone else. `PIPE_REJECT_REMOTE_CLIENTS` is set so remote SMB clients
//! cannot reach the pipe even if the DACL were permissive. Note: the Windows
//! SDK groups that flag with the `dwPipeMode` values, and this platform
//! rejects it in `dwOpenMode` with `ERROR_INVALID_PARAMETER`, so it is passed
//! as the pipe mode (see `PIPE_MODE`). The default DACL is never used. The
//! pipe name is derived from the user name only as a namespace; access is
//! enforced by the DACL, not the name. Remaining limitation: the DACL is not
//! inherited to child pipes and there is no audit ACE; a same-user process is
//! intentionally allowed.
//!
//! Bounded IO: every pipe handle is opened `FILE_FLAG_OVERLAPPED`, and
//! connect/read/write each wait on the operation event with a per-operation
//! deadline. A client that sends a partial frame, sends nothing, or never
//! reads is disconnected and cleaned up once the deadline expires; the
//! persistent `serve` loop then accepts the next connection and the one-shot
//! path returns an `Err`, so neither can be wedged by a dead peer.
//!
//! Phase C hookup: `serve_once` proves the transport with a single request;
//! `serve` is the persistent accept loop that dispatches every request
//! through one live `crate::service::Service` (mutating `stop`/`marker`,
//! reading `snapshot`/`status`). `pf-cli` `cmd_agent` owns the daemon wiring:
//! it acquires the single-instance lock, binds the command pipe via
//! `serve_with_ready` (binding failure is fatal), and drives the monitor with
//! the live/pause/stop/marker slots. See `crate::service` for the exact
//! wiring. The verb surface (`Service::handle`) and framing stay unchanged.
//!
//! Live-state path: the writer loop drops samples while paused and otherwise
//! publishes the latest battery snapshot into the slot served by `snapshot`.
//! `pause` and `resume` are part of the live pipe verb surface and mutate the
//! same shared pause flag consumed by the writer loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::time::{Duration, Instant};

use pf_core::telemetry::escape_json;

use crate::service::Service;

/// Sessions dir used by the snapshot-only dispatch shim. The persistent
/// daemon owns the real dir via its `Service`.
const DEFAULT_SESSIONS_DIR: &str = "sessions";

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        max_instances: u32,
        out_buf: u32,
        in_buf: u32,
        default_timeout: u32,
        security: usize,
    ) -> isize;
    fn ConnectNamedPipe(handle: isize, overlapped: *mut Overlapped) -> i32;
    fn ReadFile(
        handle: isize,
        buf: *mut u8,
        to_read: u32,
        out_read: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
    fn WriteFile(
        handle: isize,
        buf: *const u8,
        to_write: u32,
        out_written: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
    fn DisconnectNamedPipe(handle: isize) -> i32;
    fn CloseHandle(handle: isize) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: usize,
        creation: u32,
        flags: u32,
        template: usize,
    ) -> isize;
    fn WaitNamedPipeW(name: *const u16, timeout_ms: u32) -> i32;
    fn GetLastError() -> u32;
    fn SetLastError(code: u32);
    fn CreateEventW(attrs: usize, manual_reset: i32, initial: i32, name: *const u16) -> isize;
    fn CreateMutexW(attrs: usize, initial_owner: i32, name: *const u16) -> isize;
    fn ResetEvent(handle: isize) -> i32;
    fn WaitForSingleObject(handle: isize, millis: u32) -> u32;
    fn GetOverlappedResult(
        handle: isize,
        overlapped: *mut Overlapped,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;
    fn CancelIoEx(handle: isize, overlapped: *const Overlapped) -> i32;
    fn GetCurrentProcess() -> isize;
    #[cfg(test)]
    fn LocalFree(mem: isize) -> isize;
}

#[cfg(windows)]
#[link(name = "advapi32")]
unsafe extern "system" {
    fn GetUserNameW(buf: *mut u16, size: *mut u32) -> i32;
    fn OpenProcessToken(process: isize, access: u32, token: *mut isize) -> i32;
    fn GetTokenInformation(
        token: isize,
        class: u32,
        info: *mut u8,
        len: u32,
        ret_len: *mut u32,
    ) -> i32;
    fn InitializeSecurityDescriptor(sd: *mut u8, revision: u32) -> i32;
    fn SetSecurityDescriptorDacl(sd: *mut u8, present: i32, acl: *mut u8, defaulted: i32) -> i32;
    fn InitializeAcl(acl: *mut u8, len: u32, revision: u32) -> i32;
    fn AddAccessAllowedAce(acl: *mut u8, revision: u32, mask: u32, sid: *const u8) -> i32;
    fn GetLengthSid(sid: *const u8) -> u32;
    #[cfg(test)]
    fn GetSecurityInfo(
        handle: isize,
        object_type: u32,
        security_info: u32,
        owner: *mut usize,
        group: *mut usize,
        dacl: *mut usize,
        sacl: *mut usize,
        sd: *mut usize,
    ) -> u32;
    #[cfg(test)]
    fn GetAclInformation(acl: *const u8, info: *mut u8, len: u32, class: u32) -> i32;
    #[cfg(test)]
    fn GetAce(acl: *const u8, index: u32, ace: *mut *mut u8) -> i32;
    #[cfg(test)]
    fn EqualSid(a: *const u8, b: *const u8) -> i32;
}

#[cfg(windows)]
const INVALID_HANDLE: isize = -1;
#[cfg(windows)]
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
#[cfg(windows)]
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
#[cfg(windows)]
const PIPE_WAIT: u32 = 0x0;
#[cfg(windows)]
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
/// Open mode for every server instance: duplex and asynchronous. Access is
/// restricted by the per-user DACL and by `PIPE_MODE`'s remote-client flag.
#[cfg(windows)]
const PIPE_OPEN_MODE: u32 = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
/// Pipe mode for every server instance. `PIPE_REJECT_REMOTE_CLIENTS` is
/// grouped with the `dwPipeMode` values in the Windows SDK
/// (`winbase.h`: "Define the dwPipeMode values for CreateNamedPipe"); this
/// platform rejects it in `dwOpenMode` with `ERROR_INVALID_PARAMETER`.
#[cfg(windows)]
const PIPE_MODE: u32 = PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
#[cfg(windows)]
const GENERIC_RW: u32 = 0x8000_0000 | 0x4000_0000;
#[cfg(windows)]
const GENERIC_ALL: u32 = 0x1000_0000;
#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;
#[cfg(windows)]
const ERROR_BROKEN_PIPE: u32 = 109;
#[cfg(windows)]
const ERROR_PIPE_CONNECTED: u32 = 535;
#[cfg(windows)]
const ERROR_PIPE_NOT_CONNECTED: u32 = 233;
#[cfg(windows)]
const ERROR_IO_PENDING: u32 = 997;
#[cfg(windows)]
const ERROR_OPERATION_ABORTED: u32 = 995;
#[cfg(windows)]
const ERROR_ALREADY_EXISTS: u32 = 183;
#[cfg(windows)]
const WAIT_OBJECT_0: u32 = 0;
#[cfg(windows)]
const WAIT_TIMEOUT: u32 = 258;
#[cfg(windows)]
const TOKEN_QUERY: u32 = 0x0008;
#[cfg(windows)]
const TOKEN_USER_CLASS: u32 = 1;
#[cfg(windows)]
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
#[cfg(windows)]
const ACL_REVISION: u32 = 2;
#[cfg(all(windows, test))]
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x00;
#[cfg(all(windows, test))]
const SE_KERNEL_OBJECT: u32 = 6;
#[cfg(all(windows, test))]
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
#[cfg(all(windows, test))]
const ACL_SIZE_INFORMATION_CLASS: u32 = 2;
/// Per-request read/write deadline. A peer that cannot complete one framed
/// exchange within this window is disconnected rather than allowed to hang
/// the server.
#[cfg(windows)]
const IO_TIMEOUT_MS: u32 = 5_000;
/// One-shot accept window (`serve_once`) and the persistent accept poll.
#[cfg(windows)]
const ACCEPT_TIMEOUT_MS: u32 = 30_000;
#[cfg(windows)]
const ACCEPT_POLL_MS: u32 = 250;

/// `OVERLAPPED` is fixed-layout; the trailing event handle is the only field
/// this code sets. `internal`/`internal_high` are reserved for the OS.
#[cfg(windows)]
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: isize,
}

#[cfg(windows)]
impl Overlapped {
    fn with_event(event: isize) -> Self {
        Self {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: event,
        }
    }
}

/// Outcome of one overlapped pipe read: bytes transferred, or the peer closed
/// its end (zero-byte completion / `ERROR_BROKEN_PIPE`).
#[cfg(windows)]
enum ReadOutcome {
    Bytes(u32),
    Closed,
}

#[cfg(windows)]
#[repr(C)]
struct SecurityAttributes {
    n_length: u32,
    security_descriptor: *mut u8,
    inherit_handle: i32,
}

#[cfg(windows)]
#[repr(C)]
struct SidAndAttributes {
    sid: *mut u8,
    attributes: u32,
}

#[cfg(windows)]
#[repr(C)]
struct TokenUser {
    user: SidAndAttributes,
}

#[cfg(all(windows, test))]
#[repr(C)]
struct AclSizeInformation {
    ace_count: u32,
    bytes_in_use: u32,
    bytes_free: u32,
}

/// A buffer aligned for `SECURITY_DESCRIPTOR`/`ACL`/`SID` layout. The OS
/// requires DWORD/pointer alignment for these structures; a `Vec<u8>` does
/// not guarantee it, so the security-data scratch space is over-aligned.
#[cfg(windows)]
#[repr(C, align(8))]
struct Aligned<const N: usize>([u8; N]);

#[cfg(windows)]
impl<const N: usize> Aligned<N> {
    fn zeroed() -> Self {
        Self([0u8; N])
    }
}

/// Owns the security descriptor and ACL backing the `SECURITY_ATTRIBUTES`
/// passed to `CreateNamedPipeW`. Both buffers are boxed so their addresses
/// stay stable; `SecurityAttributes` points at the descriptor and remains
/// valid for the lifetime of this value.
#[cfg(windows)]
struct PipeSecurity {
    attrs: SecurityAttributes,
    _sd: Box<Aligned<64>>,
    _acl: Box<Aligned<512>>,
}

#[cfg(windows)]
impl PipeSecurity {
    fn ptr(&self) -> usize {
        &self.attrs as *const SecurityAttributes as usize
    }

    /// Build a descriptor whose DACL grants `GENERIC_ALL` to the current
    /// token's user SID and denies everyone else. Fails closed: any Win32
    /// failure returns `Err` and the pipe is never created with a NULL DACL.
    fn for_current_user() -> Result<Self, String> {
        let sid = current_user_sid()?;
        let sid_ptr = sid.0.as_ptr();
        let mut acl = Box::new(Aligned::<512>::zeroed());
        // SAFETY: acl is a live, 512-byte, 8-aligned buffer; revision is valid.
        if unsafe { InitializeAcl(acl.0.as_mut_ptr(), 512, ACL_REVISION) } == 0 {
            return Err(format!("InitializeAcl failed (win32 {})", unsafe {
                GetLastError()
            }));
        }
        // SAFETY: acl was initialized above and is large enough for one ACE
        // over a SID of this length; sid_ptr is a valid SID from the token.
        if unsafe { AddAccessAllowedAce(acl.0.as_mut_ptr(), ACL_REVISION, GENERIC_ALL, sid_ptr) }
            == 0
        {
            return Err(format!("AddAccessAllowedAce failed (win32 {})", unsafe {
                GetLastError()
            }));
        }
        let mut sd = Box::new(Aligned::<64>::zeroed());
        // SAFETY: sd is a live, 8-aligned buffer at least SECURITY_DESCRIPTOR-sized.
        if unsafe { InitializeSecurityDescriptor(sd.0.as_mut_ptr(), SECURITY_DESCRIPTOR_REVISION) }
            == 0
        {
            return Err(format!(
                "InitializeSecurityDescriptor failed (win32 {})",
                unsafe { GetLastError() }
            ));
        }
        // SAFETY: sd is initialized; acl outlives the descriptor inside Self.
        if unsafe { SetSecurityDescriptorDacl(sd.0.as_mut_ptr(), 1, acl.0.as_mut_ptr(), 0) } == 0 {
            return Err(format!(
                "SetSecurityDescriptorDacl failed (win32 {})",
                unsafe { GetLastError() }
            ));
        }
        let attrs = SecurityAttributes {
            n_length: std::mem::size_of::<SecurityAttributes>() as u32,
            security_descriptor: sd.0.as_mut_ptr(),
            inherit_handle: 0,
        };
        Ok(Self {
            attrs,
            _sd: sd,
            _acl: acl,
        })
    }
}

/// Copy the current process token's user SID into an owned, 8-aligned buffer
/// with the SID at offset 0. The buffer outlives the `AddAccessAllowedAce`
/// call that reads it.
#[cfg(windows)]
fn current_user_sid() -> Result<Box<Aligned<256>>, String> {
    let mut token: isize = 0;
    // SAFETY: token is an out-parameter; GetCurrentProcess is a pseudo-handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(format!("OpenProcessToken failed (win32 {})", unsafe {
            GetLastError()
        }));
    }
    let result = (|| {
        let mut info = Aligned::<256>::zeroed();
        let mut len: u32 = info.0.len() as u32;
        // SAFETY: info is a live, aligned 256-byte buffer; len is its size.
        if unsafe {
            GetTokenInformation(token, TOKEN_USER_CLASS, info.0.as_mut_ptr(), len, &mut len)
        } == 0
        {
            return Err(format!("GetTokenInformation failed (win32 {})", unsafe {
                GetLastError()
            }));
        }
        // SAFETY: on success the buffer holds a TOKEN_USER whose first field
        // is a valid SID pointer (validated below).
        let sid_ptr = unsafe { (*(info.0.as_ptr() as *const TokenUser)).user.sid };
        if sid_ptr.is_null() {
            return Err("token user SID is null".to_string());
        }
        // SAFETY: sid_ptr came from the OS and points to a valid SID.
        let sid_len = unsafe { GetLengthSid(sid_ptr) } as usize;
        if sid_len == 0 || sid_len > info.0.len() {
            return Err("token user SID length is invalid".to_string());
        }
        let mut sid = Box::new(Aligned::<256>::zeroed());
        // SAFETY: both regions are valid for sid_len bytes (SID fits the
        // 256-byte buffer); copying preserves the SID's DWORD alignment.
        unsafe { std::ptr::copy_nonoverlapping(sid_ptr, sid.0.as_mut_ptr(), sid_len) };
        Ok(sid)
    })();
    // SAFETY: token came from OpenProcessToken and is not used again.
    unsafe { CloseHandle(token) };
    result
}

/// Current values served over the pipe. Provenance-free by design: the pipe
/// carries latest readings, not evidence (evidence lives in sessions).
#[derive(Debug, Clone, Default)]
pub struct StateSnapshot {
    pub watts: Option<f64>,
    pub charge_w: Option<f64>,
    pub pct: Option<f64>,
    pub remaining_wh: Option<f64>,
    pub recent: Vec<Option<f64>>,
    pub events: usize,
    pub label: String,
}

fn num_json(o: Option<f64>) -> String {
    match o {
        Some(v) => format!("{v:.3}"),
        None => "null".to_string(),
    }
}

fn recent_json(recent: &[Option<f64>]) -> String {
    recent
        .iter()
        .map(|o| num_json(*o))
        .collect::<Vec<_>>()
        .join(",")
}

/// Render a snapshot as JSON: current values + recent_watts + label.
/// Pure; never fails (Nones become null).
pub fn snapshot_json(s: &StateSnapshot) -> String {
    format!(
        "{{\"label\":\"{}\",\"watts\":{},\"charge_w\":{},\
         \"battery_pct\":{},\"remaining_wh\":{},\"events\":{},\
         \"recent_watts\":[{}]}}",
        escape_json(&s.label),
        num_json(s.watts),
        num_json(s.charge_w),
        num_json(s.pct),
        num_json(s.remaining_wh),
        s.events,
        recent_json(&s.recent),
    )
}

/// Verb dispatch over a snapshot. Delegates to the single authoritative
/// implementation, `Service::handle`, seeding a transient service with
/// `snap`. This is the compatibility path for callers (and the one-shot
/// `serve_once` transport) that only have a snapshot, not a live daemon.
/// Never panics; every verb returns an encoded response.
pub fn dispatch(cmd: &str, arg: Option<&str>, snap: &StateSnapshot) -> String {
    let service = Service::new(DEFAULT_SESSIONS_DIR);
    match service.snapshot_handle().lock() {
        Ok(mut s) => *s = snap.clone(),
        Err(_) => return pf_core::ipc::encode_response_err("snapshot lock poisoned"),
    }
    match service.handle(cmd, arg) {
        Ok(data) => pf_core::ipc::encode_response_ok(&data),
        Err(e) => pf_core::ipc::encode_response_err(&e),
    }
}

/// Decode one framed request and answer it through `service`. Malformed
/// requests and unknown verbs become error responses; never panics.
fn respond(service: &Service, req: &str) -> String {
    match pf_core::ipc::decode_request(req) {
        Ok((cmd, arg)) => match service.handle(&cmd, arg.as_deref()) {
            Ok(data) => pf_core::ipc::encode_response_ok(&data),
            Err(e) => pf_core::ipc::encode_response_err(&e),
        },
        Err(e) => pf_core::ipc::encode_response_err(&e),
    }
}

fn current_user() -> String {
    #[cfg(windows)]
    {
        let mut buf = [0u16; 257];
        let mut n: u32 = buf.len() as u32;
        // SAFETY: buf is a live 257-u16 array; n carries its capacity in.
        let ok = unsafe { GetUserNameW(buf.as_mut_ptr(), &mut n) };
        if ok != 0 && n > 1 {
            let s = String::from_utf16_lossy(&buf[..n as usize - 1]);
            if !s.is_empty() {
                return s;
            }
        }
    }
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "default".to_string())
}

/// Default pipe for this user (GetUserNameW, else $USERNAME, else "default").
pub fn default_pipe_name() -> String {
    pf_core::ipc::pipe_name(&current_user())
}

/// Per-user named-mutex name for the single-agent ownership guarantee. Scoped
/// by the same user identity as the pipe, so two different users (who cannot
/// share the pipe anyway) do not block each other.
pub fn agent_mutex_name() -> String {
    format!("Local\\power-forensics-agent-{}", current_user())
}

/// Windows named-mutex ownership guard. A named mutex is released
/// automatically by the OS when the owning process exits or crashes, so there
/// is no stale-PID-file problem. `acquire` fails with `AlreadyExists` when
/// another live process owns the name.
pub struct SingleInstance {
    #[cfg(windows)]
    handle: isize,
}

/// Why ownership could not be acquired.
#[derive(Debug, PartialEq, Eq)]
pub enum InstanceError {
    AlreadyRunning,
    Failed(String),
}

impl std::fmt::Display for InstanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstanceError::AlreadyRunning => {
                write!(f, "another agent already owns this user's monitoring")
            }
            InstanceError::Failed(e) => write!(f, "cannot acquire agent lock: {e}"),
        }
    }
}

impl SingleInstance {
    /// Acquire exclusive ownership of `name`. Create-and-check semantics: if
    /// the mutex already existed, another instance owns it and this call
    /// fails (the just-opened handle is still closed via `Drop`).
    pub fn acquire(name: &str) -> Result<Self, InstanceError> {
        #[cfg(not(windows))]
        {
            let _ = name;
            return Err(InstanceError::Failed(
                "single-instance lock unsupported on this platform".to_string(),
            ));
        }
        #[cfg(windows)]
        {
            let wname = wide(name);
            // SAFETY: wname is nul-terminated and lives for the call; the OS
            // creates or opens the named mutex and returns an owned handle.
            // Clear last-error so ERROR_ALREADY_EXISTS reliably means the
            // mutex pre-existed (rather than a stale value).
            unsafe { SetLastError(0) };
            let handle = unsafe { CreateMutexW(0, 0, wname.as_ptr()) };
            if handle == 0 || handle == INVALID_HANDLE {
                return Err(InstanceError::Failed(format!(
                    "CreateMutexW failed (win32 {})",
                    unsafe { GetLastError() }
                )));
            }
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                // SAFETY: handle is owned and unused; close it so we do not
                // hold a reference that would keep the name alive.
                unsafe { CloseHandle(handle) };
                return Err(InstanceError::AlreadyRunning);
            }
            Ok(SingleInstance { handle })
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            if self.handle != 0 && self.handle != INVALID_HANDLE {
                // SAFETY: the handle is owned by this guard and closed once.
                unsafe { CloseHandle(self.handle) };
            }
        }
    }
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A connected server-side pipe instance. Owns both the pipe handle and the
/// manual-reset event used by every overlapped operation; `Drop` disconnects
/// and closes them exactly once, so no code path can leak the instance.
#[cfg(windows)]
struct PipeConn {
    handle: isize,
    event: isize,
}

#[cfg(windows)]
impl Drop for PipeConn {
    fn drop(&mut self) {
        // SAFETY: both handles are owned by this instance and closed once.
        unsafe {
            if self.handle != INVALID_HANDLE && self.handle != 0 {
                DisconnectNamedPipe(self.handle);
                CloseHandle(self.handle);
            }
            if self.event != 0 && self.event != INVALID_HANDLE {
                CloseHandle(self.event);
            }
        }
        self.handle = INVALID_HANDLE;
        self.event = 0;
    }
}

/// Issue one overlapped read and wait up to `timeout_ms` for completion.
/// A zero-byte completion and `ERROR_BROKEN_PIPE` both mean the peer closed.
#[cfg(windows)]
fn read_chunk(conn: &PipeConn, buf: &mut [u8], timeout_ms: u32) -> Result<ReadOutcome, String> {
    // SAFETY: ResetEvent on the instance's event handle.
    unsafe { ResetEvent(conn.event) };
    let mut ov = Overlapped::with_event(conn.event);
    let mut n: u32 = 0;
    // SAFETY: conn.handle is live; buf is live and sized; ov.event is live.
    let ok = unsafe {
        ReadFile(
            conn.handle,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut n,
            &mut ov,
        )
    };
    if ok != 0 {
        return Ok(if n == 0 {
            ReadOutcome::Closed
        } else {
            ReadOutcome::Bytes(n)
        });
    }
    // SAFETY: reads the calling thread's error code.
    let err = unsafe { GetLastError() };
    if err == ERROR_BROKEN_PIPE || err == ERROR_PIPE_NOT_CONNECTED {
        return Ok(ReadOutcome::Closed);
    }
    if err != ERROR_IO_PENDING {
        return Err(format!("pipe read failed (win32 {err})"));
    }
    match wait_io(conn, &mut ov, timeout_ms) {
        Ok(0) => Ok(ReadOutcome::Closed),
        Ok(k) => Ok(ReadOutcome::Bytes(k)),
        Err(e) => Err(e),
    }
}

/// Wait for an outstanding overlapped operation. On timeout the operation is
/// cancelled and awaited so no pending IO references the caller's buffer
/// after this function returns.
#[cfg(windows)]
fn wait_io(conn: &PipeConn, ov: &mut Overlapped, timeout_ms: u32) -> Result<u32, String> {
    // SAFETY: waits on the instance's event handle.
    match unsafe { WaitForSingleObject(conn.event, timeout_ms) } {
        WAIT_OBJECT_0 => {
            let mut n: u32 = 0;
            // SAFETY: ov is the live overlapped record for this operation.
            if unsafe { GetOverlappedResult(conn.handle, ov, &mut n, 0) } != 0 {
                Ok(n)
            } else {
                let e = unsafe { GetLastError() };
                if e == ERROR_BROKEN_PIPE || e == ERROR_OPERATION_ABORTED {
                    Ok(0)
                } else {
                    Err(format!("pipe io failed (win32 {e})"))
                }
            }
        }
        _ => {
            // SAFETY: cancels this instance's pending operation and waits for
            // it to retire (bWait=TRUE) before the buffers go out of scope.
            unsafe { CancelIoEx(conn.handle, ov) };
            let mut n: u32 = 0;
            let _ = unsafe { GetOverlappedResult(conn.handle, ov, &mut n, 1) };
            Err("pipe io timeout".to_string())
        }
    }
}

#[cfg(windows)]
impl PipeConn {
    /// Read exactly `buf.len()` bytes or fail: a peer that stalls mid-frame is
    /// dropped at the deadline instead of blocking the server. `deadline` is
    /// the absolute request lifetime: it caps total time even when every
    /// individual chunk arrives just inside the per-IO timeout.
    fn read_exact(&self, buf: &mut [u8], timeout_ms: u32, deadline: Instant) -> Result<(), String> {
        let mut off = 0;
        while off < buf.len() {
            let t = remaining_ms(deadline, timeout_ms)?;
            match read_chunk(self, &mut buf[off..], t)? {
                ReadOutcome::Bytes(n) => off += n as usize,
                ReadOutcome::Closed => {
                    return Err("pipe closed before full frame".to_string());
                }
            }
        }
        Ok(())
    }

    fn read_frame(&self, timeout_ms: u32, deadline: Instant) -> Result<String, String> {
        let mut lenb = [0u8; 4];
        self.read_exact(&mut lenb, timeout_ms, deadline)?;
        let len = u32::from_le_bytes(lenb) as usize;
        if len > pf_core::ipc::IPC_MAX_FRAME {
            return Err(format!("frame too large: {len} bytes"));
        }
        let mut buf = vec![0u8; len];
        self.read_exact(&mut buf, timeout_ms, deadline)?;
        String::from_utf8(buf).map_err(|e| format!("frame is not UTF-8: {e}"))
    }

    /// Write every byte or fail at the deadline. A peer that never reads
    /// eventually fills the pipe buffer; the bounded wait then disconnects it.
    fn write_all(&self, buf: &[u8], timeout_ms: u32, deadline: Instant) -> Result<(), String> {
        let mut off = 0;
        while off < buf.len() {
            let timeout_ms = remaining_ms(deadline, timeout_ms)?;
            // SAFETY: ResetEvent on the instance's event handle.
            unsafe { ResetEvent(self.event) };
            let mut ov = Overlapped::with_event(self.event);
            let mut n: u32 = 0;
            // SAFETY: self.handle is live; buf[off..] is live and sized.
            let ok = unsafe {
                WriteFile(
                    self.handle,
                    buf[off..].as_ptr(),
                    (buf.len() - off) as u32,
                    &mut n,
                    &mut ov,
                )
            };
            if ok != 0 {
                if n == 0 {
                    return Err("pipe write made no progress".to_string());
                }
                off += n as usize;
                continue;
            }
            // SAFETY: reads the calling thread's error code.
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return Err(format!("pipe write failed (win32 {err})"));
            }
            let n = wait_io(self, &mut ov, timeout_ms)?;
            if n == 0 {
                return Err("pipe write made no progress".to_string());
            }
            off += n as usize;
        }
        Ok(())
    }

    /// Bounded replacement for `FlushFileBuffers`: `DisconnectNamedPipe`
    /// discards unread bytes, so wait (with a deadline) for the client to
    /// consume the response and close its end. A client that never reads is
    /// dropped at the deadline rather than wedging the server.
    fn drain_until_client_closes(&self, timeout_ms: u32, deadline: Instant) -> Result<(), String> {
        let mut buf = [0u8; 64];
        let t = remaining_ms(deadline, timeout_ms)?;
        match read_chunk(self, &mut buf, t)? {
            ReadOutcome::Closed | ReadOutcome::Bytes(_) => Ok(()),
        }
    }
}

/// Remaining time before an absolute request deadline, capped by the
/// per-operation timeout. `Err` once the deadline has passed.
#[cfg(windows)]
fn remaining_ms(deadline: Instant, cap_ms: u32) -> Result<u32, String> {
    let rem = deadline.saturating_duration_since(Instant::now());
    if rem.is_zero() {
        return Err("pipe request exceeded absolute deadline".to_string());
    }
    let cap = rem.as_millis().min(cap_ms as u128).max(1) as u32;
    Ok(cap)
}

/// Serve exactly ONE request on `pipe_name`, then disconnect and exit.
///
/// The supplied `snapshot` is the request source: a transient `Service`
/// seeded with it answers every verb, so `start` / `stop` / `marker` mutate
/// state and no verb reports "not attached". `handler` is retained only for
/// source compatibility with existing callers (the snapshot is the
/// authoritative state now) and is not invoked.
pub fn serve_once(
    pipe_name: &str,
    snapshot: StateSnapshot,
    handler: impl Fn(&str) -> String,
) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = (pipe_name, snapshot, handler);
        return Err("named-pipe IPC unsupported on this platform".to_string());
    }
    #[cfg(windows)]
    {
        let _ = handler;
        let service = Service::new(DEFAULT_SESSIONS_DIR);
        match service.snapshot_handle().lock() {
            Ok(mut s) => *s = snapshot,
            Err(_) => return Err("snapshot lock poisoned".to_string()),
        }
        let answer = |req: &str| respond(&service, req);
        serve_once_windows(pipe_name, &answer, IO_TIMEOUT_MS)
    }
}

/// Serve requests on `pipe_name` until `stop` is set, dispatching each
/// through `service` (the single authoritative verb surface). This is the
/// Phase C persistent daemon loop; the accept timeout is short so `stop` is
/// observed promptly. `serve_once` remains the one-shot CLI transport proof.
pub fn serve(pipe_name: &str, service: Service, stop: Arc<AtomicBool>) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = (pipe_name, service, stop);
        return Err("named-pipe IPC unsupported on this platform".to_string());
    }
    #[cfg(windows)]
    {
        serve_with_timeouts(pipe_name, service, stop, ACCEPT_POLL_MS, IO_TIMEOUT_MS)
    }
}

/// Persistent accept loop with explicit timeouts so tests can exercise it
/// without waiting the production deadlines.
#[cfg(windows)]
fn serve_with_timeouts(
    pipe_name: &str,
    service: Service,
    stop: Arc<AtomicBool>,
    accept_poll_ms: u32,
    io_timeout_ms: u32,
) -> Result<(), String> {
    serve_loop(
        pipe_name,
        service,
        stop,
        accept_poll_ms,
        io_timeout_ms,
        None,
    )
}

/// Like [`serve`], but signals the outcome of the first pipe bind/accept
/// attempt on `ready` (once): `Ok(())` once the pipe is listening (connected
/// or idle), `Err` immediately on the first bind failure. This lets the agent
/// make a failed command pipe fatal *before* it starts monitoring, instead of
/// discovering it only after a silent headless run.
pub fn serve_with_ready(
    pipe_name: &str,
    service: Service,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let _ = (pipe_name, service, stop, ready);
        return Err("named-pipe IPC unsupported on this platform".to_string());
    }
    #[cfg(windows)]
    {
        serve_loop(
            pipe_name,
            service,
            stop,
            ACCEPT_POLL_MS,
            IO_TIMEOUT_MS,
            Some(ready),
        )
    }
}

#[cfg(windows)]
fn serve_loop(
    pipe_name: &str,
    service: Service,
    stop: Arc<AtomicBool>,
    accept_poll_ms: u32,
    io_timeout_ms: u32,
    mut ready: Option<std::sync::mpsc::Sender<Result<(), String>>>,
) -> Result<(), String> {
    let signal = |ready: &mut Option<std::sync::mpsc::Sender<Result<(), String>>>,
                  r: Result<(), String>| {
        if let Some(tx) = ready.take() {
            let _ = tx.send(r);
        }
    };
    while !stop.load(Ordering::Relaxed) {
        match create_and_accept(pipe_name, accept_poll_ms) {
            Ok(Some(conn)) => {
                signal(&mut ready, Ok(()));
                let answer = |req: &str| respond(&service, req);
                // A client error (timeout, partial frame, dead peer) must not
                // kill the server; `conn` is dropped and the next loop
                // iteration accepts a fresh connection.
                let _ = serve_connection(&conn, &answer, io_timeout_ms);
            }
            Ok(None) => signal(&mut ready, Ok(())),
            Err(e) => {
                eprintln!("serve: create/accept failed: {e}");
                signal(&mut ready, Err(e.clone()));
                return Err(e);
            }
        }
    }
    signal(&mut ready, Ok(()));
    Ok(())
}

/// Create one pipe instance with a per-user DACL and remote-client rejection.
#[cfg(windows)]
fn create_pipe(pipe_name: &str) -> Result<isize, String> {
    let security = PipeSecurity::for_current_user()?;
    let wname = wide(pipe_name);
    // SAFETY: wname is nul-terminated; `security` points at a live
    // SECURITY_ATTRIBUTES whose DACL grants only the current user SID. The OS
    // copies the descriptor during this call.
    let handle = unsafe {
        CreateNamedPipeW(
            wname.as_ptr(),
            PIPE_OPEN_MODE,
            PIPE_MODE,
            1,
            65536,
            65536,
            0,
            security.ptr(),
        )
    };
    if handle == INVALID_HANDLE {
        return Err(format!(
            "cannot create pipe {pipe_name} (win32 {})",
            unsafe { GetLastError() }
        ));
    }
    Ok(handle)
}

/// Create one pipe instance and wait up to `timeout_ms` for a client.
/// `Ok(None)` is an idle timeout; any pending overlapped connect is cancelled
/// and awaited, and the instance is closed via `PipeConn::drop` before this
/// returns, so neither the handle nor a thread is leaked. `Ok(Some(conn))` is
/// a connected instance the caller must serve via `serve_connection`.
#[cfg(windows)]
fn create_and_accept(pipe_name: &str, timeout_ms: u32) -> Result<Option<PipeConn>, String> {
    let handle = create_pipe(pipe_name)?;
    // SAFETY: null attributes/name is valid; manual-reset, initially unsignaled.
    let event = unsafe { CreateEventW(0, 1, 0, std::ptr::null()) };
    if event == 0 {
        // SAFETY: handle came from create_pipe and is not otherwise owned yet.
        unsafe { CloseHandle(handle) };
        return Err(format!("cannot create pipe event (win32 {})", unsafe {
            GetLastError()
        }));
    }
    let conn = PipeConn { handle, event };
    // SAFETY: reset the manual-reset event before issuing the connect.
    unsafe { ResetEvent(conn.event) };
    let mut ov = Overlapped::with_event(conn.event);
    // SAFETY: conn.handle is a live overlapped-mode pipe handle.
    let ok = unsafe { ConnectNamedPipe(conn.handle, &mut ov) };
    if ok != 0 {
        return Ok(Some(conn));
    }
    // SAFETY: reads the calling thread's error code.
    let err = unsafe { GetLastError() };
    if err == ERROR_PIPE_CONNECTED {
        return Ok(Some(conn));
    }
    if err != ERROR_IO_PENDING {
        return Err(format!("pipe accept failed ({err})"));
    }
    // SAFETY: waits on the instance event for the overlapped connect.
    match unsafe { WaitForSingleObject(conn.event, timeout_ms) } {
        WAIT_OBJECT_0 => {
            let mut n: u32 = 0;
            // SAFETY: ov is the live record for the pending connect.
            if unsafe { GetOverlappedResult(conn.handle, &mut ov, &mut n, 0) } != 0 {
                Ok(Some(conn))
            } else {
                // SAFETY: reads the calling thread's error code.
                let e = unsafe { GetLastError() };
                if e == ERROR_PIPE_CONNECTED {
                    Ok(Some(conn))
                } else if e == ERROR_BROKEN_PIPE || e == ERROR_PIPE_NOT_CONNECTED {
                    // A client connected then vanished mid-accept.
                    Ok(None)
                } else {
                    Err(format!("pipe accept failed ({e})"))
                }
            }
        }
        WAIT_TIMEOUT => {
            // SAFETY: cancels the pending connect and waits for it to retire
            // before `ov`/`conn` are dropped.
            unsafe { CancelIoEx(conn.handle, &ov) };
            let mut n: u32 = 0;
            let _ = unsafe { GetOverlappedResult(conn.handle, &mut ov, &mut n, 1) };
            Ok(None)
        }
        _ => {
            // SAFETY: same cancellation discipline as the timeout arm.
            unsafe { CancelIoEx(conn.handle, &ov) };
            let mut n: u32 = 0;
            let _ = unsafe { GetOverlappedResult(conn.handle, &mut ov, &mut n, 1) };
            Err(format!("pipe connect wait failed (win32 {})", unsafe {
                GetLastError()
            }))
        }
    }
}

/// Read one framed request from a connected pipe, answer it with `handler`,
/// write one framed response, then wait (bounded) for the client to read it
/// and disconnect. Every step is deadline-bounded; `conn`'s `Drop` performs
/// the actual disconnect and close on all paths.
#[cfg(windows)]
fn serve_connection(
    conn: &PipeConn,
    handler: &impl Fn(&str) -> String,
    io_timeout_ms: u32,
) -> Result<(), String> {
    // Absolute request lifetime: per-chunk IO is additionally capped by
    // `io_timeout_ms`, but a client drip-feeding a large frame cannot hold the
    // connection beyond this total (the 8 MiB frame cap still applies).
    let deadline =
        Instant::now() + Duration::from_millis((io_timeout_ms as u64).saturating_mul(3).max(1_000));
    let req = conn.read_frame(io_timeout_ms, deadline)?;
    let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&req)))
        .unwrap_or_else(|_| pf_core::ipc::encode_response_err("handler failed"));
    conn.write_all(
        &pf_core::ipc::frame(resp.as_bytes()),
        io_timeout_ms,
        deadline,
    )?;
    conn.drain_until_client_closes(io_timeout_ms, deadline)
}

#[cfg(windows)]
fn serve_once_windows(
    pipe_name: &str,
    handler: &impl Fn(&str) -> String,
    io_timeout_ms: u32,
) -> Result<(), String> {
    match create_and_accept(pipe_name, ACCEPT_TIMEOUT_MS)? {
        Some(conn) => serve_connection(&conn, handler, io_timeout_ms),
        None => Err("pipe accept timeout".to_string()),
    }
}

/// Query a pipe: open (5s wait), send one framed request, read one framed
/// response. Absent servers degrade to `Err`, never a panic or a hang.
pub fn query(pipe_name: &str, request_json: &str) -> Result<String, String> {
    #[cfg(not(windows))]
    {
        let _ = (pipe_name, request_json);
        return Err("named-pipe IPC unsupported on this platform".to_string());
    }
    #[cfg(windows)]
    {
        query_windows(pipe_name, request_json)
    }
}

/// Client-side synchronous read (the client opens its own handle without
/// `FILE_FLAG_OVERLAPPED`; polls against an absent server are bounded by the
/// `WaitNamedPipeW` loop, not by this helper).
#[cfg(windows)]
fn client_read_exact(h: isize, buf: &mut [u8]) -> Result<(), String> {
    let mut off = 0;
    while off < buf.len() {
        let mut n: u32 = 0;
        // SAFETY: h is a live synchronous pipe handle; buf[off..] is live.
        let ok = unsafe {
            ReadFile(
                h,
                buf[off..].as_mut_ptr(),
                (buf.len() - off) as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || n == 0 {
            // SAFETY: reads the calling thread's error code.
            let e = unsafe { GetLastError() };
            return Err(format!("pipe read failed or closed (win32 {e})"));
        }
        off += n as usize;
    }
    Ok(())
}

#[cfg(windows)]
fn client_read_frame(h: isize) -> Result<String, String> {
    let mut lenb = [0u8; 4];
    client_read_exact(h, &mut lenb)?;
    let len = u32::from_le_bytes(lenb) as usize;
    if len > pf_core::ipc::IPC_MAX_FRAME {
        return Err(format!("frame too large: {len} bytes"));
    }
    let mut buf = vec![0u8; len];
    client_read_exact(h, &mut buf)?;
    String::from_utf8(buf).map_err(|e| format!("frame is not UTF-8: {e}"))
}

#[cfg(windows)]
fn client_write_all(h: isize, buf: &[u8]) -> Result<(), String> {
    let mut off = 0;
    while off < buf.len() {
        let mut n: u32 = 0;
        // SAFETY: h is a live synchronous pipe handle; buf[off..] is live.
        let ok = unsafe {
            WriteFile(
                h,
                buf[off..].as_ptr(),
                (buf.len() - off) as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || n == 0 {
            return Err("pipe write failed or closed".to_string());
        }
        off += n as usize;
    }
    Ok(())
}

#[cfg(windows)]
fn query_windows(pipe_name: &str, request_json: &str) -> Result<String, String> {
    let wname = wide(pipe_name);
    // A listening pipe can be momentarily absent (the server recreates an
    // instance per accept), so retry within the documented 5s window rather
    // than reporting "no agent" on the first miss. A genuinely absent server
    // still degrades to an Err after the deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // SAFETY: wname is nul-terminated.
        let waited = unsafe { WaitNamedPipeW(wname.as_ptr(), 500) };
        if waited != 0 {
            // SAFETY: wname is nul-terminated; default security/flags/template.
            let h = unsafe { CreateFileW(wname.as_ptr(), GENERIC_RW, 0, 0, OPEN_EXISTING, 0, 0) };
            if h != INVALID_HANDLE {
                let out = (|| {
                    client_write_all(h, &pf_core::ipc::frame(request_json.as_bytes()))?;
                    client_read_frame(h)
                })();
                // SAFETY: h came from CreateFileW above.
                unsafe { CloseHandle(h) };
                return out;
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err("pipe unavailable (no agent listening)".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_snapshot() -> StateSnapshot {
        StateSnapshot {
            watts: Some(6.5),
            charge_w: None,
            pct: Some(55.0),
            remaining_wh: Some(20.0),
            recent: vec![Some(6.4), None, Some(6.6)],
            events: 2,
            label: "demo".to_string(),
        }
    }

    #[test]
    fn snapshot_json_has_current_values_and_recent() {
        let j = snapshot_json(&demo_snapshot());
        let v = pf_core::json::parse(&j).unwrap();
        assert_eq!(v.get("label").and_then(|l| l.as_str()), Some("demo"));
        assert_eq!(v.get("watts").and_then(|n| n.num()), Some(6.5));
        assert!(j.contains("\"charge_w\":null"), "{j}");
        assert!(j.contains("recent_watts"), "{j}");
        assert_eq!(v.get("events").and_then(|n| n.num()), Some(2.0));
    }

    #[test]
    fn dispatch_routes_all_verbs_through_service() {
        let snap = demo_snapshot();
        // Pure reads now come from the single Service implementation.
        // `snapshot` carries the seeded values; `status` is agent lifecycle.
        assert!(
            pf_core::ipc::decode_response(&dispatch("snapshot", None, &snap))
                .unwrap()
                .contains("demo")
        );
        assert!(
            pf_core::ipc::decode_response(&dispatch("status", None, &snap))
                .unwrap()
                .contains("running")
        );
        assert!(
            pf_core::ipc::decode_response(&dispatch("recent_timeline", None, &snap))
                .unwrap()
                .contains("recent_watts")
        );
        assert!(pf_core::ipc::decode_response(&dispatch("events", None, &snap)).is_ok());
        assert!(pf_core::ipc::decode_response(&dispatch("session_list", None, &snap)).is_ok());
        // Mutating verbs that are part of the contract work.
        let r = dispatch("marker", Some("\"lunch\""), &snap);
        assert!(
            pf_core::ipc::decode_response(&r)
                .unwrap()
                .contains("queued for session"),
            "{r}"
        );
        let r = dispatch("stop", None, &snap);
        assert!(
            pf_core::ipc::decode_response(&r)
                .unwrap()
                .contains("stopped"),
            "{r}"
        );
        // No-arg verbs reject a supplied ARG instead of silently ignoring it.
        let e =
            pf_core::ipc::decode_response(&dispatch("snapshot", Some("\"x\""), &snap)).unwrap_err();
        assert!(e.contains("takes no ARG"), "{e}");
        // Phantom/stub verbs are rejected by the protocol decoder, never
        // advertised, and never reach the service.
        for verb in [
            "start",
            "diagnostics",
            "experiment_start",
            "experiment_status",
        ] {
            let err = pf_core::ipc::decode_response(&dispatch(verb, None, &snap)).unwrap_err();
            assert!(err.contains("unknown verb"), "{verb}: {err}");
        }
    }

    #[cfg(windows)]
    fn temp_service_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pf-agent-ipc-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_pipe_uses_prefix() {
        let n = default_pipe_name();
        assert!(n.starts_with("\\\\.\\pipe\\power-forensics-"), "{n}");
    }

    #[cfg(windows)]
    fn test_pipe() -> String {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!(
            "\\\\.\\pipe\\power-forensics-test-{}-{n}",
            std::process::id()
        )
    }

    /// Seed a transient service from `snapshot` and serve exactly one request
    /// with an explicit IO deadline, so timeout tests don't wait 5s.
    #[cfg(windows)]
    fn serve_once_ms(
        pipe_name: &str,
        snapshot: StateSnapshot,
        io_timeout_ms: u32,
    ) -> Result<(), String> {
        let service = Service::new(DEFAULT_SESSIONS_DIR);
        match service.snapshot_handle().lock() {
            Ok(mut s) => *s = snapshot,
            Err(_) => return Err("snapshot lock poisoned".to_string()),
        }
        let answer = |req: &str| respond(&service, req);
        serve_once_windows(pipe_name, &answer, io_timeout_ms)
    }

    /// Connect a raw synchronous client, retrying while the server recreates
    /// its single pipe instance.
    #[cfg(windows)]
    fn raw_connect(pipe_name: &str) -> isize {
        let wname = wide(pipe_name);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // SAFETY: wname is nul-terminated.
            if unsafe { WaitNamedPipeW(wname.as_ptr(), 500) } != 0 {
                // SAFETY: wname is nul-terminated.
                let h =
                    unsafe { CreateFileW(wname.as_ptr(), GENERIC_RW, 0, 0, OPEN_EXISTING, 0, 0) };
                if h != INVALID_HANDLE {
                    return h;
                }
            }
            if std::time::Instant::now() >= deadline {
                panic!("raw_connect: pipe never appeared: {pipe_name}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[cfg(windows)]
    fn raw_write(h: isize, buf: &[u8]) {
        let mut n: u32 = 0;
        // SAFETY: h is a live synchronous client handle; buf is live.
        let ok = unsafe {
            WriteFile(
                h,
                buf.as_ptr(),
                buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        assert!(ok != 0 && n as usize == buf.len(), "raw_write short/failed");
    }

    /// Non-panicking client write for tests that intentionally outlive the
    /// server's deadline (an expected failure must not print a panic).
    #[cfg(windows)]
    fn raw_try_write(h: isize, buf: &[u8]) -> bool {
        let mut n: u32 = 0;
        // SAFETY: h is a live synchronous client handle; buf is live.
        let ok = unsafe {
            WriteFile(
                h,
                buf.as_ptr(),
                buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        ok != 0 && n as usize == buf.len()
    }

    #[cfg(windows)]
    #[test]
    fn loopback_serve_once_query_roundtrip() {
        let name = test_pipe();
        let snap = demo_snapshot();
        let owned = name.clone();
        // The handler is deliberately never called: serve_once answers from
        // the snapshot through Service::handle.
        let server = std::thread::spawn(move || {
            serve_once(&owned, snap.clone(), |_| {
                panic!("handler must not be called")
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        let resp = query(&name, &pf_core::ipc::encode_request("snapshot", None)).unwrap();
        let data = pf_core::ipc::decode_response(&resp).unwrap();
        assert!(data.contains("demo"), "{data}");
        server.join().unwrap().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn loopback_malformed_request_yields_error_not_crash() {
        let name = test_pipe();
        let snap = demo_snapshot();
        let owned = name.clone();
        let server = std::thread::spawn(move || {
            serve_once(&owned, snap.clone(), |_| {
                panic!("handler must not be called")
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        let resp = query(&name, "not json{{{").unwrap();
        assert!(pf_core::ipc::decode_response(&resp).is_err());
        // The server answered with an error response instead of crashing.
        server.join().unwrap().unwrap();
    }

    /// Full lifecycle over the real named-pipe transport against a
    /// persistent Service: mutating verbs change the daemon's state, pause
    /// freezes snapshot publishes, resume unfreezes, stop clears everything.
    #[cfg(windows)]
    #[test]
    fn loopback_service_lifecycle_pause_resume_over_pipe() {
        let name = test_pipe();
        let dir = temp_service_dir("lifecycle");
        let svc = Service::new(&dir.to_string_lossy());
        let stop = Arc::new(AtomicBool::new(false));
        let sname = name.clone();
        let ssvc = svc.clone();
        let sstop = stop.clone();
        let server = std::thread::spawn(move || serve(&sname, ssvc, sstop));

        let ask = |cmd: &str, arg: Option<&str>| -> Result<String, String> {
            let resp = query(&name, &pf_core::ipc::encode_request(cmd, arg))?;
            pf_core::ipc::decode_response(&resp)
        };

        // A live agent run owns one lifecycle (started by the daemon itself);
        // status and snapshot are observable over the pipe.
        svc.start("demo");
        let d = ask("status", None).unwrap();
        assert!(d.contains("\"running\":true") && d.contains("demo"), "{d}");

        // A published snapshot is readable over the pipe.
        assert!(svc.publish_snapshot(demo_snapshot()));
        let d = ask("snapshot", None).unwrap();
        assert_eq!(
            pf_core::json::parse(&d)
                .unwrap()
                .get("watts")
                .and_then(|n| n.num()),
            Some(6.5),
            "{d}"
        );

        // marker mutates state; status reflects the count.
        let d = ask("marker", Some("\"lunch\"")).unwrap();
        assert!(d.contains("\"markers_accepted\":1"), "{d}");
        let d = ask("status", None).unwrap();
        assert!(d.contains("\"markers_accepted\":1"), "{d}");

        // pause freezes publishes and reports paused.
        svc.pause();
        let d = ask("status", None).unwrap();
        assert!(d.contains("\"paused\":true"), "{d}");
        let frozen = StateSnapshot {
            label: "frozen".into(),
            watts: Some(99.0),
            ..Default::default()
        };
        assert!(!svc.publish_snapshot(frozen.clone()));
        let d = ask("snapshot", None).unwrap();
        assert_eq!(
            pf_core::json::parse(&d)
                .unwrap()
                .get("watts")
                .and_then(|n| n.num()),
            Some(6.5),
            "snapshot advanced while paused: {d}"
        );

        // resume lets publishes flow again.
        svc.resume();
        assert!(svc.publish_snapshot(frozen));
        let d = ask("snapshot", None).unwrap();
        assert!(d.contains("\"label\":\"frozen\""), "{d}");

        // stop clears running and paused.
        let d = ask("stop", None).unwrap();
        assert!(d.contains("\"running\":false"), "{d}");
        let d = ask("status", None).unwrap();
        assert!(
            d.contains("\"running\":false") && d.contains("\"paused\":false"),
            "{d}"
        );

        stop.store(true, Ordering::Relaxed);
        let r = server.join().unwrap();
        assert!(r.is_ok(), "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unknown, unimplemented, and malformed requests all return an encoded
    /// error over the real pipe and leave the server serving.
    #[cfg(windows)]
    #[test]
    fn loopback_bad_requests_return_errors_without_crash() {
        let name = test_pipe();
        let dir = temp_service_dir("badreq");
        let svc = Service::new(&dir.to_string_lossy());
        let stop = Arc::new(AtomicBool::new(false));
        let sname = name.clone();
        let sstop = stop.clone();
        let server = std::thread::spawn(move || serve(&sname, svc, sstop));

        // Verb outside the protocol: rejected at decode.
        let resp = query(&name, &pf_core::ipc::encode_request("reboot", None)).unwrap();
        let e = pf_core::ipc::decode_response(&resp).unwrap_err();
        assert!(e.contains("unknown verb"), "{e}");

        // Removed stub verb: rejected by the protocol decoder, not dispatch.
        let resp = query(&name, &pf_core::ipc::encode_request("diagnostics", None)).unwrap();
        let e = pf_core::ipc::decode_response(&resp).unwrap_err();
        assert!(e.contains("unknown verb"), "{e}");

        // Malformed JSON: error response, not a panic.
        let resp = query(&name, "not json{{{").unwrap();
        assert!(pf_core::ipc::decode_response(&resp).is_err());

        // The server is still alive and serving.
        let resp = query(&name, &pf_core::ipc::encode_request("status", None)).unwrap();
        assert!(pf_core::ipc::decode_response(&resp).is_ok());

        stop.store(true, Ordering::Relaxed);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn query_without_server_degrades() {
        let r = query(
            "\\\\.\\pipe\\power-forensics-test-nonexistent-58507",
            &pf_core::ipc::encode_request("status", None),
        );
        assert!(r.is_err());
    }

    /// The effective server creation flags must reject remote (SMB) clients
    /// and be asynchronous, since every IO path depends on overlapped
    /// completion. `PIPE_REJECT_REMOTE_CLIENTS` is a `dwPipeMode` flag on this
    /// platform (see `PIPE_MODE`); the create call passes `PIPE_MODE`.
    #[cfg(windows)]
    #[test]
    fn pipe_flags_reject_remote_clients_and_overlap_io() {
        assert_eq!(
            PIPE_MODE & PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_REJECT_REMOTE_CLIENTS,
            "PIPE_REJECT_REMOTE_CLIENTS missing from pipe mode"
        );
        assert_ne!(
            PIPE_OPEN_MODE & FILE_FLAG_OVERLAPPED,
            0,
            "server pipe must be opened FILE_FLAG_OVERLAPPED"
        );
    }

    /// A real pipe instance carries a non-NULL DACL with exactly one
    /// ACCESS_ALLOWED ACE, and that ACE grants the current user's SID. This is
    /// the strongest per-user rejection assertion available without a second
    /// user token on this machine.
    #[cfg(windows)]
    #[test]
    fn pipe_dacl_grants_only_current_user() {
        let name = test_pipe();
        let handle = create_pipe(&name).expect("create_pipe");
        let mut dacl: usize = 0;
        let mut sd: usize = 0;
        // SAFETY: handle is a live kernel object; out-pointers are valid.
        let rc = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(rc, 0, "GetSecurityInfo failed ({rc})");
        assert_ne!(dacl, 0, "pipe DACL is NULL (default security)");

        let mut info = AclSizeInformation {
            ace_count: 0,
            bytes_in_use: 0,
            bytes_free: 0,
        };
        // SAFETY: dacl came from GetSecurityInfo; info is sized for the class.
        let ok = unsafe {
            GetAclInformation(
                dacl as *const u8,
                &mut info as *mut AclSizeInformation as *mut u8,
                std::mem::size_of::<AclSizeInformation>() as u32,
                ACL_SIZE_INFORMATION_CLASS,
            )
        };
        assert!(ok != 0, "GetAclInformation failed");
        assert_eq!(
            info.ace_count, 1,
            "expected exactly one ACE (current user only)"
        );

        let mut ace: *mut u8 = std::ptr::null_mut();
        // SAFETY: dacl came from GetSecurityInfo; index 0 exists (ace_count==1).
        assert!(
            unsafe { GetAce(dacl as *const u8, 0, &mut ace) } != 0,
            "GetAce failed"
        );
        // SAFETY: ace points at an ACCESS_ALLOWED_ACE.
        assert_eq!(
            unsafe { *ace },
            ACCESS_ALLOWED_ACE_TYPE,
            "ACE is not ACCESS_ALLOWED"
        );
        // ACCESS_ALLOWED_ACE::SidStart is at byte offset 8.
        let ace_sid = unsafe { ace.add(8) };
        let user = current_user_sid().expect("current user SID");
        // SAFETY: both pointers reference valid SIDs.
        assert_ne!(
            unsafe { EqualSid(ace_sid, user.0.as_ptr()) },
            0,
            "ACE does not grant the current user SID"
        );

        // SAFETY: sd came from GetSecurityInfo; handle from create_pipe.
        unsafe {
            LocalFree(sd as isize);
            CloseHandle(handle);
        }
    }

    /// A client that sends only a partial frame must not wedge the server: the
    /// one-shot path returns a timeout error within the bounded window.
    #[cfg(windows)]
    #[test]
    fn partial_frame_client_errors_within_bound() {
        let name = test_pipe();
        let owned = name.clone();
        let server = std::thread::spawn(move || serve_once_ms(&owned, demo_snapshot(), 300));
        let h = raw_connect(&name);
        raw_write(h, &[2, 0]);
        let start = std::time::Instant::now();
        let r = server.join().unwrap();
        let elapsed = start.elapsed();
        // SAFETY: h came from raw_connect.
        unsafe { CloseHandle(h) };
        let e = r.expect_err("partial frame should not be served");
        assert!(e.contains("timeout"), "expected a timeout error, got {e:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "server took {elapsed:?}"
        );
    }

    /// A client that drip-feeds a frame one byte at a time (each within the
    /// per-IO timeout) must still be cut off by the absolute request deadline.
    #[cfg(windows)]
    #[test]
    fn drip_fed_frame_hits_absolute_deadline() {
        let name = test_pipe();
        let owned = name.clone();
        let server = std::thread::spawn(move || serve_once_ms(&owned, demo_snapshot(), 300));
        let h = raw_connect(&name);
        // Advertise an 8-byte payload, then release it 1 byte / 150 ms so the
        // per-chunk timeout (300 ms) never fires under scheduling jitter but
        // total lifetime exceeds 3*300 ms absolute deadline.
        raw_write(h, &8u32.to_le_bytes());
        let start = std::time::Instant::now();
        // Tolerate the (expected) write failure once the server enforces its
        // deadline and drops the connection; no panic output.
        for _ in 0..12 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let _ = raw_try_write(h, &[0u8]);
        }
        let r = server.join().unwrap();
        let elapsed = start.elapsed();
        // SAFETY: h came from raw_connect.
        unsafe { CloseHandle(h) };
        let e = r.expect_err("drip-fed frame should be rejected");
        // The connection must be cut off within the bounded window. Whether the
        // per-chunk timeout or the absolute request deadline wins is timing
        // dependent; both are correct bounded rejections.
        assert!(
            e.contains("absolute deadline") || e.contains("timeout"),
            "expected a bounded rejection, got {e:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "server took {elapsed:?}"
        );
    }

    /// A client that connects and sends nothing is dropped at the deadline.
    #[cfg(windows)]
    #[test]
    fn silent_client_is_cleaned_up() {
        let name = test_pipe();
        let owned = name.clone();
        let server = std::thread::spawn(move || serve_once_ms(&owned, demo_snapshot(), 300));
        let h = raw_connect(&name);
        let start = std::time::Instant::now();
        let r = server.join().unwrap();
        let elapsed = start.elapsed();
        // SAFETY: h came from raw_connect.
        unsafe { CloseHandle(h) };
        let e = r.expect_err("silent client should be dropped");
        assert!(e.contains("timeout"), "expected a timeout error, got {e:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "server took {elapsed:?}"
        );
    }

    /// An accept that times out closes its instance and leaves the pipe name
    /// reusable for a later real connection (no leaked handle/thread).
    #[cfg(windows)]
    #[test]
    fn accept_timeout_reclaims_instance() {
        let name = test_pipe();
        let start = std::time::Instant::now();
        assert!(create_and_accept(&name, 100).unwrap().is_none());
        assert!(create_and_accept(&name, 100).unwrap().is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(3));

        let owned = name.clone();
        let server = std::thread::spawn(move || serve_once_ms(&owned, demo_snapshot(), 1000));
        std::thread::sleep(std::time::Duration::from_millis(200));
        let resp = query(&name, &pf_core::ipc::encode_request("status", None)).unwrap();
        assert!(pf_core::ipc::decode_response(&resp).is_ok());
        server
            .join()
            .unwrap()
            .expect("later request must be served");
    }

    /// The persistent loop stays alive after a partial-frame client times out
    /// and keeps serving subsequent well-formed clients.
    #[cfg(windows)]
    #[test]
    fn serve_survives_partial_frame_client() {
        let name = test_pipe();
        let dir = temp_service_dir("partial");
        let svc = Service::new(&dir.to_string_lossy());
        let stop = Arc::new(AtomicBool::new(false));
        let sname = name.clone();
        let sstop = stop.clone();
        let server = std::thread::spawn(move || serve_with_timeouts(&sname, svc, sstop, 100, 300));

        // Bad client: connect, advertise a 2-byte frame, then stall.
        let h = raw_connect(&name);
        raw_write(h, &[2, 0]);
        std::thread::sleep(std::time::Duration::from_millis(600));
        // SAFETY: h came from raw_connect.
        unsafe { CloseHandle(h) };

        // The server is still accepting and answers a fresh request.
        let resp = query(&name, &pf_core::ipc::encode_request("status", None)).unwrap();
        assert!(
            pf_core::ipc::decode_response(&resp).is_ok(),
            "server did not recover after a partial-frame client"
        );

        stop.store(true, Ordering::Relaxed);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Named-mutex ownership: a live owner blocks a second acquire; releasing
    /// it lets the next acquire succeed. This is the primitive behind the
    /// single-agent guarantee.
    #[cfg(windows)]
    #[test]
    fn single_instance_is_exclusive_and_releasable() {
        let name = format!(
            "Local\\pf-test-single-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let first = SingleInstance::acquire(&name).expect("first acquire");
        assert_eq!(
            SingleInstance::acquire(&name).err(),
            Some(InstanceError::AlreadyRunning),
            "second acquire must be rejected while the first is live"
        );
        drop(first);
        let _again = SingleInstance::acquire(&name).expect("reacquire after release");
    }
}
