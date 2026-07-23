//! The videoout present seam (`sceVideoOutRegisterBuffers` / `sceVideoOutSubmitFlip`).
//!
//! This is the *one* remaining kernel→GPU edge the seam philosophy (doc-2 §3/§5, the same
//! rationale behind [`crate::gpu::PresentSink`] / [`crate::gpu::DisplayBufferSource`]) puts
//! in `ps4-core`: the kernel's [`crate::kernel::KernelInterface`] impl (`ps4-kernel`) fires
//! these two videoout calls, but the concrete present path lives on the display thread in
//! `ps4-gpu` and names `ash::vk`. Routing them through this trait lets `ps4-kernel` drive
//! the live present path **without depending on `ps4-gpu`** — the app wires the concrete
//! `GpuManager` impl at boot, exactly like [`crate::gpu::register_present_sink`].
//!
//! Registered once at boot before guest threads start (uncontended write lock); `get`
//! degrades to `None` headless (no display thread), so `register_buffers`/`submit_flip`
//! are decoded/traced but not presented.

use crate::registered::Registered;

/// Geometry of a framebuffer the guest registers with `sceVideoOutRegisterBuffers`, parsed
/// from the guest `SceVideoOutBufferAttribute` the call carries. Plain data so `ps4-kernel`
/// can thread the *real* dimensions through this seam instead of hardcoding them; the
/// display side (`ps4-gpu`) turns it into the videoout scanout target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoOutBufferAttribute {
    /// Framebuffer width in pixels.
    pub width: u32,
    /// Framebuffer height in pixels.
    pub height: u32,
    /// The guest scanout pixel format, parsed from the first field of the guest
    /// attribute (`int32_t format` @+0 — OpenOrbis SDK `include/orbis/_types/video.h`,
    /// struct `OrbisVideoOutBufferAttribute`, whose first member is `format`). The value is
    /// a `sceVideoOut` pixel-format code: `0x80002200` is
    /// `ORBIS_VIDEO_OUT_PIXEL_FORMAT_A8B8G8R8_SRGB` (same header), and `0x80000000` is the
    /// SRGB scanout format the OpenOrbis sample passes as the `format` argument to
    /// `sceVideoOutSetBufferAttribute` (`samples/_common/graphics.cpp`, "Set SRGB pixel
    /// format"). Which channel byte order each code implies, and the R↔B present swap it
    /// drives, is our present-shader mapping (task-154 residual #2): we treat `0x80000000`
    /// as BGRA (Celeste — swap at present) and the `A8B8G8R8_SRGB` variant as RGBA (the
    /// ps4-gcn-textured-quad example — no swap). Threaded through the display seam to the
    /// present frag shader instead of hardcoding.
    pub pixel_format: u32,
}

impl VideoOutBufferAttribute {
    /// Fallback scanout geometry used when the guest passes no attribute pointer (or a
    /// zero-sized one): our historical hardcoded 1080p default, preserved so the live present
    /// path behaves identically when the guest omits explicit dimensions. Choosing 1920×1080
    /// as the fallback is our design default, not a fixed hardware constant. `pixel_format`
    /// defaults to `0x8000_0000` — the SRGB scanout format the OpenOrbis sample passes to
    /// `sceVideoOutSetBufferAttribute` (`samples/_common/graphics.cpp`); see the channel-order
    /// note on [`VideoOutBufferAttribute::pixel_format`].
    pub const DEFAULT: VideoOutBufferAttribute = VideoOutBufferAttribute {
        width: 1920,
        height: 1080,
        pixel_format: 0x8000_0000,
    };
}

impl Default for VideoOutBufferAttribute {
    fn default() -> VideoOutBufferAttribute {
        VideoOutBufferAttribute::DEFAULT
    }
}

/// The videoout present seam the kernel's `KernelInterface` impl drives (doc-2 §3). The sole
/// impl (over the `GpuManager` display channel in `ps4-gpu`) registers scanout framebuffers
/// and ships flips across the block-until-vsync handshake; keeping the trait here keeps
/// `ps4-kernel` free of a `ps4-gpu`/`ash::vk` dependency — it names only this trait.
pub trait VideoOutSink: Send + Sync {
    /// Register a scanout framebuffer at guest address `base` under `(handle, index)` with
    /// the parsed `attr` geometry, mirroring the display side's map so a subsequent draw or
    /// flip resolves against it. Called on the kernel thread before the guest submits.
    fn register_buffer(&self, base: u64, attr: VideoOutBufferAttribute, handle: i32, index: u32);

    /// Present the framebuffer `(handle, index)` names, blocking the calling guest thread
    /// until the display thread has presented it (the block-until-vsync handshake the guest
    /// flip loop expects).
    fn submit_flip(&self, handle: i32, index: u32);
}

static VIDEO_OUT_SINK: Registered<dyn VideoOutSink> = Registered::new();

/// Register the process-global videoout sink, mirroring [`crate::gpu::register_present_sink`].
/// The app wires the `ps4-gpu` impl (over `GpuManager`) at boot; the kernel bridge reaches it
/// through [`video_out_sink`] on `sceVideoOutRegisterBuffers`/`sceVideoOutSubmitFlip`. Called
/// once at boot, before guest threads start (uncontended write lock).
pub fn register_video_out_sink(sink: std::sync::Arc<dyn VideoOutSink>) {
    VIDEO_OUT_SINK.register(sink);
}

/// The registered videoout sink, or `None` when none is wired (headless: no display thread,
/// so registrations/flips are traced but not presented).
pub fn video_out_sink() -> Option<std::sync::Arc<dyn VideoOutSink>> {
    VIDEO_OUT_SINK.get()
}

/// Whether a videoout sink is wired, for the composition root's boot-time all-seams-wired
/// assert (mirrors [`crate::registered::Registered::is_registered`]).
pub fn video_out_sink_wired() -> bool {
    VIDEO_OUT_SINK.is_registered()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockSink {
        registered: Mutex<Vec<(u64, VideoOutBufferAttribute, i32, u32)>>,
        flips: Mutex<Vec<(i32, u32)>>,
    }

    impl VideoOutSink for MockSink {
        fn register_buffer(
            &self,
            base: u64,
            attr: VideoOutBufferAttribute,
            handle: i32,
            index: u32,
        ) {
            self.registered
                .lock()
                .unwrap()
                .push((base, attr, handle, index));
        }
        fn submit_flip(&self, handle: i32, index: u32) {
            self.flips.lock().unwrap().push((handle, index));
        }
    }

    #[test]
    fn default_attr_is_1080p() {
        assert_eq!(
            VideoOutBufferAttribute::default(),
            VideoOutBufferAttribute {
                width: 1920,
                height: 1080,
                pixel_format: 0x8000_0000
            }
        );
    }

    /// Pins the scanout pixel-format value the videoout default carries to its OpenOrbis
    /// witness. `0x8000_0000` is the SRGB scanout format the OpenOrbis SDK sample passes as
    /// the `format` argument to `sceVideoOutSetBufferAttribute` (`samples/_common/graphics.cpp`,
    /// "Set SRGB pixel format"); `0x8000_2200` is `ORBIS_VIDEO_OUT_PIXEL_FORMAT_A8B8G8R8_SRGB`
    /// from OpenOrbis `include/orbis/_types/video.h`, the sibling scanout format the present
    /// path distinguishes (RGBA → no R↔B swap). Fails if the default format drifts from the
    /// value the guest ABI uses.
    #[test]
    fn pixel_format_matches_openorbis_oracle() {
        // OpenOrbis samples/_common/graphics.cpp:96 —
        //   sceVideoOutSetBufferAttribute(&attr, 0x80000000, 1, 0, w, h, w); // Set SRGB pixel format
        assert_eq!(VideoOutBufferAttribute::DEFAULT.pixel_format, 0x8000_0000);
        // OpenOrbis include/orbis/_types/video.h:33 —
        //   #define ORBIS_VIDEO_OUT_PIXEL_FORMAT_A8B8G8R8_SRGB 0x80002200
        const ORBIS_VIDEO_OUT_PIXEL_FORMAT_A8B8G8R8_SRGB: u32 = 0x8000_2200;
        // The default scanout uses the other SRGB code, not the A8B8G8R8 variant — the two
        // are distinct format values, which is why the present path's swap decision differs.
        assert_ne!(
            VideoOutBufferAttribute::DEFAULT.pixel_format,
            ORBIS_VIDEO_OUT_PIXEL_FORMAT_A8B8G8R8_SRGB
        );
    }

    #[test]
    fn registration_roundtrips() {
        let sink = Arc::new(MockSink::default());
        register_video_out_sink(sink.clone());
        assert!(video_out_sink_wired());
        let got = video_out_sink().expect("registered sink is retrievable");
        got.register_buffer(0x4000, VideoOutBufferAttribute::DEFAULT, 0, 0);
        got.submit_flip(0, 0);
        assert_eq!(sink.registered.lock().unwrap().len(), 1);
        assert_eq!(sink.flips.lock().unwrap().as_slice(), &[(0, 0)]);
    }
}
