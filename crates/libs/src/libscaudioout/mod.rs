use crate::context::NativeContext;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ps4_core::guest_ptr::{GuestPtr, GuestSlice};
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use tracing::info;

#[derive(Clone, Copy)]
struct PortParams {
    grain_len: u32,
    sample_rate: u32,
    format: u32,
}

static NEXT_HANDLE: AtomicI32 = AtomicI32::new(1);
static PORTS: RwLock<Option<HashMap<i32, PortParams>>> = RwLock::new(None);
// Rolling per-handle grain deadline (wall clock) for consumer-paced Output.
static DEADLINES: Mutex<Option<HashMap<i32, Instant>>> = Mutex::new(None);

// Per-handle streaming-resampler state. The host cpal stream is fixed at
// AUDIO_RATE, so a port opened at any other rate must be resampled to it before
// its grains enter the ring; `frac` (fractional read phase) and `prev` (the last
// input frame) carry across grains so consecutive grains join without a seam.
struct ResampleState {
    frac: f64,
    prev: (f32, f32),
}
static RESAMPLERS: Mutex<Option<HashMap<i32, ResampleState>>> = Mutex::new(None);

// OrbisAudioOutParamFormat (the `format` arg to sceAudioOutOpen):
//   0 = S16_MONO      (1ch, i16)
//   1 = S16_STEREO    (2ch, i16)
//   2 = S16_8CH       (8ch, i16)
//   3 = FLOAT_MONO    (1ch, f32)
//   4 = FLOAT_STEREO  (2ch, f32)
//   5 = FLOAT_8CH     (8ch, f32)
// The two axes (channel count, sample encoding) were previously conflated: the
// old code treated every non-{0,3} format as stereo-S16, so a FLOAT port (3/4/5)
// had its f32 samples reinterpreted as pairs of i16 — pure loud noise. FMOD
// (Celeste) opens a FLOAT port, which is exactly candidate (a) in task-147.
fn channels_for_format(format: u32) -> u16 {
    match format {
        0 | 3 => 1, // MONO
        2 | 5 => 8, // 8CH
        _ => 2,     // STEREO (1, 4)
    }
}

// True when the grain samples are 32-bit float (FLOAT_* formats), false for S16.
fn is_float_format(format: u32) -> bool {
    matches!(format, 3..=5)
}

fn bytes_per_sample(format: u32) -> usize {
    if is_float_format(format) { 4 } else { 2 }
}

fn bytes_per_frame(format: u32) -> usize {
    channels_for_format(format) as usize * bytes_per_sample(format)
}

// ---------------------------------------------------------------------------
// SAFETY (task-147): hard output attenuation applied to every sample handed to
// the host DAC. First Celeste/FMOD audio came out garbage AND dangerously loud
// (near ear-damage on headphones). Until the pipeline is fully trusted, the
// final host-submitted signal is scaled by a conservative gain and its peak is
// hard-clamped to a safe ceiling so a live run can NEVER blast full-scale noise.
// Overridable via UNEMUPS4_AUDIO_GAIN (linear, e.g. 0.25); default is a SAFE low
// value while first-audio correctness is unresolved. Raise to 1.0 once trusted.
// ---------------------------------------------------------------------------
const SAFE_GAIN_DEFAULT: f32 = 0.20; // ~-14 dB: audible but nowhere near loud
const SAFE_PEAK_CEILING: f32 = 0.35; // hard cap on |sample| after gain

static SAFE_GAIN: OnceLock<f32> = OnceLock::new();

fn safe_gain() -> f32 {
    *SAFE_GAIN.get_or_init(|| {
        let g = std::env::var("UNEMUPS4_AUDIO_GAIN")
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .filter(|g| g.is_finite() && *g >= 0.0)
            .unwrap_or(SAFE_GAIN_DEFAULT)
            // Never let the env lever exceed a hard sanity bound either.
            .clamp(0.0, 1.0);
        info!("[AUDIO] safety output gain = {g} (peak ceiling {SAFE_PEAK_CEILING})");
        g
    })
}

// Apply the safety gain and hard peak clamp to a single normalized f32 sample.
fn attenuate(s: f32) -> f32 {
    let s = if s.is_finite() { s } else { 0.0 };
    (s * safe_gain()).clamp(-SAFE_PEAK_CEILING, SAFE_PEAK_CEILING)
}

// ---------------------------------------------------------------------------
// Host output sink. sceAudioOutOutput pushes the guest's S16 grain into a
// shared ring; a cpal output stream drains it to the host speakers. cpal's
// `Stream` is `!Send` on some backends, so it is built + kept alive on a
// dedicated thread and the ring (Send+Sync) is the only channel to it. cpal is
// cross-platform (Linux ALSA/PulseAudio, Windows WASAPI, macOS CoreAudio),
// keeping the mac/MoltenVK north star intact. If no output device is available
// (headless/CI), the thread exits and the bounded push simply drops — no sound,
// no crash; the pacing sleep in Output still runs.
// ---------------------------------------------------------------------------
const AUDIO_RATE: u32 = 48000; // host stream rate; the guest (Doom) opens 48 kHz
const RING_CAP: usize = 16384; // f32 samples (~170 ms of 48 kHz stereo) — caps latency
const PREFILL: usize = 4096; // f32 samples (~43 ms) — cushion the consumer waits for

static RING: OnceLock<Arc<Mutex<VecDeque<f32>>>> = OnceLock::new();
// The consumer drains only once the ring holds >= PREFILL samples, and re-primes
// after a full underrun. Without this cushion the ring hovers near empty and any
// producer jitter (JIT render spikes) starves cpal for a sample → audible click.
static PRIMED: AtomicBool = AtomicBool::new(false);
// Set once a cpal output stream is actually playing. When true, Output paces
// against ring occupancy (the DAC hardware clock is the master); when false
// (headless/CI/WAV-only) it falls back to a wall-clock deadline.
static HOST_SINK_OK: AtomicBool = AtomicBool::new(false);

// Pull `n` f32 samples from the ring, applying prefill priming, and hand each to
// `sink` (which writes it to the format-specific cpal buffer). While priming (or
// after an underrun) it emits silence until the cushion refills.
fn drain_primed(ring: &Mutex<VecDeque<f32>>, n: usize, mut sink: impl FnMut(usize, f32)) {
    let Ok(mut r) = ring.lock() else {
        for i in 0..n {
            sink(i, 0.0);
        }
        return;
    };
    if !PRIMED.load(Ordering::Relaxed) {
        if r.len() < PREFILL {
            for i in 0..n {
                sink(i, 0.0);
            }
            return;
        }
        PRIMED.store(true, Ordering::Relaxed);
    }
    for i in 0..n {
        match r.pop_front() {
            Some(s) => sink(i, s),
            None => {
                // Cushion exhausted: re-prime so it rebuilds before draining again.
                PRIMED.store(false, Ordering::Relaxed);
                for j in i..n {
                    sink(j, 0.0);
                }
                return;
            }
        }
    }
}

fn host_ring() -> &'static Arc<Mutex<VecDeque<f32>>> {
    RING.get_or_init(|| {
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP)));
        let ring_thread = ring.clone();
        let _ = std::thread::Builder::new()
            .name("host-audio".into())
            .spawn(move || match build_output_stream(ring_thread) {
                Some(stream) => {
                    if stream.play().is_ok() {
                        HOST_SINK_OK.store(true, Ordering::Relaxed);
                        loop {
                            std::thread::sleep(Duration::from_secs(3600));
                        }
                    }
                }
                None => info!("[AUDIO] no host output device — sound disabled (pacing/WAV only)"),
            });
        ring
    })
}

fn build_output_stream(ring: Arc<Mutex<VecDeque<f32>>>) -> Option<cpal::Stream> {
    let device = cpal::default_host().default_output_device()?;
    let sample_format = device.default_output_config().ok()?.sample_format();
    let config = cpal::StreamConfig {
        channels: 2,
        sample_rate: AUDIO_RATE,
        buffer_size: cpal::BufferSize::Default,
    };
    let err_fn = |e| tracing::warn!("[AUDIO] cpal stream error: {e}");
    let stream = match sample_format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _| {
                drain_primed(&ring, data.len(), |i, s| data[i] = s);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _| {
                drain_primed(&ring, data.len(), |i, s| {
                    data[i] = (s.clamp(-1.0, 1.0) * 32767.0) as i16
                });
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_output_stream(
            config,
            move |data: &mut [u16], _| {
                drain_primed(&ring, data.len(), |i, s| {
                    data[i] = ((s.clamp(-1.0, 1.0) + 1.0) * 0.5 * 65535.0) as u16
                });
            },
            err_fn,
            None,
        ),
        _ => return None,
    }
    .ok()?;
    info!(
        "[AUDIO] host output stream opened: {} Hz stereo, fmt={:?}",
        AUDIO_RATE, sample_format
    );
    Some(stream)
}

// Decode one guest sample (S16 or f32, per format) from `b` into a normalized
// [-1, 1] f32, then apply the SAFETY attenuation before it reaches the host DAC.
fn decode_sample(format: u32, b: &[u8]) -> f32 {
    let s = if is_float_format(format) {
        f32::from_le_bytes([b[0], b[1], b[2], b[3]])
    } else {
        i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0
    };
    attenuate(s)
}

// Convert the guest grain (S16 or f32; 1/2/8 channels) to attenuated f32 stereo
// and enqueue it for the host stream. The host cpal stream is always 2ch:
//  - mono  → duplicate the single sample into L+R
//  - stereo→ pass L/R through
//  - 8ch   → downmix by taking the front L/R pair (channels 0,1), dropping the rest
fn push_to_host(handle: i32, params: &PortParams, pcm: &[u8]) {
    let fmt = params.format;
    let chans = channels_for_format(fmt) as usize;
    let bps = bytes_per_sample(fmt);
    let frame_bytes = chans * bps;
    if frame_bytes == 0 {
        return;
    }
    // Decode the guest grain to attenuated f32 stereo frames at the *guest* rate.
    let mut frames: Vec<(f32, f32)> = Vec::with_capacity(pcm.len() / frame_bytes);
    for frame in pcm.chunks_exact(frame_bytes) {
        let l = decode_sample(fmt, &frame[0..bps]);
        let r = if chans == 1 {
            l // duplicate mono into L+R
        } else {
            decode_sample(fmt, &frame[bps..2 * bps]) // stereo/8ch front-right
        };
        frames.push((l, r));
    }

    let ring = host_ring();
    let Ok(mut guard) = ring.lock() else {
        return;
    };
    // The host cpal stream is fixed at AUDIO_RATE. A port opened at any other rate
    // must be resampled to it, otherwise the ring is drained (48 kHz) faster or
    // slower than it is filled: the audio is pitch-shifted and the producer/consumer
    // clock mismatch trends toward underrun (audible gaps/clicks). The common 48 kHz
    // case (and the degenerate rate==0 case, which the Output pacing already skips)
    // passes straight through so its ring contents are unchanged.
    if params.sample_rate == 0 || params.sample_rate == AUDIO_RATE {
        for (l, r) in frames {
            guard.push_back(l);
            guard.push_back(r);
        }
    } else {
        resample_into(handle, params.sample_rate, &frames, &mut guard);
    }
    // Bound latency/memory: if the consumer fell behind, drop the oldest samples.
    while guard.len() > RING_CAP {
        guard.pop_front();
    }
}

// Streaming linear resampler from `in_rate` (> 0) to AUDIO_RATE, one grain at a
// time. `step` is the number of input frames advanced per emitted output frame;
// per-handle `frac`/`prev` are carried across grains so grain seams don't click.
fn resample_into(handle: i32, in_rate: u32, frames: &[(f32, f32)], out: &mut VecDeque<f32>) {
    let step = in_rate as f64 / AUDIO_RATE as f64;
    let Ok(mut guard) = RESAMPLERS.lock() else {
        return;
    };
    let state = guard
        .get_or_insert_with(HashMap::new)
        .entry(handle)
        .or_insert(ResampleState {
            frac: 0.0,
            prev: (0.0, 0.0),
        });
    for &(cl, cr) in frames {
        while state.frac < 1.0 {
            let f = state.frac as f32;
            out.push_back(state.prev.0 + (cl - state.prev.0) * f);
            out.push_back(state.prev.1 + (cr - state.prev.1) * f);
            state.frac += step;
        }
        state.frac -= 1.0;
        state.prev = (cl, cr);
    }
}

#[ps4_syscall(id = SyscallId::SCE_AUDIO_OUT_INIT, lib = crate::libs::LIB_SCE_AUDIO_OUT, name = "sceAudioOutInit")]
pub fn sce_audio_out_init() -> i32 {
    info!("[AUDIO] sceAudioOutInit");
    0
}

#[ps4_syscall(id = SyscallId::SCE_AUDIO_OUT_OPEN, lib = crate::libs::LIB_SCE_AUDIO_OUT, name = "sceAudioOutOpen")]
pub fn sce_audio_out_open(
    _user_id: i32,
    _port_type: i32,
    _index: i32,
    grain_len: u32,
    sample_rate: u32,
    format: u32,
) -> i32 {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let params = PortParams {
        grain_len,
        sample_rate,
        format,
    };
    if let Ok(mut guard) = PORTS.write() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(handle, params);
    }
    info!(
        "[AUDIO] sceAudioOutOpen handle={} grain={} rate={} fmt={} ({}, {}ch, {}B/frame)",
        handle,
        grain_len,
        sample_rate,
        format,
        if is_float_format(format) {
            "FLOAT"
        } else {
            "S16"
        },
        channels_for_format(format),
        bytes_per_frame(format),
    );
    handle
}

#[ps4_syscall(id = SyscallId::SCE_AUDIO_OUT_OUTPUT, lib = crate::libs::LIB_SCE_AUDIO_OUT, name = "sceAudioOutOutput")]
pub fn sce_audio_out_output(handle: i32, ptr: *const u8) -> i32 {
    let params = match PORTS.read() {
        Ok(guard) => guard.as_ref().and_then(|m| m.get(&handle).copied()),
        Err(_) => None,
    };
    let Some(params) = params else {
        return 0;
    };

    // task-115: pull the guest PCM grain through the bounded GuestSlice seam. A junk pointer
    // (or a grain that straddles an unmapped page) yields None and the submission is skipped,
    // instead of over-reading past the arena and segfaulting the host under the identity map.
    let byte_len = params.grain_len as usize * bytes_per_frame(params.format);
    if let Some(pcm) = GuestSlice::<u8>::new(ptr as u64, byte_len).and_then(GuestSlice::read_vec) {
        dump_grain(&params, &pcm);
        push_to_host(handle, &params, &pcm);
    }

    // sceAudioOutOutput is the guest's audio clock: on hardware it blocks until
    // the DAC has consumed the submitted grain. Reproduce that with backpressure
    // against the ring so the cpal DAC's hardware clock is the SOLE master — the
    // guest advances only as the consumer drains. A fixed software pace (sleep a
    // grain period per call) is a second, independent clock that drifts against
    // the DAC; the ring then slowly fills or empties until an overflow-drop or
    // underrun snaps it back — heard as periodic clicks. Backpressure removes
    // that drift entirely. Fallback (no host device): wall-clock deadline pacing.
    if params.sample_rate > 0 {
        let period = Duration::from_secs_f64(params.grain_len as f64 / params.sample_rate as f64);

        if HOST_SINK_OK.load(Ordering::Relaxed) {
            // Wait until the DAC has drained this grain back down to the target
            // cushion (~PREFILL). Steady state keeps the ring ~one grain above
            // the prime watermark, so jitter is absorbed but no drift builds up.
            let grain_samples = params.grain_len as usize * 2; // stereo f32 samples
            let target = PREFILL + grain_samples;
            let ring = host_ring();
            let start = Instant::now();
            loop {
                let len = ring.lock().map(|r| r.len()).unwrap_or(0);
                if len <= target {
                    break;
                }
                // Safety valve: never stall the guest indefinitely if the DAC
                // hiccups — after a few grains, proceed (ring bound drops excess).
                if start.elapsed() > period * 4 {
                    break;
                }
                std::thread::sleep(period / 8);
            }
        } else {
            // No host sink: pace against a rolling wall-clock deadline so WAV/CI
            // runs stay ~real-time. Rebase to now when the guest falls behind.
            let now = Instant::now();
            let deadline = {
                let mut guard = DEADLINES.lock().unwrap();
                let map = guard.get_or_insert_with(HashMap::new);
                let d = match map.get(&handle).copied() {
                    Some(d) if d > now => d,
                    _ => now,
                };
                map.insert(handle, d + period);
                d
            };
            if let Some(dur) = deadline.checked_duration_since(Instant::now()) {
                std::thread::sleep(dur);
            }
        }
    }
    0
}

// sceAudioOutSetVolume(handle, flag, vol): flag is a per-channel bitmask, vol a
// guest array of per-channel gains (0..32768). We drive a single host stream at
// unity and let the guest's own mix set levels, so the volume is accepted and
// ignored — the guest only needs the call to succeed. vol is guarded because a
// junk pointer would segfault the host under the identity map.
#[ps4_syscall(id = SyscallId::SCE_AUDIO_OUT_SET_VOLUME, lib = crate::libs::LIB_SCE_AUDIO_OUT, name = "sceAudioOutSetVolume")]
pub fn sce_audio_out_set_volume(handle: i32, flag: i32, vol: *const i32) -> i32 {
    // task-115: read the first channel gain through the validated GuestPtr seam; a junk
    // pointer is None (logged as such) rather than a host segfault.
    let first = GuestPtr::<i32>::new(vol as u64).and_then(GuestPtr::read);
    info!(
        "[AUDIO] sceAudioOutSetVolume handle={} flag={:#x} vol[0]={:?}",
        handle, flag, first
    );
    0
}

// sceAudioOutOutputs(states, num): batch variant of sceAudioOutOutput. `states`
// is a guest array of `num` {handle, ptr} pairs submitted in one call. Forward
// each to the single-port path so the same host sink and pacing apply.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputParam {
    handle: i32,
    _pad: i32,
    ptr: *const u8,
}

#[ps4_syscall(id = SyscallId::SCE_AUDIO_OUT_OUTPUTS, lib = crate::libs::LIB_SCE_AUDIO_OUT, name = "sceAudioOutOutputs")]
pub fn sce_audio_out_outputs(states: *const OutputParam, num: i32) -> i32 {
    if num <= 0 {
        return 0;
    }
    // task-115: pull the guest {handle, ptr} array through the bounded GuestSlice seam; a junk
    // base (or a run crossing the arena top) yields None and the batch is skipped rather than
    // over-reading past the arena under the identity map.
    let Some(items) =
        GuestSlice::<OutputParam>::new(states as u64, num as usize).and_then(GuestSlice::read_vec)
    else {
        return 0;
    };
    for item in items {
        sce_audio_out_output(item.handle, item.ptr);
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_AUDIO_OUT_CLOSE, lib = crate::libs::LIB_SCE_AUDIO_OUT, name = "sceAudioOutClose")]
pub fn sce_audio_out_close(handle: i32) -> i32 {
    info!("[AUDIO] sceAudioOutClose handle={}", handle);
    if let Ok(mut guard) = PORTS.write()
        && let Some(m) = guard.as_mut()
    {
        m.remove(&handle);
    }
    if let Ok(mut guard) = DEADLINES.lock()
        && let Some(m) = guard.as_mut()
    {
        m.remove(&handle);
    }
    if let Ok(mut guard) = RESAMPLERS.lock()
        && let Some(m) = guard.as_mut()
    {
        m.remove(&handle);
    }
    0
}

struct WavSink {
    file: File,
    data_bytes: u32,
    channels: u16,
    sample_rate: u32,
    format: u32,
}

static WAV_SINK: OnceLock<Option<Mutex<WavSink>>> = OnceLock::new();

fn dump_grain(params: &PortParams, pcm: &[u8]) {
    let sink = WAV_SINK.get_or_init(|| {
        let path = std::env::var("UNEMUPS4_DUMP_WAV").ok()?;
        let channels = channels_for_format(params.format);
        let sink = WavSink::create(&path, channels, params.sample_rate, params.format)?;
        Some(Mutex::new(sink))
    });
    if let Some(m) = sink
        && let Ok(mut guard) = m.lock()
    {
        // The single-file WAV oracle captures ONE port (the first to emit a grain).
        // A title that opens several ports (e.g. a mono voice port and a stereo
        // FLOAT music port) would otherwise interleave incompatible sample formats
        // under one header — a silent lie. Only append grains whose port matches
        // the captured encoding; skip the rest so the dump stays faithful. `format`
        // fixes both the channel count and the sample encoding, so matching it (plus
        // the rate) is sufficient.
        if guard.format == params.format && guard.sample_rate == params.sample_rate {
            guard.append(pcm);
        }
    }
}

impl WavSink {
    fn create(path: &str, channels: u16, sample_rate: u32, format: u32) -> Option<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .ok()?;
        write_wav_header(&mut file, channels, sample_rate, format, 0).ok()?;
        Some(WavSink {
            file,
            data_bytes: 0,
            channels,
            sample_rate,
            format,
        })
    }

    fn append(&mut self, pcm: &[u8]) {
        if self.file.write_all(pcm).is_err() {
            return;
        }
        self.data_bytes = self.data_bytes.saturating_add(pcm.len() as u32);
        if self.file.seek(SeekFrom::Start(0)).is_ok() {
            let _ = write_wav_header(
                &mut self.file,
                self.channels,
                self.sample_rate,
                self.format,
                self.data_bytes,
            );
        }
        let _ = self.file.seek(SeekFrom::End(0));
    }
}

fn write_wav_header(
    file: &mut File,
    channels: u16,
    sample_rate: u32,
    format: u32,
    data_bytes: u32,
) -> std::io::Result<()> {
    // The `fmt ` audio-format tag and bit depth follow the guest port's sample
    // encoding, not a fixed 16-bit PCM: S16_* ports are 16-bit integer PCM
    // (WAVE_FORMAT_PCM = 1); FLOAT_* ports are 32-bit little-endian IEEE float
    // (WAVE_FORMAT_IEEE_FLOAT = 3), which is exactly what the grain already holds.
    // Tags per the Microsoft WAVEFORMATEX registered format list.
    let (audio_format, bits_per_sample): (u16, u16) = if is_float_format(format) {
        (3, (bytes_per_sample(format) * 8) as u16)
    } else {
        (1, (bytes_per_sample(format) * 8) as u16)
    };
    let block_align = channels * (bits_per_sample / 8);
    let byte_rate = sample_rate * block_align as u32;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36u32 + data_bytes).to_le_bytes());
    h.extend_from_slice(b"WAVE");
    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&audio_format.to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&sample_rate.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&block_align.to_le_bytes());
    h.extend_from_slice(&bits_per_sample.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_bytes.to_le_bytes());
    file.write_all(&h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    // A port opened below the host rate must be upsampled so the ring is filled at
    // the same rate cpal drains it (AUDIO_RATE). 441 input frames at 44.1 kHz map to
    // ~480 output frames at 48 kHz, and a DC-constant signal stays constant after
    // the initial one-frame priming ramp from the zero-initialized `prev`.
    #[test]
    fn resample_upsamples_44100_to_host_rate() {
        let mut out: VecDeque<f32> = VecDeque::new();
        let frames = vec![(0.5f32, 0.5f32); 441];
        resample_into(0x7fff_0001, 44100, &frames, &mut out);
        assert!(
            out.len().is_multiple_of(2),
            "output must stay stereo-interleaved"
        );
        let out_frames = out.len() / 2;
        let expected = 441 * AUDIO_RATE as usize / 44100; // == 480
        assert!(
            out_frames.abs_diff(expected) <= 1,
            "441 frames @44.1k -> ~{expected} frames @48k, got {out_frames}"
        );
        // Steady state reproduces the DC level (linear interp of a constant).
        let last = out.back().copied().unwrap();
        assert!(
            (last - 0.5).abs() < 1e-4,
            "steady-state value drifted: {last}"
        );
    }

    // Reading the header a player would use: FLOAT_* ports must advertise 32-bit
    // IEEE float (tag 3), S16_* ports 16-bit PCM (tag 1) — not a hardcoded 16.
    fn header_of(format: u32, channels: u16, sample_rate: u32) -> Vec<u8> {
        let path = std::env::temp_dir().join(format!(
            "unemups4-wavtest-{}-{}.wav",
            std::process::id(),
            format
        ));
        let p = path.to_str().unwrap();
        let mut sink = WavSink::create(p, channels, sample_rate, format).unwrap();
        sink.append(&[0u8; 16]); // one grain's worth of bytes
        let bytes = std::fs::read(p).unwrap();
        let _ = std::fs::remove_file(p);
        bytes
    }

    fn u16_at(b: &[u8], off: usize) -> u16 {
        u16::from_le_bytes([b[off], b[off + 1]])
    }

    #[test]
    fn wav_header_float_port_is_ieee_float_32bit() {
        // FLOAT_STEREO (format 4): tag=3 (WAVE_FORMAT_IEEE_FLOAT), 32-bit, 8B/frame.
        let h = header_of(4, 2, 48000);
        assert_eq!(u16_at(&h, 20), 3, "audio format tag");
        assert_eq!(u16_at(&h, 34), 32, "bits per sample");
        assert_eq!(u16_at(&h, 32), 8, "block align = 2ch * 4B");
    }

    #[test]
    fn wav_header_s16_port_is_pcm_16bit() {
        // S16_STEREO (format 1): tag=1 (WAVE_FORMAT_PCM), 16-bit, 4B/frame.
        let h = header_of(1, 2, 44100);
        assert_eq!(u16_at(&h, 20), 1, "audio format tag");
        assert_eq!(u16_at(&h, 34), 16, "bits per sample");
        assert_eq!(u16_at(&h, 32), 4, "block align = 2ch * 2B");
    }
}
