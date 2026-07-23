//! `GcnShaderProvider` (doc-4 §4, phase 4): a `.sb` (OrbShdr) GCN binary →
//! recompiled SPIR-V, behind the [`ShaderProvider`] seam and chained *after* the
//! [`EmbeddedShaderProvider`](super::embedded::EmbeddedShaderProvider).
//!
//! On a [`ShaderRef::GcnBinary`] this provider:
//!   1. parses the `.sb` container via [`parse_sb`], reading through the process-global
//!      **bounded** seam ([`bounded_read`]) — never the caller's unbounded `mem`, because
//!      the shader address is register-derived and untrusted (see [`GcnShaderProvider::resolve`]);
//!   2. skips straight to a cached [`HostShader`] if this shader hash was recompiled before
//!      (the cache is the whole point — [`parse_sb`] alone is up to 256 windowed reads and
//!      `derive_bound_shaders` runs per draw);
//!   3. otherwise decodes the GCN code ([`decode_all`]) and recompiles it to SPIR-V
//!      ([`recompile`]), building a [`HostShader`] that carries the SPIR-V + the recompiler's
//!      [`IoLayout`] (I/O + resource layout, HW-stage role);
//!   4. a [`RecompileError`] is a clean **defer**, not a crash: it returns
//!      [`ShaderUnsupported`] (the chain's recognized-but-unsupported signal, which the
//!      executor maps to a skipped draw) and logs the offending instruction/reason.
//!
//! ## Cache + invalidation (doc-4 §8.3, mirrors the resource cache)
//!
//! The cache is keyed by the `.sb` `ShaderBinaryInfo` header material
//! (`shader_hash0`/`shader_hash1` + `crc32` + `code_len`) — a re-bind of the same shader
//! hash is a refcount bump on a cached `Arc<HostShader>`, not a re-parse + re-recompile.
//! It watches each recompiled shader's code range through the same [`DirtySource`] seam the
//! resource cache uses ([`GcnShaderProvider::drain_dirty`]): a guest write to a watched
//! range drops the overlapping cache entries so the next resolve re-recompiles the (possibly
//! self-modified) code. [`ShaderProvider::resolve`] takes `&self`, so the cache state is behind
//! a [`Mutex`]; the recompile itself runs outside the lock.
//!
//! Vulkan-free (decision-3): this provider names no `ash`/`vk` type — it hands the recompiler's
//! `Arc<[u32]>` SPIR-V words straight onto the [`HostShader`]; the display side builds the
//! actual pipeline (task-52/53).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ps4_core::bounded_read::{BoundedRead, bounded_read};
use ps4_core::dirty::DirtySource;
use ps4_core::memory::VirtualMemoryManager;
use ps4_gcn::{RecompileError, ShaderStage, decode_all, recompile};

use crate::shader::fetch::{FetchResolveError, VertexInputState, resolve_fetch_vertex_input};
use crate::shader::sb::{SbParseError, SbShader, SbStage, Semantics, parse_sb};
use crate::shader::source::{HostShader, ShaderProvider, ShaderRef, ShaderUnsupported, Stage};

/// The `.sb` header material that identities a shader across re-binds (doc-4 §8.3,
/// task-42 finding #5): the two 32-bit content hashes, the crc32, and the code length.
/// A guest that re-programs `SPI_SHADER_PGM_*` to the same shader mints the same key, so
/// the recompile is done once. Value-typed so it is a `HashMap` key directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ShaderCacheKey {
    hash0: u32,
    hash1: u32,
    crc32: u32,
    code_len: u32,
}

impl ShaderCacheKey {
    fn from_sb(sb: &SbShader) -> Self {
        ShaderCacheKey {
            hash0: sb.info.shader_hash0,
            hash1: sb.info.shader_hash1,
            crc32: sb.info.crc32,
            code_len: sb.info.code_len,
        }
    }
}

/// A cached recompiled shader plus the guest code range it was recompiled from — the range
/// is what [`GcnShaderProvider::drain_dirty`] watches and matches a dirtied write against.
struct CacheEntry {
    host: Arc<HostShader>,
    code_range: std::ops::Range<u64>,
}

/// GCN `.sb` → recompiled SPIR-V provider, chained after the embedded provider (doc-4 §4).
/// Owns a hash-keyed `Arc<HostShader>` cache so a re-bind skips the recompiler; the cache is
/// invalidated through the [`DirtySource`] seam exactly like the resource cache.
pub struct GcnShaderProvider {
    cache: Mutex<HashMap<ShaderCacheKey, CacheEntry>>,
    /// Recompiler invocations actually run (cache misses). A test hook proving a 2nd resolve
    /// of the same hash SKIPS the recompiler (AC #2): the counter must not advance on a hit.
    recompiles: AtomicU64,
}

impl Default for GcnShaderProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl GcnShaderProvider {
    pub fn new() -> Self {
        GcnShaderProvider {
            cache: Mutex::new(HashMap::new()),
            recompiles: AtomicU64::new(0),
        }
    }

    /// How many times the recompiler has run — the AC #2 test hook. It advances once per
    /// cache miss and stays put on a cache hit, so a test can assert a 2nd resolve of the
    /// same shader hash did not re-recompile.
    pub fn recompile_count(&self) -> u64 {
        self.recompiles.load(Ordering::SeqCst)
    }

    /// Drain the dirty source once per submit and drop every cache entry whose code range
    /// overlaps a dirtied write (doc-4 §8.3) — the same drain-then-invalidate shape the
    /// resource cache uses ([`crate::cache::ResourceCache::drain_dirty`]). A dropped entry is
    /// re-parsed + re-recompiled on its next resolve, so self-modifying / reloaded shader
    /// code is picked up. Call at each submit boundary before the draws that resolve shaders.
    pub fn drain_dirty(&self, dirty: &dyn DirtySource) {
        let dirtied = dirty.take_dirty();
        if dirtied.is_empty() {
            return;
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.retain(|_, e| {
                !dirtied.iter().any(|&(addr, size)| {
                    ranges_overlap(e.code_range.start, e.code_range.end, addr, size)
                })
            });
        }
    }

    /// Mark any cached shader whose code range overlaps `[addr, addr+size)` for
    /// re-recompilation (doc-4 §8.1). A direct hook for callers that already know a guest
    /// write happened, bypassing the [`DirtySource`] poll; [`Self::drain_dirty`] is the
    /// per-submit path.
    pub fn invalidate_range(&self, addr: u64, size: u64) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.retain(|_, e| !ranges_overlap(e.code_range.start, e.code_range.end, addr, size));
        }
    }

    /// Resolve a `GcnBinary` ref: parse `.sb`, hit-or-recompile, wrap in a [`HostShader`].
    /// `Ok(Some(host))` on success, `Err(ShaderUnsupported)` on any clean defer (parse
    /// reject, unmodeled stage, or a [`RecompileError`]). The `reader` is the bounded seam
    /// (never the caller's unbounded identity view).
    fn resolve_gcn(
        &self,
        addr: u64,
        reader: &dyn BoundedRead,
        dirty: Option<&dyn DirtySource>,
    ) -> Result<Option<HostShader>, ShaderUnsupported> {
        let sb = match parse_sb(addr, reader) {
            Ok(sb) => sb,
            Err(e) => {
                // A malformed / non-plaintext container is not this provider's shader to
                // bind. It is recognized-but-unbindable → a clean defer, never a crash.
                tracing::warn!("[GNM] GCN shader parse rejected at {addr:#x}: {e}");
                return Err(ShaderUnsupported);
            }
        };

        let key = ShaderCacheKey::from_sb(&sb);
        if let Ok(cache) = self.cache.lock()
            && let Some(entry) = cache.get(&key)
        {
            // Cache hit (AC #2): refcount bump on the shared Arc, recompiler NOT run.
            return Ok(Some(host_from_arc(&entry.host)));
        }

        let stage = match recompile_stage(sb.stage) {
            Some(s) => s,
            None => {
                // A GCN stage the recompiler does not model yet (compute/geometry/hull):
                // a clean defer, distinct from a malformed shader.
                tracing::warn!(
                    "[GNM] GCN shader at {addr:#x} has unmodeled stage {:?}; deferring",
                    sb.stage
                );
                return Err(ShaderUnsupported);
            }
        };

        // Read the raw GCN machine code through the SAME bounded seam and decode it. The
        // range is header-validated by parse_sb (`end - start == code_len`), so this read is
        // exactly the code region — no scan.
        let code = match read_code_words(reader, &sb.code_range) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[GNM] GCN code read faulted at {addr:#x}: {e}");
                return Err(ShaderUnsupported);
            }
        };
        let insts = decode_all(&code);

        let recompiled = match recompile(&insts, stage) {
            Ok(r) => r,
            Err(e) => {
                // A RecompileError is a DEFER, not a break (task-90 / task-41): log naming the
                // instruction/reason and return the chain's recognized-but-unsupported signal.
                tracing::warn!(
                    "[GNM] GCN recompile deferred at {addr:#x}: {}",
                    defer_reason(&e)
                );
                return Err(ShaderUnsupported);
            }
        };
        self.recompiles.fetch_add(1, Ordering::SeqCst);

        // The recompiler's stage carries the logical Stage the executor keys on; the .sb
        // header's logical_stage agrees (both derive from m_type). Prefer the header's, so
        // the HostShader.stage matches what the draw path bound.
        let logical = sb.logical_stage().unwrap_or(Stage::Vertex);
        let host = Arc::new(HostShader {
            stage: logical,
            spirv: recompiled.spirv.into(),
            io: Some(recompiled.io),
        });

        // Watch the code range so a later guest write to it invalidates this entry — the same
        // watch-on-insert the resource cache does. Skipped when no dirty source is threaded
        // (headless / no-invalidation callers): the entry then stays until process exit,
        // which is correct for immutable shader code.
        if let Some(d) = dirty {
            d.watch(sb.code_range.start, sb.code_range.end - sb.code_range.start);
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                key,
                CacheEntry {
                    host: host.clone(),
                    code_range: sb.code_range.clone(),
                },
            );
        }
        Ok(Some(host_from_arc(&host)))
    }

    /// Recover a VS's vertex-input state from its fetch shader (doc-4 §C4). The main
    /// VS calls a small fetch subroutine (at `fetch_addr`, `len` bytes) to pull its
    /// vertex attributes; this reads that subroutine's machine code through the
    /// **bounded** seam (register-derived, untrusted — never `mem`), parses the fetch
    /// convention, and merges the recovered per-attribute table with the `.sb`
    /// `VertexInputSemantic` table into the vertex-input layout the pipeline key needs.
    ///
    /// It does NOT execute the fetch shader — the returned [`VertexInputState`]
    /// replaces it, feeding the recompiled SSBO loads / the vertex-input state. A
    /// non-conforming fetch shader or a faulting read defers cleanly
    /// ([`ShaderUnsupported`]); nothing here panics.
    pub fn resolve_fetch_vertex_input(
        &self,
        fetch_addr: u64,
        len: usize,
        semantics: &Semantics,
    ) -> Result<VertexInputState, ShaderUnsupported> {
        let reader = bounded_read().ok_or_else(|| {
            tracing::warn!(
                "[GNM] bounded read seam not wired; fetch-shader parse cannot proceed \
                 (addr={fetch_addr:#x})"
            );
            ShaderUnsupported
        })?;
        resolve_fetch_vertex_input(fetch_addr, len, reader.as_ref(), semantics).map_err(|e| {
            // A non-conforming / unreadable fetch shader is a clean defer (AC #3): the
            // recovered layout is unavailable, so the draw's vertex input stays unbound
            // rather than the parser fabricating a bogus table.
            let _: FetchResolveError = e;
            tracing::warn!("[GNM] fetch-shader resolve deferred at {fetch_addr:#x}: {e}");
            ShaderUnsupported
        })
    }
}

impl ShaderProvider for GcnShaderProvider {
    /// `mem` is the caller's memory view, which in the executor path is an unbounded
    /// `IdentityMem` (identity mapping: `get_host_ptr` returns `Some` for EVERY address).
    /// The `GcnBinary` address is **register-derived and untrusted**, so this provider does
    /// NOT read it through `mem` — a near-unmapped address would let [`parse_sb`]'s magic scan
    /// walk up to 1 MiB of raw host memory. It reads through the process-global **bounded**
    /// seam ([`bounded_read`]), whose reads are range-validated against the live VMA set.
    /// When no seam is wired (headless / unit tests with no VM) the ref cannot be bound
    /// safely, so it defers — never a fall-through to the unbounded `mem`.
    ///
    /// DIRTY SEAM: `dirty` is the guest-memory dirty-tracking source threaded through the
    /// trait (the same seam [`crate::cache::ResourceCache::get`] takes). It is passed straight
    /// to [`resolve_gcn`](Self::resolve_gcn), which `watch`es the recompiled `.sb` code range
    /// on a cache miss so a later guest write to that range invalidates the entry on the next
    /// per-submit [`drain_dirty`](Self::drain_dirty). When `None` (headless / no VM) the range
    /// is not watched — correct for immutable shader code that never dirties.
    fn resolve(
        &self,
        r: &ShaderRef,
        _mem: &dyn VirtualMemoryManager,
        dirty: Option<&dyn DirtySource>,
    ) -> Result<Option<HostShader>, ShaderUnsupported> {
        let addr = match *r {
            ShaderRef::GcnBinary { addr, .. } => addr,
            // Not a GCN binary → not this provider's kind; chain onward.
            ShaderRef::Embedded { .. } => return Ok(None),
        };
        match bounded_read() {
            Some(reader) => self.resolve_gcn(addr, reader.as_ref(), dirty),
            None => {
                tracing::warn!(
                    "[GNM] bounded read seam not wired; GCN shader parse cannot proceed \
                     (addr={addr:#x}) — a real MemoryFault means an unmapped address, this \
                     means register_bounded_read was never called"
                );
                Err(ShaderUnsupported)
            }
        }
    }
}

/// A fresh [`HostShader`] view of a cached one: same `Arc` SPIR-V + cloned layout, a refcount
/// bump not a SPIR-V copy. The trait returns an owned `HostShader`, so a cache hit reconstructs
/// this cheap wrapper around the shared words.
fn host_from_arc(host: &Arc<HostShader>) -> HostShader {
    HostShader {
        stage: host.stage,
        spirv: host.spirv.clone(),
        io: host.io.clone(),
    }
}

/// Map the `.sb` HW stage to the recompiler's [`ShaderStage`]. The vertex-family HW roles
/// present as a vertex shader; pixel maps to fragment. Stages the recompiler does not model
/// (compute/geometry/hull, and the tessellation family beyond a plain VS) yield `None` so the
/// caller defers cleanly rather than mis-recompiling.
fn recompile_stage(stage: SbStage) -> Option<ShaderStage> {
    match stage {
        SbStage::Vertex => Some(ShaderStage::Vertex),
        SbStage::Pixel => Some(ShaderStage::Fragment),
        SbStage::Export | SbStage::Local | SbStage::Domain => None,
        SbStage::Compute | SbStage::Geometry | SbStage::Hull => None,
    }
}

/// Read `[range.start, range.end)` through the bounded seam and reinterpret it as GCN
/// dwords. The range is header-validated (a whole number of instructions' worth of code),
/// so a non-multiple-of-4 length can only be a corrupt header — treated as a read fault.
fn read_code_words(
    reader: &dyn BoundedRead,
    range: &std::ops::Range<u64>,
) -> Result<Vec<u32>, SbParseError> {
    let len = (range.end - range.start) as usize;
    let bytes = reader
        .read_ranged(range.start, len)
        .map_err(|_| SbParseError::MemoryFault)?;
    if bytes.len() % 4 != 0 {
        return Err(SbParseError::Truncated);
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// A human-readable defer reason for a [`RecompileError`], naming the instruction/offset so a
/// deferred shader is diagnosable (task-90 / task-41: "unsupported == deferred, not broken").
fn defer_reason(e: &RecompileError) -> String {
    e.to_string()
}

/// Whether two `[start, end)` code ranges overlap a `[addr, addr+size)` write (half-open).
fn ranges_overlap(a_start: u64, a_end: u64, b_start: u64, b_len: u64) -> bool {
    if a_end <= a_start || b_len == 0 {
        return false;
    }
    let b_end = b_start.saturating_add(b_len);
    a_start < b_end && b_start < a_end
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps4_core::bounded_read::registered_source;
    use ps4_core::dirty::AlwaysDirty;
    use std::path::Path;
    use std::sync::RwLock;

    /// Guest base the corpus blobs load at (arbitrary, 256-aligned like a real PGM code start).
    const BASE: u64 = 0x0020_0000;

    /// A precise dirty source: it records the ranges `watch` was called with, and only reports
    /// a simulated write as dirty when that write lands inside one of those watched ranges. It
    /// is deliberately NOT [`AlwaysDirty`] — a write outside every watched range is dropped, so
    /// a resolve that fails to `watch` its code range yields NOTHING on the next drain. That is
    /// the independent lever: the test proves invalidation happened *because the production
    /// resolve path watched the right range*, not because the source reports everything.
    #[derive(Default)]
    struct MockDirty {
        watched: RwLock<Vec<(u64, u64)>>,
        written: RwLock<Vec<(u64, u64)>>,
    }

    impl MockDirty {
        /// How many ranges have been handed to `watch` — an independent witness that the
        /// production `resolve` path reached the watch call at all (asserted separately from
        /// the recompile count).
        fn watch_count(&self) -> usize {
            self.watched.read().unwrap().len()
        }

        /// Simulate a guest write of `size` bytes at `addr`, recorded as dirty ONLY if it falls
        /// wholly inside some watched range. A write to an unwatched address is a no-op — so a
        /// missing `watch` (the bug this task fixes) makes the next `take_dirty` empty.
        fn simulate_write(&self, addr: u64, size: u64) {
            let watched = self.watched.read().unwrap();
            let inside = watched
                .iter()
                .any(|&(a, n)| addr >= a && addr.saturating_add(size) <= a.saturating_add(n));
            if inside {
                self.written.write().unwrap().push((addr, size));
            }
        }
    }

    impl DirtySource for MockDirty {
        fn watch(&self, addr: u64, size: u64) {
            self.watched.write().unwrap().push((addr, size));
        }
        fn unwatch(&self, addr: u64, size: u64) {
            self.watched.write().unwrap().retain(|&r| r != (addr, size));
        }
        fn take_dirty(&self) -> Vec<(u64, u64)> {
            std::mem::take(&mut *self.written.write().unwrap())
        }
    }

    /// A committed corpus `.sb` blob (built + header-checked by the `ps4-gcn` crate). Reused
    /// here so the provider is exercised against the SAME blobs the task-38..41 tests use.
    fn corpus_sb(name: &str) -> Vec<u8> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gcn/tests/corpus")
            .join(format!("{name}.sb"));
        std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    }

    /// A flat backing-buffer bounded reader: guest addr == `base + index`, bounds-checked so an
    /// over-read is a clean fault. The minimal [`BoundedRead`] seam the provider takes.
    struct BufMem {
        base: u64,
        buf: Vec<u8>,
    }

    impl BoundedRead for BufMem {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            let start = addr.checked_sub(self.base).ok_or("segfault")? as usize;
            let end = start.checked_add(size).ok_or("segfault")?;
            if end > self.buf.len() {
                return Err("segfault");
            }
            Ok(self.buf[start..end].to_vec())
        }
    }

    /// Wire a corpus blob into the process-global bounded seam and return an RAII guard that
    /// restores the prior source on drop (panic-safe; serializes against other bounded-read
    /// tests). The `ShaderProvider::resolve` path reads through this seam, not `mem`.
    fn wire_corpus(name: &str) -> ps4_core::registered::ScopeGuard<'static, dyn BoundedRead> {
        let blob = corpus_sb(name);
        let reader: Arc<dyn BoundedRead> = Arc::new(BufMem {
            base: BASE,
            buf: blob,
        });
        registered_source().override_scoped(reader)
    }

    /// A do-nothing VMM: the provider never reads guest memory through `mem` (it uses the
    /// bounded seam), so `resolve`'s `mem` argument only needs the trait-object shape.
    struct NullMem;
    impl VirtualMemoryManager for NullMem {
        fn map(
            &mut self,
            _a: u64,
            _s: usize,
            _p: ps4_core::memory::MemoryProtection,
            _n: Option<&str>,
        ) -> Result<u64, &'static str> {
            Err("stub")
        }
        fn unmap(&mut self, _a: u64, _s: usize) -> Result<(), &'static str> {
            Err("stub")
        }
        fn protect(
            &mut self,
            _a: u64,
            _s: usize,
            _p: ps4_core::memory::MemoryProtection,
        ) -> Result<(), &'static str> {
            Err("stub")
        }
        unsafe fn get_host_ptr(&self, _a: u64) -> Option<*mut u8> {
            None
        }
        fn find_free_region(&mut self, _s: usize) -> u64 {
            0
        }
        fn is_memory_free(&self, _a: u64, _s: usize) -> bool {
            false
        }
    }

    fn gcn_ref() -> ShaderRef {
        ShaderRef::GcnBinary {
            addr: BASE,
            res: crate::shader::source::GcnResources::default(),
        }
    }

    #[test]
    fn resolves_corpus_vs_to_valid_spirv() {
        // AC #1: a GcnBinary ref over the passthrough_vs corpus blob resolves to valid
        // SPIR-V, carrying the recompiler's I/O layout.
        let _guard = wire_corpus("passthrough_vs");
        let provider = GcnShaderProvider::new();
        let host = provider
            .resolve(&gcn_ref(), &NullMem, None)
            .expect("no defer")
            .expect("VS resolved");
        assert_eq!(host.stage, Stage::Vertex);
        // A valid SPIR-V module starts with the magic 0x0723_0203.
        assert_eq!(host.spirv[0], 0x0723_0203);
        assert!(host.spirv.len() > 4);
        let io = host.io.expect("GCN shader carries IoLayout");
        assert_eq!(io.stage, ShaderStage::Vertex);
        assert_eq!(provider.recompile_count(), 1);
    }

    #[test]
    fn resolves_corpus_ps_to_valid_spirv() {
        // AC #1: a fragment corpus blob resolves through the provider too.
        let _guard = wire_corpus("interp_color_ps");
        let provider = GcnShaderProvider::new();
        let host = provider
            .resolve(&gcn_ref(), &NullMem, None)
            .expect("no defer")
            .expect("PS resolved");
        assert_eq!(host.stage, Stage::Pixel);
        assert_eq!(host.spirv[0], 0x0723_0203);
        let io = host.io.expect("GCN shader carries IoLayout");
        assert_eq!(io.stage, ShaderStage::Fragment);
    }

    #[test]
    fn second_resolve_of_same_hash_skips_recompiler() {
        // AC #2: a 2nd resolve of the same shader hash is a cache hit — recompile_count
        // stays at 1 — and hands back the SAME Arc SPIR-V words.
        let _guard = wire_corpus("passthrough_vs");
        let provider = GcnShaderProvider::new();

        let first = provider
            .resolve(&gcn_ref(), &NullMem, None)
            .unwrap()
            .unwrap();
        assert_eq!(provider.recompile_count(), 1);

        let second = provider
            .resolve(&gcn_ref(), &NullMem, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            provider.recompile_count(),
            1,
            "2nd resolve must skip the recompiler"
        );
        // Same underlying Arc: a refcount bump, not a re-recompile.
        assert!(Arc::ptr_eq(&first.spirv, &second.spirv));
    }

    #[test]
    fn dirty_write_invalidates_and_forces_recompile() {
        // The DirtySource drain drops an entry whose code range was written, so the next
        // resolve re-recompiles — the resource-cache invalidation shape, for shaders.
        let _guard = wire_corpus("passthrough_vs");
        let provider = GcnShaderProvider::new();
        let dirty = AlwaysDirty::new();

        // Resolve WITH a dirty source so the code range is watched.
        let reader = bounded_read().unwrap();
        provider
            .resolve_gcn(BASE, reader.as_ref(), Some(&dirty))
            .unwrap()
            .unwrap();
        assert_eq!(provider.recompile_count(), 1);

        // A submit boundary drains the dirty source: AlwaysDirty reports the watched range as
        // written, so the entry is dropped and the next resolve recompiles again.
        provider.drain_dirty(&dirty);
        provider
            .resolve_gcn(BASE, reader.as_ref(), Some(&dirty))
            .unwrap()
            .unwrap();
        assert_eq!(
            provider.recompile_count(),
            2,
            "a dirtied code range must force a re-recompile"
        );
    }

    #[test]
    fn production_resolve_watches_code_range_so_mutation_reresolves() {
        // AC #1, through the PRODUCTION path: `ShaderProvider::resolve` (not `resolve_gcn`
        // directly) must thread the dirty source so the `.sb` code range is watched. A guest
        // write inside that watched range, drained at the submit boundary, drops the entry and
        // forces a re-recompile on the next resolve — the resource cache's watch-on-insert +
        // drain-invalidate shape, for shaders.
        //
        // This guards the exact bug the task fixes: `resolve` used to pass `dirty: None`, so no
        // code range was ever watched and `drain_dirty` could never invalidate a stale
        // recompile. The lever is independent of the production count: MockDirty reports a write
        // as dirty ONLY if it lands inside a range that was actually `watch`ed. If `resolve`
        // failed to watch, `simulate_write` is a no-op, `take_dirty` is empty, the entry
        // survives, and the final `recompile_count()` stays at 1 — the assertion below fails.
        let _guard = wire_corpus("passthrough_vs");
        let provider = GcnShaderProvider::new();
        let dirty = MockDirty::default();

        // First resolve through the trait method with a threaded dirty source: one cache miss →
        // one recompile, and the code range is registered with the source.
        provider
            .resolve(&gcn_ref(), &NullMem, Some(&dirty))
            .expect("no defer")
            .expect("VS resolved");
        assert_eq!(provider.recompile_count(), 1);
        assert_eq!(
            dirty.watch_count(),
            1,
            "the production resolve must watch the .sb code range exactly once"
        );

        // Independently reason the watched range: the corpus loads at BASE, and the watched
        // range recorded by the source is `[start, start+len)`. Write the FIRST code dword of
        // that range — a mutation of the shader's own bytes.
        let (code_start, code_len) = dirty.watched.read().unwrap()[0];
        assert_eq!(code_start, BASE, "code range starts at the shader address");
        assert!(code_len >= 4, "a real .sb has at least one code dword");
        dirty.simulate_write(code_start, 4);

        // Negative control BEFORE the drain: a write OUTSIDE every watched range must not be
        // reported dirty (proves the source is precise, not AlwaysDirty). Placed past the code
        // range so it cannot overlap.
        dirty.simulate_write(code_start + code_len + 0x1000, 4);

        // Submit boundary: drain the source and drop the overlapping entry, then re-resolve.
        // The watched-range write invalidated the entry → the second resolve is a miss → a 2nd
        // recompile. Had `resolve` not watched, the write would have been dropped and the count
        // would still be 1.
        provider.drain_dirty(&dirty);
        provider
            .resolve(&gcn_ref(), &NullMem, Some(&dirty))
            .expect("no defer")
            .expect("VS re-resolved");
        assert_eq!(
            provider.recompile_count(),
            2,
            "a write to the watched .sb code range must force a re-recompile via the trait path"
        );

        // And the negative half: a fresh resolve with NO further writes is a cache hit — the
        // out-of-range write above never made it dirty, so nothing else was invalidated.
        provider.drain_dirty(&dirty);
        provider
            .resolve(&gcn_ref(), &NullMem, Some(&dirty))
            .expect("no defer")
            .expect("VS resolved");
        assert_eq!(
            provider.recompile_count(),
            2,
            "an unwritten shader stays cached; an out-of-range write never invalidated it"
        );
    }

    #[test]
    fn unsupported_instruction_defers_no_crash() {
        // AC #3: a well-formed .sb whose GCN code the recompiler cannot lower defers cleanly
        // (Err(ShaderUnsupported)) with a logged reason — never a panic.
        //
        // Build a minimal VS .sb whose single "instruction" is an all-ones dword: the decoder
        // yields Inst::Unknown, which the recompiler rejects as UnsupportedInst — a structured
        // defer, exactly the RecompileError path AC #3 requires.
        let base = 0x0040_0000u64;
        let code: Vec<u8> = 0xFFFF_FFFFu32.to_le_bytes().to_vec();
        let blob = build_sb_with_code(1 /* VS */, &code);

        // The blob is a VALID .sb (parses cleanly) — so a defer here is the recompiler's
        // structured reject, distinct from `parse_reject_defers_not_panics`.
        let reader = BufMem {
            base,
            buf: blob.clone(),
        };
        parse_sb(base, &reader).expect("broken-instruction blob still parses as a .sb");

        let reader: Arc<dyn BoundedRead> = Arc::new(BufMem { base, buf: blob });
        let _guard = registered_source().override_scoped(reader);

        let provider = GcnShaderProvider::new();
        let r = ShaderRef::GcnBinary {
            addr: base,
            res: crate::shader::source::GcnResources::default(),
        };
        assert!(
            matches!(provider.resolve(&r, &NullMem, None), Err(ShaderUnsupported)),
            "an unsupported instruction must defer, not resolve or panic"
        );
        assert_eq!(
            provider.recompile_count(),
            0,
            "a defer must not count as a recompile"
        );
    }

    #[test]
    fn parse_reject_defers_not_panics() {
        // AC #3: garbage with no OrbShdr magic is a clean defer, not a crash.
        let base = 0x0050_0000u64;
        let reader: Arc<dyn BoundedRead> = Arc::new(BufMem {
            base,
            buf: vec![0xABu8; 0x100],
        });
        let _guard = registered_source().override_scoped(reader);
        let provider = GcnShaderProvider::new();
        let r = ShaderRef::GcnBinary {
            addr: base,
            res: crate::shader::source::GcnResources::default(),
        };
        assert!(matches!(
            provider.resolve(&r, &NullMem, None),
            Err(ShaderUnsupported)
        ));
    }

    #[test]
    fn resolve_fetch_vertex_input_through_provider() {
        // AC #2: a VS resolve through GcnShaderProvider consumes the parsed fetch table
        // + the .sb VertexInputSemantic table and produces vertex-input state WITHOUT
        // executing the fetch blob. The fetch-shader machine code is the committed
        // hand-assembled `fetch_vs` corpus (real GFX7 bytes), wired into the bounded
        // seam; the expected attributes are hand-reasoned from the fetch ABI + the
        // semantic table, NOT captured from the parser.
        use crate::shader::sb::{Semantics, VertexInputSemantic};

        let fetch_base = 0x0060_0000u64;
        let code = {
            let p =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../gcn/tests/corpus/fetch_vs.code.bin");
            std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
        };
        let len = code.len();
        let reader: Arc<dyn BoundedRead> = Arc::new(BufMem {
            base: fetch_base,
            buf: code,
        });

        // .sb semantics: v4 → semantic 0, v8 → semantic 1 (hand-reasoned).
        let semantics = Semantics {
            vertex_inputs: vec![
                VertexInputSemantic {
                    semantic: 0,
                    vgpr: 4,
                    size_in_elements: 4,
                },
                VertexInputSemantic {
                    semantic: 1,
                    vgpr: 8,
                    size_in_elements: 2,
                },
            ],
            ..Semantics::default()
        };

        let provider = GcnShaderProvider::new();

        // The `override_scoped` guard holds the process-global bounded-seam serialization
        // lock (non-reentrant), so each wiring is scoped and dropped before the next — two
        // live guards on the same registered source would deadlock.
        {
            let _guard = registered_source().override_scoped(reader);
            let state = provider
                .resolve_fetch_vertex_input(fetch_base, len, &semantics)
                .expect("fetch shader resolves to vertex-input state");

            assert_eq!(state.attributes.len(), 2);
            assert_eq!(state.attributes[0].semantic, 0);
            assert_eq!(state.attributes[0].vsharp_sgpr, 8);
            assert_eq!(state.attributes[0].dest_vgpr, 4);
            assert_eq!(state.attributes[0].components, 4);
            assert_eq!(state.attributes[1].semantic, 1);
            assert_eq!(state.attributes[1].vsharp_sgpr, 12);
            assert_eq!(state.attributes[1].dest_vgpr, 8);
            assert_eq!(state.attributes[1].components, 2);
        }

        // AC #3 through the provider: garbage that is not a fetch shape defers cleanly.
        let junk_base = 0x0070_0000u64;
        let junk: Arc<dyn BoundedRead> = Arc::new(BufMem {
            base: junk_base,
            buf: vec![0xFFu8; 16],
        });
        {
            let _junk_guard = registered_source().override_scoped(junk);
            assert!(matches!(
                provider.resolve_fetch_vertex_input(junk_base, 16, &semantics),
                Err(ShaderUnsupported)
            ));
        }
    }

    #[test]
    fn embedded_ref_chains_onward() {
        // The GCN provider does not handle embedded ids: Ok(None) so the chain composite
        // (which puts embedded FIRST) is unaffected by appending this provider.
        let provider = GcnShaderProvider::new();
        let r = ShaderRef::Embedded {
            stage: Stage::Vertex,
            id: 0,
        };
        assert!(matches!(provider.resolve(&r, &NullMem, None), Ok(None)));
    }

    #[test]
    fn chain_reaches_gcn_provider_and_keeps_embedded_precedence() {
        // The production wiring is a chain [embedded, gcn] — this exercises it end to end,
        // the coverage the per-provider tests above miss. It guards the phase-4 regression
        // where EmbeddedShaderProvider returned Err on a GcnBinary: ChainProvider propagates
        // Err, so the GCN provider would never run and every real .sb draw would defer.
        use crate::shader::embedded::{EMBEDDED_VS_FULLSCREEN_QUAD, EmbeddedShaderProvider};
        use crate::shader::source::ChainProvider;

        let _guard = wire_corpus("passthrough_vs");
        let embedded = EmbeddedShaderProvider::new();
        let gcn = GcnShaderProvider::new();
        let providers: [&dyn ShaderProvider; 2] = [&embedded, &gcn];
        let chain = ChainProvider::new(&providers);

        // A GcnBinary flows past embedded (Ok(None)) into the GCN provider and resolves.
        let host = chain
            .resolve(&gcn_ref(), &NullMem, None)
            .expect("chain must not short-circuit a recompilable GCN shader")
            .expect("GCN shader resolved through the chain");
        assert_eq!(host.spirv[0], 0x0723_0203);
        assert_eq!(gcn.recompile_count(), 1);

        // Embedded precedence intact: an embedded ref is served by embedded and the GCN
        // provider is never consulted (recompile_count stays 1).
        let r = ShaderRef::Embedded {
            stage: Stage::Vertex,
            id: EMBEDDED_VS_FULLSCREEN_QUAD,
        };
        let emb = chain
            .resolve(&r, &NullMem, None)
            .unwrap()
            .expect("embedded VS");
        assert_eq!(emb.stage, Stage::Vertex);
        assert_eq!(
            gcn.recompile_count(),
            1,
            "an embedded ref must not reach the GCN provider"
        );
    }

    /// Build a `.sb` container: `code` bytes of raw GCN followed by a 28-byte OrbShdr header
    /// whose `m_length` equals `code.len()`. A local mini-builder so the AC #3 unsupported-
    /// instruction test does not need a corpus blob (the corpus has no deliberately-broken
    /// shader).
    fn build_sb_with_code(m_type: u32, code: &[u8]) -> Vec<u8> {
        let code_len = code.len() as u32;
        let word: u32 = ((m_type & 0xF) << 2) | ((code_len & 0x00FF_FFFF) << 8);
        let mut blob = code.to_vec();
        blob.extend_from_slice(b"OrbShdr");
        blob.push(1); // m_version
        blob.extend_from_slice(&word.to_le_bytes());
        blob.push(0); // m_chunkUsageBaseOffsetInDW
        blob.push(0); // m_numInputUsageSlots
        blob.push(0); // flags
        blob.push(0); // m_reserved3
        blob.extend_from_slice(&0u32.to_le_bytes()); // hash0
        blob.extend_from_slice(&0u32.to_le_bytes()); // hash1
        blob.extend_from_slice(&0u32.to_le_bytes()); // crc32
        blob
    }
}
