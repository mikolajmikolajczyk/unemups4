use crate::context::NativeContext;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
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

fn channels_for_format(format: u32) -> u16 {
    // OrbisAudioOutParam: MONO variants (0, 3) are 1 channel, STEREO (1, 4) are 2.
    match format {
        0 | 3 => 1,
        _ => 2,
    }
}

fn bytes_per_frame(format: u32) -> usize {
    channels_for_format(format) as usize * 2
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

// Convert the guest S16 grain to f32 stereo and enqueue it for the host stream.
fn push_to_host(params: &PortParams, pcm: &[u8]) {
    let ring = host_ring();
    let Ok(mut guard) = ring.lock() else {
        return;
    };
    let mono = channels_for_format(params.format) == 1;
    for b in pcm.chunks_exact(2) {
        let s = i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0;
        guard.push_back(s);
        if mono {
            guard.push_back(s); // duplicate mono into L+R
        }
    }
    // Bound latency/memory: if the consumer fell behind, drop the oldest samples.
    while guard.len() > RING_CAP {
        guard.pop_front();
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
        "[AUDIO] sceAudioOutOpen handle={} grain={} rate={} fmt={}",
        handle, grain_len, sample_rate, format
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

    if !ptr.is_null() {
        let byte_len = params.grain_len as usize * bytes_per_frame(params.format);
        let pcm = unsafe { std::slice::from_raw_parts(ptr, byte_len) };
        dump_grain(&params, pcm);
        push_to_host(&params, pcm);
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
        let period =
            Duration::from_secs_f64(params.grain_len as f64 / params.sample_rate as f64);

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
    0
}

struct WavSink {
    file: File,
    data_bytes: u32,
    channels: u16,
    sample_rate: u32,
}

static WAV_SINK: OnceLock<Option<Mutex<WavSink>>> = OnceLock::new();

fn dump_grain(params: &PortParams, pcm: &[u8]) {
    let sink = WAV_SINK.get_or_init(|| {
        let path = std::env::var("UNEMUPS4_DUMP_WAV").ok()?;
        let channels = channels_for_format(params.format);
        let sink = WavSink::create(&path, channels, params.sample_rate)?;
        Some(Mutex::new(sink))
    });
    if let Some(m) = sink
        && let Ok(mut guard) = m.lock()
    {
        guard.append(pcm);
    }
}

impl WavSink {
    fn create(path: &str, channels: u16, sample_rate: u32) -> Option<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .ok()?;
        write_wav_header(&mut file, channels, sample_rate, 0).ok()?;
        Some(WavSink {
            file,
            data_bytes: 0,
            channels,
            sample_rate,
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
    data_bytes: u32,
) -> std::io::Result<()> {
    let bits_per_sample: u16 = 16;
    let block_align = channels * (bits_per_sample / 8);
    let byte_rate = sample_rate * block_align as u32;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36u32 + data_bytes).to_le_bytes());
    h.extend_from_slice(b"WAVE");
    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&sample_rate.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&block_align.to_le_bytes());
    h.extend_from_slice(&bits_per_sample.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_bytes.to_le_bytes());
    file.write_all(&h)
}
