//! The console's **capture and share** features — screenshots, exporting media to the
//! player's gallery — answered as a system that accepts what the title asks of it but
//! never captures or exports anything.
//!
//! These libraries sit next to each other in an engine's boot for one reason: they are all
//! about what leaves the machine as media. None of them is on the path to a rendered frame,
//! and unemups4 implements none of them for real. What they share is a failure mode: an
//! engine initialises them unconditionally at startup and treats a missing library as
//! fatal, long before the player could ever press SHARE.
//!
//! # What `libSceScreenShot` actually is
//!
//! Screenshots on a PS4 are taken by the **system**, not by the game: the player presses
//! SHARE and the OS grabs the frame. What the title gets is a veto. It calls
//! `sceScreenShotDisable` before showing something it does not want captured (an ending, a
//! licensed video, a store page with a real price) and `sceScreenShotEnable` afterwards.
//! Sony's TRC requires that pairing, which is why an engine touches this during boot even
//! though the player has pressed nothing.
//!
//! So the honest model is not "screenshots are unavailable" — it is **the veto is recorded
//! and the system honours it by never capturing anything**. We keep the flag the title sets
//! and report it back unchanged. That makes `IsDisabled` agree with the last Enable/Disable,
//! which is the only relationship a title can observe from inside. A hardcoded "not
//! disabled" would answer a question the title asked about *its own* prior call with a
//! contradiction, and an engine that re-asserts state on mismatch would fight us for it.
//!
//! Nothing here reaches the GPU: there is no capture path in unemups4, so there is no frame
//! for the flag to protect. It costs one atomic to stay coherent anyway, and coherence is
//! what keeps a title out of a correction loop.

use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::sync::atomic::{AtomicBool, Ordering};

/// The title's own screenshot veto. Starts `false`: a title that never calls Disable has
/// not vetoed anything, which is the state hardware boots in.
static SCREENSHOT_DISABLED: AtomicBool = AtomicBool::new(false);

/// `sceScreenShotDisable()` — the title forbids system screen capture until it re-enables.
#[ps4_syscall(
    id = SyscallId::SCE_SCREEN_SHOT_DISABLE,
    lib = crate::libs::LIB_SCE_SCREEN_SHOT,
    name = "sceScreenShotDisable"
)]
pub fn sce_screen_shot_disable() -> i32 {
    SCREENSHOT_DISABLED.store(true, Ordering::Relaxed);
    0
}

/// `sceScreenShotEnable()` — the title lifts its veto.
#[ps4_syscall(
    id = SyscallId::SCE_SCREEN_SHOT_ENABLE,
    lib = crate::libs::LIB_SCE_SCREEN_SHOT,
    name = "sceScreenShotEnable"
)]
pub fn sce_screen_shot_enable() -> i32 {
    SCREENSHOT_DISABLED.store(false, Ordering::Relaxed);
    0
}

/// `sceScreenShotIsDisabled()` — read back the veto the title itself set. The one query
/// that makes the flag worth keeping; it must agree with the last Enable/Disable.
#[ps4_syscall(
    id = SyscallId::SCE_SCREEN_SHOT_IS_DISABLED,
    lib = crate::libs::LIB_SCE_SCREEN_SHOT,
    name = "sceScreenShotIsDisabled"
)]
pub fn sce_screen_shot_is_disabled() -> i32 {
    i32::from(SCREENSHOT_DISABLED.load(Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// libSceContentExport — copying a capture out to the player's media gallery.
// ---------------------------------------------------------------------------

/// A refusal for an export that cannot happen. Deliberately **not** dressed up as a
/// documented `SCE_CONTENT_EXPORT_ERROR_*` value: we do not have the real ones, and a
/// plausible-looking `0x80xxxxxx` invented here would be indistinguishable from a
/// verified constant to whoever reads this next. `-1` is unmistakably ours, and a title
/// branches on `< 0` either way.
const EXPORT_REFUSED: i32 = -1;

/// `sceContentExportInit()` — set the library up. Local work, and it really is available;
/// the refusal belongs on the export itself, not here (this is the call an engine makes at
/// boot, and the one whose failure it treats as fatal).
#[ps4_syscall(
    id = SyscallId::SCE_CONTENT_EXPORT_INIT,
    lib = crate::libs::LIB_SCE_CONTENT_EXPORT,
    name = "sceContentExportInit"
)]
pub fn sce_content_export_init() -> i32 {
    0
}

/// `sceContentExportTerm()` — tear down. Nothing was held.
#[ps4_syscall(
    id = SyscallId::SCE_CONTENT_EXPORT_TERM,
    lib = crate::libs::LIB_SCE_CONTENT_EXPORT,
    name = "sceContentExportTerm"
)]
pub fn sce_content_export_term() -> i32 {
    0
}

/// `sceContentExportStart()` — begin an export. Refuses **at once**: there is no gallery to
/// export into. Never "in progress" — a title told that would poll for a completion that
/// cannot arrive (doc-5 case 28).
#[ps4_syscall(
    id = SyscallId::SCE_CONTENT_EXPORT_START,
    lib = crate::libs::LIB_SCE_CONTENT_EXPORT,
    name = "sceContentExportStart"
)]
pub fn sce_content_export_start() -> i32 {
    EXPORT_REFUSED
}

/// `sceContentExportFromFile()` — export an existing file. Same refusal, same reason.
#[ps4_syscall(
    id = SyscallId::SCE_CONTENT_EXPORT_FROM_FILE,
    lib = crate::libs::LIB_SCE_CONTENT_EXPORT,
    name = "sceContentExportFromFile"
)]
pub fn sce_content_export_from_file() -> i32 {
    EXPORT_REFUSED
}

/// `sceContentExportFinish()` — release an export that was started. Nothing ever started,
/// so there is nothing to release and nothing to complain about.
#[ps4_syscall(
    id = SyscallId::SCE_CONTENT_EXPORT_FINISH,
    lib = crate::libs::LIB_SCE_CONTENT_EXPORT,
    name = "sceContentExportFinish"
)]
pub fn sce_content_export_finish() -> i32 {
    0
}

// ---------------------------------------------------------------------------
// libSceVideoRecording — the system video recorder the SHARE button drives.
// ---------------------------------------------------------------------------

/// A refusal for a recording that cannot happen. Same reasoning as [`EXPORT_REFUSED`]: we
/// do not know the documented `SCE_VIDEO_RECORDING_ERROR_*` values, so we do not pretend to.
const RECORDING_REFUSED: i32 = -1;

/// `sceVideoRecordingSetInfo()` — the title hands the recorder metadata about what is on
/// screen (which chapter, whether this segment may be recorded at all). Accepted: it is a
/// statement to the system, not a request, and an engine makes it at boot before any
/// recording exists. Nothing consumes it because nothing records.
#[ps4_syscall(
    id = SyscallId::SCE_VIDEO_RECORDING_SET_INFO,
    lib = crate::libs::LIB_SCE_VIDEO_RECORDING,
    name = "sceVideoRecordingSetInfo"
)]
pub fn sce_video_recording_set_info() -> i32 {
    0
}

/// `sceVideoRecordingQueryMemSize2()` — how much memory the recorder would need. Refused
/// rather than answered with a size: a size is a promise that `Open2` will then work, and
/// it will not. Refusing here is the earliest honest point, before the title allocates.
#[ps4_syscall(
    id = SyscallId::SCE_VIDEO_RECORDING_QUERY_MEM_SIZE2,
    lib = crate::libs::LIB_SCE_VIDEO_RECORDING,
    name = "sceVideoRecordingQueryMemSize2"
)]
pub fn sce_video_recording_query_mem_size2() -> i32 {
    RECORDING_REFUSED
}

/// `sceVideoRecordingOpen2()` — claim the recorder. Refused; there is no encoder here.
#[ps4_syscall(
    id = SyscallId::SCE_VIDEO_RECORDING_OPEN2,
    lib = crate::libs::LIB_SCE_VIDEO_RECORDING,
    name = "sceVideoRecordingOpen2"
)]
pub fn sce_video_recording_open2() -> i32 {
    RECORDING_REFUSED
}

/// `sceVideoRecordingStart()` — begin recording. Refused immediately, never "starting".
#[ps4_syscall(
    id = SyscallId::SCE_VIDEO_RECORDING_START,
    lib = crate::libs::LIB_SCE_VIDEO_RECORDING,
    name = "sceVideoRecordingStart"
)]
pub fn sce_video_recording_start() -> i32 {
    RECORDING_REFUSED
}

/// `sceVideoRecordingStop()` — stop recording. Succeeds: nothing is running, which is
/// exactly the state the caller wants to reach.
#[ps4_syscall(
    id = SyscallId::SCE_VIDEO_RECORDING_STOP,
    lib = crate::libs::LIB_SCE_VIDEO_RECORDING,
    name = "sceVideoRecordingStop"
)]
pub fn sce_video_recording_stop() -> i32 {
    0
}

/// `sceVideoRecordingClose()` — release the recorder. Nothing was ever claimed.
#[ps4_syscall(
    id = SyscallId::SCE_VIDEO_RECORDING_CLOSE,
    lib = crate::libs::LIB_SCE_VIDEO_RECORDING,
    name = "sceVideoRecordingClose"
)]
pub fn sce_video_recording_close() -> i32 {
    0
}

/// `sceVideoRecordingGetStatus()` — is a recording running? Refused rather than answered
/// through an out-parameter whose layout we have not verified; a title that cannot read the
/// status takes the same branch as one told "not recording", and a wrong write into a guest
/// struct would corrupt state we cannot see.
#[ps4_syscall(
    id = SyscallId::SCE_VIDEO_RECORDING_GET_STATUS,
    lib = crate::libs::LIB_SCE_VIDEO_RECORDING,
    name = "sceVideoRecordingGetStatus"
)]
pub fn sce_video_recording_get_status() -> i32 {
    RECORDING_REFUSED
}

// ---------------------------------------------------------------------------
// libSceSharePlay — handing a remote friend the pad over the network.
// ---------------------------------------------------------------------------

/// `sceSharePlayInitialize()` — set the library up. Succeeds. A Share Play session needs a
/// second console and a link, neither of which exists here, but *initialising* is local and
/// an engine does it at boot regardless of whether a session ever happens. The absence
/// shows up where it belongs: no session is ever reported and no event is ever delivered.
#[ps4_syscall(
    id = SyscallId::SCE_SHARE_PLAY_INITIALIZE,
    lib = crate::libs::LIB_SCE_SHARE_PLAY,
    name = "sceSharePlayInitialize"
)]
pub fn sce_share_play_initialize() -> i32 {
    0
}

/// `sceSharePlayTerminate()` — tear down. Nothing was held.
#[ps4_syscall(
    id = SyscallId::SCE_SHARE_PLAY_TERMINATE,
    lib = crate::libs::LIB_SCE_SHARE_PLAY,
    name = "sceSharePlayTerminate"
)]
pub fn sce_share_play_terminate() -> i32 {
    0
}

/// `sceSharePlaySetProhibition()` — the title restricts what a visiting player may do
/// (block the pad hand-off during a purchase screen, say). Accepted: it is a rule filed
/// with the system, and with no visitor there is nothing it could fail to constrain.
#[ps4_syscall(
    id = SyscallId::SCE_SHARE_PLAY_SET_PROHIBITION,
    lib = crate::libs::LIB_SCE_SHARE_PLAY,
    name = "sceSharePlaySetProhibition"
)]
pub fn sce_share_play_set_prohibition() -> i32 {
    0
}

// ---------------------------------------------------------------------------
// libSceGameLiveStreaming — broadcasting the game to Twitch/YouTube.
// ---------------------------------------------------------------------------

/// A refusal for a broadcast query that has no broadcast behind it. See [`EXPORT_REFUSED`]
/// for why this is `-1` and not an invented `SCE_GAME_LIVE_STREAMING_ERROR_*`.
const STREAMING_REFUSED: i32 = -1;

/// `sceGameLiveStreamingInitialize()` — set the library up. Succeeds, for the same reason
/// Share Play's does: the library is present, the broadcast is what is missing.
#[ps4_syscall(
    id = SyscallId::SCE_GAME_LIVE_STREAMING_INITIALIZE,
    lib = crate::libs::LIB_SCE_GAME_LIVE_STREAMING,
    name = "sceGameLiveStreamingInitialize"
)]
pub fn sce_game_live_streaming_initialize() -> i32 {
    0
}

/// `sceGameLiveStreamingTerminate()` — tear down.
#[ps4_syscall(
    id = SyscallId::SCE_GAME_LIVE_STREAMING_TERMINATE,
    lib = crate::libs::LIB_SCE_GAME_LIVE_STREAMING,
    name = "sceGameLiveStreamingTerminate"
)]
pub fn sce_game_live_streaming_terminate() -> i32 {
    0
}

/// `sceGameLiveStreamingEnableLiveStreaming()` — the title permits or forbids broadcasting
/// (forbidden during a spoiler, permitted after). Accepted: a permission the title grants,
/// not a request it makes. Nothing can broadcast either way.
#[ps4_syscall(
    id = SyscallId::SCE_GAME_LIVE_STREAMING_ENABLE_LIVE_STREAMING,
    lib = crate::libs::LIB_SCE_GAME_LIVE_STREAMING,
    name = "sceGameLiveStreamingEnableLiveStreaming"
)]
pub fn sce_game_live_streaming_enable_live_streaming() -> i32 {
    0
}

/// `sceGameLiveStreamingGetCurrentStatus2()` — is a broadcast running, and to where?
/// Refused rather than answered by zero-filling a status struct whose layout we have not
/// verified. A title that cannot read the status takes the same branch as one told "not
/// broadcasting"; a wrong write would corrupt guest state instead.
#[ps4_syscall(
    id = SyscallId::SCE_GAME_LIVE_STREAMING_GET_CURRENT_STATUS2,
    lib = crate::libs::LIB_SCE_GAME_LIVE_STREAMING,
    name = "sceGameLiveStreamingGetCurrentStatus2"
)]
pub fn sce_game_live_streaming_get_current_status2() -> i32 {
    STREAMING_REFUSED
}

/// `sceGameLiveStreamingGetProgramInfo()` — details of the broadcast in progress. There is
/// none; same refusal, same reason.
#[ps4_syscall(
    id = SyscallId::SCE_GAME_LIVE_STREAMING_GET_PROGRAM_INFO,
    lib = crate::libs::LIB_SCE_GAME_LIVE_STREAMING,
    name = "sceGameLiveStreamingGetProgramInfo"
)]
pub fn sce_game_live_streaming_get_program_info() -> i32 {
    STREAMING_REFUSED
}

// ---------------------------------------------------------------------------
// libSceRemoteplay — streaming the game to a Vita or a phone.
// ---------------------------------------------------------------------------

/// `sceRemoteplayInitialize()` — set the library up. Succeeds; the absence is the *session*,
/// not the library, and an engine initialises this at boot to learn whether it should
/// down-scale its UI for a remote screen.
#[ps4_syscall(
    id = SyscallId::SCE_REMOTEPLAY_INITIALIZE,
    lib = crate::libs::LIB_SCE_REMOTEPLAY,
    name = "sceRemoteplayInitialize"
)]
pub fn sce_remoteplay_initialize() -> i32 {
    0
}

/// `sceRemoteplayTerminate()` — tear down.
#[ps4_syscall(
    id = SyscallId::SCE_REMOTEPLAY_TERMINATE,
    lib = crate::libs::LIB_SCE_REMOTEPLAY,
    name = "sceRemoteplayTerminate"
)]
pub fn sce_remoteplay_terminate() -> i32 {
    0
}

/// `sceRemoteplayGetConnectionStatus()` — is a remote device attached? Refused rather than
/// answered through a status struct we have not verified. The title's fallback is the local
/// screen, which is the one that exists.
#[ps4_syscall(
    id = SyscallId::SCE_REMOTEPLAY_GET_CONNECTION_STATUS,
    lib = crate::libs::LIB_SCE_REMOTEPLAY,
    name = "sceRemoteplayGetConnectionStatus"
)]
pub fn sce_remoteplay_get_connection_status() -> i32 {
    STREAMING_REFUSED
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The only observable relationship this library has: what the title vetoed is what it
    /// reads back. A constant answer here would contradict the title's own prior call.
    #[test]
    fn is_disabled_reports_the_veto_the_title_set() {
        assert_eq!(sce_screen_shot_disable(), 0);
        assert_eq!(sce_screen_shot_is_disabled(), 1);
        assert_eq!(sce_screen_shot_enable(), 0);
        assert_eq!(sce_screen_shot_is_disabled(), 0);
    }
}
