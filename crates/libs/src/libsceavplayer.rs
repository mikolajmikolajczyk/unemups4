//! `libSceAvPlayer` HLE — the system video player, answered as a player that **exists but
//! can play nothing**.
//!
//! An engine reaches for this to run its startup movies (logos, an attract-mode intro) and
//! cutscenes. Playing one for real means demuxing an MP4, decoding H.264 into a GPU-visible
//! surface and AAC into the audio ring — none of which unemups4 has.
//!
//! # The one call that decides whether a title hangs
//!
//! `sceAvPlayerIsActive` answers **false**, always. The universal shape of movie playback is
//!
//! ```text
//! while (sceAvPlayerIsActive(player)) { get frames; present; }
//! ```
//!
//! and a title told "yes, still playing" for a movie that produces no frames spins there
//! forever. False makes that loop fall straight through to whatever comes after the movie,
//! which is the behaviour of a movie that has just ended — the state we want the title in.
//!
//! # Where the refusal goes
//!
//! On the *source*, not on the player. `sceAvPlayerInit` hands back a real handle, because
//! the player object is local and a title that cannot even construct one may treat that as a
//! broken system. `sceAvPlayerAddSource` is where the file would be opened and demuxed, so
//! that is where "no" is said. Everything downstream then answers consistently: no streams,
//! no frames, time zero, not active.
//!
//! The frame getters (`GetVideoDataEx`, `GetAudioData`) return false rather than refusing
//! with an error, because false *is* their "nothing ready this call" answer — a title polls
//! them between `IsActive` checks and handles a miss every frame. They also write nothing
//! into the caller's frame-info struct, so a title cannot mistake a zeroed struct for a
//! decoded black frame.

use crate::context::NativeContext;
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;

/// The one opaque player handle we hand out. `SceAvPlayerHandle` is a pointer on hardware,
/// but the guest only ever passes it back to us, so any non-zero value works — and non-zero
/// matters, because a title reads a null handle as an allocation failure.
const AV_PLAYER_HANDLE: u64 = 0x4176_0001;

/// A refusal for anything that would need media behind it. As elsewhere in this tree, `-1`
/// rather than a fabricated `SCE_AVPLAYER_ERROR_*`: we do not have the documented values.
const AV_REFUSED: i32 = -1;

/// `sceAvPlayerInit(SceAvPlayerInitData *data)` — construct a player. Succeeds with a
/// non-zero handle; the player is real, its media is not.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_INIT, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerInit")]
pub fn sce_av_player_init(_init_data: u64) -> u64 {
    AV_PLAYER_HANDLE
}

/// `sceAvPlayerPostInit(handle, SceAvPlayerPostInitData *data)` — second-stage setup
/// (decoder tuning, memory callbacks). Accepted.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_POST_INIT, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerPostInit")]
pub fn sce_av_player_post_init(_handle: u64, _post_init_data: u64) -> i32 {
    0
}

/// `sceAvPlayerClose(handle)` — destroy the player. Nothing was held.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_CLOSE, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerClose")]
pub fn sce_av_player_close(_handle: u64) -> i32 {
    0
}

/// `sceAvPlayerAddSource(handle, const char *filename)` — open a movie. **This is where the
/// absence lives.** Refusing here, rather than accepting and then producing no frames, is
/// what lets a title take its "movie unavailable" path with the filename still in hand.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_ADD_SOURCE, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerAddSource")]
pub fn sce_av_player_add_source(_handle: u64, _filename: u64) -> i32 {
    AV_REFUSED
}

/// `sceAvPlayerAddSourceEx(handle, sourceType, SceAvPlayerSource *source)` — the same, for a
/// source described by a struct (memory-backed or callback-fed) rather than a path.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_ADD_SOURCE_EX, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerAddSourceEx")]
pub fn sce_av_player_add_source_ex(_handle: u64, _source_type: i32, _source: u64) -> i32 {
    AV_REFUSED
}

/// `sceAvPlayerIsActive(handle)` — is playback running? **No, always.** See the module
/// header: this is the answer that keeps a movie-pump loop from spinning forever.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_IS_ACTIVE, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerIsActive")]
pub fn sce_av_player_is_active(_handle: u64) -> i32 {
    0
}

/// `sceAvPlayerStart(handle)` — begin playback. Refused: there is no source to start.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_START, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerStart")]
pub fn sce_av_player_start(_handle: u64) -> i32 {
    AV_REFUSED
}

/// `sceAvPlayerStop(handle)` — stop playback. Succeeds: "stopped" is already true, and a
/// title calling this wants to reach that state, not to be told it failed to.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_STOP, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerStop")]
pub fn sce_av_player_stop(_handle: u64) -> i32 {
    0
}

/// `sceAvPlayerPause(handle)` — same reasoning as Stop.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_PAUSE, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerPause")]
pub fn sce_av_player_pause(_handle: u64) -> i32 {
    0
}

/// `sceAvPlayerResume(handle)` — accepted; nothing resumes, and `IsActive` keeps saying so.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_RESUME, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerResume")]
pub fn sce_av_player_resume(_handle: u64) -> i32 {
    0
}

/// `sceAvPlayerSetLooping(handle, bool)` — a playback setting, recorded nowhere and
/// harmless: with nothing playing, looping changes nothing observable.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_SET_LOOPING, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerSetLooping")]
pub fn sce_av_player_set_looping(_handle: u64, _looping: i32) -> i32 {
    0
}

/// `sceAvPlayerSetTrickSpeed(handle, speed)` — fast-forward / rewind rate. Same.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_SET_TRICK_SPEED, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerSetTrickSpeed")]
pub fn sce_av_player_set_trick_speed(_handle: u64, _speed: i32) -> i32 {
    0
}

/// `sceAvPlayerJumpToTime(handle, offsetMs)` — seek. Refused: there is no timeline to seek
/// within, and reporting success would imply one.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_JUMP_TO_TIME, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerJumpToTime")]
pub fn sce_av_player_jump_to_time(_handle: u64, _offset_ms: u64) -> i32 {
    AV_REFUSED
}

/// `sceAvPlayerCurrentTime(handle)` — playback position in milliseconds. Zero: nothing has
/// played, so no time has elapsed. Consistent with `IsActive` reporting stopped.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_CURRENT_TIME, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerCurrentTime")]
pub fn sce_av_player_current_time(_handle: u64) -> u64 {
    0
}

/// `sceAvPlayerStreamCount(handle)` — how many elementary streams the source has. None: no
/// source was accepted.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_STREAM_COUNT, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerStreamCount")]
pub fn sce_av_player_stream_count(_handle: u64) -> i32 {
    0
}

/// `sceAvPlayerGetStreamInfo(handle, argStreamId, SceAvPlayerStreamInfo *info)` — describe
/// one stream. Refused, and the out-struct is left untouched: a zeroed `StreamInfo` would
/// describe a 0x0 video track, which a title may hand straight to its texture allocator.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_GET_STREAM_INFO, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerGetStreamInfo")]
pub fn sce_av_player_get_stream_info(_handle: u64, _stream_id: u32, _info: u64) -> i32 {
    AV_REFUSED
}

/// `sceAvPlayerEnableStream(handle, streamId)` — select a track for playback. There are
/// none to select.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_ENABLE_STREAM, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerEnableStream")]
pub fn sce_av_player_enable_stream(_handle: u64, _stream_id: u32) -> i32 {
    AV_REFUSED
}

/// `sceAvPlayerDisableStream(handle, streamId)` — deselect one. Same absence.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_DISABLE_STREAM, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerDisableStream")]
pub fn sce_av_player_disable_stream(_handle: u64, _stream_id: u32) -> i32 {
    AV_REFUSED
}

/// `sceAvPlayerGetVideoDataEx(handle, SceAvPlayerFrameInfoEx *info)` — hand over the next
/// decoded frame if one is ready. Returns **false** (no frame) and writes nothing.
///
/// False, not an error: this getter answers false on every frame a real decoder has not
/// finished one, so a title's polling loop already handles it. Leaving `info` untouched
/// matters as much — a zero-filled frame info is a valid-looking pointer to a 0x0 image.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_GET_VIDEO_DATA_EX, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerGetVideoDataEx")]
pub fn sce_av_player_get_video_data_ex(_handle: u64, _frame_info: u64) -> i32 {
    0
}

/// `sceAvPlayerGetAudioData(handle, SceAvPlayerFrameInfo *info)` — the audio counterpart.
/// Also false, for the same reasons.
#[ps4_syscall(id = SyscallId::SCE_AV_PLAYER_GET_AUDIO_DATA, lib = crate::libs::LIB_SCE_AV_PLAYER, name = "sceAvPlayerGetAudioData")]
pub fn sce_av_player_get_audio_data(_handle: u64, _frame_info: u64) -> i32 {
    0
}
