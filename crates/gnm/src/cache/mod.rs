//! Unified-memory → host resource cache (doc-2 §8): maps a `(guest range, kind,
//! layout)` to a backend handle, upload-on-use / invalidate-on-dirty. Vulkan-free —
//! emits `BackendCmd`s (`CreateBuffer`/`UploadBuffer`/`ImportBuffer`) for the display
//! thread to replay against the real `GpuBackend`.
//!
//! Phase 3.5 scope: linear vertex / index / constant BUFFERS only (doc-2 §8.6). No
//! textures/tiling (the [`ResLayout`] enum may gain variants, but only linear buffer
//! kinds are implemented here) and no readback (the GPU→guest reverse direction stays
//! off the hot path, doc-2 §8.5).
//!
//! ## Id ownership across the guest/display thread boundary
//!
//! doc-2 §8.1 gives [`ResourceCache::get`] a **synchronous** `-> ResourceId`
//! signature, but at runtime the cache runs on the **guest thread** (inside the
//! `libSceGnmDriver` submit handler, where only a `&dyn PresentSink` reaches the
//! display side) while the sole `GpuBackend` — and, in the old shape, id minting —
//! live on the **display thread** across a one-way channel (doc-2 §3). Two seams
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
//! ## Eviction policy: guest-free driven, plus a byte budget on buffers
//!
//! The cache grows one entry per distinct `(addr, size, layout)`. The primary trim is
//! [`ResourceCache::free_range`], driven by the guest freeing/unmapping the backing range
//! (`sceKernelReleaseDirectMemory`/`munmap`): the guest owns the lifetime of every
//! GPU-visible buffer and frees a range before reusing it for something else, so a
//! free-driven evict tracks its allocator exactly.
//!
//! That is the whole story only for a title that reuses a bounded set of ranges. Celeste
//! does not. Its dynamic geometry comes out of a ring, and **both halves of the key move
//! together**: the V# base is the ring write cursor, and that V#'s `num_records` spans the
//! cursor to the end of the ring, so `size` shrinks by exactly as many bytes as `addr`
//! advances. Every batch therefore mints a key that will never be asked for again — the
//! cache cannot hit by construction — while the guest frees the ring only at teardown. The
//! entry map, and the device buffers behind it, grew for the whole session.
//!
//! So the linear buffer kinds carry a second bound: [`ResourceCache::trim`] holds them to
//! [`BUFFER_BUDGET_BYTES`], evicting least-recently-used first. Textures and render targets
//! are exempt — they are expensive to rebuild and are not subject to the ring pattern.
//! Evicting is only ever wasteful, never wrong: a later [`get`] for the same key mints a
//! fresh id and re-uploads the current guest bytes. A byte budget rather than an age is
//! what keeps it from being wasteful. An age bound was tried first and was measurably
//! worse: a ring wraps on its own period, not the frame's, so a cursor position is reused
//! several frames after it was written, and a short TTL evicted exactly the entries about
//! to be hit again — vertex creates went from 1.0 to 12.3 per flip. Under a byte budget a
//! title whose whole working set fits is never trimmed at all, and one that overruns keeps
//! the most recent window of it.
//!
//! The dirty-tracking seam ([`DirtySource`], doc-2 §8.3) lives in `ps4-core`
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

// --- task-178 probe (2): texture-cache reuse/staleness (env UNEMUPS4_TEXCACHE_TRACE=1, zero-cost off) ---
// Tests the maintainer's hypothesis: PS4 games reuse the same memory for different textures;
// if our dirty-tracking misses a reuse write we serve a STALE cached texture. On each
// get_texture we hash the first guest bytes at the base; a CLEAN cache hit whose content
// hash changed since the last upload = a missed reuse write (STALE-HIT), logged loudly.
fn texcache_trace() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("UNEMUPS4_TEXCACHE_TRACE").is_ok_and(|v| v != "0" && !v.is_empty())
    })
}
fn texcache_hashes() -> &'static std::sync::Mutex<HashMap<u64, u64>> {
    use std::sync::OnceLock;
    static H: OnceLock<std::sync::Mutex<HashMap<u64, u64>>> = OnceLock::new();
    H.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}
fn texcache_content_hash(mem: &dyn VirtualMemoryManager, addr: u64, size: u64) -> Option<u64> {
    let n = size.min(256) as usize;
    let bytes = mem.read_bytes_ranged(addr, n).ok()?;
    let mut h: u64 = 0xcbf29ce484222325;
    for b in &bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    Some(h)
}
/// task-178 probe (3): record the content hash of a buffer range at upload time, so a later
/// CLEAN hit can detect a guest rewrite our dirty-tracking missed (STALE-HIT). No-op when the
/// probe env is off. Keyed by (addr ^ size<<1) to match the clean-hit lookup.
fn store_upload_hash(key: ResourceKey, mem: &dyn VirtualMemoryManager) {
    if !texcache_trace()
        || !matches!(
            key.layout,
            ResLayout::VertexBuf | ResLayout::IndexBuf | ResLayout::ConstBuf
        )
    {
        return;
    }
    if let Some(h) = texcache_content_hash(mem, key.addr, key.size) {
        texcache_hashes()
            .lock()
            .unwrap()
            .insert(key.addr ^ (key.size << 1), h);
    }
}

/// The pixel data/number format of a tiled surface (doc-2 §C3). Carried on the texture /
/// render-target key so the same bytes viewed under two formats key separately; a full
/// GCN `dfmt`/`nfmt` decode is deferred — this holds the raw hardware pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceFormat {
    /// GCN data format (`dfmt`).
    pub dfmt: u8,
    /// GCN number format (`nfmt`).
    pub nfmt: u8,
}

/// Disambiguates the same guest bytes viewed as different resource kinds (doc-2 §8.1).
/// Carries tiling/compression/coherence fields per doc-2 §C3/§C5/§C9 as they land.
/// The buffer kinds are linear; the `Texture`/`RenderTarget` kinds carry the tiling +
/// compression fields the §C3/§C9 detile seam dispatches on ([`tile::detile`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResLayout {
    VertexBuf,
    IndexBuf,
    ConstBuf,
    /// A sampled texture: format + full tiled byte layout (extent, tile mode,
    /// compression). Detiled on upload; never zero-copy when tiled (doc-2 §C3).
    Texture {
        format: SurfaceFormat,
        surface: SurfaceLayout,
    },
    /// A render target: same format + tiled layout as a texture; the split key exists so
    /// a range aliased as both RT and texture yields two entries (doc-2 §8.1).
    RenderTarget {
        format: SurfaceFormat,
        surface: SurfaceLayout,
    },
}

/// Flips a linear buffer entry survives without being handed out before
/// [`ResourceCache::trim`] evicts it. Four is comfortably longer than the deepest
/// double/triple buffering a guest can have in flight, so an entry the guest genuinely
/// reuses every frame is never a candidate, while a one-shot ring window is gone within a
/// few frames of the batch that used it.
const BUFFER_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Whether a layout is one of the linear buffer kinds the byte budget covers.
fn is_linear_buffer(layout: &ResLayout) -> bool {
    matches!(
        layout,
        ResLayout::VertexBuf | ResLayout::IndexBuf | ResLayout::ConstBuf
    )
}

/// Whether dirty-tracking a layout's range earns back what it costs (task-227).
///
/// The barrier is not free and its price is paid per guest STORE: every write into a
/// watched range leaves compiled code and calls into Rust (x86jit's
/// `note_watched_write_helper`, measured at 298M calls per 10 s of Celeste gameplay — 250x
/// every other helper combined). What it buys is the re-uploads a *clean hit* avoids. So
/// watching pays exactly when writes are rare relative to hits, and is pure loss when they
/// are not.
///
/// The split is not guessed, it is read off the per-kind counters in a Celeste gameplay
/// window (`gets/flip` = clean + dirty + create):
///
/// ```text
/// vertex   57.9 =  0.0 clean + 35.7 dirty + 22.1 create   watching buys nothing
/// const    39.0 =  0.0 clean + 37.0 dirty +  1.9 create   watching buys nothing
/// index    19.9 = 15.6 clean +  0.0 dirty +  4.2 create   watching saves 15.6 uploads/flip
/// texture   7.6 =  7.6 clean +  0.0 dirty +  0.0 create   watching saves the detile too
/// ```
///
/// **Zero clean hits is the whole argument.** A clean hit is the only thing dirty state can
/// buy: it is the case where tracking said "untouched" and an upload was skipped. Vertex
/// and constant ranges never reach it — MonoGame streams geometry through a ring whose V#
/// base *is* the write cursor (so `addr` and `size` move together and every batch mints an
/// unseen key), and it rewrites the per-draw constant buffers every frame. Every get on
/// those is either a create or a dirty hit, both of which upload. Unwatching them changes
/// no command the backend receives; it only stops paying the barrier to learn what we
/// already act on unconditionally.
///
/// Index buffers look superficially similar (small, linear, next to the vertex data) but
/// measure as the opposite: 15.6 clean hits per flip and no dirty ones at all, because
/// sprite batching indexes quads through one static buffer. They keep their tracking, as do
/// textures. Render targets were never watched — the GPU writes them, not the guest.
///
/// Correctness pairing: an unwatched range can never be *reported* dirty, so [`Self::get`]
/// must not serve it as a clean hit. It takes the re-upload path unconditionally instead,
/// which is what the cache did for these ranges anyway for most of the project's life —
/// `watch_range` silently no-opped above x86jit's old 4 GiB window while our buffers sit
/// near 41 GiB, and Celeste rendered correctly throughout.
fn watch_pays(layout: &ResLayout) -> bool {
    !matches!(layout, ResLayout::VertexBuf | ResLayout::ConstBuf)
}

/// Entries the profiler's miss-classification side table holds before it stops growing.
const PROBE_SEEN_CAP: usize = 1 << 16;

/// Cache key: the same bytes seen as two kinds get two entries (doc-2 §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResourceKey {
    pub addr: u64,
    pub size: u64,
    pub layout: ResLayout,
}

/// The onion/garlic memory-coherence hint that feeds the §8.2 policy decision
/// (doc-2 §C5): garlic (GPU-optimized, CPU-uncached) ranges are zero-copy candidates;
/// onion (CPU-coherent) ranges are copy + dirty-track. The flag originates in the
/// kernel memory manager and is threaded in per-range once that dependency lands; the
/// cache's policy step consults it here without hardcoding one coherence assumption.
///
/// Phase 3.5 defaults to [`Coherence::CopySide`] (the portable default, doc-2 §8.2):
/// everything is treated as copy-side regardless of the flag until the memory-manager
/// ↔ cache dependency is wired.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Coherence {
    /// CPU-coherent (onion) or unknown: copy + dirty-track. The portable default.
    #[default]
    CopySide,
    /// GPU-optimized (garlic): a zero-copy import candidate (doc-2 §C5), still subject
    /// to the backend's [`ImportProbe`] actually accepting the range.
    ZeroCopyCandidate,
}

/// Guest-side mirror of the backend's zero-copy import capability (doc-2 §8.2, §3).
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
/// is correct on MoltenVK and any backend without `external_memory_host` (doc-2 §8.2).
#[derive(Default)]
pub struct NoImport;

impl ImportProbe for NoImport {
    fn can_import(&self, _addr: u64, _size: u64, _coherence: Coherence) -> bool {
        false
    }
}

/// The per-range coherence resolver (doc-2 §C5 onion/garlic seam). It reads the
/// memory-type flag the kernel memory manager attaches to a range and maps it to a
/// [`Coherence`] the cache's policy step consults. Kept a trait so the memory-manager ↔
/// cache dependency can be threaded in later without changing [`ResourceCache::get`].
pub trait CoherenceSource: Send {
    /// The coherence hint for `[addr, addr+size)`. Defaults conservative.
    fn coherence(&self, addr: u64, size: u64) -> Coherence;
}

/// The phase-3.5 default: everything copy-side (doc-2 §8.2), regardless of range —
/// no memory-manager wiring yet, so the onion/garlic flag is not consulted.
#[derive(Default)]
pub struct AlwaysCopySide;

impl CoherenceSource for AlwaysCopySide {
    fn coherence(&self, _addr: u64, _size: u64) -> Coherence {
        Coherence::CopySide
    }
}

/// The policy inputs the cache consults per range (doc-2 §C5 onion/garlic seam, §8.2).
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
    /// Copy-side default (doc-2 §8.2): never import zero-copy.
    pub fn copy_side() -> CachePolicy {
        CachePolicy::default()
    }

    /// Install a coherence source + zero-copy import mirror (doc-2 §C5): the memory-type
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
/// zero-copy, whether it is a GPU-filled render target, and whether the guest has written
/// its backing range since the last upload.
struct Entry {
    id: ResourceId,
    /// Zero-copy imports never need re-upload — the GPU always sees current guest bytes
    /// (doc-2 §8.2), so `dirty` is irrelevant for them and stays `false`.
    imported: bool,
    /// A GPU-filled render target (doc-2 §8.5, task-56): its bytes come from the GPU
    /// rendering into it, never from a guest-byte upload. A guest write to its backing range
    /// therefore does NOT invalidate it (the CPU never authored those bytes) —
    /// [`drain_dirty`] skips it exactly like an imported entry, and it never carries an
    /// upload command.
    ///
    /// [`drain_dirty`]: ResourceCache::drain_dirty
    is_rt: bool,
    dirty: bool,
    /// The flip this entry was last handed out on, driving [`ResourceCache::trim`].
    last_used: u64,
}

/// The unified-memory resource cache (doc-2 §8.1/§8.2). Guest-thread-resident, Vulkan-
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
    /// TEST (white-dummy hypothesis): a 1x1 opaque-white sampled image, minted once and
    /// reused, bound in place of a degenerate/unmappable T# so `texel × vcol = vcol`.
    white_dummy: Option<ResourceId>,
    policy: CachePolicy,
    /// Profiler-only (task-223): the sizes ever requested at each `(base, kind)`, so a
    /// create can be classified as a base never seen, a new size at a known base, or a
    /// rebuild of a key that was evicted. Stays empty when the profiler is off.
    probe_seen: HashMap<(u64, usize), Vec<u64>>,
    /// The flip [`ResourceCache::trim`] last ran on, so the sweep happens once per flip
    /// rather than once per `get`.
    trimmed_flip: u64,
    /// Guest bytes the live linear buffer entries cover, against [`BUFFER_BUDGET_BYTES`].
    /// Maintained incrementally so the common (under-budget) trim is a single comparison.
    buffer_bytes: u64,
}

impl Default for ResourceCache {
    fn default() -> ResourceCache {
        ResourceCache::new()
    }
}

impl ResourceCache {
    /// A copy-side cache (doc-2 §8.2 portable default): no zero-copy import mirror.
    pub fn new() -> ResourceCache {
        ResourceCache::with_policy(CachePolicy::copy_side())
    }

    /// A cache with an explicit onion/garlic + import policy (doc-2 §C5 seam).
    pub fn with_policy(policy: CachePolicy) -> ResourceCache {
        ResourceCache {
            entries: HashMap::new(),
            samplers: HashMap::new(),
            next_id: 1,
            white_dummy: None,
            policy,
            probe_seen: HashMap::new(),
            trimmed_flip: 0,
            buffer_bytes: 0,
        }
    }

    /// Hold the linear buffer entries to a byte budget, evicting least-recently-used first
    /// and appending their `FreeResource` teardown to `out`. Runs at most once per flip and
    /// only once the budget is exceeded; a no-op when the flip counter has not moved
    /// (headless tests never flip, so they never trim).
    ///
    /// The module doc explains why the primary trim is guest-free driven and why that is not
    /// enough for a ring-fed title. This is the bound for that case: a rolling window of the
    /// most recently used ranges, sized so a title whose whole working set fits is never
    /// touched at all.
    ///
    /// Only the linear buffer kinds are evicted. A texture or render target is expensive to
    /// rebuild (a detile, or GPU-authored contents a re-create would lose) and neither is
    /// subject to the ring pattern — in a Celeste window both miss zero times.
    ///
    /// **The evicted range stays watched.** Only [`Self::free_range`] unwatches, because
    /// only there did the guest actually give the memory back. Dirty tracking is
    /// page-granular: unwatching an evicted entry's range would drop the write protection
    /// for every page it shares with entries that are still live — and a ring's entries all
    /// share pages, as do the small per-frame constant buffers — so their guest rewrites
    /// would go unseen and the cache would serve them as clean, stale hits. A watch with no
    /// entry behind it costs one dirty range per flip that matches nothing.
    fn trim(&mut self, out: &mut Vec<BackendCmd>) {
        let now = ps4_core::clock::flip_count();
        if now == self.trimmed_flip {
            return;
        }
        self.trimmed_flip = now;
        if self.buffer_bytes <= BUFFER_BUDGET_BYTES {
            return;
        }
        let mut candidates: Vec<(u64, u64, ResourceKey)> = self
            .entries
            .iter()
            .filter(|(key, entry)| is_linear_buffer(&key.layout) && !entry.imported)
            .map(|(key, entry)| (entry.last_used, key.size, *key))
            .collect();
        candidates.sort_unstable_by_key(|&(last_used, _, _)| last_used);
        for (_, size, key) in candidates {
            if self.buffer_bytes <= BUFFER_BUDGET_BYTES {
                break;
            }
            if let Some(entry) = self.entries.remove(&key) {
                out.push(BackendCmd::FreeResource { id: entry.id });
                self.buffer_bytes = self.buffer_bytes.saturating_sub(size);
            }
        }
        self.note_live();
    }

    /// Profiler-only (task-223): count one `get`/`get_texture`/`get_render_target` call.
    #[inline]
    fn note_get(layout: &ResLayout) {
        if crate::profile::enabled() {
            crate::profile::CACHE.gets[crate::profile::kind_index(layout)]
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Profiler-only (task-223): count one hit, `dirty` telling the re-upload apart from
    /// the free clean hit.
    #[inline]
    fn note_hit(layout: &ResLayout, dirty: bool) {
        if crate::profile::enabled() {
            let k = crate::profile::kind_index(layout);
            let c = if dirty {
                &crate::profile::CACHE.dirty_hits
            } else {
                &crate::profile::CACHE.clean_hits
            };
            c[k].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Profiler-only (task-223): count one create and say why the cache could not hit —
    /// the base was never requested, the base is known but this size is not, or the exact
    /// key was requested before and its entry has since been evicted. Also flags a create
    /// whose range lies wholly inside a still-live entry of the same kind, i.e. bytes that
    /// are already on the GPU and only needed an offset.
    fn note_create(&mut self, key: ResourceKey) {
        if !crate::profile::enabled() {
            return;
        }
        use std::sync::atomic::Ordering::Relaxed;
        let stats = &crate::profile::CACHE;
        let k = crate::profile::kind_index(&key.layout);
        stats.creates[k].fetch_add(1, Relaxed);
        stats.create_bytes[k].fetch_add(key.size, Relaxed);
        let sub_range = self.entries.keys().any(|e| {
            crate::profile::kind_index(&e.layout) == k
                && e.addr <= key.addr
                && key.addr.saturating_add(key.size) <= e.addr.saturating_add(e.size)
        });
        if sub_range {
            stats.miss_sub_range[k].fetch_add(1, Relaxed);
        }
        match self.probe_seen.get_mut(&(key.addr, k)) {
            None => {
                stats.miss_new_base[k].fetch_add(1, Relaxed);
                stats.distinct_bases.fetch_add(1, Relaxed);
                // Capped: a rotating ring mints a base per batch forever, and this table
                // exists only to classify. Past the cap every base reads as new, which is
                // the honest answer for exactly the workload that reaches it.
                if self.probe_seen.len() < PROBE_SEEN_CAP {
                    self.probe_seen.insert((key.addr, k), vec![key.size]);
                }
            }
            Some(sizes) if sizes.contains(&key.size) => {
                stats.miss_recreate[k].fetch_add(1, Relaxed);
            }
            Some(sizes) => {
                stats.miss_new_size[k].fetch_add(1, Relaxed);
                sizes.push(key.size);
            }
        }
    }

    /// Profiler-only (task-223): republish the live-entry gauge after an insert or evict.
    #[inline]
    fn note_live(&self) {
        if crate::profile::enabled() {
            crate::profile::CACHE.live_entries.store(
                self.entries.len() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    /// TEST (white-dummy hypothesis): get-or-mint a 1x1 opaque-white sampled image and
    /// return its id. Minted once (CreateImage + a 4-byte 0xFFFFFFFF UploadImage) and
    /// reused for every degenerate/unmappable T#, so sampling it yields white and
    /// `texel(white) × vcol = vcol`.
    pub fn get_white_dummy(&mut self, out: &mut Vec<BackendCmd>) -> ResourceId {
        if let Some(id) = self.white_dummy {
            return id;
        }
        let id = self.mint_id();
        out.push(BackendCmd::CreateImage {
            id,
            width: 1,
            height: 1,
            format: ps4_core::gpu::TextureFormat::R8G8B8A8Unorm,
        });
        out.push(BackendCmd::UploadImage {
            id,
            data: std::sync::Arc::from(vec![0xFFu8, 0xFF, 0xFF, 0xFF].into_boxed_slice()),
        });
        self.white_dummy = Some(id);
        id
    }

    /// Get-or-create a sampler for `desc` (doc-2 §C4): mints one id per distinct
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
        // Ids are never reused, so exhausting the u32 space must fail LOUD, not wrap: a wrapped
        // `next_id` would re-issue an id still bound on the display side to a live resource (the
        // white_dummy id 1, a sampler, or a cache entry), and the `id -> vk::Buffer` map would
        // then serve the wrong backend resource. In a release build `+= 1` wraps silently;
        // `checked_add` turns that into a hard error instead. (`ResourceId` is a core `u32`
        // newtype, so widening the space is out of this crate's reach — fail loud is the fix.)
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("ResourceCache id space (u32) exhausted — too many never-reused ring mints");
        id
    }

    /// The single entry point (doc-2 §8.1): "I need this guest `key` range on the GPU."
    /// Appends the [`BackendCmd`]s needed to make it so to `out` and returns the resource
    /// id; the display thread replays `out` against the real backend (there is no backend
    /// to call on the guest thread — see the module doc).
    ///
    /// - **First use:** pick policy (zero-copy import if the range is a garlic candidate
    ///   *and* the guest-side mirror accepts it, else copy). Import path: append one
    ///   `ImportBuffer` (no upload — the entry is imported, authoritative per the probe).
    ///   Copy path: append `CreateBuffer` + `UploadBuffer`. `watch` the range for dirty
    ///   tracking (unless [`watch_pays`] says the barrier cannot earn it back), record the
    ///   entry.
    /// - **Clean hit:** return the cached id with no commands (§6 linchpin, AC #1). Only a
    ///   watched entry can take this path; an unwatched one has no dirty state to trust.
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
        Self::note_get(&key.layout);
        self.trim(out);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.trimmed_flip;
            let entry = &*entry;
            // task-178 workaround, REMOVED once x86jit `873563f` landed: dynamic COPY-path
            // buffers (the MonoGame vertex ring + per-frame projection const buffers) are
            // rewritten by the guest EVERY frame, and x86jit's watched-range dirty source
            // used to miss those writes entirely, so the dirty flag served STALE bytes →
            // the Celeste title-screen frame-alternating garbage. The cause was on the
            // x86jit side (its task-275): `watch_page` was sized with the CODE-page sizing,
            // capped at `CODE_WINDOW` (4 GiB). Correct for code, exactly wrong for watched
            // DATA — our GPU buffers sit around 41 GiB in the direct-memory heap, so every
            // `watch_range` silently no-opped and `take_dirty` came back empty on 3709 of
            // 3709 submits. With watched-page tracking now spanning the whole guest address
            // space, these buffers take the incremental dirty path like every other entry.
            // task-184 had an experiment knob here (`UNEMUPS4_X_FORCE_CONST_REUPLOAD=1`)
            // that re-uploaded constant buffers on every hit instead of trusting the dirty
            // flag. task-227 removed it because that behaviour is now unconditional:
            // constant buffers are no longer watched, so they always take the re-upload
            // path below and the knob could not change anything.
            //
            // An unwatched layout has no dirty state to trust — nothing reports its writes,
            // so the only safe reading of `entry.dirty == false` is "unknown".
            if entry.imported || (!entry.dirty && watch_pays(&key.layout)) {
                // task-178 probe (3): buffer STALE-HIT. A clean hit re-uploads NOTHING; if the
                // guest rewrote this range but dirty-tracking (SMC) missed it, we serve stale
                // bytes. Compare the CURRENT guest content hash to the one stored at the last
                // upload — a mismatch on a clean hit = missed reuse write (the vertex-ring double-
                // buffer alternation suspect). Keyed by (addr,layout) so a buffer and a texture at
                // the same base don't collide.
                if texcache_trace()
                    && matches!(
                        key.layout,
                        ResLayout::VertexBuf | ResLayout::IndexBuf | ResLayout::ConstBuf
                    )
                    && let Some(cur) = texcache_content_hash(mem, key.addr, key.size)
                {
                    let hk = key.addr ^ ((key.size) << 1);
                    let prev = texcache_hashes().lock().unwrap().get(&hk).copied();
                    if let Some(prev) = prev
                        && prev != cur
                    {
                        tracing::info!(
                            "[BUFCACHE STALE-HIT] flip={} addr={:#x} size={} layout={:?} \
                             prev={prev:016x} cur={cur:016x} — dirty-tracking MISSED a guest \
                             rewrite, serving STALE buffer",
                            ps4_core::clock::flip_count(),
                            key.addr,
                            key.size,
                            key.layout,
                        );
                    }
                }
                Self::note_hit(&key.layout, false);
                return entry.id; // clean hit: no commands (AC #1)
            }
            Self::note_hit(&key.layout, true);
            // Dirty hit: emit a re-upload command, then clear the flag — but ONLY if the
            // re-upload actually snapshotted bytes. If reading the guest range fails (e.g.
            // the guest remapped/freed it between submits), no command is emitted and the
            // backend buffer still holds stale bytes; clearing `dirty` would strand it
            // clean-but-wrong and never retry. Leave it dirty so the next `get` retries
            // once the range is readable again.
            let id = entry.id;
            let uploaded = Self::emit_upload(id, key, mem, out);
            if uploaded {
                store_upload_hash(key, mem);
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
        self.note_create(key);
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
            if Self::emit_upload(id, key, mem, out) {
                store_upload_hash(key, mem);
            } else {
                dirty_after_create = true;
                tracing::debug!(
                    addr = key.addr,
                    size = key.size,
                    "cache initial upload skipped (guest range unreadable); entry inserted dirty"
                );
            }
        }
        // Dirty-track the backing range either way; imported entries ignore it. Layouts the
        // barrier cannot pay for itself on are left unwatched (task-227, [`watch_pays`]) —
        // their hits re-upload unconditionally instead.
        if watch_pays(&key.layout) {
            dirty.watch(key.addr, key.size);
        }
        if is_linear_buffer(&key.layout) && !imported {
            self.buffer_bytes = self.buffer_bytes.saturating_add(key.size);
        }
        self.entries.insert(
            key,
            Entry {
                id,
                imported,
                is_rt: false,
                dirty: dirty_after_create,
                last_used: self.trimmed_flip,
            },
        );
        self.note_live();
        id
    }

    /// Get-or-create a sampled texture on the GPU (doc-2 §C3), the image analogue of
    /// [`Self::get`]. `key` must carry a [`ResLayout::Texture`]; `surface` is its byte
    /// layout (extent + tile mode + format). On first use this reads the guest texture
    /// through the bounded seam, **detiles** it to linear RGBA (`tile::detile`), and
    /// appends `CreateImage` + `UploadImage`; a dirty hit appends one `UploadImage` (the
    /// re-detiled bytes); a clean hit appends nothing (§6 linchpin — exactly one
    /// re-upload per guest write). Returns the image [`ResourceId`].
    ///
    /// Textures are always copy-path (a tiled surface is never host-linear, so it can't
    /// be imported zero-copy — doc-2 §C3); a guest write is tracked via [`DirtySource`]
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
        Self::note_get(&key.layout);
        self.trim(out);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.trimmed_flip;
            let entry = &*entry;
            Self::note_hit(&key.layout, entry.dirty);
            if !entry.dirty {
                if texcache_trace()
                    && let Some(nh) = texcache_content_hash(mem, key.addr, key.size)
                {
                    let mut m = texcache_hashes().lock().unwrap();
                    if let Some(&old) = m.get(&key.addr)
                        && old != nh
                    {
                        tracing::warn!(
                            "[TEXCACHE] STALE-HIT base={:#x} {}x{} — served CLEAN-cached but guest content changed (old={:#x} new={:#x}); dirty-tracking MISSED a reuse write",
                            key.addr,
                            surface.extent.width,
                            surface.extent.height,
                            old,
                            nh
                        );
                    }
                    m.insert(key.addr, nh);
                }
                return entry.id; // clean hit: no commands (exactly one upload per write)
            }
            let id = entry.id;
            let uploaded = Self::emit_image_upload(id, key, &surface, mem, out);
            if uploaded {
                if let Some(e) = self.entries.get_mut(&key) {
                    e.dirty = false;
                }
                if texcache_trace() {
                    if let Some(nh) = texcache_content_hash(mem, key.addr, key.size) {
                        texcache_hashes().lock().unwrap().insert(key.addr, nh);
                    }
                    tracing::info!(
                        "[TEXCACHE] REUPLOAD base={:#x} {}x{} (dirty hit)",
                        key.addr,
                        surface.extent.width,
                        surface.extent.height
                    );
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
        self.note_create(key);
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
                is_rt: false,
                dirty: dirty_after_create,
                last_used: self.trimmed_flip,
            },
        );
        self.note_live();
        if texcache_trace() {
            if let Some(nh) = texcache_content_hash(mem, key.addr, key.size) {
                texcache_hashes().lock().unwrap().insert(key.addr, nh);
            }
            tracing::info!(
                "[TEXCACHE] FIRST base={:#x} {}x{}",
                key.addr,
                surface.extent.width,
                surface.extent.height
            );
        }
        id
    }

    /// Get-or-create an offscreen **render target** on the GPU (doc-2 §8.5, task-56), the
    /// RT analogue of [`Self::get_texture`]. `key` must carry a [`ResLayout::RenderTarget`];
    /// `surface` is its host extent + format. On first use this mints an id and appends
    /// **exactly one** [`BackendCmd::CreateRenderTarget`] — and **never** an upload: a render
    /// target is filled by the GPU rendering into it, so uploading the guest bytes at its
    /// base (which the CPU never wrote) would ship garbage. On any later use it is a clean
    /// hit that appends nothing (a render target is never dirty-driven — the guest does not
    /// author its bytes). Returns the render target [`ResourceId`].
    ///
    /// The entry is flagged `is_rt`, so [`Self::drain_dirty`] and [`Self::invalidate_range`]
    /// leave it clean even when the guest writes its backing range: the RT bytes live on the
    /// GPU, decoupled from that range's CPU contents (the split [`ResLayout::RenderTarget`]
    /// key means a range aliased as both RT and texture yields two independent entries).
    pub fn get_render_target(
        &mut self,
        key: ResourceKey,
        surface: SurfaceLayout,
        format: ps4_core::gpu::ColorFormat,
        out: &mut Vec<BackendCmd>,
    ) -> ResourceId {
        debug_assert!(
            matches!(key.layout, ResLayout::RenderTarget { .. }),
            "get_render_target requires a RenderTarget-layout key"
        );
        Self::note_get(&key.layout);
        let trimmed_flip = self.trimmed_flip;
        if let Some(entry) = self.entries.get_mut(&key) {
            // A render target is never dirty-driven: the GPU authors its bytes, so every hit
            // after the first is a clean no-op (no create, no upload).
            entry.last_used = trimmed_flip;
            Self::note_hit(&key.layout, false);
            return entry.id;
        }
        self.note_create(key);
        let id = self.mint_id();
        out.push(BackendCmd::CreateRenderTarget {
            id,
            width: surface.extent.width,
            height: surface.extent.height,
            format,
        });
        // No `dirty.watch`: the guest never writes this entry's bytes (the GPU does), so
        // there is nothing for the dirty source to invalidate. `is_rt` keeps drain_dirty /
        // invalidate_range off it defensively even if the range is also watched via a
        // separate aliasing texture entry.
        self.entries.insert(
            key,
            Entry {
                id,
                imported: false,
                is_rt: true,
                dirty: false,
                last_used: self.trimmed_flip,
            },
        );
        self.note_live();
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
                // DIAGNOSTIC (env UNEMUPS4_DUMP_TEX): dump the detiled RGBA to a PNG, dedup by base.
                crate::texdump::dump_texture(key, surface, &linear);
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

    /// Drain the dirty source once per submit (doc-2 §8.3) and mark every cache entry
    /// whose backing range overlaps a dirtied range for re-upload (§6 linchpin). Call at
    /// each submit boundary before the draws that consume cached buffers.
    ///
    /// Imported entries are left clean: the GPU reads their guest pages directly, so a
    /// guest write is already visible with no re-upload (doc-2 §8.2).
    pub fn drain_dirty(&mut self, dirty: &dyn DirtySource) {
        let dirtied = dirty.take_dirty();
        self.apply_dirty(&dirtied);
    }

    /// Mark entries overlapping any range in `dirtied` dirty. Split out from
    /// [`drain_dirty`] so a single [`DirtySource::take_dirty`] drain can feed BOTH the buffer
    /// cache and the GCN shader provider (task-178): `take_dirty` is a DRAINING read, so
    /// draining it twice per submit left the second consumer (this cache) with nothing and
    /// served STALE dynamic buffers (the MonoGame vertex ring / projection const buffers) —
    /// the Celeste title-screen frame-alternating garbage. The caller drains once and calls
    /// `apply_dirty` on each consumer with the shared ranges.
    pub fn apply_dirty(&mut self, dirtied: &[(u64, u64)]) {
        if dirtied.is_empty() {
            return;
        }
        for (key, entry) in self.entries.iter_mut() {
            // Imported entries read guest pages directly; render targets are GPU-filled and
            // decoupled from the guest bytes at their base (doc-2 §8.2/§8.5). Neither is
            // re-uploaded from a guest write.
            if entry.imported || entry.is_rt {
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

    /// Mark any cached entry overlapping `[addr, addr+size)` dirty (doc-2 §8.1). A direct
    /// hook for callers that already know a guest write happened, bypassing the
    /// [`DirtySource`] poll; [`Self::drain_dirty`] is the per-submit path.
    pub fn invalidate_range(&mut self, addr: u64, size: u64) {
        for (key, entry) in self.entries.iter_mut() {
            // Imported + render-target entries are never re-uploaded from a guest write
            // (doc-2 §8.2/§8.5) — leave them clean.
            if !entry.imported && !entry.is_rt && ranges_overlap(key.addr, key.size, addr, size) {
                entry.dirty = true;
            }
        }
    }

    /// The guest freed/unmapped `[addr, addr + size)` — evict every cache entry whose
    /// backing range overlaps it (doc-2 §8). This is the lifecycle counterpart to
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
                // Only unwatch what we watched: dirty tracking is page-granular, so
                // unwatching a range we never protected can drop the protection a
                // co-resident live entry depends on (the stale-texture regression caught by
                // eye in task-223). A render target is `watch_pays`-true yet
                // `get_render_target` deliberately never watches it (`is_rt`) — the GPU, not
                // the guest, authors its bytes — so gate on `!is_rt` as well, else freeing an
                // RT would unwatch pages a live index/texture entry still shares.
                if watch_pays(&key.layout) && !entry.is_rt {
                    dirty.unwatch(key.addr, key.size);
                }
                if is_linear_buffer(&key.layout) && !entry.imported {
                    self.buffer_bytes = self.buffer_bytes.saturating_sub(key.size);
                }
            }
        }
        self.note_live();
    }
}

/// Whether two `[start, start+len)` byte ranges overlap (half-open, doc-2 §8.1). A
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
