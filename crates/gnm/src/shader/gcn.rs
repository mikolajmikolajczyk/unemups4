//! `GcnShaderProvider` (doc-2 §4, phase 4): a `.sb` (OrbShdr) GCN binary →
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
//!      ([`recompile_with`]), building a [`HostShader`] that carries the SPIR-V + the recompiler's
//!      [`IoLayout`] (I/O + resource layout, HW-stage role);
//!   4. a [`RecompileError`] is a clean **defer**, not a crash: it returns
//!      [`ShaderUnsupported`] (the chain's recognized-but-unsupported signal, which the
//!      executor maps to a skipped draw) and logs the offending instruction/reason.
//!
//! ## Cache + invalidation (doc-2 §8.3, mirrors the resource cache)
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
use ps4_gcn::{
    Decoded, FetchResolveError as GcnFetchResolveError, PsInputMap, RecompileError, ShaderStage,
    decode_all, has_fetch_call, recompile_with, resolve_fetch_call,
};

use crate::shader::fetch::{FetchResolveError, VertexInputState, resolve_fetch_vertex_input};
use crate::shader::sb::{SbParseError, SbShader, SbStage, Semantics, parse_sb};
use crate::shader::source::{HostShader, ShaderProvider, ShaderRef, ShaderUnsupported, Stage};

/// The `.sb` header material that identities a shader across re-binds (doc-2 §8.3,
/// task-42 finding #5): the two 32-bit content hashes, the crc32, and the code length.
/// A guest that re-programs `SPI_SHADER_PGM_*` to the same shader mints the same key, so
/// the recompile is done once. Value-typed so it is a `HashMap` key directly.
///
/// The [`PsInputMap`] is part of the key, not just an input: the same PS binary drawn
/// under a different `SPI_PS_INPUT_CNTL` routing recompiles to a DIFFERENT SPIR-V module
/// (its interpolant `Location`s move). Keying on the header material alone would hand the
/// first-seen variant to every later draw — silently mis-routing its attributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ShaderCacheKey {
    hash0: u32,
    hash1: u32,
    crc32: u32,
    code_len: u32,
    ps_input_map: PsInputMap,
}

impl ShaderCacheKey {
    fn from_sb(sb: &SbShader, ps_input_map: PsInputMap) -> Self {
        ShaderCacheKey {
            hash0: sb.info.shader_hash0,
            hash1: sb.info.shader_hash1,
            crc32: sb.info.crc32,
            code_len: sb.info.code_len,
            ps_input_map,
        }
    }
}

/// A cached recompiled shader plus the guest code range it was recompiled from — the range
/// is what [`GcnShaderProvider::drain_dirty`] watches and matches a dirtied write against.
struct CacheEntry {
    host: Arc<HostShader>,
    code_range: std::ops::Range<u64>,
}

/// GCN `.sb` → recompiled SPIR-V provider, chained after the embedded provider (doc-2 §4).
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
    /// overlaps a dirtied write (doc-2 §8.3) — the same drain-then-invalidate shape the
    /// resource cache uses ([`crate::cache::ResourceCache::drain_dirty`]). A dropped entry is
    /// re-parsed + re-recompiled on its next resolve, so self-modifying / reloaded shader
    /// code is picked up. Call at each submit boundary before the draws that resolve shaders.
    pub fn drain_dirty(&self, dirty: &dyn DirtySource) {
        let dirtied = dirty.take_dirty();
        self.apply_dirty(&dirtied);
    }

    /// Drop every cache entry whose code range overlaps a dirtied write. Split out from
    /// [`drain_dirty`] so ONE [`DirtySource::take_dirty`] drain feeds both this provider and
    /// the resource cache (task-178): `take_dirty` drains, so calling it in both consumers
    /// left the second empty. The caller drains once and calls `apply_dirty` on each.
    pub fn apply_dirty(&self, dirtied: &[(u64, u64)]) {
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
    /// re-recompilation (doc-2 §8.1). A direct hook for callers that already know a guest
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
    ///
    /// `fetch_addr` is the guest address the driver preloaded into VS user-SGPR `s[0:1]`
    /// (doc-6 Entry 9), threaded down from the ref because the provider cannot read
    /// registers. A retail VS opens with a `s_swappc_b64 s[0:1], s[0:1]` call into a
    /// separate **fetch shader** at that address; when the decoded stream contains that
    /// call ([`has_fetch_call`]) this reads the fetch body through the SAME bounded seam
    /// and splices it inline ([`resolve_fetch_call`]) before recompiling, so the recompiler
    /// sees a straight-line stream (the inlined `buffer_load_format … idxen` fetch lowers
    /// to the existing SSBO vertex-pull the executor already binds). Strict-or-defer: any
    /// failure — no fetch address, an unreadable window, or a [`GcnFetchResolveError`] — is
    /// a clean `ShaderUnsupported` with a `tracing::warn!`, never a partial recompile.
    ///
    /// `ps_input_map` is the draw's PS attribute→VS-parameter routing, likewise threaded
    /// down from the ref. It changes the emitted SPIR-V, so it is part of the cache key.
    fn resolve_gcn(
        &self,
        addr: u64,
        fetch_addr: Option<u64>,
        ps_input_map: PsInputMap,
        reader: &dyn BoundedRead,
        dirty: Option<&dyn DirtySource>,
    ) -> Result<Option<HostShader>, ShaderUnsupported> {
        dump_gcn_window(addr, reader);
        let sb = match parse_sb(addr, reader) {
            Ok(sb) => sb,
            Err(e) => {
                // A malformed / non-plaintext container is not this provider's shader to
                // bind. It is recognized-but-unbindable → a clean defer, never a crash.
                tracing::warn!("[GNM] GCN shader parse rejected at {addr:#x}: {e}");
                return Err(ShaderUnsupported::plain());
            }
        };

        let key = ShaderCacheKey::from_sb(&sb, ps_input_map);
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
                return Err(ShaderUnsupported::plain());
            }
        };

        // Read the raw GCN machine code through the SAME bounded seam and decode it. The
        // range is header-validated by parse_sb (`end - start == code_len`), so this read is
        // exactly the code region — no scan.
        let code = match read_code_words(reader, &sb.code_range) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("[GNM] GCN code read faulted at {addr:#x}: {e}");
                return Err(ShaderUnsupported::plain());
            }
        };
        let insts = decode_all(&code);

        // A retail VS calls a separate fetch shader via `s_swappc_b64 s[0:1], s[0:1]`
        // (doc-6 Entry 9). Resolve that call by inlining the fetch body BEFORE recompiling,
        // so the recompiler sees a straight-line stream. A self-fetching VS / any PS has no
        // such call and recompiles as-is. Strict-or-defer: a missing/unreadable fetch or a
        // non-conforming body is a clean `ShaderUnsupported`, never a partial recompile.
        let resolved: Vec<Decoded> = if has_fetch_call(&insts) {
            let Some(faddr) = fetch_addr.filter(|&a| a != 0) else {
                tracing::warn!(
                    "[GNM] GCN VS at {addr:#x} calls a fetch shader (s_swappc_b64) but no \
                     fetch-shader pointer is bound in s[0:1]; deferring draw"
                );
                return Err(ShaderUnsupported::plain());
            };
            let fetch_code = match read_fetch_code(reader, faddr) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "[GNM] GCN VS at {addr:#x} fetch-shader read faulted at {faddr:#x}: {e}"
                    );
                    return Err(ShaderUnsupported::plain());
                }
            };
            let fetch_insts = decode_all(&fetch_code);
            match resolve_fetch_call(&insts, &fetch_insts) {
                Ok(r) => r,
                Err(e) => {
                    // A `GcnFetchResolveError` (multi-call, non-SGPR pointer, unrecognized
                    // fetch body, no return) is a clean defer — the inline is never faked.
                    let _: GcnFetchResolveError = e;
                    tracing::warn!(
                        "[GNM] GCN VS at {addr:#x} fetch-shader resolve deferred \
                         (fetch={faddr:#x}): {e}"
                    );
                    return Err(ShaderUnsupported::plain());
                }
            }
        } else {
            insts
        };

        let recompiled = match recompile_with(&resolved, stage, &ps_input_map) {
            Ok(r) => r,
            Err(e) => {
                // A RecompileError is a DEFER, not a break (task-90 / task-41): log naming the
                // instruction/reason and return the chain's recognized-but-unsupported signal.
                tracing::warn!(
                    "[GNM] GCN recompile deferred at {addr:#x}: {}",
                    defer_reason(&e)
                );
                // Carry the structured error up so the snapshot can name the exact unsupported
                // instruction (task-195). `defer_reason` above only BORROWS it for the log; the
                // move here allocates nothing (the decoded `Inst` is already boxed), and the
                // `to_string` that formats it happens later, gated on the snapshot being armed.
                return Err(ShaderUnsupported::recompile(e));
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

    /// Recover a VS's vertex-input state from its fetch shader (doc-2 §C4). The main
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
            ShaderUnsupported::plain()
        })?;
        resolve_fetch_vertex_input(fetch_addr, len, reader.as_ref(), semantics).map_err(|e| {
            // A non-conforming / unreadable fetch shader is a clean defer (AC #3): the
            // recovered layout is unavailable, so the draw's vertex input stays unbound
            // rather than the parser fabricating a bogus table.
            let _: FetchResolveError = e;
            tracing::warn!("[GNM] fetch-shader resolve deferred at {fetch_addr:#x}: {e}");
            ShaderUnsupported::plain()
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
        let (addr, fetch_addr, ps_input_map) = match *r {
            ShaderRef::GcnBinary {
                addr,
                res,
                ps_input_map,
            } => (addr, res.fetch_addr, ps_input_map),
            // Not a GCN binary → not this provider's kind; chain onward.
            ShaderRef::Embedded { .. } => return Ok(None),
        };
        match bounded_read() {
            Some(reader) => {
                self.resolve_gcn(addr, fetch_addr, ps_input_map, reader.as_ref(), dirty)
            }
            None => {
                tracing::warn!(
                    "[GNM] bounded read seam not wired; GCN shader parse cannot proceed \
                     (addr={addr:#x}) — a real MemoryFault means an unmapped address, this \
                     means register_bounded_read was never called"
                );
                Err(ShaderUnsupported::plain())
            }
        }
    }
}

/// The `UNEMUPS4_DUMP_GCN` target dir, read from the environment exactly once (mirrors the
/// decoder's `TRACE_ENV` `OnceLock`): [`resolve_gcn`](GcnShaderProvider::resolve_gcn) runs per
/// draw, so a per-call `env::var` (a getenv + `String` alloc) would land on the draw path.
/// `None` when unset — the common case, cheap after the first call.
fn dump_gcn_dir() -> Option<&'static str> {
    static DIR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var("UNEMUPS4_DUMP_GCN").ok())
        .as_deref()
}

/// Diagnostic (env `UNEMUPS4_DUMP_GCN=<dir>`): dump a memory window at a register-derived
/// GCN code address to `<dir>/gcn_<addr>.bin` so a rejected/deferred real shader can be
/// inspected offline (disasm the code, read the OrbShdr header) without re-running the guest.
/// Called before [`parse_sb`] on purpose — the point is to capture shaders that FAIL to parse.
/// Best-effort: no-ops when unset, skips the 64 KiB read + write once a given address is already
/// dumped (this fn runs per draw), shrinks the read on a mapping boundary, and swallows I/O errors.
fn dump_gcn_window(addr: u64, reader: &dyn BoundedRead) {
    let Some(dir) = dump_gcn_dir() else {
        return;
    };
    let path = format!("{dir}/gcn_{addr:#x}.bin");
    // Already captured this address — don't re-read 64 KiB and rewrite the file every draw.
    if std::path::Path::new(&path).exists() {
        return;
    }
    // Grab as much as the mapping allows, up to 64 KiB (well past any single shader's code +
    // header + semantic tables), shrinking on a bounded-read fault.
    let mut len = 64 * 1024;
    let bytes = loop {
        if let Ok(b) = reader.read_ranged(addr, len) {
            break b;
        }
        if len <= 256 {
            return;
        }
        len /= 2;
    };
    let _ = std::fs::write(&path, &bytes);
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

/// The minimum fetch-shader window: a smallest real fetch shader is an
/// `s_load_dwordx4` (2 dwords) + one `buffer_load_format` (2 dwords) + `s_setpc_b64` (1
/// dword) = 5 dwords. 8 dwords is a safe floor the seam can satisfy even for a fetch
/// shader that sits at the very end of a small mapping.
const FETCH_WINDOW_MIN_DWORDS: usize = 8;
/// The maximum fetch-shader window to scan for the terminating return. A retail fetch
/// shader is a handful of attribute loads + fetches + one return, well under this; the cap
/// bounds the scan so a pointer into a huge mapping with no return never reads unboundedly.
const FETCH_WINDOW_MAX_DWORDS: usize = 256;

/// Read a **fetch-shader** code window at `fetch_addr` through the bounded seam (doc-6
/// Entry 9). Unlike the main `.sb` code (whose length the header validates), a fetch
/// shader is a small leaf subroutine (`s_load_dwordx4*`, `buffer_load_format … idxen`,
/// `s_setpc_b64`) with **no length declared** at the pointer, so this grows the window from
/// a small floor until it contains the terminating `s_setpc_b64`/`s_swappc_b64` return that
/// [`ps4_gcn::parse_fetch_shader`] breaks at — bytes past the return are ignored, so an
/// over-long window is harmless but an under-long one would truncate the body.
///
/// The `fetch_addr` is register-derived and untrusted, so every read is the bounded seam
/// (never the identity view), which validates the whole window against one contiguous
/// mapping. Growing (rather than shrinking a fixed large window) means a fetch shader that
/// sits near a mapping boundary — where a large read would fault — is still read exactly:
/// the smallest window that captures its return succeeds. A readable window that holds no
/// aligned return — whether it reached [`FETCH_WINDOW_MAX_DWORDS`] or a *larger* read faulted
/// at a mapping boundary — is returned as `Ok` of the largest window that did read, so
/// [`resolve_fetch_call`] defers on the non-conforming body; only the **floor** read itself
/// faulting (a genuinely unmapped pointer) is a clean `Err`. Returns the window as GCN dwords.
///
/// ALIGNMENT (task-125): a fetch shader has no declared length, so its terminating return
/// is *found*, not *given*. The find MUST be a return an instruction-aligned decode walk
/// **from offset 0** actually lands on — the exact walk [`resolve_fetch_call`] /
/// [`ps4_gcn::parse_fetch_shader`] use downstream — not merely some dword anywhere in the
/// window that, read in isolation, looks like `s_setpc`/`s_swappc`. A `0x20`/`0x21` byte
/// pattern that happens to sit *inside* an SMRD literal or a `buffer_load` immediate is
/// interior to a multi-dword instruction: the from-0 walk consumes it and never treats it
/// as a return. Accepting such an interior dword would stop the window growth early and
/// hand the resolver a **truncated** body — the recompiled VS would fetch fewer/wrong
/// vertex attributes with no defer and no error (scrambled geometry). So growth stops on
/// the aligned terminator only, and the window is returned **sliced to end exactly after
/// it**, so the downstream from-0 decode lands on the same return this walk did.
fn read_fetch_code(reader: &dyn BoundedRead, fetch_addr: u64) -> Result<Vec<u32>, &'static str> {
    // TERMINATION (finding-7): the window only ever GROWS — `dwords` starts at the floor and
    // strictly increases every iteration (doubling, clamped to the cap); it is never reset,
    // so no `(dwords)` state is revisited and the loop makes progress on each pass. Every
    // iteration either returns or increases `dwords` toward `FETCH_WINDOW_MAX_DWORDS`, so with
    // a deterministic reader the loop halts in ≤ log2(MAX/MIN)+1 passes at one of three
    // terminals: an aligned return is found, the cap is reached, or a grown read faults. A
    // faulting grown read does NOT drop back to the floor and re-grow (the old reset oscillated
    // forever when a floor window read but a grown one faulted at a boundary) — it hands back
    // `best`, the largest window that DID read, and stops.
    let mut dwords = FETCH_WINDOW_MIN_DWORDS;
    // The largest successfully-read window that held no aligned return yet. `None` until the
    // floor read succeeds, so `best.is_none()` is exactly "we are still on the floor read".
    let mut best: Option<Vec<u32>> = None;
    loop {
        match reader.read_ranged(fetch_addr, dwords * 4) {
            Ok(bytes) => {
                let words: Vec<u32> = bytes
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                // Stop as soon as an instruction-aligned decode FROM OFFSET 0 reaches the
                // terminating return, and slice the window to end right after it — bytes
                // past the return are ignored by the splice, so a bigger window is
                // pointless, and slicing pins the resolver's own from-0 walk to the same
                // return (no interior 0x20/0x21 byte can masquerade as a terminator). A
                // window that reaches the cap with no aligned return is returned whole;
                // resolve_fetch_call rejects a non-conforming body cleanly.
                if let Some(end) = fetch_aligned_return_end(&words) {
                    return Ok(words[..end].to_vec());
                }
                if dwords >= FETCH_WINDOW_MAX_DWORDS {
                    return Ok(words);
                }
                // No return yet, room to grow: remember this readable body so a fault on the
                // next (larger) read can hand it back instead of looping, then grow.
                best = Some(words);
                dwords = (dwords * 2).min(FETCH_WINDOW_MAX_DWORDS);
            }
            Err(e) => {
                // A grown window faults past the mapping. If a smaller window already read
                // (a boundary-truncated body whose return, if any, sits before the fault),
                // hand back that largest readable body — resolve_fetch_call rejects/defers a
                // return-less/non-conforming body cleanly, and the window never re-grows. Only
                // the FLOOR read faulting (`best` still `None`) means the pointer is genuinely
                // unmapped → a clean Err (the caller defers on the read fault).
                return match best {
                    Some(words) => Ok(words),
                    None => Err(e),
                };
            }
        }
    }
}

/// SOP1 opcode (bits [15:8] of the SOP1 dword) for `s_setpc_b64` — the subroutine-return
/// that terminates a fetch shader (why we match on it: doc-6 Entry 9). The value is the GCN
/// SOP1 op field verified by `llvm-mc --mcpu=gfx700`: `s_setpc_b64 s[0:1]` assembles to
/// `0xbe80_2000`, whose op byte [15:8] is `0x20`; AMD CI-ISA (`amd/ci-isa.pdf`, "SOP1
/// Instructions", `S_SETPC_B64`) documents the mnemonic. Matched on the decoded `Inst::Sop1`
/// op field rather than reaching into `ps4-gcn`'s private opcode table. Pinned by
/// `sop1_terminator_opcodes_match_amd_oracle`.
const SOP1_S_SETPC_B64: u8 = 0x20;
/// SOP1 opcode for `s_swappc_b64` (subroutine call/return) — also ends a fetch window. GCN
/// SOP1 op field per `llvm-mc --mcpu=gfx700`: `s_swappc_b64 s[0:1], s[0:1]` assembles to
/// `0xbe80_2100`, op byte [15:8] = `0x21` (AMD CI-ISA "SOP1 Instructions", `S_SWAPPC_B64`).
const SOP1_S_SWAPPC_B64: u8 = 0x21;

/// The dword count up to **and including** the first instruction-aligned fetch-shader
/// return (`s_setpc_b64` / `s_swappc_b64`), or `None` if a from-offset-0 decode walk over
/// `words` reaches no return (task-125).
///
/// The alignment is the whole point: it decodes with the same [`decode_all`] the resolver
/// ([`resolve_fetch_call`] / [`ps4_gcn::parse_fetch_shader`]) uses, walking from offset 0 so
/// a multi-dword instruction (an SMRD/`s_mov` with a trailing 32-bit literal, a 2-dword
/// MUBUF/VOP3) consumes its interior dwords. A `0x20`/`0x21` SOP1 byte pattern buried inside
/// one of those literals/immediates is therefore never mistaken for a terminator — only a
/// return the aligned walk actually *lands on* counts. Returning the end offset (rather than
/// a bare bool) lets [`read_fetch_code`] slice the window to that exact aligned boundary.
///
/// An `s_swappc` return also counts: a fetch that itself calls is outside the RE'd leaf
/// shape and `resolve_fetch_call` rejects it, but the window is complete either way.
fn fetch_aligned_return_end(words: &[u32]) -> Option<usize> {
    use ps4_gcn::Inst;
    decode_all(words).iter().find_map(|d| {
        matches!(&d.inst, Inst::Sop1 { op, .. }
            if *op == SOP1_S_SETPC_B64 || *op == SOP1_S_SWAPPC_B64)
        .then(|| (d.offset_dwords + d.size_dwords) as usize)
    })
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
            ps_input_map: PsInputMap::default(),
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
            .resolve_gcn(
                BASE,
                None,
                PsInputMap::default(),
                reader.as_ref(),
                Some(&dirty),
            )
            .unwrap()
            .unwrap();
        assert_eq!(provider.recompile_count(), 1);

        // A submit boundary drains the dirty source: AlwaysDirty reports the watched range as
        // written, so the entry is dropped and the next resolve recompiles again.
        provider.drain_dirty(&dirty);
        provider
            .resolve_gcn(
                BASE,
                None,
                PsInputMap::default(),
                reader.as_ref(),
                Some(&dirty),
            )
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
        // Unsupported instruction (all-ones → Inst::Unknown) followed by s_endpgm so the .sb
        // parses (the parser requires the code to end in the terminator); the recompiler hits
        // the Unknown and defers before ever reaching the terminator.
        let mut code: Vec<u8> = 0xFFFF_FFFFu32.to_le_bytes().to_vec();
        code.extend_from_slice(&0xBF81_0000u32.to_le_bytes());
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
            ps_input_map: PsInputMap::default(),
        };
        let err = match provider.resolve(&r, &NullMem, None) {
            Err(e) => e,
            Ok(_) => panic!("an unsupported instruction must defer, not resolve or panic"),
        };
        // task-195 seam #1: the structured RecompileError is carried OUT of the provider (no
        // longer swallowed into a bare unit ShaderUnsupported), so the snapshot can later name
        // the exact unsupported instruction + dword offset without a log grep.
        let rec = err
            .recompile_err
            .expect("a recompile-time defer carries its structured RecompileError");
        assert!(
            matches!(rec, RecompileError::UnsupportedInst { .. }),
            "the all-ones dword decodes to Inst::Unknown → UnsupportedInst, got {rec:?}"
        );
        assert!(
            rec.to_string().contains("dword offset"),
            "the carried error formats to the offset+instruction detail: {rec}"
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
            ps_input_map: PsInputMap::default(),
        };
        assert!(provider.resolve(&r, &NullMem, None).is_err());
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
            assert!(
                provider
                    .resolve_fetch_vertex_input(junk_base, 16, &semantics)
                    .is_err()
            );
        }
    }

    /// A two-region bounded reader: a `.sb` main-VS blob based at `sb_base`, and a raw
    /// fetch-shader code blob based at `fetch_base`. Mirrors the retail layout where the
    /// main VS and its fetch shader live at two distinct guest addresses (the fetch pointer
    /// is a separate user-SGPR pair). A read resolves against whichever region contains it;
    /// anything else is a clean fault (never an over-read across regions).
    struct TwoRegion {
        sb_base: u64,
        sb: Vec<u8>,
        fetch_base: u64,
        fetch: Vec<u8>,
    }

    impl BoundedRead for TwoRegion {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            for (base, buf) in [(self.sb_base, &self.sb), (self.fetch_base, &self.fetch)] {
                if addr >= base {
                    let start = (addr - base) as usize;
                    if let Some(end) = start.checked_add(size)
                        && end <= buf.len()
                    {
                        return Ok(buf[start..end].to_vec());
                    }
                }
            }
            Err("segfault")
        }
    }

    /// Read a raw fetch-shader code blob (`fetch_pos_vs.code.bin`) — the committed
    /// hand-assembled GFX7 callee bytes.
    fn fetch_callee_code(name: &str) -> Vec<u8> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gcn/tests/corpus")
            .join(format!("{name}.code.bin"));
        std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    }

    #[test]
    fn fetch_call_vs_resolves_through_provider() {
        // Gap 1 (task-113.4.1): a VS that CALLS a separate fetch shader via s_swappc_b64
        // (the 5 real Celeste VS class, doc-6 Entry 9) resolves end-to-end through the
        // provider to a HostShader. The provider reads the fetch body from the bounded seam
        // at the ref's `fetch_addr` (the s[0:1] pointer), splices it inline, and recompiles
        // the straight-line stream — the inlined `buffer_load_format … idxen` becomes the
        // recompiler's SSBO vertex-pull, so `io.buffers` is non-empty. Corpus:
        // `inline_fetch_vs.sb` (caller) + `fetch_pos_vs.code.bin` (callee); self-authored,
        // zero copyrighted assets.
        const SB_BASE: u64 = 0x0080_0000;
        const FETCH_BASE: u64 = 0x0090_0000;
        // The fetch shader lives inside a larger shader-code mapping on real HW, so a
        // provider window read past the tiny code blob still lands in mapped memory. Model
        // that by padding the fetch region well past the 24-byte code (the padding is never
        // consumed — the splice stops at the s_setpc return). `read_fetch_code`'s floor read
        // would otherwise fault against a code-sized region a real mapping never is.
        let mut fetch = fetch_callee_code("fetch_pos_vs");
        fetch.resize(4096, 0);
        let reader: Arc<dyn BoundedRead> = Arc::new(TwoRegion {
            sb_base: SB_BASE,
            sb: corpus_sb("inline_fetch_vs"),
            fetch_base: FETCH_BASE,
            fetch,
        });
        let _guard = registered_source().override_scoped(reader);

        let provider = GcnShaderProvider::new();
        let r = ShaderRef::GcnBinary {
            addr: SB_BASE,
            res: crate::shader::source::GcnResources {
                fetch_addr: Some(FETCH_BASE),
                ..Default::default()
            },
            ps_input_map: PsInputMap::default(),
        };
        let host = provider
            .resolve(&r, &NullMem, None)
            .expect("fetch-call VS must not defer")
            .expect("fetch-call VS resolves to a HostShader");
        assert_eq!(host.stage, Stage::Vertex);
        assert_eq!(host.spirv[0], 0x0723_0203, "valid SPIR-V module");
        let io = host.io.expect("recompiled VS carries IoLayout");
        assert_eq!(io.stage, ShaderStage::Vertex);
        // The inlined fetch's idxen MUBUF lowered to the SSBO vertex-pull binding.
        assert!(
            !io.buffers.is_empty(),
            "the inlined fetch's attribute load must declare an SSBO vertex-buffer binding"
        );
        assert_eq!(provider.recompile_count(), 1);
    }

    #[test]
    fn fetch_call_vs_without_fetch_pointer_defers() {
        // Gap 1 strict-or-defer: a VS with an s_swappc_b64 but NO bound fetch pointer
        // (s[0:1] == 0 → fetch_addr None) is a clean defer, never a crash or a partial
        // recompile of an unresolved call.
        const SB_BASE: u64 = 0x00A0_0000;
        let reader: Arc<dyn BoundedRead> = Arc::new(BufMem {
            base: SB_BASE,
            buf: corpus_sb("inline_fetch_vs"),
        });
        let _guard = registered_source().override_scoped(reader);
        let provider = GcnShaderProvider::new();
        let r = ShaderRef::GcnBinary {
            addr: SB_BASE,
            res: crate::shader::source::GcnResources::default(), // fetch_addr: None
            ps_input_map: PsInputMap::default(),
        };
        assert!(
            provider.resolve(&r, &NullMem, None).is_err(),
            "a fetch-call VS with no fetch pointer must defer"
        );
        assert_eq!(provider.recompile_count(), 0, "a defer is not a recompile");
    }

    #[test]
    fn fetch_call_vs_unreadable_fetch_defers() {
        // Gap 1 strict-or-defer: a fetch pointer the bounded seam cannot satisfy (unmapped)
        // defers cleanly — the fetch body is never fabricated, and no over-read occurs.
        const SB_BASE: u64 = 0x00B0_0000;
        let reader: Arc<dyn BoundedRead> = Arc::new(BufMem {
            base: SB_BASE,
            buf: corpus_sb("inline_fetch_vs"),
        });
        let _guard = registered_source().override_scoped(reader);
        let provider = GcnShaderProvider::new();
        let r = ShaderRef::GcnBinary {
            addr: SB_BASE,
            res: crate::shader::source::GcnResources {
                // Points into a region the single-region BufMem does not map → clean fault.
                fetch_addr: Some(0xDEAD_0000),
                ..Default::default()
            },
            ps_input_map: PsInputMap::default(),
        };
        assert!(
            provider.resolve(&r, &NullMem, None).is_err(),
            "an unreadable fetch shader must defer"
        );
        assert_eq!(provider.recompile_count(), 0);
    }

    #[test]
    fn interior_setpc_byte_pattern_does_not_truncate_fetch_window() {
        // task-125: a `0x20`/`0x21` SOP1 byte pattern sitting *inside* a multi-dword
        // instruction (here the second dword of a `buffer_load_format` MUBUF) must NOT stop
        // the window growth early. `read_fetch_code` grows until an instruction-aligned
        // decode FROM OFFSET 0 reaches the terminating return; the interior `0xbe80_2000`
        // (which read in isolation is `s_setpc_b64 s[0:1]`) is consumed as the MUBUF's second
        // dword and is never mistaken for the terminator, so the FULL fetch body — up to and
        // including the real return two instructions later — is returned, not truncated.
        //
        // Crafted body (all self-authored GFX7 bytes, hand-reasoned):
        //   s_load_dwordx4 s[0:3], s[2:3], 0x0          ; V# into s0 (the MUBUF's srsrc)
        //   buffer_load_format_xyzw v32, v0, s[0:3] idxen ; 2nd dword == 0xbe80_2000 (interior)
        //   s_waitcnt vmcnt(0)
        //   s_setpc_b64 s[0:1]                          ; the REAL, aligned return
        const FETCH_BASE: u64 = 0x00C0_0000;
        // `smrd` loads the V# into s[0:3] so the following idxen fetch's srsrc (0, derived
        // from the interior dword) resolves — otherwise parse_fetch_shader would defer on an
        // unloaded V#, which would still be "never truncate" but would not prove the body is
        // read WHOLE. GCN SMRD `s_load_dwordx4 s[0:3], s[2:3], 0x0` per `llvm-mc --mcpu=gfx700`
        // assembles to `0xc080_0300`; its SMRD field layout is [31:27]=0b11000, OP[26:22]=0x02,
        // SDST[21:15]=s0 (field 0), SBASE[14:9]=s[2:3] (sgpr/2 = 1), IMM[8]=1, OFFSET[7:0]=0,
        // reconstructed below (== 0xc080_0300).
        let smrd: u32 = (0b11000 << 27) | (0x02 << 22) | (1 << 9) | (1 << 8);
        // `llvm-mc --mcpu=gfx700`: `buffer_load_format_xyzw v[32:35], v0, s[0:3], 0 idxen` →
        // first dword 0xe00c_2000 (MUBUF is a 2-dword instruction).
        let mubuf_lo: u32 = 0xe00c_2000; // buffer_load_format_xyzw … idxen (first dword)
        let mubuf_hi: u32 = 0xbe80_2000; // 2nd dword CRAFTED == the s_setpc byte pattern (interior)
        let waitcnt: u32 = 0xbf8c_0f70; // llvm-mc gfx700: s_waitcnt vmcnt(0)
        let setpc: u32 = 0xbe80_2000; // llvm-mc gfx700: s_setpc_b64 s[0:1] — the real return
        let body_words = [smrd, mubuf_lo, mubuf_hi, waitcnt, setpc];
        let mut buf: Vec<u8> = body_words.iter().flat_map(|w| w.to_le_bytes()).collect();
        // A real fetch shader lives inside a larger code mapping, so a window read past the
        // tiny body still lands in mapped memory. Pad so read_fetch_code's floor (8 dwords)
        // read does not fault; the padding is never consumed — growth stops at the return.
        buf.resize(4096, 0);

        // The aligned return is the 5th dword (index 4, one dword long), so the correctly
        // sliced window is exactly 5 dwords. An interior-byte truncation would have stopped
        // at 2 dwords (the MUBUF's first dword + the phantom).
        assert_eq!(
            fetch_aligned_return_end(&body_words),
            Some(5),
            "the aligned terminator is the 5th dword; the interior 0xbe80_2000 is not it"
        );

        let reader = BufMem {
            base: FETCH_BASE,
            buf,
        };
        let window = read_fetch_code(&reader, FETCH_BASE).expect("fetch window read");
        // The window is the full body sliced at the aligned return — NOT truncated at the
        // interior dword. Its own from-0 decode still parses as a complete fetch shader
        // (load + idxen fetch + return), the exact walk resolve_fetch_call performs.
        assert_eq!(
            window.as_slice(),
            &body_words[..],
            "the full fetch body up to the aligned return must be returned, not truncated"
        );
        let layout = ps4_gcn::parse_fetch_shader(&decode_all(&window))
            .expect("the un-truncated body parses as a fetch shader");
        assert_eq!(
            layout.attributes.len(),
            1,
            "the single idxen attribute fetch survives — a truncated window would have lost it"
        );
    }

    #[test]
    fn boundary_truncated_fetch_window_terminates_and_defers() {
        // finding-7 (infinite-loop): a fetch pointer whose readable FLOOR window holds no
        // aligned s_setpc return, but whose next (GROWN) window faults at the mapping boundary,
        // MUST terminate. The old code reset `dwords` back to the floor on that fault and
        // re-grew, oscillating forever (floor reads OK → grow → fault → reset → floor → …) and
        // hanging the draw. The fix makes the window monotone: on a grown-read fault it returns
        // the largest window that DID read — a return-less body — which the downstream fetch
        // parser rejects, so resolve_gcn defers cleanly instead of spinning a core.
        const FETCH_BASE: u64 = 0x00D0_0000;
        // Eight `s_waitcnt vmcnt(0)` (SOPP, 1 dword each; llvm-mc gfx700 → 0xbf8c_0f70): a
        // decodable filler body with NO s_setpc/s_swappc terminator anywhere in the floor
        // window — the "readable prefix holds no aligned return" precondition of the bug.
        let filler: u32 = 0xbf8c_0f70;
        let floor_words = [filler; FETCH_WINDOW_MIN_DWORDS];
        assert_eq!(
            fetch_aligned_return_end(&floor_words),
            None,
            "the floor window must hold no aligned return to exercise the grow path"
        );
        // The mapping is EXACTLY the floor window: the floor read (8 dwords / 32 bytes) fits,
        // but the first grown read (16 dwords / 64 bytes) faults past the boundary — precisely
        // the oscillation trigger the fix must not loop on.
        let buf: Vec<u8> = floor_words.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(buf.len(), FETCH_WINDOW_MIN_DWORDS * 4);

        // Bound the call so a REGRESSION to the old reset-and-re-grow surfaces as a timeout,
        // not a hung test binary. A correct monotone growth returns essentially instantly.
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let reader = BufMem {
                base: FETCH_BASE,
                buf,
            };
            let _ = tx.send(read_fetch_code(&reader, FETCH_BASE));
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(5)).expect(
            "read_fetch_code must TERMINATE — a timeout here means the window oscillated (finding-7)",
        );
        worker.join().unwrap();

        // The pointer is MAPPED (floor read succeeded), just return-less: the grown-read fault
        // yields Ok of the largest readable body, NOT Err — Err is reserved for a faulting
        // FLOOR read (a genuinely unmapped pointer).
        let window = got.expect("a readable-but-return-less body is Ok, not a read fault");
        assert_eq!(
            window.as_slice(),
            &floor_words[..],
            "the largest window that read is returned whole (never re-grown, never reset)"
        );
        // Downstream: a body with no terminating return is not the recognized fetch shape, so
        // the parser returns None — the clean defer resolve_gcn turns into ShaderUnsupported
        // rather than a partial/scrambled recompile.
        assert!(
            ps4_gcn::parse_fetch_shader(&decode_all(&window)).is_none(),
            "a return-less fetch body must defer, not resolve"
        );
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

    /// Pins the two fetch-shader terminator opcodes to their AMD GCN values. The right-hand
    /// literals are the encodings emitted by `llvm-mc --assemble --arch=amdgcn --mcpu=gfx700
    /// --show-encoding` for the Sea Islands / Liverpool ISA (AMD CI-ISA `amd/ci-isa.pdf`, "SOP1
    /// Instructions", names both mnemonics): `s_setpc_b64 s[0:1]` → `0xbe80_2000` and
    /// `s_swappc_b64 s[0:1], s[0:1]` → `0xbe80_2100`. The SOP1 opcode is bits [15:8] of the
    /// dword, so our `u8` op constants must equal those bytes; this test fails if either drifts.
    #[test]
    fn sop1_terminator_opcodes_match_amd_oracle() {
        // (dword llvm-mc emitted for the mnemonic, our op const). Op field = byte [15:8].
        let oracle: [(u32, u8); 2] = [
            (0xbe80_2000, SOP1_S_SETPC_B64),  // s_setpc_b64 s[0:1]
            (0xbe80_2100, SOP1_S_SWAPPC_B64), // s_swappc_b64 s[0:1], s[0:1]
        ];
        for (dword, ours) in oracle {
            let op = ((dword >> 8) & 0xFF) as u8;
            assert_eq!(
                ours, op,
                "SOP1 op {ours:#04X} != llvm-mc op field {op:#04X}"
            );
        }
        assert_eq!(SOP1_S_SETPC_B64, 0x20);
        assert_eq!(SOP1_S_SWAPPC_B64, 0x21);
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
