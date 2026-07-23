//! Unified-memory → host resource cache (doc-4 §8): maps a `(guest range, kind,
//! layout)` to a backend handle, upload-on-use / invalidate-on-dirty. Vulkan-free —
//! emits `BackendCmd`s (`CreateBuffer`/`UploadBuffer`/`ImportBuffer`) for the display
//! thread to replay against the real `GpuBackend`.
//!
//! Phase 3.5 scope: linear vertex / index / constant BUFFERS only (doc-4 §8.6). No
//! textures/tiling (the [`ResLayout`] enum may gain variants, but only linear buffer
//! kinds are implemented here) and no readback (the GPU→guest reverse direction stays
//! off the hot path, doc-4 §8.5).
//!
//! ## Id ownership across the guest/display thread boundary
//!
//! doc-4 §8.1 gives [`ResourceCache::get`] a **synchronous** `-> ResourceId`
//! signature, but at runtime the cache runs on the **guest thread** (inside the
//! `libSceGnmDriver` submit handler, where only a `&dyn PresentSink` reaches the
//! display side) while the sole `GpuBackend` — and, in the old shape, id minting —
//! live on the **display thread** across a one-way channel (doc-4 §3). Two seams
//! collide there: a fire-and-forget `BackendCmd` can't round-trip a backend-minted
//! id back to the caller, and the zero-copy fork ([`GpuBackend::try_import_host_range`],
//! AC #3) needs a synchronous yes/no answer that depends on device caps only the
//! display thread knows.
//!
//! **Decision (adopted from the review's recommendation): `ResourceId`s are minted
//! GUEST-SIDE by this cache** — a monotonic counter it owns — and handed *into* the
//! backend via [`BackendCmd`]s (`CreateBuffer { id, .. }`, `UploadBuffer { id, .. }`,
//! `ImportBuffer { id, .. }`), which the display thread replays, keeping a display-side
//! `id -> vk::Buffer` map. No id round-trip.
//!
//! ## Commands, not synchronous backend calls
//!
//! [`ResourceCache::get`] takes `out: &mut Vec<BackendCmd>` and **appends** the
//! create / upload / import commands rather than calling a `&mut dyn GpuBackend`
//! directly — because on the guest thread there is no backend to call. First use of a
//! copy-path range appends `CreateBuffer` + `UploadBuffer`; a dirty hit appends one
//! `UploadBuffer`; a clean hit appends nothing; a zero-copy import appends one
//! `ImportBuffer` and no upload.
//!
//! ## The [`ImportProbe`] is authoritative
//!
//! The import decision is made **entirely guest-side** by the policy's [`ImportProbe`]
//! (device caps resolved at boot): if it says import, [`get`] appends an `ImportBuffer`
//! command and records the entry as imported (never re-uploaded). There is no backend
//! round-trip that could override it. The display thread MUST honor the import; a range
//! the probe promised but the device cannot import is a **fatal invariant violation on
//! the display side** (the replay panics), not a silent copy fallback — a silent fallback
//! would strand the cache believing the entry is imported (and so clean forever) while the
//! buffer holds stale bytes, so every subsequent draw would consume an absent/zero
//! resource with no retry. A probe that promises an import the device cannot deliver is a
//! programming error in the probe, and crashing loud is strictly safer than stranding the
//! cache. `GpuBackend::try_import_host_range`'s synchronous `bool` is now purely
//! backend-internal (used only when the display thread executes an `ImportBuffer`),
//! never part of `get`'s control flow.
//!
//! Why fail-fast and not a "provably-honorable" mirror: whether a *specific* host pointer
//! imports depends on per-pointer runtime facts (its alignment against the device's
//! `minImportedHostPointerAlignment`, and the driver accepting it) that the guest-side
//! mirror cannot know when it answers. A boot-resolved mirror can reflect coarse device
//! caps (extension present, min alignment) but cannot *guarantee* any given pointer
//! honors, so it must only say yes when it is certain; if it says yes and the device still
//! declines, that is a bug in the mirror, surfaced as a display-side panic rather than
//! masked. A downgrade command was rejected: it would require the display thread to reach
//! back across the one-way channel to un-import the cache entry, reintroducing the
//! round-trip the guest-side authoritative-probe design exists to avoid.
//!
//! [`get`]: ResourceCache::get
//!
//! ## Eviction policy: unbounded-until-free (no LRU / size cap)
//!
//! The cache grows one entry per distinct `(addr, size, layout)` and is trimmed only by
//! [`ResourceCache::free_range`], driven by the guest freeing/unmapping the backing range
//! (`sceKernelReleaseDirectMemory`/`munmap`). There is deliberately **no** LRU or byte-cap
//! eviction: the phase-4 corpus reuses a small, bounded working set of vertex/index/const
//! ranges, and the guest owns the lifetime of every GPU-visible buffer — it frees a range
//! before it can be reused for something else, so a free-driven evict tracks the guest's
//! own allocator exactly and bounds the cache to the guest's live GPU allocation. An LRU
//! would risk evicting a still-live entry the guest expects to keep (re-uploading it on the
//! next draw — pure overhead) and buys nothing while the corpus fits in memory. If a later
//! milestone streams unbounded distinct ranges (e.g. a title that never reuses addresses),
//! a size-capped LRU layers on top of this same eviction path without reshaping [`get`].
//!
//! The dirty-tracking seam ([`DirtySource`], doc-4 §8.3) lives in `ps4-core`
//! ([`ps4_core::dirty`]) and is consumed here: [`ResourceCache::drain_dirty`] polls it
//! once per submit and marks overlapping entries dirty. `ps4-gnm` reaches the real
//! x86jit-backed impl through [`ps4_core::dirty::dirty_source`] without depending on
//! `ps4-cpu`. Re-exported here for the cache's own use.

use std::collections::HashMap;

pub use ps4_core::dirty::DirtySource;
use ps4_core::gpu::{BackendCmd, ResourceId};
use ps4_core::memory::VirtualMemoryManager;

pub mod tile;
pub use tile::{Compression, Extent, SurfaceLayout, TexelSize, Tiling};

/// The pixel data/number format of a tiled surface (doc-4 §C3). Carried on the texture /
/// render-target key so the same bytes viewed under two formats key separately; a full
/// GCN `dfmt`/`nfmt` decode is deferred — this holds the raw hardware pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceFormat {
    /// GCN data format (`dfmt`).
    pub dfmt: u8,
    /// GCN number format (`nfmt`).
    pub nfmt: u8,
}

/// Disambiguates the same guest bytes viewed as different resource kinds (doc-4 §8.1).
/// Carries tiling/compression/coherence fields per doc-4 §C3/§C5/§C9 as they land.
/// The buffer kinds are linear; the `Texture`/`RenderTarget` kinds carry the tiling +
/// compression fields the §C3/§C9 detile seam dispatches on ([`tile::detile`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResLayout {
    VertexBuf,
    IndexBuf,
    ConstBuf,
    /// A sampled texture: format + full tiled byte layout (extent, tile mode,
    /// compression). Detiled on upload; never zero-copy when tiled (doc-4 §C3).
    Texture {
        format: SurfaceFormat,
        surface: SurfaceLayout,
    },
    /// A render target: same format + tiled layout as a texture; the split key exists so
    /// a range aliased as both RT and texture yields two entries (doc-4 §8.1).
    RenderTarget {
        format: SurfaceFormat,
        surface: SurfaceLayout,
    },
}

/// Cache key: the same bytes seen as two kinds get two entries (doc-4 §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResourceKey {
    pub addr: u64,
    pub size: u64,
    pub layout: ResLayout,
}

/// The onion/garlic memory-coherence hint that feeds the §8.2 policy decision
/// (doc-4 §C5): garlic (GPU-optimized, CPU-uncached) ranges are zero-copy candidates;
/// onion (CPU-coherent) ranges are copy + dirty-track. The flag originates in the
/// kernel memory manager and is threaded in per-range once that dependency lands; the
/// cache's policy step consults it here without hardcoding one coherence assumption.
///
/// Phase 3.5 defaults to [`Coherence::CopySide`] (the portable default, doc-4 §8.2):
/// everything is treated as copy-side regardless of the flag until the memory-manager
/// ↔ cache dependency is wired.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Coherence {
    /// CPU-coherent (onion) or unknown: copy + dirty-track. The portable default.
    #[default]
    CopySide,
    /// GPU-optimized (garlic): a zero-copy import candidate (doc-4 §C5), still subject
    /// to the backend's [`ImportProbe`] actually accepting the range.
    ZeroCopyCandidate,
}

/// Guest-side mirror of the backend's zero-copy import capability (doc-4 §8.2, §3).
///
/// The real device-cap answer (`VK_EXT_external_memory_host` present + alignment)
/// lives on the display thread, but the cache must decide policy synchronously on the
/// guest thread. So capability is mirrored here as a boot-resolved probe: it answers
/// "could this range import zero-copy?" without a channel round-trip, and that answer is
/// **authoritative** — when it says import, [`ResourceCache::get`] emits an
/// `ImportBuffer` command and records the entry as imported (never re-uploaded). The
/// display thread MUST honor the import; a probe-yes it cannot fulfil is a fatal
/// invariant violation there (the replay panics), not a silent copy fallback (which would
/// strand the cache treating a stale copy-path buffer as an always-current import). The
/// probe must therefore only answer yes when the boot-resolved device caps make the import
/// certain to be honorable — an over-eager probe is a programming error, surfaced as a
/// display-side crash rather than a silently stranded cache.
pub trait ImportProbe: Send {
    /// Whether a range of `size` bytes at `addr` with `coherence` may be imported
    /// zero-copy. Defaults conservative (never import) — the portable MoltenVK default.
    fn can_import(&self, addr: u64, size: u64, coherence: Coherence) -> bool;
}

/// The conservative default probe: never zero-copy (copy+invalidate everywhere), which
/// is correct on MoltenVK and any backend without `external_memory_host` (doc-4 §8.2).
#[derive(Default)]
pub struct NoImport;

impl ImportProbe for NoImport {
    fn can_import(&self, _addr: u64, _size: u64, _coherence: Coherence) -> bool {
        false
    }
}

/// The per-range coherence resolver (doc-4 §C5 onion/garlic seam). It reads the
/// memory-type flag the kernel memory manager attaches to a range and maps it to a
/// [`Coherence`] the cache's policy step consults. Kept a trait so the memory-manager ↔
/// cache dependency can be threaded in later without changing [`ResourceCache::get`].
pub trait CoherenceSource: Send {
    /// The coherence hint for `[addr, addr+size)`. Defaults conservative.
    fn coherence(&self, addr: u64, size: u64) -> Coherence;
}

/// The phase-3.5 default: everything copy-side (doc-4 §8.2), regardless of range —
/// no memory-manager wiring yet, so the onion/garlic flag is not consulted.
#[derive(Default)]
pub struct AlwaysCopySide;

impl CoherenceSource for AlwaysCopySide {
    fn coherence(&self, _addr: u64, _size: u64) -> Coherence {
        Coherence::CopySide
    }
}

/// The policy inputs the cache consults per range (doc-4 §C5 onion/garlic seam, §8.2).
/// An optional hook so the memory-manager coherence flag ([`CoherenceSource`]) and the
/// guest-side import mirror ([`ImportProbe`]) can be threaded in later without changing
/// [`ResourceCache::get`]; defaults to copy-side ([`AlwaysCopySide`] + [`NoImport`]),
/// the portable §8.2 default. Zero-copy is only reached when *both* the coherence
/// source reports a candidate range *and* the import mirror accepts it.
pub struct CachePolicy {
    coherence: Box<dyn CoherenceSource>,
    probe: Box<dyn ImportProbe>,
}

impl Default for CachePolicy {
    fn default() -> CachePolicy {
        CachePolicy {
            coherence: Box::new(AlwaysCopySide),
            probe: Box::new(NoImport),
        }
    }
}

impl CachePolicy {
    /// Copy-side default (doc-4 §8.2): never import zero-copy.
    pub fn copy_side() -> CachePolicy {
        CachePolicy::default()
    }

    /// Install a coherence source + zero-copy import mirror (doc-4 §C5): the memory-type
    /// flag and the backend's device-cap answer, both mirrored guest-side so `get` picks
    /// the zero-copy fork synchronously with no channel round-trip.
    pub fn new(coherence: Box<dyn CoherenceSource>, probe: Box<dyn ImportProbe>) -> CachePolicy {
        CachePolicy { coherence, probe }
    }

    fn coherence(&self, addr: u64, size: u64) -> Coherence {
        self.coherence.coherence(addr, size)
    }
}

/// One cached resource: its guest-minted backend handle, whether it was imported
/// zero-copy, and whether the guest has written its backing range since the last upload.
struct Entry {
    id: ResourceId,
    /// Zero-copy imports never need re-upload — the GPU always sees current guest bytes
    /// (doc-4 §8.2), so `dirty` is irrelevant for them and stays `false`.
    imported: bool,
    dirty: bool,
}

/// The unified-memory resource cache (doc-4 §8.1/§8.2). Guest-thread-resident, Vulkan-
/// free: mints [`ResourceId`]s itself and emits [`BackendCmd`]s for the display thread
/// to replay against the real backend.
///
/// See the module doc for the id-ownership + command-emitting decision above.
pub struct ResourceCache {
    entries: HashMap<ResourceKey, Entry>,
    /// Samplers keyed by their (portable) parameters: a sampler is immutable + tiny, so
    /// one `CreateSampler` per distinct [`SamplerDesc`] is emitted and the id reused.
    samplers: HashMap<ps4_core::gpu::SamplerDesc, ResourceId>,
    /// Guest-side monotonic id allocator: ids mint here, no backend round-trip.
    next_id: u32,
    policy: CachePolicy,
}

impl Default for ResourceCache {
    fn default() -> ResourceCache {
        ResourceCache::new()
    }
}

impl ResourceCache {
    /// A copy-side cache (doc-4 §8.2 portable default): no zero-copy import mirror.
    pub fn new() -> ResourceCache {
        ResourceCache::with_policy(CachePolicy::copy_side())
    }

    /// A cache with an explicit onion/garlic + import policy (doc-4 §C5 seam).
    pub fn with_policy(policy: CachePolicy) -> ResourceCache {
        ResourceCache {
            entries: HashMap::new(),
            samplers: HashMap::new(),
            next_id: 1,
            policy,
        }
    }

    /// Get-or-create a sampler for `desc` (doc-4 §C4): mints one id per distinct
    /// [`SamplerDesc`] and appends `CreateSampler` on first use, reusing the id after.
    /// A sampler is immutable and never dirty, so there is no re-upload path.
    pub fn get_sampler(
        &mut self,
        desc: ps4_core::gpu::SamplerDesc,
        out: &mut Vec<BackendCmd>,
    ) -> ResourceId {
        if let Some(&id) = self.samplers.get(&desc) {
            return id;
        }
        let id = self.mint_id();
        out.push(BackendCmd::CreateSampler { id, desc });
        self.samplers.insert(desc, id);
        id
    }

    fn mint_id(&mut self) -> ResourceId {
        let id = ResourceId(self.next_id);
        self.next_id += 1;
        id
    }

    /// The single entry point (doc-4 §8.1): "I need this guest `key` range on the GPU."
    /// Appends the [`BackendCmd`]s needed to make it so to `out` and returns the resource
    /// id; the display thread replays `out` against the real backend (there is no backend
    /// to call on the guest thread — see the module doc).
    ///
    /// - **First use:** pick policy (zero-copy import if the range is a garlic candidate
    ///   *and* the guest-side mirror accepts it, else copy). Import path: append one
    ///   `ImportBuffer` (no upload — the entry is imported, authoritative per the probe).
    ///   Copy path: append `CreateBuffer` + `UploadBuffer`. `watch` the range for dirty
    ///   tracking, record the entry.
    /// - **Clean hit:** return the cached id with no commands (§6 linchpin, AC #1).
    /// - **Dirty hit:** append one `UploadBuffer` with the current guest bytes, clear the
    ///   flag.
    ///
    /// Imported entries are never dirty (the GPU reads guest pages directly, §8.2), so a
    /// hit on one is always a clean no-op.
    pub fn get(
        &mut self,
        key: ResourceKey,
        mem: &dyn VirtualMemoryManager,
        dirty: &dyn DirtySource,
        out: &mut Vec<BackendCmd>,
    ) -> ResourceId {
        if let Some(entry) = self.entries.get(&key) {
            if entry.imported || !entry.dirty {
                return entry.id; // clean hit: no commands (AC #1)
            }
            // Dirty hit: emit a re-upload command, then clear the flag — but ONLY if the
            // re-upload actually snapshotted bytes. If reading the guest range fails (e.g.
            // the guest remapped/freed it between submits), no command is emitted and the
            // backend buffer still holds stale bytes; clearing `dirty` would strand it
            // clean-but-wrong and never retry. Leave it dirty so the next `get` retries
            // once the range is readable again.
            let id = entry.id;
            let uploaded = Self::emit_upload(id, key, mem, out);
            if uploaded {
                if let Some(e) = self.entries.get_mut(&key) {
                    e.dirty = false;
                }
            } else {
                tracing::debug!(
                    addr = key.addr,
                    size = key.size,
                    "cache re-upload skipped (guest range unreadable); entry stays dirty"
                );
            }
            return id;
        }

        // First use. Mint the id guest-side (see module doc id-ownership rationale), then choose policy.
        let id = self.mint_id();
        let coherence = self.policy.coherence(key.addr, key.size);
        // The import decision is authoritative and made entirely guest-side: if the probe
        // accepts the range we emit an ImportBuffer and record the entry imported. The
        // display thread MUST honor it (a probe-yes it can't fulfil is a fatal invariant
        // violation there); there is no backend round-trip that could veto this.
        let imported = coherence == Coherence::ZeroCopyCandidate
            && self.policy.probe.can_import(key.addr, key.size, coherence);

        // On the copy path the entry is dirty until the initial upload command carries
        // bytes into the backend buffer. If that first read fails, we record the entry
        // `dirty = true` over a never-uploaded (zero-filled) buffer rather than a clean
        // one — so a later `get` retries via the standard dirty-hit path instead of
        // handing out a buffer that looks ready but holds no guest data. Imported entries
        // are always clean (the GPU reads guest pages directly).
        let mut dirty_after_create = false;
        if imported {
            out.push(BackendCmd::ImportBuffer {
                id,
                addr: key.addr,
                size: key.size,
            });
        } else {
            // Copy path: create host VRAM and upload the current guest bytes.
            out.push(BackendCmd::CreateBuffer { id, size: key.size });
            if !Self::emit_upload(id, key, mem, out) {
                dirty_after_create = true;
                tracing::debug!(
                    addr = key.addr,
                    size = key.size,
                    "cache initial upload skipped (guest range unreadable); entry inserted dirty"
                );
            }
        }
        // Dirty-track the backing range either way; imported entries ignore it.
        dirty.watch(key.addr, key.size);
        self.entries.insert(
            key,
            Entry {
                id,
                imported,
                dirty: dirty_after_create,
            },
        );
        id
    }

    /// Get-or-create a sampled texture on the GPU (doc-4 §C3), the image analogue of
    /// [`Self::get`]. `key` must carry a [`ResLayout::Texture`]; `surface` is its byte
    /// layout (extent + tile mode + format). On first use this reads the guest texture
    /// through the bounded seam, **detiles** it to linear RGBA (`tile::detile`), and
    /// appends `CreateImage` + `UploadImage`; a dirty hit appends one `UploadImage` (the
    /// re-detiled bytes); a clean hit appends nothing (§6 linchpin — exactly one
    /// re-upload per guest write). Returns the image [`ResourceId`].
    ///
    /// Textures are always copy-path (a tiled surface is never host-linear, so it can't
    /// be imported zero-copy — doc-4 §C3); a guest write is tracked via [`DirtySource`]
    /// exactly like a copy buffer.
    pub fn get_texture(
        &mut self,
        key: ResourceKey,
        surface: SurfaceLayout,
        format: ps4_core::gpu::TextureFormat,
        mem: &dyn VirtualMemoryManager,
        dirty: &dyn DirtySource,
        out: &mut Vec<BackendCmd>,
    ) -> ResourceId {
        if let Some(entry) = self.entries.get(&key) {
            if !entry.dirty {
                return entry.id; // clean hit: no commands (exactly one upload per write)
            }
            let id = entry.id;
            let uploaded = Self::emit_image_upload(id, key, &surface, mem, out);
            if uploaded {
                if let Some(e) = self.entries.get_mut(&key) {
                    e.dirty = false;
                }
            } else {
                tracing::debug!(
                    addr = key.addr,
                    size = key.size,
                    "texture re-upload skipped (guest range unreadable); entry stays dirty"
                );
            }
            return id;
        }

        // First use: mint the id, create the image, upload the detiled texels.
        let id = self.mint_id();
        out.push(BackendCmd::CreateImage {
            id,
            width: surface.extent.width,
            height: surface.extent.height,
            format,
        });
        let mut dirty_after_create = false;
        if !Self::emit_image_upload(id, key, &surface, mem, out) {
            dirty_after_create = true;
            tracing::debug!(
                addr = key.addr,
                size = key.size,
                "texture initial upload skipped (guest range unreadable); entry inserted dirty"
            );
        }
        dirty.watch(key.addr, key.size);
        self.entries.insert(
            key,
            Entry {
                id,
                imported: false,
                dirty: dirty_after_create,
            },
        );
        id
    }

    /// Read the guest texture bytes (bounded), detile to linear RGBA per `surface`, and
    /// append an `UploadImage` command. Returns whether a command was emitted: `false`
    /// when the guest range is unreadable OR the detile fails (short buffer), in which
    /// case nothing is appended and the caller keeps the entry dirty for a later retry.
    fn emit_image_upload(
        id: ResourceId,
        key: ResourceKey,
        surface: &SurfaceLayout,
        mem: &dyn VirtualMemoryManager,
        out: &mut Vec<BackendCmd>,
    ) -> bool {
        let Ok(bytes) = mem.read_bytes_ranged(key.addr, key.size as usize) else {
            return false;
        };
        // Detile the guest bytes into row-major linear RGBA (identity copy for a linear
        // surface). A short/malformed buffer fails cleanly — no partial upload.
        match tile::detile(&bytes, surface) {
            Ok(linear) => {
                out.push(BackendCmd::UploadImage {
                    id,
                    data: std::sync::Arc::from(linear.into_boxed_slice()),
                });
                true
            }
            Err(_) => false,
        }
    }

    /// Snapshot the current guest bytes of `key`'s range and append an `UploadBuffer`
    /// command for resource `id` (copy path). Returns whether a command was emitted:
    /// `false` when reading the guest range fails, in which case nothing is appended (the
    /// backend buffer is left untouched, stale/zero) and the caller must keep the entry
    /// dirty so a later `get` retries.
    fn emit_upload(
        id: ResourceId,
        key: ResourceKey,
        mem: &dyn VirtualMemoryManager,
        out: &mut Vec<BackendCmd>,
    ) -> bool {
        // Range-validated read: the whole `[addr, addr+size)` must be backed by a single
        // contiguous mapping before any byte is copied. The unbounded `read_bytes` only
        // checks the start address and then copies `size` bytes, silently over-reading past
        // a VMA boundary/gap into raw host memory (a SIGSEGV, or garbage shipped in the
        // snapshot). On `Err` we emit no upload and the caller keeps the entry dirty.
        match mem.read_bytes_ranged(key.addr, key.size as usize) {
            Ok(bytes) => {
                // Reuse the `Vec`'s allocation for the `Arc<[u8]>` rather than `Vec -> Arc`
                // via `into()`, which would allocate + memcpy a second time.
                out.push(BackendCmd::UploadBuffer {
                    id,
                    offset: 0,
                    data: std::sync::Arc::from(bytes.into_boxed_slice()),
                });
                true
            }
            Err(_) => false,
        }
    }

    /// Drain the dirty source once per submit (doc-4 §8.3) and mark every cache entry
    /// whose backing range overlaps a dirtied range for re-upload (§6 linchpin). Call at
    /// each submit boundary before the draws that consume cached buffers.
    ///
    /// Imported entries are left clean: the GPU reads their guest pages directly, so a
    /// guest write is already visible with no re-upload (doc-4 §8.2).
    pub fn drain_dirty(&mut self, dirty: &dyn DirtySource) {
        let dirtied = dirty.take_dirty();
        if dirtied.is_empty() {
            return;
        }
        for (key, entry) in self.entries.iter_mut() {
            if entry.imported {
                continue;
            }
            if dirtied
                .iter()
                .any(|&(addr, size)| ranges_overlap(key.addr, key.size, addr, size))
            {
                entry.dirty = true;
            }
        }
    }

    /// Mark any cached entry overlapping `[addr, addr+size)` dirty (doc-4 §8.1). A direct
    /// hook for callers that already know a guest write happened, bypassing the
    /// [`DirtySource`] poll; [`Self::drain_dirty`] is the per-submit path.
    pub fn invalidate_range(&mut self, addr: u64, size: u64) {
        for (key, entry) in self.entries.iter_mut() {
            if !entry.imported && ranges_overlap(key.addr, key.size, addr, size) {
                entry.dirty = true;
            }
        }
    }

    /// The guest freed/unmapped `[addr, addr + size)` — evict every cache entry whose
    /// backing range overlaps it (doc-4 §8). This is the lifecycle counterpart to
    /// [`Self::get`]: without it, entries are only ever inserted (keyed by
    /// `(addr, size, layout)`), so a guest that frees and reallocs the *same* range — a
    /// common `sceKernelAllocateDirectMemory` reuse — would hit a clean cache entry
    /// returning the OLD [`ResourceId`], i.e. the old (now freed) backend buffer:
    /// wrong data or a use-after-free.
    ///
    /// For each overlapping entry this:
    /// - drops the entry from the map (a subsequent [`Self::get`] for the same key mints a
    ///   NEW id and re-creates the resource — no stale-id clean hit), and
    /// - appends a [`BackendCmd::FreeResource`] so the display thread destroys the backend
    ///   buffer or, for a zero-copy `imported` entry, **revokes** the external-memory
    ///   import — which is the sole path that unimports an entry ([`Self::drain_dirty`] /
    ///   [`Self::invalidate_range`] both skip imported entries), so without this a freed
    ///   garlic import would dangle into the freed host pages the GPU keeps reading, and
    /// - `unwatch`es the backing range so the dirty source stops tracking freed pages.
    ///
    /// The display thread frees fence-safely (it waits on the in-flight draw list's fence
    /// before destroying), so a resource the GPU may still read this frame is not pulled
    /// out from under it. Returns nothing; the caller ships `out` across the display
    /// channel exactly like a submit's command list. A freed range with no overlapping
    /// entry appends nothing.
    pub fn free_range(
        &mut self,
        addr: u64,
        size: u64,
        dirty: &dyn DirtySource,
        out: &mut Vec<BackendCmd>,
    ) {
        // Collect the freed keys first: `HashMap::retain`'s closure cannot also borrow
        // `out`/`dirty` freely while iterating, and eviction must both emit a command and
        // unwatch per key. Small (a free touches a handful of entries), so a Vec is fine.
        let freed: Vec<ResourceKey> = self
            .entries
            .keys()
            .filter(|k| ranges_overlap(k.addr, k.size, addr, size))
            .copied()
            .collect();
        for key in freed {
            if let Some(entry) = self.entries.remove(&key) {
                out.push(BackendCmd::FreeResource { id: entry.id });
                dirty.unwatch(key.addr, key.size);
            }
        }
    }
}

/// Whether two `[start, start+len)` byte ranges overlap (half-open, doc-4 §8.1). A
/// zero-length range overlaps nothing.
fn ranges_overlap(a_start: u64, a_len: u64, b_start: u64, b_len: u64) -> bool {
    if a_len == 0 || b_len == 0 {
        return false;
    }
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_start < b_end && b_start < a_end
}

#[cfg(test)]
mod tests;
