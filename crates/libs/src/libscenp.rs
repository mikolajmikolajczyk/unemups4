//! PSN — the player's account, and every service that hangs off it — answered as a console
//! that is **signed out**.
//!
//! The three rules this file follows are stated in full in [`crate::libsceoffline`], which
//! models the layer below it (link state, HTTP). They are worth restating in one line each,
//! because 150-odd entry points here are just those rules applied over and over:
//!
//! 1. **Initialisation and object creation succeed** — they are local work, and the library
//!    really is present.
//! 2. **State says signed out** — `sceNpGetState` is where the truth is told.
//! 3. **Anything that would reach a Sony server refuses IMMEDIATELY** — never "in progress",
//!    because a completion that never arrives turns a clean refusal into a hang.
//!
//! Rule 3 has extra force here. Most of PSN is asynchronous: a title creates a request,
//! fires it, then polls. Every `*Async` call below therefore refuses at the *fire*, so the
//! title never reaches a poll loop. The one thing never to do in this file is return
//! "pending" for something not modelled yet.
//!
//! Most handlers below take no arguments at all. That is not laziness: their verdict does
//! not depend on the arguments, and declaring parameters we never read would imply we had
//! verified signatures we have not. The few that *do* take arguments are the ones that
//! write through an out-pointer, where the position matters.
//!
//! Out-parameters are left untouched on refusal. A zero-filled `OrbisNpId` or ranking board
//! is not "no data" — it is *plausible* data with empty fields, which a title may cache,
//! compare or display. Refusing leaves no such ghost behind.

use crate::context::NativeContext;
use ps4_core::guest_ptr::GuestPtr;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;

// ---------------------------------------------------------------------------
// libSceNpManager — the player's PSN identity. Offline, that identity is SIGNED OUT.
// ---------------------------------------------------------------------------

/// `SCE_NP_STATE_SIGNED_OUT`. The documented NP state enum is
/// `UNKNOWN = 0, SIGNED_OUT = 1, SIGNED_IN = 2`.
///
/// Reporting SIGNED_OUT rather than UNKNOWN is deliberate: UNKNOWN invites a title to wait
/// for the state to settle, and waiting for a state that never changes is the unbounded
/// poll of doc-5 case 28. SIGNED_OUT is a settled answer with an offline path behind it.
///
/// If a title ever behaves as though it were online, this constant is the first thing to
/// suspect — but rule 3 backstops it: every operation refuses regardless of state, so a
/// wrong value here costs a wasted attempt, not a hang.
const NP_STATE_SIGNED_OUT: i32 = 1;

/// A refusal for anything that needs a signed-in account. Deliberately not dressed up as a
/// documented `SCE_NP_ERROR_*`: we do not have the real values, and inventing a plausible
/// `0x80550xxx` would be indistinguishable from a verified constant to the next reader.
/// A title branches on `< 0` either way.
const NP_REFUSED: i32 = -1;

/// Opaque ids handed back for NP request/context objects. Creating them is local work that
/// succeeds (rule 1); what refuses is using them (rule 3). Distinct values so a log shows
/// which kind a guest passed back.
const NP_REQUEST_ID: i32 = 0x501;
const NP_CONTEXT_ID: i32 = 0x511;
const NP_TSS_CTX_ID: i32 = 0x601;

/// `sceNpSetNpTitleId(titleId, titleSecret)` — register which PSN title this is. Purely
/// local bookkeeping; it names the title, it does not contact anything.
#[ps4_syscall(id = SyscallId::SCE_NP_SET_NP_TITLE_ID, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpSetNpTitleId")]
pub fn sce_np_set_np_title_id(_title_id: u64, _title_secret: u64) -> i32 {
    0
}

/// `sceNpSetContentRestriction(restriction)` — the title declares its own age rating so the
/// system can enforce parental controls. A statement filed with the system, accepted.
#[ps4_syscall(id = SyscallId::SCE_NP_SET_CONTENT_RESTRICTION, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpSetContentRestriction")]
pub fn sce_np_set_content_restriction(_restriction: u64) -> i32 {
    0
}

/// `sceNpGetState(userId, state*)` — the one call where "offline" is actually said for NP.
/// Succeeds, and writes SIGNED_OUT.
#[ps4_syscall(id = SyscallId::SCE_NP_GET_STATE, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpGetState")]
pub fn sce_np_get_state(_user_id: i32, state_out: u64) -> i32 {
    let Some(gp) = GuestPtr::<i32>::new(state_out) else {
        return NP_REFUSED;
    };
    // Propagate a failed write: the per-VMA write seam can reject an in-arena address on an
    // unmapped page, and reporting success (0) while leaving the out-param untouched would let
    // the title read a ghost UNKNOWN(0) and wait for the state to "settle" (doc-5 case 28).
    // Refuse instead — matching this file's "out-params untouched on refusal" invariant.
    if gp.write(NP_STATE_SIGNED_OUT).is_err() {
        return NP_REFUSED;
    }
    0
}

/// `sceNpCheckCallback()` — pump queued NP state-change callbacks on the calling thread.
/// Succeeds and delivers nothing, because nothing changed. Never synthesise a state event:
/// a fabricated "signed in" would push the title down a path with no account behind it,
/// which is the NetCtl-callback mistake in a different library.
#[ps4_syscall(id = SyscallId::SCE_NP_CHECK_CALLBACK, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpCheckCallback")]
pub fn sce_np_check_callback() -> i32 {
    0
}

/// `sceNpCheckCallbackForLib(ctx)` — the same pump, scoped to one library's context.
#[ps4_syscall(id = SyscallId::SCE_NP_CHECK_CALLBACK_FOR_LIB, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpCheckCallbackForLib")]
pub fn sce_np_check_callback_for_lib(_ctx: u64) -> i32 {
    0
}

/// `sceNpRegisterStateCallbackForToolkit(cb, userdata)` — remember a callback we will never
/// call. Registration succeeds; the callback stays silent for the reason above.
#[ps4_syscall(id = SyscallId::SCE_NP_REGISTER_STATE_CALLBACK_FOR_TOOLKIT, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpRegisterStateCallbackForToolkit")]
pub fn sce_np_register_state_callback_for_toolkit(_cb: u64, _userdata: u64) -> i32 {
    0
}

/// `sceNpUnregisterStateCallbackForToolkit()` — forget it again.
#[ps4_syscall(id = SyscallId::SCE_NP_UNREGISTER_STATE_CALLBACK_FOR_TOOLKIT, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpUnregisterStateCallbackForToolkit")]
pub fn sce_np_unregister_state_callback_for_toolkit() -> i32 {
    0
}

/// `sceNpCreateRequest()` — mint a request handle. Local allocation, so it succeeds; the
/// refusal belongs on the request's *use*, exactly as with HTTP.
#[ps4_syscall(id = SyscallId::SCE_NP_CREATE_REQUEST, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpCreateRequest")]
pub fn sce_np_create_request() -> i32 {
    NP_REQUEST_ID
}

/// `sceNpDeleteRequest(reqId)` — release it.
#[ps4_syscall(id = SyscallId::SCE_NP_DELETE_REQUEST, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpDeleteRequest")]
pub fn sce_np_delete_request(_req_id: i32) -> i32 {
    0
}

/// `sceNpCheckNpAvailability(reqId, ...)` — "can this account use PSN right now?". No, and
/// the answer comes back immediately rather than as an in-progress request.
#[ps4_syscall(id = SyscallId::SCE_NP_CHECK_NP_AVAILABILITY, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpCheckNpAvailability")]
pub fn sce_np_check_np_availability(_req_id: i32, _arg1: u64, _arg2: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpCheckNpAvailabilityA(reqId, ...)` — the newer form of the same question.
#[ps4_syscall(id = SyscallId::SCE_NP_CHECK_NP_AVAILABILITY_A, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpCheckNpAvailabilityA")]
pub fn sce_np_check_np_availability_a(_req_id: i32, _arg1: u64, _arg2: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpCheckPlus(reqId, ...)` — is this account a PS Plus subscriber? There is no account
/// to subscribe. Refused immediately, never pending.
#[ps4_syscall(id = SyscallId::SCE_NP_CHECK_PLUS, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpCheckPlus")]
pub fn sce_np_check_plus(_req_id: i32, _arg1: u64, _arg2: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpNotifyPlusFeature(...)` — the title tells the system a Plus-gated feature was
/// used. A notification, not a question; accepted and dropped.
#[ps4_syscall(id = SyscallId::SCE_NP_NOTIFY_PLUS_FEATURE, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpNotifyPlusFeature")]
pub fn sce_np_notify_plus_feature(_arg0: u64, _arg1: u64) -> i32 {
    0
}

/// `sceNpGetNpId(userId, npId*)` — the signed-in player's PSN id. There is none, and the
/// out-struct is left untouched: a zeroed `OrbisNpId` is a *valid-looking* id with an empty
/// handle, which a title may then compare, cache or send. Refusing leaves no such ghost.
#[ps4_syscall(id = SyscallId::SCE_NP_GET_NP_ID, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpGetNpId")]
pub fn sce_np_get_np_id(_user_id: i32, _np_id_out: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpGetOnlineId(userId, onlineId*)` — the player's online handle. Same refusal, same
/// reason: an empty handle is not the same thing as no handle.
#[ps4_syscall(id = SyscallId::SCE_NP_GET_ONLINE_ID, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpGetOnlineId")]
pub fn sce_np_get_online_id(_user_id: i32, _online_id_out: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpGetAccountId(onlineId, accountId*)` — the numeric account behind a handle.
#[ps4_syscall(id = SyscallId::SCE_NP_GET_ACCOUNT_ID, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpGetAccountId")]
pub fn sce_np_get_account_id(_online_id: u64, _account_id_out: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpGetUserIdByOnlineId(onlineId, userId*)` — map a PSN handle to a local user. That
/// mapping is made when an account signs in; none has.
#[ps4_syscall(id = SyscallId::SCE_NP_GET_USER_ID_BY_ONLINE_ID, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpGetUserIdByOnlineId")]
pub fn sce_np_get_user_id_by_online_id(_online_id: u64, _user_id_out: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpGetParentalControlInfo(userId, age*, info*)` — the account's parental limits. They
/// live on the account; there is no account.
#[ps4_syscall(id = SyscallId::SCE_NP_GET_PARENTAL_CONTROL_INFO, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpGetParentalControlInfo")]
pub fn sce_np_get_parental_control_info(_user_id: i32, _age_out: u64, _info_out: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpCmpNpId(a, b)` — compare two PSN ids. Refused rather than answered: every path
/// that could have produced an id here refused, so any pointer reaching this call holds
/// something we did not write, and "equal"/"not equal" would both be inventions.
#[ps4_syscall(id = SyscallId::SCE_NP_CMP_NP_ID, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpCmpNpId")]
pub fn sce_np_cmp_np_id(_a: u64, _b: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpCmpOnlineId(a, b)` — the same, for online handles.
#[ps4_syscall(id = SyscallId::SCE_NP_CMP_ONLINE_ID, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpCmpOnlineId")]
pub fn sce_np_cmp_online_id(_a: u64, _b: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpCommerceShowPsStoreIcon(pos)` — show the store shortcut in the corner. A local UI
/// request with no store behind it; accepted so the title's UI bookkeeping stays balanced.
#[ps4_syscall(id = SyscallId::SCE_NP_COMMERCE_SHOW_PS_STORE_ICON, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpCommerceShowPsStoreIcon")]
pub fn sce_np_commerce_show_ps_store_icon(_pos: i32) -> i32 {
    0
}

/// `sceNpCommerceHidePsStoreIcon()` — hide it again.
#[ps4_syscall(id = SyscallId::SCE_NP_COMMERCE_HIDE_PS_STORE_ICON, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpCommerceHidePsStoreIcon")]
pub fn sce_np_commerce_hide_ps_store_icon() -> i32 {
    0
}

// --- NP TSS: title-small-storage, a server-side blob fetched by title id. ---

/// `sceNpTssCreateNpTitleCtx(...)` — mint a TSS context. Local; succeeds.
#[ps4_syscall(id = SyscallId::SCE_NP_TSS_CREATE_NP_TITLE_CTX, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpTssCreateNpTitleCtx")]
pub fn sce_np_tss_create_np_title_ctx(_service_label: u64, _np_id: u64) -> i32 {
    NP_TSS_CTX_ID
}

/// `sceNpTssGetData(...)` — fetch the blob. It lives on a Sony server. Refused.
#[ps4_syscall(id = SyscallId::SCE_NP_TSS_GET_DATA, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpTssGetData")]
pub fn sce_np_tss_get_data(_ctx: i32, _req: u64, _arg2: u64, _arg3: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpTssGetDataAsync(...)` — the async form. Refused *now*, not later: an async call
/// answered with "in progress" is a promise of a completion we can never deliver.
#[ps4_syscall(id = SyscallId::SCE_NP_TSS_GET_DATA_ASYNC, lib = crate::libs::LIB_SCE_NP_MANAGER, name = "sceNpTssGetDataAsync")]
pub fn sce_np_tss_get_data_async(_ctx: i32, _req: u64, _arg2: u64, _arg3: u64) -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// libSceNpWebApi — the generic REST pipe to Sony's services. Shaped exactly like the
// HTTP library below it: build objects locally, refuse at the send.
// ---------------------------------------------------------------------------

const NP_WEBAPI_LIB_CTX_ID: i32 = 0x701;
const NP_WEBAPI_CTX_ID: i32 = 0x711;
const NP_WEBAPI_HANDLE_ID: i32 = 0x721;
const NP_WEBAPI_REQUEST_ID: i32 = 0x731;
const NP_WEBAPI_FILTER_ID: i32 = 0x741;
const NP_WEBAPI_CALLBACK_ID: i32 = 0x751;

/// `sceNpWebApiInitialize(libHttpCtxId, poolSize)` — start the Web API library on top of an
/// HTTP context. Returns a positive library context id. Succeeds: this is the call an engine
/// makes at boot, and nothing has been asked of the network yet.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_INITIALIZE, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiInitialize")]
pub fn sce_np_web_api_initialize(_http_ctx_id: i32, _pool_size: u64) -> i32 {
    NP_WEBAPI_LIB_CTX_ID
}

/// `sceNpWebApiTerminate(libCtxId)` — shut it down.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_TERMINATE, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiTerminate")]
pub fn sce_np_web_api_terminate(_lib_ctx_id: i32) -> i32 {
    0
}

/// `sceNpWebApiCreateContext(libCtxId, userId)` — a per-user context. Local; succeeds.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_CREATE_CONTEXT, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiCreateContext")]
pub fn sce_np_web_api_create_context(_lib_ctx_id: i32, _user_id: i32) -> i32 {
    NP_WEBAPI_CTX_ID
}

/// `sceNpWebApiDeleteContext(ctxId)` — release it.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_DELETE_CONTEXT, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiDeleteContext")]
pub fn sce_np_web_api_delete_context(_ctx_id: i32) -> i32 {
    0
}

/// `sceNpWebApiCreateHandle(libCtxId)` — a cancellation handle. Local; succeeds.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_CREATE_HANDLE, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiCreateHandle")]
pub fn sce_np_web_api_create_handle(_lib_ctx_id: i32) -> i32 {
    NP_WEBAPI_HANDLE_ID
}

/// `sceNpWebApiDeleteHandle(libCtxId, handleId)` — release it.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_DELETE_HANDLE, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiDeleteHandle")]
pub fn sce_np_web_api_delete_handle(_lib_ctx_id: i32, _handle_id: i32) -> i32 {
    0
}

/// `sceNpWebApiCreateRequest(...)` — build a REST request. Building it is local work, so it
/// succeeds; the refusal lands on `SendRequest2`, where the network would actually be used.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_CREATE_REQUEST, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiCreateRequest")]
pub fn sce_np_web_api_create_request(
    _ctx_id: i32,
    _api_group: u64,
    _path: u64,
    _method: i32,
    _content: u64,
    _req_id_out: u64,
) -> i32 {
    NP_WEBAPI_REQUEST_ID
}

/// `sceNpWebApiCreateMultipartRequest(...)` — the multipart form of the same.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_CREATE_MULTIPART_REQUEST, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiCreateMultipartRequest")]
pub fn sce_np_web_api_create_multipart_request(
    _ctx_id: i32,
    _api_group: u64,
    _path: u64,
    _method: i32,
    _req_id_out: u64,
) -> i32 {
    NP_WEBAPI_REQUEST_ID
}

/// `sceNpWebApiDeleteRequest(reqId)` — drop a request.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_DELETE_REQUEST, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiDeleteRequest")]
pub fn sce_np_web_api_delete_request(_req_id: i64) -> i32 {
    0
}

/// `sceNpWebApiAbortRequest(reqId)` — cancel one. Nothing is in flight, so the cancel has
/// already succeeded by the time it is asked for.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_ABORT_REQUEST, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiAbortRequest")]
pub fn sce_np_web_api_abort_request(_req_id: i64) -> i32 {
    0
}

/// `sceNpWebApiAddHttpRequestHeader(reqId, name, value)` — accumulate a header on the
/// request object. Local string work; accepted.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_ADD_HTTP_REQUEST_HEADER, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiAddHttpRequestHeader")]
pub fn sce_np_web_api_add_http_request_header(_req_id: i64, _name: u64, _value: u64) -> i32 {
    0
}

/// `sceNpWebApiAddMultipartPart(...)` — accumulate one part of a multipart body. Local.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_ADD_MULTIPART_PART, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiAddMultipartPart")]
pub fn sce_np_web_api_add_multipart_part(_req_id: i64, _part: u64, _part_size: u64) -> i32 {
    0
}

/// `sceNpWebApiSendRequest2(...)` — the send. **This** is where offline is said, and it is
/// said at once rather than by leaving the request pending.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_SEND_REQUEST2, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiSendRequest2")]
pub fn sce_np_web_api_send_request2(
    _req_id: i64,
    _data: u64,
    _data_size: u64,
    _resp_out: u64,
) -> i32 {
    NP_REFUSED
}

/// `sceNpWebApiSendMultipartRequest2(...)` — same send, multipart body.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_SEND_MULTIPART_REQUEST2, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiSendMultipartRequest2")]
pub fn sce_np_web_api_send_multipart_request2(_req_id: i64, _resp_out: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpWebApiReadData(reqId, buf, size)` — read the response body. No request ever left,
/// so there is no body. Refused rather than answered with 0 bytes: "zero bytes read" reads
/// as a complete, empty response, which a title will happily parse as valid JSON-of-nothing.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_READ_DATA, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiReadData")]
pub fn sce_np_web_api_read_data(_req_id: i64, _buf: u64, _size: u64) -> i32 {
    NP_REFUSED
}

/// `sceNpWebApiGetHttpResponseHeaderValue(...)` — a header from a response that never came.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_GET_HTTP_RESPONSE_HEADER_VALUE, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiGetHttpResponseHeaderValue")]
pub fn sce_np_web_api_get_http_response_header_value(
    _req_id: i64,
    _name: u64,
    _value_out: u64,
    _size: u64,
) -> i32 {
    NP_REFUSED
}

/// `sceNpWebApiGetHttpResponseHeaderValueLength(...)` — its length. Same absence.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_GET_HTTP_RESPONSE_HEADER_VALUE_LENGTH, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiGetHttpResponseHeaderValueLength")]
pub fn sce_np_web_api_get_http_response_header_value_length(
    _req_id: i64,
    _name: u64,
    _len_out: u64,
) -> i32 {
    NP_REFUSED
}

// --- Push events: server-initiated notifications. The filters and callbacks are local
// bookkeeping and succeed; no event is ever delivered, because none can arrive. ---

/// `sceNpWebApiCreatePushEventFilter(...)` — describe which push events to receive.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_CREATE_PUSH_EVENT_FILTER, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiCreatePushEventFilter")]
pub fn sce_np_web_api_create_push_event_filter(
    _lib_ctx_id: i32,
    _filters: u64,
    _count: u64,
) -> i32 {
    NP_WEBAPI_FILTER_ID
}

/// `sceNpWebApiDeletePushEventFilter(libCtxId, filterId)` — drop it.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_DELETE_PUSH_EVENT_FILTER, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiDeletePushEventFilter")]
pub fn sce_np_web_api_delete_push_event_filter(_lib_ctx_id: i32, _filter_id: i32) -> i32 {
    0
}

/// `sceNpWebApiCreateServicePushEventFilter(...)` — the service-scoped variant.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_CREATE_SERVICE_PUSH_EVENT_FILTER, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiCreateServicePushEventFilter")]
pub fn sce_np_web_api_create_service_push_event_filter(
    _lib_ctx_id: i32,
    _ctx_id: i32,
    _service: u64,
    _filters: u64,
    _count: u64,
) -> i32 {
    NP_WEBAPI_FILTER_ID
}

/// `sceNpWebApiDeleteServicePushEventFilter(libCtxId, filterId)` — drop it.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_DELETE_SERVICE_PUSH_EVENT_FILTER, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiDeleteServicePushEventFilter")]
pub fn sce_np_web_api_delete_service_push_event_filter(_lib_ctx_id: i32, _filter_id: i32) -> i32 {
    0
}

/// `sceNpWebApiRegisterPushEventCallback(...)` — remember a callback that will never fire.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_REGISTER_PUSH_EVENT_CALLBACK, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiRegisterPushEventCallback")]
pub fn sce_np_web_api_register_push_event_callback(
    _ctx_id: i32,
    _filter_id: i32,
    _cb: u64,
    _userdata: u64,
) -> i32 {
    NP_WEBAPI_CALLBACK_ID
}

/// `sceNpWebApiUnregisterPushEventCallback(ctxId, cbId)` — forget it.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_UNREGISTER_PUSH_EVENT_CALLBACK, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiUnregisterPushEventCallback")]
pub fn sce_np_web_api_unregister_push_event_callback(_ctx_id: i32, _cb_id: i32) -> i32 {
    0
}

/// `sceNpWebApiRegisterServicePushEventCallback(...)` — service-scoped registration.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_REGISTER_SERVICE_PUSH_EVENT_CALLBACK, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiRegisterServicePushEventCallback")]
pub fn sce_np_web_api_register_service_push_event_callback(
    _ctx_id: i32,
    _filter_id: i32,
    _cb: u64,
    _userdata: u64,
) -> i32 {
    NP_WEBAPI_CALLBACK_ID
}

/// `sceNpWebApiUnregisterServicePushEventCallback(ctxId, cbId)` — forget it.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_UNREGISTER_SERVICE_PUSH_EVENT_CALLBACK, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiUnregisterServicePushEventCallback")]
pub fn sce_np_web_api_unregister_service_push_event_callback(_ctx_id: i32, _cb_id: i32) -> i32 {
    0
}

/// `sceNpWebApiUtilityParseNpId(json, npId*)` — parse an NP id out of a JSON response.
/// Refused: every path that could have produced that JSON refused first, so the input here
/// cannot be something we produced, and filling the out-struct would invent an identity.
#[ps4_syscall(id = SyscallId::SCE_NP_WEB_API_UTILITY_PARSE_NP_ID, lib = crate::libs::LIB_SCE_NP_WEB_API, name = "sceNpWebApiUtilityParseNpId")]
pub fn sce_np_web_api_utility_parse_np_id(_json: u64, _np_id_out: u64) -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// libSceNpScore — leaderboards. Every board lives on a Sony server, so every read and
// every write refuses; only the local context/request objects are real.
// ---------------------------------------------------------------------------

/// `sceNpScoreCreateNpTitleCtx()` — a local context id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_CREATE_NP_TITLE_CTX,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreCreateNpTitleCtx"
)]
pub fn sce_np_score_create_np_title_ctx() -> i32 {
    NP_CONTEXT_ID
}

/// `sceNpScoreDeleteNpTitleCtx()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_DELETE_NP_TITLE_CTX,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreDeleteNpTitleCtx"
)]
pub fn sce_np_score_delete_np_title_ctx() -> i32 {
    0
}

/// `sceNpScoreCreateRequest()` — a local request id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_CREATE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreCreateRequest"
)]
pub fn sce_np_score_create_request() -> i32 {
    NP_REQUEST_ID
}

/// `sceNpScoreDeleteRequest()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_DELETE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreDeleteRequest"
)]
pub fn sce_np_score_delete_request() -> i32 {
    0
}

/// `sceNpScoreAbortRequest()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_ABORT_REQUEST,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreAbortRequest"
)]
pub fn sce_np_score_abort_request() -> i32 {
    0
}

/// `sceNpScoreGetBoardInfo()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_GET_BOARD_INFO,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreGetBoardInfo"
)]
pub fn sce_np_score_get_board_info() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreGetFriendsRanking()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_GET_FRIENDS_RANKING,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreGetFriendsRanking"
)]
pub fn sce_np_score_get_friends_ranking() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreGetFriendsRankingAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_GET_FRIENDS_RANKING_ASYNC,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreGetFriendsRankingAsync"
)]
pub fn sce_np_score_get_friends_ranking_async() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreGetGameData()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_GET_GAME_DATA,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreGetGameData"
)]
pub fn sce_np_score_get_game_data() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreGetGameDataAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_GET_GAME_DATA_ASYNC,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreGetGameDataAsync"
)]
pub fn sce_np_score_get_game_data_async() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreGetRankingByNpId()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_GET_RANKING_BY_NP_ID,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreGetRankingByNpId"
)]
pub fn sce_np_score_get_ranking_by_np_id() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreGetRankingByNpIdAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_GET_RANKING_BY_NP_ID_ASYNC,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreGetRankingByNpIdAsync"
)]
pub fn sce_np_score_get_ranking_by_np_id_async() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreGetRankingByRange()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_GET_RANKING_BY_RANGE,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreGetRankingByRange"
)]
pub fn sce_np_score_get_ranking_by_range() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreGetRankingByRangeAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_GET_RANKING_BY_RANGE_ASYNC,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreGetRankingByRangeAsync"
)]
pub fn sce_np_score_get_ranking_by_range_async() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreRecordGameData()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_RECORD_GAME_DATA,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreRecordGameData"
)]
pub fn sce_np_score_record_game_data() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreRecordGameDataAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_RECORD_GAME_DATA_ASYNC,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreRecordGameDataAsync"
)]
pub fn sce_np_score_record_game_data_async() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreRecordScore()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_RECORD_SCORE,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreRecordScore"
)]
pub fn sce_np_score_record_score() -> i32 {
    NP_REFUSED
}

/// `sceNpScoreRecordScoreAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_RECORD_SCORE_ASYNC,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScoreRecordScoreAsync"
)]
pub fn sce_np_score_record_score_async() -> i32 {
    NP_REFUSED
}

/// `sceNpScorePollAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SCORE_POLL_ASYNC,
    lib = crate::libs::LIB_SCE_NP_SCORE,
    name = "sceNpScorePollAsync"
)]
pub fn sce_np_score_poll_async() -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// libSceNpTus — Title User Storage: per-player key/value data kept on Sony's servers.
// Same shape as Score: local objects succeed, every transfer refuses.
// ---------------------------------------------------------------------------

/// `sceNpTusCreateNpTitleCtx()` — a local context id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_CREATE_NP_TITLE_CTX,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusCreateNpTitleCtx"
)]
pub fn sce_np_tus_create_np_title_ctx() -> i32 {
    NP_CONTEXT_ID
}

/// `sceNpTusDeleteNpTitleCtx()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_DELETE_NP_TITLE_CTX,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusDeleteNpTitleCtx"
)]
pub fn sce_np_tus_delete_np_title_ctx() -> i32 {
    0
}

/// `sceNpTusCreateRequest()` — a local request id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_CREATE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusCreateRequest"
)]
pub fn sce_np_tus_create_request() -> i32 {
    NP_REQUEST_ID
}

/// `sceNpTusDeleteRequest()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_DELETE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusDeleteRequest"
)]
pub fn sce_np_tus_delete_request() -> i32 {
    0
}

/// `sceNpTusPollAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_POLL_ASYNC,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusPollAsync"
)]
pub fn sce_np_tus_poll_async() -> i32 {
    NP_REFUSED
}

/// `sceNpTusAddAndGetVariable()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_ADD_AND_GET_VARIABLE,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusAddAndGetVariable"
)]
pub fn sce_np_tus_add_and_get_variable() -> i32 {
    NP_REFUSED
}

/// `sceNpTusGetData()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_GET_DATA,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusGetData"
)]
pub fn sce_np_tus_get_data() -> i32 {
    NP_REFUSED
}

/// `sceNpTusGetDataAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_GET_DATA_ASYNC,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusGetDataAsync"
)]
pub fn sce_np_tus_get_data_async() -> i32 {
    NP_REFUSED
}

/// `sceNpTusSetData()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_SET_DATA,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusSetData"
)]
pub fn sce_np_tus_set_data() -> i32 {
    NP_REFUSED
}

/// `sceNpTusSetDataAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_SET_DATA_ASYNC,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusSetDataAsync"
)]
pub fn sce_np_tus_set_data_async() -> i32 {
    NP_REFUSED
}

/// `sceNpTusGetMultiSlotVariable()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_GET_MULTI_SLOT_VARIABLE,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusGetMultiSlotVariable"
)]
pub fn sce_np_tus_get_multi_slot_variable() -> i32 {
    NP_REFUSED
}

/// `sceNpTusSetMultiSlotVariable()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_SET_MULTI_SLOT_VARIABLE,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusSetMultiSlotVariable"
)]
pub fn sce_np_tus_set_multi_slot_variable() -> i32 {
    NP_REFUSED
}

/// `sceNpTusGetMultiSlotDataStatusAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_GET_MULTI_SLOT_DATA_STATUS_ASYNC,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusGetMultiSlotDataStatusAsync"
)]
pub fn sce_np_tus_get_multi_slot_data_status_async() -> i32 {
    NP_REFUSED
}

/// `sceNpTusDeleteMultiSlotData()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_DELETE_MULTI_SLOT_DATA,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusDeleteMultiSlotData"
)]
pub fn sce_np_tus_delete_multi_slot_data() -> i32 {
    NP_REFUSED
}

/// `sceNpTusDeleteMultiSlotDataAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_TUS_DELETE_MULTI_SLOT_DATA_ASYNC,
    lib = crate::libs::LIB_SCE_NP_TUS,
    name = "sceNpTusDeleteMultiSlotDataAsync"
)]
pub fn sce_np_tus_delete_multi_slot_data_async() -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// libSceNpLookup — resolving PSN handles to account ids through the directory service.
// ---------------------------------------------------------------------------

/// `sceNpLookupCreateTitleCtx()` — a local context id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_LOOKUP_CREATE_TITLE_CTX,
    lib = crate::libs::LIB_SCE_NP_LOOKUP,
    name = "sceNpLookupCreateTitleCtx"
)]
pub fn sce_np_lookup_create_title_ctx() -> i32 {
    NP_CONTEXT_ID
}

/// `sceNpLookupDeleteTitleCtx()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_LOOKUP_DELETE_TITLE_CTX,
    lib = crate::libs::LIB_SCE_NP_LOOKUP,
    name = "sceNpLookupDeleteTitleCtx"
)]
pub fn sce_np_lookup_delete_title_ctx() -> i32 {
    0
}

/// `sceNpLookupCreateRequest()` — a local request id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_LOOKUP_CREATE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_LOOKUP,
    name = "sceNpLookupCreateRequest"
)]
pub fn sce_np_lookup_create_request() -> i32 {
    NP_REQUEST_ID
}

/// `sceNpLookupCreateAsyncRequest()` — a local request id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_LOOKUP_CREATE_ASYNC_REQUEST,
    lib = crate::libs::LIB_SCE_NP_LOOKUP,
    name = "sceNpLookupCreateAsyncRequest"
)]
pub fn sce_np_lookup_create_async_request() -> i32 {
    NP_REQUEST_ID
}

/// `sceNpLookupDeleteRequest()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_LOOKUP_DELETE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_LOOKUP,
    name = "sceNpLookupDeleteRequest"
)]
pub fn sce_np_lookup_delete_request() -> i32 {
    0
}

/// `sceNpLookupNpId()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_LOOKUP_NP_ID,
    lib = crate::libs::LIB_SCE_NP_LOOKUP,
    name = "sceNpLookupNpId"
)]
pub fn sce_np_lookup_np_id() -> i32 {
    NP_REFUSED
}

/// `sceNpLookupPollAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_LOOKUP_POLL_ASYNC,
    lib = crate::libs::LIB_SCE_NP_LOOKUP,
    name = "sceNpLookupPollAsync"
)]
pub fn sce_np_lookup_poll_async() -> i32 {
    NP_REFUSED
}

/// `sceNpLookupWaitAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_LOOKUP_WAIT_ASYNC,
    lib = crate::libs::LIB_SCE_NP_LOOKUP,
    name = "sceNpLookupWaitAsync"
)]
pub fn sce_np_lookup_wait_async() -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// libSceNpWordFilter — server-side profanity filtering for player-entered text. Refusing
// is the safe direction: a title that cannot get text sanitised should decline to publish
// it, and there is nowhere to publish it to anyway.
// ---------------------------------------------------------------------------

/// `sceNpWordFilterCreateTitleCtx()` — a local context id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_WORD_FILTER_CREATE_TITLE_CTX,
    lib = crate::libs::LIB_SCE_NP_WORD_FILTER,
    name = "sceNpWordFilterCreateTitleCtx"
)]
pub fn sce_np_word_filter_create_title_ctx() -> i32 {
    NP_CONTEXT_ID
}

/// `sceNpWordFilterDeleteTitleCtx()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_WORD_FILTER_DELETE_TITLE_CTX,
    lib = crate::libs::LIB_SCE_NP_WORD_FILTER,
    name = "sceNpWordFilterDeleteTitleCtx"
)]
pub fn sce_np_word_filter_delete_title_ctx() -> i32 {
    0
}

/// `sceNpWordFilterCreateRequest()` — a local request id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_WORD_FILTER_CREATE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_WORD_FILTER,
    name = "sceNpWordFilterCreateRequest"
)]
pub fn sce_np_word_filter_create_request() -> i32 {
    NP_REQUEST_ID
}

/// `sceNpWordFilterCreateAsyncRequest()` — a local request id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_WORD_FILTER_CREATE_ASYNC_REQUEST,
    lib = crate::libs::LIB_SCE_NP_WORD_FILTER,
    name = "sceNpWordFilterCreateAsyncRequest"
)]
pub fn sce_np_word_filter_create_async_request() -> i32 {
    NP_REQUEST_ID
}

/// `sceNpWordFilterDeleteRequest()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_WORD_FILTER_DELETE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_WORD_FILTER,
    name = "sceNpWordFilterDeleteRequest"
)]
pub fn sce_np_word_filter_delete_request() -> i32 {
    0
}

/// `sceNpWordFilterAbortRequest()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_WORD_FILTER_ABORT_REQUEST,
    lib = crate::libs::LIB_SCE_NP_WORD_FILTER,
    name = "sceNpWordFilterAbortRequest"
)]
pub fn sce_np_word_filter_abort_request() -> i32 {
    0
}

/// `sceNpWordFilterCensorComment()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_WORD_FILTER_CENSOR_COMMENT,
    lib = crate::libs::LIB_SCE_NP_WORD_FILTER,
    name = "sceNpWordFilterCensorComment"
)]
pub fn sce_np_word_filter_censor_comment() -> i32 {
    NP_REFUSED
}

/// `sceNpWordFilterSanitizeComment()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_WORD_FILTER_SANITIZE_COMMENT,
    lib = crate::libs::LIB_SCE_NP_WORD_FILTER,
    name = "sceNpWordFilterSanitizeComment"
)]
pub fn sce_np_word_filter_sanitize_comment() -> i32 {
    NP_REFUSED
}

/// `sceNpWordFilterPollAsync()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_WORD_FILTER_POLL_ASYNC,
    lib = crate::libs::LIB_SCE_NP_WORD_FILTER,
    name = "sceNpWordFilterPollAsync"
)]
pub fn sce_np_word_filter_poll_async() -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// libSceNpAuth — obtaining an OAuth authorization code for a title's own web service.
// It is minted by Sony against a signed-in account; there is neither.
// ---------------------------------------------------------------------------

/// `sceNpAuthCreateRequest()` — a local request id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_AUTH_CREATE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_AUTH,
    name = "sceNpAuthCreateRequest"
)]
pub fn sce_np_auth_create_request() -> i32 {
    NP_REQUEST_ID
}

/// `sceNpAuthDeleteRequest()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_AUTH_DELETE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_AUTH,
    name = "sceNpAuthDeleteRequest"
)]
pub fn sce_np_auth_delete_request() -> i32 {
    0
}

/// `sceNpAuthGetAuthorizationCode()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_AUTH_GET_AUTHORIZATION_CODE,
    lib = crate::libs::LIB_SCE_NP_AUTH,
    name = "sceNpAuthGetAuthorizationCode"
)]
pub fn sce_np_auth_get_authorization_code() -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// libSceNpBandwidthTest — measuring the link to Sony's servers before matchmaking.
// There is no link to measure, and the measurement refuses instead of reporting zero:
// a zero-bandwidth result is a *number*, and a title may render it as a real reading.
// ---------------------------------------------------------------------------

/// `sceNpBandwidthTestInitStart()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_BANDWIDTH_TEST_INIT_START,
    lib = crate::libs::LIB_SCE_NP_BANDWIDTH_TEST,
    name = "sceNpBandwidthTestInitStart"
)]
pub fn sce_np_bandwidth_test_init_start() -> i32 {
    NP_REFUSED
}

/// `sceNpBandwidthTestGetStatus()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_BANDWIDTH_TEST_GET_STATUS,
    lib = crate::libs::LIB_SCE_NP_BANDWIDTH_TEST,
    name = "sceNpBandwidthTestGetStatus"
)]
pub fn sce_np_bandwidth_test_get_status() -> i32 {
    NP_REFUSED
}

/// `sceNpBandwidthTestShutdown()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_BANDWIDTH_TEST_SHUTDOWN,
    lib = crate::libs::LIB_SCE_NP_BANDWIDTH_TEST,
    name = "sceNpBandwidthTestShutdown"
)]
pub fn sce_np_bandwidth_test_shutdown() -> i32 {
    0
}

// ---------------------------------------------------------------------------
// libSceNpInGameMessage — sending messages to another player's PSN inbox from inside the
// game. Setup is local and succeeds; delivery refuses.
// ---------------------------------------------------------------------------

/// `sceNpInGameMessageInitialize()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_IN_GAME_MESSAGE_INITIALIZE,
    lib = crate::libs::LIB_SCE_NP_IN_GAME_MESSAGE,
    name = "sceNpInGameMessageInitialize"
)]
pub fn sce_np_in_game_message_initialize() -> i32 {
    0
}

/// `sceNpInGameMessageTerminate()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_IN_GAME_MESSAGE_TERMINATE,
    lib = crate::libs::LIB_SCE_NP_IN_GAME_MESSAGE,
    name = "sceNpInGameMessageTerminate"
)]
pub fn sce_np_in_game_message_terminate() -> i32 {
    0
}

/// `sceNpInGameMessageCreateHandle()` — a local context id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_IN_GAME_MESSAGE_CREATE_HANDLE,
    lib = crate::libs::LIB_SCE_NP_IN_GAME_MESSAGE,
    name = "sceNpInGameMessageCreateHandle"
)]
pub fn sce_np_in_game_message_create_handle() -> i32 {
    NP_CONTEXT_ID
}

/// `sceNpInGameMessageDeleteHandle()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_IN_GAME_MESSAGE_DELETE_HANDLE,
    lib = crate::libs::LIB_SCE_NP_IN_GAME_MESSAGE,
    name = "sceNpInGameMessageDeleteHandle"
)]
pub fn sce_np_in_game_message_delete_handle() -> i32 {
    0
}

/// `sceNpInGameMessagePrepare()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_IN_GAME_MESSAGE_PREPARE,
    lib = crate::libs::LIB_SCE_NP_IN_GAME_MESSAGE,
    name = "sceNpInGameMessagePrepare"
)]
pub fn sce_np_in_game_message_prepare() -> i32 {
    NP_REFUSED
}

/// `sceNpInGameMessageSendData()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_IN_GAME_MESSAGE_SEND_DATA,
    lib = crate::libs::LIB_SCE_NP_IN_GAME_MESSAGE,
    name = "sceNpInGameMessageSendData"
)]
pub fn sce_np_in_game_message_send_data() -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// libSceNpSnsFacebook — the Facebook link Sony brokered on behalf of the account.
// ---------------------------------------------------------------------------

/// `sceNpSnsFacebookCreateRequest()` — a local request id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SNS_FACEBOOK_CREATE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_SNS_FACEBOOK,
    name = "sceNpSnsFacebookCreateRequest"
)]
pub fn sce_np_sns_facebook_create_request() -> i32 {
    NP_REQUEST_ID
}

/// `sceNpSnsFacebookDeleteRequest()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SNS_FACEBOOK_DELETE_REQUEST,
    lib = crate::libs::LIB_SCE_NP_SNS_FACEBOOK,
    name = "sceNpSnsFacebookDeleteRequest"
)]
pub fn sce_np_sns_facebook_delete_request() -> i32 {
    0
}

/// `sceNpSnsFacebookGetAccessToken()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_SNS_FACEBOOK_GET_ACCESS_TOKEN,
    lib = crate::libs::LIB_SCE_NP_SNS_FACEBOOK,
    name = "sceNpSnsFacebookGetAccessToken"
)]
pub fn sce_np_sns_facebook_get_access_token() -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// libSceNpMatching2 — rooms, lobbies and the signaling that connects players. The whole
// library is a conversation with a matchmaking server.
//
// Two things here are local and therefore succeed: creating/destroying a context, and
// registering callbacks. Registration matters — a title registers before it connects, and
// failing that would look like a broken library rather than an empty one. The callbacks are
// never invoked: a synthesised room event would put the title in a room that does not
// exist, with members that do not exist, which is far worse than never joining one.
//
// `ContextStart` is where the connection would be made, so that is where this refuses.
// ---------------------------------------------------------------------------

/// `sceNpMatching2Initialize()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_INITIALIZE,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2Initialize"
)]
pub fn sce_np_matching2_initialize() -> i32 {
    0
}

/// `sceNpMatching2Terminate()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_TERMINATE,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2Terminate"
)]
pub fn sce_np_matching2_terminate() -> i32 {
    0
}

/// `sceNpMatching2CreateContext()` — a local context id.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_CREATE_CONTEXT,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2CreateContext"
)]
pub fn sce_np_matching2_create_context() -> i32 {
    NP_CONTEXT_ID
}

/// `sceNpMatching2DestroyContext()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_DESTROY_CONTEXT,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2DestroyContext"
)]
pub fn sce_np_matching2_destroy_context() -> i32 {
    0
}

/// `sceNpMatching2ContextStart()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_CONTEXT_START,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2ContextStart"
)]
pub fn sce_np_matching2_context_start() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2ContextStop()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_CONTEXT_STOP,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2ContextStop"
)]
pub fn sce_np_matching2_context_stop() -> i32 {
    0
}

/// `sceNpMatching2RegisterContextCallback()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_REGISTER_CONTEXT_CALLBACK,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2RegisterContextCallback"
)]
pub fn sce_np_matching2_register_context_callback() -> i32 {
    0
}

/// `sceNpMatching2RegisterLobbyEventCallback()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_REGISTER_LOBBY_EVENT_CALLBACK,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2RegisterLobbyEventCallback"
)]
pub fn sce_np_matching2_register_lobby_event_callback() -> i32 {
    0
}

/// `sceNpMatching2RegisterRoomEventCallback()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_REGISTER_ROOM_EVENT_CALLBACK,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2RegisterRoomEventCallback"
)]
pub fn sce_np_matching2_register_room_event_callback() -> i32 {
    0
}

/// `sceNpMatching2RegisterRoomMessageCallback()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_REGISTER_ROOM_MESSAGE_CALLBACK,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2RegisterRoomMessageCallback"
)]
pub fn sce_np_matching2_register_room_message_callback() -> i32 {
    0
}

/// `sceNpMatching2RegisterSignalingCallback()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_REGISTER_SIGNALING_CALLBACK,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2RegisterSignalingCallback"
)]
pub fn sce_np_matching2_register_signaling_callback() -> i32 {
    0
}

/// `sceNpMatching2SetDefaultRequestOptParam()` — accepted.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_SET_DEFAULT_REQUEST_OPT_PARAM,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2SetDefaultRequestOptParam"
)]
pub fn sce_np_matching2_set_default_request_opt_param() -> i32 {
    0
}

/// `sceNpMatching2GetServerId()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_GET_SERVER_ID,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2GetServerId"
)]
pub fn sce_np_matching2_get_server_id() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2GetWorldInfoList()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_GET_WORLD_INFO_LIST,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2GetWorldInfoList"
)]
pub fn sce_np_matching2_get_world_info_list() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2GetUserInfoList()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_GET_USER_INFO_LIST,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2GetUserInfoList"
)]
pub fn sce_np_matching2_get_user_info_list() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2SetUserInfo()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_SET_USER_INFO,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2SetUserInfo"
)]
pub fn sce_np_matching2_set_user_info() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2CreateJoinRoom()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_CREATE_JOIN_ROOM,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2CreateJoinRoom"
)]
pub fn sce_np_matching2_create_join_room() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2JoinRoom()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_JOIN_ROOM,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2JoinRoom"
)]
pub fn sce_np_matching2_join_room() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2LeaveRoom()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_LEAVE_ROOM,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2LeaveRoom"
)]
pub fn sce_np_matching2_leave_room() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2JoinLobby()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_JOIN_LOBBY,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2JoinLobby"
)]
pub fn sce_np_matching2_join_lobby() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2SearchRoom()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_SEARCH_ROOM,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2SearchRoom"
)]
pub fn sce_np_matching2_search_room() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2KickoutRoomMember()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_KICKOUT_ROOM_MEMBER,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2KickoutRoomMember"
)]
pub fn sce_np_matching2_kickout_room_member() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2SendRoomChatMessage()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_SEND_ROOM_CHAT_MESSAGE,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2SendRoomChatMessage"
)]
pub fn sce_np_matching2_send_room_chat_message() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2SendRoomMessage()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_SEND_ROOM_MESSAGE,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2SendRoomMessage"
)]
pub fn sce_np_matching2_send_room_message() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2GetRoomDataExternalList()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_GET_ROOM_DATA_EXTERNAL_LIST,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2GetRoomDataExternalList"
)]
pub fn sce_np_matching2_get_room_data_external_list() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2GetRoomDataInternal()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_GET_ROOM_DATA_INTERNAL,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2GetRoomDataInternal"
)]
pub fn sce_np_matching2_get_room_data_internal() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2GetRoomMemberDataInternal()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_GET_ROOM_MEMBER_DATA_INTERNAL,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2GetRoomMemberDataInternal"
)]
pub fn sce_np_matching2_get_room_member_data_internal() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2SetRoomDataExternal()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_SET_ROOM_DATA_EXTERNAL,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2SetRoomDataExternal"
)]
pub fn sce_np_matching2_set_room_data_external() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2SetRoomDataInternal()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_SET_ROOM_DATA_INTERNAL,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2SetRoomDataInternal"
)]
pub fn sce_np_matching2_set_room_data_internal() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2SetRoomMemberDataInternal()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_SET_ROOM_MEMBER_DATA_INTERNAL,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2SetRoomMemberDataInternal"
)]
pub fn sce_np_matching2_set_room_member_data_internal() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2SignalingGetConnectionInfo()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_SIGNALING_GET_CONNECTION_INFO,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2SignalingGetConnectionInfo"
)]
pub fn sce_np_matching2_signaling_get_connection_info() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2SignalingGetConnectionStatus()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_SIGNALING_GET_CONNECTION_STATUS,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2SignalingGetConnectionStatus"
)]
pub fn sce_np_matching2_signaling_get_connection_status() -> i32 {
    NP_REFUSED
}

/// `sceNpMatching2SignalingGetPingInfo()` — refused immediately.
#[ps4_syscall(
    id = SyscallId::SCE_NP_MATCHING2_SIGNALING_GET_PING_INFO,
    lib = crate::libs::LIB_SCE_NP_MATCHING2,
    name = "sceNpMatching2SignalingGetPingInfo"
)]
pub fn sce_np_matching2_signaling_get_ping_info() -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// The NP dialogs — store, profile, Facebook link. System-drawn screens over the game.
//
// There is no system dialog surface here, so `Open` refuses. What matters more is
// `UpdateStatus`, the call a title pumps every frame while a dialog is up: it must never
// answer RUNNING. A title that opened a dialog and then polls for it to close will spin
// forever on a dialog that was never drawn — doc-5 case 28 wearing a UI costume. FINISHED
// is the only answer that lets the caller move on, and `GetResult` then refuses, so the
// title learns nothing came of it.
// ---------------------------------------------------------------------------

/// `SCE_COMMON_DIALOG_STATUS_FINISHED` — the dialog is over. See above for why this is the
/// only status this file ever reports.
const DIALOG_STATUS_FINISHED: i32 = 3;

/// `sceNpCommerceDialogInitialize()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_NP_COMMERCE_DIALOG_INITIALIZE, lib = crate::libs::LIB_SCE_NP_COMMERCE, name = "sceNpCommerceDialogInitialize")]
pub fn sce_np_commerce_dialog_initialize() -> i32 {
    0
}

/// `sceNpCommerceDialogTerminate()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_NP_COMMERCE_DIALOG_TERMINATE, lib = crate::libs::LIB_SCE_NP_COMMERCE, name = "sceNpCommerceDialogTerminate")]
pub fn sce_np_commerce_dialog_terminate() -> i32 {
    0
}

/// `sceNpCommerceDialogOpen()` — the PS Store overlay. Refused: no store, no surface.
#[ps4_syscall(id = SyscallId::SCE_NP_COMMERCE_DIALOG_OPEN, lib = crate::libs::LIB_SCE_NP_COMMERCE, name = "sceNpCommerceDialogOpen")]
pub fn sce_np_commerce_dialog_open() -> i32 {
    NP_REFUSED
}

/// `sceNpCommerceDialogUpdateStatus()` — always FINISHED, never RUNNING.
#[ps4_syscall(id = SyscallId::SCE_NP_COMMERCE_DIALOG_UPDATE_STATUS, lib = crate::libs::LIB_SCE_NP_COMMERCE, name = "sceNpCommerceDialogUpdateStatus")]
pub fn sce_np_commerce_dialog_update_status() -> i32 {
    DIALOG_STATUS_FINISHED
}

/// `sceNpCommerceDialogGetResult()` — what the player chose. Nothing was shown, so refused
/// rather than answered with a zeroed result the title would read as a real choice.
#[ps4_syscall(id = SyscallId::SCE_NP_COMMERCE_DIALOG_GET_RESULT, lib = crate::libs::LIB_SCE_NP_COMMERCE, name = "sceNpCommerceDialogGetResult")]
pub fn sce_np_commerce_dialog_get_result() -> i32 {
    NP_REFUSED
}

/// `sceNpProfileDialogInitialize()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_NP_PROFILE_DIALOG_INITIALIZE, lib = crate::libs::LIB_SCE_NP_PROFILE_DIALOG, name = "sceNpProfileDialogInitialize")]
pub fn sce_np_profile_dialog_initialize() -> i32 {
    0
}

/// `sceNpProfileDialogTerminate()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_NP_PROFILE_DIALOG_TERMINATE, lib = crate::libs::LIB_SCE_NP_PROFILE_DIALOG, name = "sceNpProfileDialogTerminate")]
pub fn sce_np_profile_dialog_terminate() -> i32 {
    0
}

/// `sceNpProfileDialogOpen()` — another player's PSN profile card. Refused.
#[ps4_syscall(id = SyscallId::SCE_NP_PROFILE_DIALOG_OPEN, lib = crate::libs::LIB_SCE_NP_PROFILE_DIALOG, name = "sceNpProfileDialogOpen")]
pub fn sce_np_profile_dialog_open() -> i32 {
    NP_REFUSED
}

/// `sceNpProfileDialogUpdateStatus()` — always FINISHED.
#[ps4_syscall(id = SyscallId::SCE_NP_PROFILE_DIALOG_UPDATE_STATUS, lib = crate::libs::LIB_SCE_NP_PROFILE_DIALOG, name = "sceNpProfileDialogUpdateStatus")]
pub fn sce_np_profile_dialog_update_status() -> i32 {
    DIALOG_STATUS_FINISHED
}

/// `sceNpSnsFacebookDialogInitialize()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_NP_SNS_FACEBOOK_DIALOG_INITIALIZE, lib = crate::libs::LIB_SCE_NP_SNS_FACEBOOK, name = "sceNpSnsFacebookDialogInitialize")]
pub fn sce_np_sns_facebook_dialog_initialize() -> i32 {
    0
}

/// `sceNpSnsFacebookDialogTerminate()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_NP_SNS_FACEBOOK_DIALOG_TERMINATE, lib = crate::libs::LIB_SCE_NP_SNS_FACEBOOK, name = "sceNpSnsFacebookDialogTerminate")]
pub fn sce_np_sns_facebook_dialog_terminate() -> i32 {
    0
}

/// `sceNpSnsFacebookDialogOpen()` — the Facebook link flow. Refused.
#[ps4_syscall(id = SyscallId::SCE_NP_SNS_FACEBOOK_DIALOG_OPEN, lib = crate::libs::LIB_SCE_NP_SNS_FACEBOOK, name = "sceNpSnsFacebookDialogOpen")]
pub fn sce_np_sns_facebook_dialog_open() -> i32 {
    NP_REFUSED
}

/// `sceNpSnsFacebookDialogUpdateStatus()` — always FINISHED.
#[ps4_syscall(id = SyscallId::SCE_NP_SNS_FACEBOOK_DIALOG_UPDATE_STATUS, lib = crate::libs::LIB_SCE_NP_SNS_FACEBOOK, name = "sceNpSnsFacebookDialogUpdateStatus")]
pub fn sce_np_sns_facebook_dialog_update_status() -> i32 {
    DIALOG_STATUS_FINISHED
}

/// `sceNpSnsFacebookDialogGetResult()` — refused; nothing was shown.
#[ps4_syscall(id = SyscallId::SCE_NP_SNS_FACEBOOK_DIALOG_GET_RESULT, lib = crate::libs::LIB_SCE_NP_SNS_FACEBOOK, name = "sceNpSnsFacebookDialogGetResult")]
pub fn sce_np_sns_facebook_dialog_get_result() -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// libSceVoiceQoS — the voice-chat transport between players. Endpoints are local objects
// and succeed; connecting them to a peer, and moving packets, refuse.
//
// `ReadPacket` refuses rather than returning "zero bytes available". Zero-bytes is the
// normal, expected answer on a live-but-idle voice link, so a title would keep polling
// forever; a refusal tells it the link is not there.
// ---------------------------------------------------------------------------

/// `sceVoiceQoSInit()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SINIT, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSInit")]
pub fn sce_voice_qo_s_init() -> i32 {
    0
}

/// `sceVoiceQoSEnd()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SEND, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSEnd")]
pub fn sce_voice_qo_s_end() -> i32 {
    0
}

/// `sceVoiceQoSCreateLocalEndpoint()` — a local endpoint id.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SCREATE_LOCAL_ENDPOINT, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSCreateLocalEndpoint")]
pub fn sce_voice_qo_s_create_local_endpoint() -> i32 {
    NP_CONTEXT_ID
}

/// `sceVoiceQoSDeleteLocalEndpoint()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SDELETE_LOCAL_ENDPOINT, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSDeleteLocalEndpoint")]
pub fn sce_voice_qo_s_delete_local_endpoint() -> i32 {
    0
}

/// `sceVoiceQoSCreateRemoteEndpoint()` — a local endpoint id.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SCREATE_REMOTE_ENDPOINT, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSCreateRemoteEndpoint")]
pub fn sce_voice_qo_s_create_remote_endpoint() -> i32 {
    NP_CONTEXT_ID
}

/// `sceVoiceQoSDeleteRemoteEndpoint()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SDELETE_REMOTE_ENDPOINT, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSDeleteRemoteEndpoint")]
pub fn sce_voice_qo_s_delete_remote_endpoint() -> i32 {
    0
}

/// `sceVoiceQoSConnect()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SCONNECT, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSConnect")]
pub fn sce_voice_qo_s_connect() -> i32 {
    NP_REFUSED
}

/// `sceVoiceQoSDisconnect()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SDISCONNECT, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSDisconnect")]
pub fn sce_voice_qo_s_disconnect() -> i32 {
    0
}

/// `sceVoiceQoSGetLocalEndpointAttribute()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SGET_LOCAL_ENDPOINT_ATTRIBUTE, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSGetLocalEndpointAttribute")]
pub fn sce_voice_qo_s_get_local_endpoint_attribute() -> i32 {
    NP_REFUSED
}

/// `sceVoiceQoSSetLocalEndpointAttribute()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SSET_LOCAL_ENDPOINT_ATTRIBUTE, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSSetLocalEndpointAttribute")]
pub fn sce_voice_qo_s_set_local_endpoint_attribute() -> i32 {
    NP_REFUSED
}

/// `sceVoiceQoSReadPacket()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SREAD_PACKET, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSReadPacket")]
pub fn sce_voice_qo_s_read_packet() -> i32 {
    NP_REFUSED
}

/// `sceVoiceQoSWritePacket()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_VOICE_QO_SWRITE_PACKET, lib = crate::libs::LIB_SCE_VOICE_QOS, name = "sceVoiceQoSWritePacket")]
pub fn sce_voice_qo_s_write_packet() -> i32 {
    NP_REFUSED
}

// ---------------------------------------------------------------------------
// The remaining PSN dialogs — sign-in, invitations, the in-game browser, custom-data
// upload. Same contract as the NP dialogs above: Open refuses, UpdateStatus always says
// FINISHED so no pump loop can hang, GetResult refuses.
//
// Sign-in deserves a note. It is the one dialog whose whole purpose is to CHANGE the
// answer `sceNpGetState` gives. Refusing to open it keeps that answer stable: the title
// asks the player to sign in, cannot, and stays on its offline path. Pretending the
// dialog succeeded would leave a title convinced an account had just signed in.
// ---------------------------------------------------------------------------

/// `sceSigninDialogInitialize()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_SIGNIN_DIALOG_INITIALIZE, lib = crate::libs::LIB_SCE_SIGNIN_DIALOG, name = "sceSigninDialogInitialize")]
pub fn sce_signin_dialog_initialize() -> i32 {
    0
}

/// `sceSigninDialogTerminate()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_SIGNIN_DIALOG_TERMINATE, lib = crate::libs::LIB_SCE_SIGNIN_DIALOG, name = "sceSigninDialogTerminate")]
pub fn sce_signin_dialog_terminate() -> i32 {
    0
}

/// `sceSigninDialogOpen()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_SIGNIN_DIALOG_OPEN, lib = crate::libs::LIB_SCE_SIGNIN_DIALOG, name = "sceSigninDialogOpen")]
pub fn sce_signin_dialog_open() -> i32 {
    NP_REFUSED
}

/// `sceSigninDialogClose()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_SIGNIN_DIALOG_CLOSE, lib = crate::libs::LIB_SCE_SIGNIN_DIALOG, name = "sceSigninDialogClose")]
pub fn sce_signin_dialog_close() -> i32 {
    0
}

/// `sceSigninDialogUpdateStatus()` — always FINISHED, never RUNNING.
#[ps4_syscall(id = SyscallId::SCE_SIGNIN_DIALOG_UPDATE_STATUS, lib = crate::libs::LIB_SCE_SIGNIN_DIALOG, name = "sceSigninDialogUpdateStatus")]
pub fn sce_signin_dialog_update_status() -> i32 {
    DIALOG_STATUS_FINISHED
}

/// `sceSigninDialogGetResult()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_SIGNIN_DIALOG_GET_RESULT, lib = crate::libs::LIB_SCE_SIGNIN_DIALOG, name = "sceSigninDialogGetResult")]
pub fn sce_signin_dialog_get_result() -> i32 {
    NP_REFUSED
}

/// `sceInvitationDialogInitialize()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_INVITATION_DIALOG_INITIALIZE, lib = crate::libs::LIB_SCE_INVITATION_DIALOG, name = "sceInvitationDialogInitialize")]
pub fn sce_invitation_dialog_initialize() -> i32 {
    0
}

/// `sceInvitationDialogTerminate()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_INVITATION_DIALOG_TERMINATE, lib = crate::libs::LIB_SCE_INVITATION_DIALOG, name = "sceInvitationDialogTerminate")]
pub fn sce_invitation_dialog_terminate() -> i32 {
    0
}

/// `sceInvitationDialogOpen()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_INVITATION_DIALOG_OPEN, lib = crate::libs::LIB_SCE_INVITATION_DIALOG, name = "sceInvitationDialogOpen")]
pub fn sce_invitation_dialog_open() -> i32 {
    NP_REFUSED
}

/// `sceInvitationDialogUpdateStatus()` — always FINISHED, never RUNNING.
#[ps4_syscall(id = SyscallId::SCE_INVITATION_DIALOG_UPDATE_STATUS, lib = crate::libs::LIB_SCE_INVITATION_DIALOG, name = "sceInvitationDialogUpdateStatus")]
pub fn sce_invitation_dialog_update_status() -> i32 {
    DIALOG_STATUS_FINISHED
}

/// `sceInvitationDialogGetResult()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_INVITATION_DIALOG_GET_RESULT, lib = crate::libs::LIB_SCE_INVITATION_DIALOG, name = "sceInvitationDialogGetResult")]
pub fn sce_invitation_dialog_get_result() -> i32 {
    NP_REFUSED
}

/// `sceWebBrowserDialogInitialize()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_WEB_BROWSER_DIALOG_INITIALIZE, lib = crate::libs::LIB_SCE_WEB_BROWSER_DIALOG, name = "sceWebBrowserDialogInitialize")]
pub fn sce_web_browser_dialog_initialize() -> i32 {
    0
}

/// `sceWebBrowserDialogTerminate()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_WEB_BROWSER_DIALOG_TERMINATE, lib = crate::libs::LIB_SCE_WEB_BROWSER_DIALOG, name = "sceWebBrowserDialogTerminate")]
pub fn sce_web_browser_dialog_terminate() -> i32 {
    0
}

/// `sceWebBrowserDialogOpen()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_WEB_BROWSER_DIALOG_OPEN, lib = crate::libs::LIB_SCE_WEB_BROWSER_DIALOG, name = "sceWebBrowserDialogOpen")]
pub fn sce_web_browser_dialog_open() -> i32 {
    NP_REFUSED
}

/// `sceWebBrowserDialogOpenForPredeterminedContent()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_WEB_BROWSER_DIALOG_OPEN_FOR_PREDETERMINED_CONTENT, lib = crate::libs::LIB_SCE_WEB_BROWSER_DIALOG, name = "sceWebBrowserDialogOpenForPredeterminedContent")]
pub fn sce_web_browser_dialog_open_for_predetermined_content() -> i32 {
    NP_REFUSED
}

/// `sceWebBrowserDialogClose()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_WEB_BROWSER_DIALOG_CLOSE, lib = crate::libs::LIB_SCE_WEB_BROWSER_DIALOG, name = "sceWebBrowserDialogClose")]
pub fn sce_web_browser_dialog_close() -> i32 {
    0
}

/// `sceWebBrowserDialogResetCookie()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_WEB_BROWSER_DIALOG_RESET_COOKIE, lib = crate::libs::LIB_SCE_WEB_BROWSER_DIALOG, name = "sceWebBrowserDialogResetCookie")]
pub fn sce_web_browser_dialog_reset_cookie() -> i32 {
    0
}

/// `sceWebBrowserDialogUpdateStatus()` — always FINISHED, never RUNNING.
#[ps4_syscall(id = SyscallId::SCE_WEB_BROWSER_DIALOG_UPDATE_STATUS, lib = crate::libs::LIB_SCE_WEB_BROWSER_DIALOG, name = "sceWebBrowserDialogUpdateStatus")]
pub fn sce_web_browser_dialog_update_status() -> i32 {
    DIALOG_STATUS_FINISHED
}

/// `sceWebBrowserDialogGetResult()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_WEB_BROWSER_DIALOG_GET_RESULT, lib = crate::libs::LIB_SCE_WEB_BROWSER_DIALOG, name = "sceWebBrowserDialogGetResult")]
pub fn sce_web_browser_dialog_get_result() -> i32 {
    NP_REFUSED
}

/// `sceGameCustomDataDialogInitialize()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_GAME_CUSTOM_DATA_DIALOG_INITIALIZE, lib = crate::libs::LIB_SCE_GAME_CUSTOM_DATA, name = "sceGameCustomDataDialogInitialize")]
pub fn sce_game_custom_data_dialog_initialize() -> i32 {
    0
}

/// `sceGameCustomDataDialogTerminate()` — accepted.
#[ps4_syscall(id = SyscallId::SCE_GAME_CUSTOM_DATA_DIALOG_TERMINATE, lib = crate::libs::LIB_SCE_GAME_CUSTOM_DATA, name = "sceGameCustomDataDialogTerminate")]
pub fn sce_game_custom_data_dialog_terminate() -> i32 {
    0
}

/// `sceGameCustomDataDialogOpen()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_GAME_CUSTOM_DATA_DIALOG_OPEN, lib = crate::libs::LIB_SCE_GAME_CUSTOM_DATA, name = "sceGameCustomDataDialogOpen")]
pub fn sce_game_custom_data_dialog_open() -> i32 {
    NP_REFUSED
}

/// `sceGameCustomDataDialogUpdateStatus()` — always FINISHED, never RUNNING.
#[ps4_syscall(id = SyscallId::SCE_GAME_CUSTOM_DATA_DIALOG_UPDATE_STATUS, lib = crate::libs::LIB_SCE_GAME_CUSTOM_DATA, name = "sceGameCustomDataDialogUpdateStatus")]
pub fn sce_game_custom_data_dialog_update_status() -> i32 {
    DIALOG_STATUS_FINISHED
}

/// `sceGameCustomDataDialogGetResult()` — refused immediately.
#[ps4_syscall(id = SyscallId::SCE_GAME_CUSTOM_DATA_DIALOG_GET_RESULT, lib = crate::libs::LIB_SCE_GAME_CUSTOM_DATA, name = "sceGameCustomDataDialogGetResult")]
pub fn sce_game_custom_data_dialog_get_result() -> i32 {
    NP_REFUSED
}
