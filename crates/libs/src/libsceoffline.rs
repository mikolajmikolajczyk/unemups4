//! The libraries a title uses to reach the network, answered as a console with **no
//! connectivity** — not as a console whose network libraries are broken.
//!
//! # Why the distinction decides everything
//!
//! A PS4 offline is not a PS4 with a failed SSL stack. `sceSslInit` succeeds in airplane mode;
//! what reports the absence of a network is the *connection state*, not the initialisation.
//! A title branches on those two things very differently: "library unavailable" is an
//! unexpected condition many titles treat as fatal, while "signed out, no link" is the
//! ordinary path every PS4 game ships with and exercises constantly.
//!
//! So the model here is three rules, in order of how often getting them wrong hurts:
//!
//! 1. **Initialisation succeeds.** Init/Term of every network library returns success. There is
//!    nothing dishonest about it — the library really is available; it simply has no link.
//! 2. **State says disconnected.** The state queries are where the truth lives:
//!    `sceNetCtlGetState` reports DISCONNECTED, NP reports SIGNED OUT.
//! 3. **Operations refuse IMMEDIATELY.** Anything that would talk to a server returns its
//!    "not connected" error at once. Never block, never pretend to be in progress. A title
//!    that is told "in progress" will poll, and a poll that is never answered is the
//!    unbounded-loop failure of doc-5 case 28 in a new costume.
//!
//! Rule 3 is the one to hold onto while extending this file. It is always tempting to return
//! "busy, ask again" for something not modelled yet; that converts a clean refusal the title
//! knows how to handle into a spin it does not.

use crate::context::NativeContext;
use ps4_core::guest_ptr::GuestPtr;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;

/// `SCE_NET_CTL_STATE_DISCONNECTED` — no link at all (not "connecting", not "IP obtained").
pub const NET_CTL_STATE_DISCONNECTED: i32 = 0;

/// `SCE_NET_CTL_ERROR_NOT_CONNECTED` — the API's own way of saying "there is no link", which
/// is what a title's offline path is written against.
const SCE_NET_CTL_ERROR_NOT_CONNECTED: i32 = 0x80412104u32 as i32;

/// `SCE_NET_CTL_ERROR_INVALID_ADDR` — a bad out-pointer.
const SCE_NET_CTL_ERROR_INVALID_ADDR: i32 = 0x80412101u32 as i32;

/// `SCE_HTTP_ERROR_NETWORK` — a request could not reach the network. The refusal a title's
/// offline path is written to receive from a send.
const SCE_HTTP_ERROR_NETWORK: i32 = 0x80431068u32 as i32;

/// `SCE_HTTP_ERROR_INVALID_VALUE` — a bad argument.
const SCE_HTTP_ERROR_INVALID_VALUE: i32 = 0x804310FEu32 as i32;

/// Handles handed back for HTTP objects. They are opaque to the guest and only ever passed
/// back to us, so one fixed positive value per kind is enough — and keeps a mismatched
/// delete/use obvious rather than plausible. Distinct per kind so a log shows which is which.
const HTTP_TEMPLATE_ID: i32 = 0x101;
const HTTP_CONNECTION_ID: i32 = 0x201;
const HTTP_REQUEST_ID: i32 = 0x301;
const HTTP_EPOLL_ID: i32 = 0x401;

// ---------------------------------------------------------------------------
// libSceSsl — the TLS library. Initialising it is local work; it needs no network.
// ---------------------------------------------------------------------------

#[ps4_syscall(id = SyscallId::SCE_SSL_INIT, lib = crate::libs::LIB_SCE_SSL, name = "sceSslInit")]
pub fn sce_ssl_init(_pool_size: u64) -> i32 {
    // Returns a positive "SSL context id" on hardware; any non-zero id is fine here, and 1 is
    // distinguishable from both a zero handle and an error code.
    1
}

#[ps4_syscall(id = SyscallId::SCE_SSL_TERM, lib = crate::libs::LIB_SCE_SSL, name = "sceSslTerm")]
pub fn sce_ssl_term(_ctx_id: i32) -> i32 {
    0
}

// ---------------------------------------------------------------------------
// libSceNetCtl — the connection state. THIS is where "offline" is actually said.
// ---------------------------------------------------------------------------

#[ps4_syscall(id = SyscallId::SCE_NET_CTL_INIT, lib = crate::libs::LIB_SCE_NET_CTL, name = "sceNetCtlInit")]
pub fn sce_net_ctl_init() -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_NET_CTL_TERM, lib = crate::libs::LIB_SCE_NET_CTL, name = "sceNetCtlTerm")]
pub fn sce_net_ctl_term() -> i32 {
    0
}

/// `sceNetCtlGetState(int *state)`: DISCONNECTED, always.
///
/// The single most important answer in this file. A title polls this to decide whether to
/// attempt anything online at all, so a truthful DISCONNECTED here is what keeps it off every
/// path that would otherwise wait on a server.
#[ps4_syscall(id = SyscallId::SCE_NET_CTL_GET_STATE, lib = crate::libs::LIB_SCE_NET_CTL, name = "sceNetCtlGetState")]
pub fn sce_net_ctl_get_state(state_out: *mut i32) -> i32 {
    let Some(gp) = GuestPtr::<i32>::new(state_out as u64) else {
        return SCE_NET_CTL_ERROR_INVALID_ADDR;
    };
    let _ = gp.write(NET_CTL_STATE_DISCONNECTED);
    0
}

/// `sceNetCtlGetInfo(code, SceNetCtlInfo *info)`: per-field connection details — IP address,
/// SSID, link speed, MAC.
///
/// Every one of them is unavailable without a link, and the API has a specific error for that.
/// Returning it beats zero-filling the union: a zeroed IP reads as `0.0.0.0`, which a title
/// may take as a *valid* address and then try to bind or advertise.
#[ps4_syscall(id = SyscallId::SCE_NET_CTL_GET_INFO, lib = crate::libs::LIB_SCE_NET_CTL, name = "sceNetCtlGetInfo")]
pub fn sce_net_ctl_get_info(_code: i32, _info_out: u64) -> i32 {
    SCE_NET_CTL_ERROR_NOT_CONNECTED
}

/// `sceNetCtlGetResult(eventType, int *errorCode)`: the outcome of the last connection attempt.
/// Nothing was attempted, so there is no error to report — success with a zero code.
#[ps4_syscall(id = SyscallId::SCE_NET_CTL_GET_RESULT, lib = crate::libs::LIB_SCE_NET_CTL, name = "sceNetCtlGetResult")]
pub fn sce_net_ctl_get_result(_event_type: i32, error_code_out: *mut i32) -> i32 {
    if let Some(gp) = GuestPtr::<i32>::new(error_code_out as u64) {
        let _ = gp.write(0);
    }
    0
}

/// `sceNetCtlGetNatInfo(SceNetCtlNatInfo *info)`: NAT type. Unavailable with no link.
#[ps4_syscall(id = SyscallId::SCE_NET_CTL_GET_NAT_INFO, lib = crate::libs::LIB_SCE_NET_CTL, name = "sceNetCtlGetNatInfo")]
pub fn sce_net_ctl_get_nat_info(_info_out: u64) -> i32 {
    SCE_NET_CTL_ERROR_NOT_CONNECTED
}

/// `sceNetCtlRegisterCallback(cb, arg, int *cbId)`: register a state-change callback.
///
/// Accepted, and never invoked — the state never changes, so there is nothing to report. This
/// is honest rather than lazy: a callback that fires with a fabricated state change would send
/// the title down a connection path with no connection behind it.
#[ps4_syscall(id = SyscallId::SCE_NET_CTL_REGISTER_CALLBACK, lib = crate::libs::LIB_SCE_NET_CTL, name = "sceNetCtlRegisterCallback")]
pub fn sce_net_ctl_register_callback(_cb: u64, _arg: u64, cb_id_out: *mut i32) -> i32 {
    if let Some(gp) = GuestPtr::<i32>::new(cb_id_out as u64) {
        let _ = gp.write(1);
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_NET_CTL_UNREGISTER_CALLBACK, lib = crate::libs::LIB_SCE_NET_CTL, name = "sceNetCtlUnregisterCallback")]
pub fn sce_net_ctl_unregister_callback(_cb_id: i32) -> i32 {
    0
}

/// `sceNetCtlCheckCallback()`: pump registered callbacks. Nothing to deliver, ever.
#[ps4_syscall(id = SyscallId::SCE_NET_CTL_CHECK_CALLBACK, lib = crate::libs::LIB_SCE_NET_CTL, name = "sceNetCtlCheckCallback")]
pub fn sce_net_ctl_check_callback() -> i32 {
    0
}

// ---------------------------------------------------------------------------
// libSceHttp — initialising is local; every request refuses at once.
// ---------------------------------------------------------------------------

#[ps4_syscall(id = SyscallId::SCE_HTTP_INIT, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpInit")]
pub fn sce_http_init(_net_mem_id: i32, _ssl_ctx_id: i32, _pool_size: u64) -> i32 {
    // A positive libhttp context id, same reasoning as sceSslInit.
    1
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_TERM, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpTerm")]
pub fn sce_http_term(_http_ctx_id: i32) -> i32 {
    0
}

// The object side of libSceHttp: creating templates, connections and requests is LOCAL work
// that succeeds on a console with no link — the refusal belongs on the send, which is where a
// title's error handling is aimed. Answering an early failure here would exercise a rarer path
// for no gain in honesty.

#[ps4_syscall(id = SyscallId::SCE_HTTP_CREATE_TEMPLATE, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpCreateTemplate")]
pub fn sce_http_create_template(
    _http_ctx_id: i32,
    _user_agent: u64,
    _http_ver: i32,
    _auto_proxy_conf: i32,
) -> i32 {
    HTTP_TEMPLATE_ID
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_DELETE_TEMPLATE, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpDeleteTemplate")]
pub fn sce_http_delete_template(_template_id: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_CREATE_CONNECTION_WITH_URL, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpCreateConnectionWithURL")]
pub fn sce_http_create_connection_with_url(
    _template_id: i32,
    _url: u64,
    _enable_keep_alive: i32,
) -> i32 {
    // Creating a connection OBJECT does not open a socket; the connect happens on send.
    HTTP_CONNECTION_ID
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_DELETE_CONNECTION, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpDeleteConnection")]
pub fn sce_http_delete_connection(_conn_id: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_CREATE_REQUEST_WITH_URL, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpCreateRequestWithURL")]
pub fn sce_http_create_request_with_url(
    _conn_id: i32,
    _method: i32,
    _url: u64,
    _content_length: u64,
) -> i32 {
    HTTP_REQUEST_ID
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_DELETE_REQUEST, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpDeleteRequest")]
pub fn sce_http_delete_request(_req_id: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_ADD_REQUEST_HEADER, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpAddRequestHeader")]
pub fn sce_http_add_request_header(_req_id: i32, _name: u64, _value: u64, _mode: i32) -> i32 {
    0
}

/// `sceHttpSendRequest`: the refusal, and it is IMMEDIATE.
///
/// Not "in progress", not a timeout after some delay — a title told the request is pending will
/// poll for a completion that can never come, which is the unbounded-loop failure of doc-5
/// case 28. `SCE_HTTP_ERROR_NETWORK` is what its offline path expects.
#[ps4_syscall(id = SyscallId::SCE_HTTP_SEND_REQUEST, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpSendRequest")]
pub fn sce_http_send_request(_req_id: i32, _post_data: u64, _size: u64) -> i32 {
    SCE_HTTP_ERROR_NETWORK
}

/// Reading a response that was never sent. Zero bytes, not an error: a caller that loops
/// "read until 0" terminates, where an error might send it round again.
#[ps4_syscall(id = SyscallId::SCE_HTTP_READ_DATA, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpReadData")]
pub fn sce_http_read_data(_req_id: i32, _data: u64, _size: u64) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_GET_STATUS_CODE, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpGetStatusCode")]
pub fn sce_http_get_status_code(_req_id: i32, status_out: *mut i32) -> i32 {
    let Some(gp) = GuestPtr::<i32>::new(status_out as u64) else {
        return SCE_HTTP_ERROR_INVALID_VALUE;
    };
    // There is no response, so there is no status. Report the failure rather than inventing a
    // code — a fabricated 200 would tell the title its request succeeded.
    let _ = gp.write(0);
    SCE_HTTP_ERROR_NETWORK
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_GET_RESPONSE_CONTENT_LENGTH, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpGetResponseContentLength")]
pub fn sce_http_get_response_content_length(
    _req_id: i32,
    _result_out: *mut i32,
    _length_out: *mut u64,
) -> i32 {
    SCE_HTTP_ERROR_NETWORK
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_GET_ALL_RESPONSE_HEADERS, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpGetAllResponseHeaders")]
pub fn sce_http_get_all_response_headers(
    _req_id: i32,
    _header_out: *mut u64,
    _len_out: *mut u64,
) -> i32 {
    SCE_HTTP_ERROR_NETWORK
}

/// The last transport errno behind a failed request. `ENETDOWN` names the actual condition.
#[ps4_syscall(id = SyscallId::SCE_HTTP_GET_LAST_ERRNO, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpGetLastErrno")]
pub fn sce_http_get_last_errno(_req_id: i32, errno_out: *mut i32) -> i32 {
    if let Some(gp) = GuestPtr::<i32>::new(errno_out as u64) {
        let _ = gp.write(50); // ENETDOWN (FreeBSD)
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_CREATE_EPOLL, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpCreateEpoll")]
pub fn sce_http_create_epoll(_http_ctx_id: i32, epoll_out: *mut i32) -> i32 {
    if let Some(gp) = GuestPtr::<i32>::new(epoll_out as u64) {
        let _ = gp.write(HTTP_EPOLL_ID);
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_HTTP_DESTROY_EPOLL, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpDestroyEpoll")]
pub fn sce_http_destroy_epoll(_http_ctx_id: i32, _epoll_id: i32) -> i32 {
    0
}

/// `sceHttpUriEscape(out, required, size, in)`: percent-encoding. PURE STRING WORK — no
/// network involved, so this one is implemented for real rather than refused. A title escapes
/// a URI before it discovers it cannot send it, and handing back an empty string here would
/// corrupt whatever it logs or caches.
#[ps4_syscall(id = SyscallId::SCE_HTTP_URI_ESCAPE, lib = crate::libs::LIB_SCE_HTTP, name = "sceHttpUriEscape")]
pub fn sce_http_uri_escape(out: u64, required_out: *mut u64, size: u64, input: u64) -> i32 {
    const UNRESERVED: &[u8] = b"-_.!~*'()";
    let Some(src) = ps4_core::guest_ptr::read_cstr(input, 4096) else {
        return SCE_HTTP_ERROR_INVALID_VALUE;
    };
    let mut escaped = Vec::with_capacity(src.len());
    for &b in src.as_bytes() {
        if b.is_ascii_alphanumeric() || UNRESERVED.contains(&b) {
            escaped.push(b);
        } else {
            escaped.extend_from_slice(format!("%{b:02X}").as_bytes());
        }
    }
    escaped.push(0);

    if let Some(gp) = GuestPtr::<u64>::new(required_out as u64) {
        let _ = gp.write(escaped.len() as u64);
    }
    // A caller may size the buffer first with out == NULL; that is a pure size query and
    // succeeds (required_out written above). But a NON-NULL buffer too small to hold the
    // escaped string is an error, not a success: returning 0 here would let a single-pass
    // caller (one that checks only the return code, not required_out) treat its untouched
    // buffer as the encoded URI and log/cache stack garbage. Refuse so it retries with a
    // buffer sized from required_out.
    if out != 0 && (size as usize) < escaped.len() {
        return SCE_HTTP_ERROR_INVALID_VALUE;
    }
    if out != 0
        && let Some(gs) = ps4_core::guest_ptr::GuestSlice::<u8>::new(out, escaped.len())
    {
        let _ = gs.write_slice(&escaped);
    }
    0
}
