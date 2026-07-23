//! Headless cache unit tests (doc-2 §6, §8.4): the cache emits a `Vec<BackendCmd>`
//! instead of driving a backend, so the tests assert on the emitted command list
//! (create/upload/import), a mock `DirtySource` over a range set, and a buffer-backed
//! `VirtualMemoryManager`. Exercises the AC #1..#4 linchpins with no GPU driver.

use std::collections::HashSet;
use std::sync::RwLock;

use ps4_core::dirty::DirtySource;
use ps4_core::gpu::{BackendCmd, ResourceId};
use ps4_core::memory::{MemoryProtection, VirtualMemoryManager};

use super::*;

/// Count each command kind in a drained list — the command-emitting analogue of the old
/// `MockBackend` counters (creates/uploads/imports).
#[derive(Default, Debug, PartialEq, Eq)]
struct CmdCounts {
    creates: u32,
    uploads: u32,
    imports: u32,
}

fn count(cmds: &[BackendCmd]) -> CmdCounts {
    let mut c = CmdCounts::default();
    for cmd in cmds {
        match cmd {
            BackendCmd::CreateBuffer { .. } => c.creates += 1,
            BackendCmd::UploadBuffer { .. } => c.uploads += 1,
            BackendCmd::ImportBuffer { .. } => c.imports += 1,
            BackendCmd::CreatePipeline { .. }
            | BackendCmd::BindPipeline { .. }
            | BackendCmd::DrawAuto { .. }
            | BackendCmd::BindVertexBuffer { .. }
            | BackendCmd::BindStorageBuffer { .. }
            | BackendCmd::BindConstBuffer { .. }
            | BackendCmd::DrawIndexed { .. }
            | BackendCmd::SetViewport(_)
            | BackendCmd::SetScissor(_)
            | BackendCmd::FreeResource { .. }
            | BackendCmd::CreateImage { .. }
            | BackendCmd::UploadImage { .. }
            | BackendCmd::CreateSampler { .. }
            | BackendCmd::BindTexture { .. }
            | BackendCmd::CreateRenderTarget { .. }
            | BackendCmd::ReadbackRenderTarget { .. }
            | BackendCmd::DumpRenderTargetPng { .. }
            | BackendCmd::SetRenderTarget { .. } => {}
        }
    }
    c
}

/// `get` into a fresh command buffer, returning `(id, emitted commands)`.
fn get_cmds(
    cache: &mut ResourceCache,
    key: ResourceKey,
    mem: &dyn VirtualMemoryManager,
    dirty: &dyn DirtySource,
) -> (ResourceId, Vec<BackendCmd>) {
    let mut out = Vec::new();
    let id = cache.get(key, mem, dirty, &mut out);
    (id, out)
}

/// A `DirtySource` whose `take_dirty` returns (and drains) a staged range set.
#[derive(Default)]
struct MockDirty {
    watched: RwLock<Vec<(u64, u64)>>,
    pending: RwLock<Vec<(u64, u64)>>,
}

impl MockDirty {
    /// Stage a guest write to surface on the next `take_dirty` (one drain).
    fn stage_write(&self, addr: u64, size: u64) {
        self.pending.write().unwrap().push((addr, size));
    }
    /// Whether `(addr, size)` is currently watched (set by `watch`, cleared by `unwatch`).
    fn is_watched(&self, addr: u64, size: u64) -> bool {
        self.watched.read().unwrap().contains(&(addr, size))
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
        std::mem::take(&mut *self.pending.write().unwrap())
    }
}

/// A flat-buffer `VirtualMemoryManager`: guest addr == index into `buf` (an identity
/// mapping over a `Vec`), enough for the cache's `get_host_ptr`/`read_bytes` calls.
struct BufMem {
    buf: RwLock<Vec<u8>>,
}

impl BufMem {
    fn new(size: usize) -> BufMem {
        BufMem {
            buf: RwLock::new(vec![0u8; size]),
        }
    }
}

impl VirtualMemoryManager for BufMem {
    fn map(
        &mut self,
        addr: u64,
        _s: usize,
        _p: MemoryProtection,
        _n: Option<&str>,
    ) -> Result<u64, &'static str> {
        Ok(addr)
    }
    fn unmap(&mut self, _a: u64, _s: usize) -> Result<(), &'static str> {
        Ok(())
    }
    fn protect(&mut self, _a: u64, _s: usize, _p: MemoryProtection) -> Result<(), &'static str> {
        Ok(())
    }
    unsafe fn get_host_ptr(&self, addr: u64) -> Option<*mut u8> {
        let mut b = self.buf.write().unwrap();
        if (addr as usize) < b.len() {
            Some(unsafe { b.as_mut_ptr().add(addr as usize) })
        } else {
            None
        }
    }
    /// The cache upload path uses the range-validated read; this identity-mapped stub
    /// validates the whole range against `buf`'s length (a boundary-crossing key `Err`s).
    fn read_bytes_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        let b = self.buf.read().unwrap();
        let start = addr as usize;
        match start.checked_add(size).and_then(|end| b.get(start..end)) {
            Some(slice) => Ok(slice.to_vec()),
            None => Err("range crosses a mapping boundary"),
        }
    }
    fn find_free_region(&mut self, _s: usize) -> u64 {
        0
    }
    fn is_memory_free(&self, _a: u64, _s: usize) -> bool {
        true
    }
}

/// A `VirtualMemoryManager` whose ranged read fails while `fail_read` is set — models
/// the guest remapping/freeing a backing range between submits so a copy-path read
/// `Err`s. `get_host_ptr` still resolves (the import path is out of scope here); only the
/// ranged read is made to fail, which is the exact seam the stale-clean bug rides on.
struct FailMem {
    buf: RwLock<Vec<u8>>,
    fail_read: RwLock<bool>,
}

impl FailMem {
    fn new(size: usize) -> FailMem {
        FailMem {
            buf: RwLock::new(vec![0u8; size]),
            fail_read: RwLock::new(false),
        }
    }
    fn set_fail(&self, fail: bool) {
        *self.fail_read.write().unwrap() = fail;
    }
}

impl VirtualMemoryManager for FailMem {
    fn map(
        &mut self,
        addr: u64,
        _s: usize,
        _p: MemoryProtection,
        _n: Option<&str>,
    ) -> Result<u64, &'static str> {
        Ok(addr)
    }
    fn unmap(&mut self, _a: u64, _s: usize) -> Result<(), &'static str> {
        Ok(())
    }
    fn protect(&mut self, _a: u64, _s: usize, _p: MemoryProtection) -> Result<(), &'static str> {
        Ok(())
    }
    unsafe fn get_host_ptr(&self, addr: u64) -> Option<*mut u8> {
        let mut b = self.buf.write().unwrap();
        if (addr as usize) < b.len() {
            Some(unsafe { b.as_mut_ptr().add(addr as usize) })
        } else {
            None
        }
    }
    fn read_bytes_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
        if *self.fail_read.read().unwrap() {
            return Err("guest range unreadable");
        }
        let b = self.buf.read().unwrap();
        let start = addr as usize;
        match start.checked_add(size).and_then(|end| b.get(start..end)) {
            Some(slice) => Ok(slice.to_vec()),
            None => Err("out of range"),
        }
    }
    fn find_free_region(&mut self, _s: usize) -> u64 {
        0
    }
    fn is_memory_free(&self, _a: u64, _s: usize) -> bool {
        true
    }
}

fn key(addr: u64, size: u64, layout: ResLayout) -> ResourceKey {
    ResourceKey { addr, size, layout }
}

/// AC #1: first use emits 1 create + 1 upload command; clean reuse emits nothing; a dirty
/// range emits exactly one re-upload command for the overlapped entry (§6 linchpin).
#[test]
fn first_use_clean_reuse_dirty_reupload() {
    let mem = BufMem::new(0x4000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    // Index buffers: only a WATCHED layout has clean and dirty hits to tell apart
    // (task-227 leaves the vertex ring and the constant buffers unwatched, always
    // re-uploading).
    let k_a = key(0x1000, 0x100, ResLayout::IndexBuf);
    let k_b = key(0x2000, 0x100, ResLayout::IndexBuf);

    // First use of A: exactly one create + one upload, carrying the guest-minted id.
    let (id_a, cmds) = get_cmds(&mut cache, k_a, &mem, &dirty);
    assert_eq!(
        count(&cmds),
        CmdCounts {
            creates: 1,
            uploads: 1,
            imports: 0
        },
        "first use emits one create + one upload"
    );
    assert_eq!(
        cmds,
        vec![
            BackendCmd::CreateBuffer {
                id: id_a,
                size: 0x100
            },
            BackendCmd::UploadBuffer {
                id: id_a,
                offset: 0,
                data: vec![0u8; 0x100].into()
            },
        ],
        "commands carry the guest-minted id, size, and offset"
    );

    // First use of B: a second entry with its own create + upload.
    let (id_b, cmds) = get_cmds(&mut cache, k_b, &mem, &dirty);
    assert_ne!(id_a, id_b, "distinct ranges get distinct ids");
    assert_eq!(
        count(&cmds),
        CmdCounts {
            creates: 1,
            uploads: 1,
            imports: 0
        }
    );

    // Clean reuse of both: no commands.
    let (rid_a, cmds_a) = get_cmds(&mut cache, k_a, &mem, &dirty);
    let (rid_b, cmds_b) = get_cmds(&mut cache, k_b, &mem, &dirty);
    assert_eq!(rid_a, id_a);
    assert_eq!(rid_b, id_b);
    assert!(cmds_a.is_empty(), "clean reuse emits nothing");
    assert!(cmds_b.is_empty(), "clean reuse emits nothing");

    // Dirty A only: drain marks A dirty; next get(A) emits one re-upload, get(B) none.
    dirty.stage_write(0x1000, 8);
    cache.drain_dirty(&dirty);
    let (_, cmds_a) = get_cmds(&mut cache, k_a, &mem, &dirty);
    assert_eq!(
        count(&cmds_a),
        CmdCounts {
            creates: 0,
            uploads: 1,
            imports: 0
        },
        "dirty A re-uploads exactly once (no re-create)"
    );
    let (_, cmds_b) = get_cmds(&mut cache, k_b, &mem, &dirty);
    assert!(cmds_b.is_empty(), "B was not dirtied → no re-upload");
}

/// AC #1 (overlap precision): a dirtied range marks exactly the overlapping entries.
#[test]
fn drain_marks_only_overlapping_entries() {
    let mem = BufMem::new(0x8000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    // A watched layout (IndexBuf, like B and C) so a clean hit and a dirty re-upload are
    // distinguishable: an unwatched layout (VertexBuf/ConstBuf) re-uploads on every get
    // regardless of the dirty flag (watch_pays == false → the clean-hit branch is never
    // taken), which would make A's `uploads == 1` assertion pass without the overlap logic
    // ever marking A dirty.
    let k_a = key(0x1000, 0x100, ResLayout::IndexBuf); // [0x1000, 0x1100)
    let k_b = key(0x1080, 0x100, ResLayout::IndexBuf); // [0x1080, 0x1180) — overlaps write
    let k_c = key(0x5000, 0x100, ResLayout::IndexBuf); // far away — no overlap
    let (id_a, _) = get_cmds(&mut cache, k_a, &mem, &dirty);
    let (id_b, _) = get_cmds(&mut cache, k_b, &mem, &dirty);
    let (id_c, _) = get_cmds(&mut cache, k_c, &mem, &dirty);

    // A write at [0x10F0, 0x1100) overlaps A and B but not C.
    dirty.stage_write(0x10F0, 0x10);
    cache.drain_dirty(&dirty);

    let (_, cmds_a) = get_cmds(&mut cache, k_a, &mem, &dirty);
    let (_, cmds_b) = get_cmds(&mut cache, k_b, &mem, &dirty);
    let (_, cmds_c) = get_cmds(&mut cache, k_c, &mem, &dirty);
    let _ = (id_a, id_b, id_c);
    assert_eq!(count(&cmds_a).uploads, 1, "A overlaps → re-upload");
    assert_eq!(count(&cmds_b).uploads, 1, "B overlaps → re-upload");
    assert!(cmds_c.is_empty(), "C does not overlap → no command");
}

/// AC #2: the same bytes as two different layouts get two entries (two ids, two
/// create+upload command pairs).
#[test]
fn same_bytes_two_layouts_two_entries() {
    let mem = BufMem::new(0x4000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    let as_vtx = key(0x1000, 0x100, ResLayout::VertexBuf);
    let as_const = key(0x1000, 0x100, ResLayout::ConstBuf);

    let (id_vtx, cmds_vtx) = get_cmds(&mut cache, as_vtx, &mem, &dirty);
    let (id_const, cmds_const) = get_cmds(&mut cache, as_const, &mem, &dirty);

    assert_ne!(id_vtx, id_const, "same bytes, two layouts → two ids");
    assert_eq!(count(&cmds_vtx).creates, 1);
    assert_eq!(count(&cmds_const).creates, 1);
    assert_eq!(count(&cmds_vtx).uploads, 1);
    assert_eq!(count(&cmds_const).uploads, 1);

    // Distinct guest-minted ids appear on the two create commands.
    let created: HashSet<u32> = cmds_vtx
        .iter()
        .chain(cmds_const.iter())
        .filter_map(|c| match c {
            BackendCmd::CreateBuffer { id, .. } => Some(id.0),
            _ => None,
        })
        .collect();
    assert_eq!(created.len(), 2, "distinct guest-minted ids");
}

/// A coherence source reporting every range as a garlic zero-copy candidate — the
/// desktop-with-extension mirror that reaches the import fork (doc-2 §C5).
struct AllGarlic;
impl CoherenceSource for AllGarlic {
    fn coherence(&self, _a: u64, _s: u64) -> Coherence {
        Coherence::ZeroCopyCandidate
    }
}
/// An import mirror that accepts every candidate range (doc-2 §8.2 desktop mirror).
struct AllImportable;
impl ImportProbe for AllImportable {
    fn can_import(&self, _a: u64, _s: u64, _c: Coherence) -> bool {
        true
    }
}

fn zero_copy_policy() -> CachePolicy {
    CachePolicy::new(Box::new(AllGarlic), Box::new(AllImportable))
}

/// AC #3: zero-copy fork with an authoritative guest-side probe. A probe-yes emits one
/// `ImportBuffer` and no create/upload; the entry is imported and never re-uploads even
/// after a guest write. There is no backend round-trip to decline — the display thread
/// must honor the import.
#[test]
fn zero_copy_import_emits_import_command_no_upload() {
    let mem = BufMem::new(0x4000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::with_policy(zero_copy_policy());

    let k = key(0x1000, 0x100, ResLayout::VertexBuf);
    let (id, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(
        count(&cmds),
        CmdCounts {
            creates: 0,
            uploads: 0,
            imports: 1
        },
        "import path emits one ImportBuffer, no create/upload"
    );
    assert_eq!(
        cmds,
        vec![BackendCmd::ImportBuffer {
            id,
            addr: 0x1000,
            size: 0x100
        }],
        "ImportBuffer carries the guest-minted id and range"
    );

    // A hit on an imported entry is always clean (GPU reads guest pages directly).
    dirty.stage_write(0x1000, 8);
    cache.drain_dirty(&dirty);
    let (_, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert!(cmds.is_empty(), "imported entry never re-uploads");
}

/// AC #4: the onion/garlic hook is an optional policy input defaulting to copy-side
/// (doc-2 §C5). The default cache never imports even when the range would be a
/// candidate; an explicit policy is required to opt into zero-copy.
#[test]
fn policy_defaults_copy_side() {
    let mem = BufMem::new(0x4000);
    let dirty = MockDirty::default();

    // Default cache: copy-side. Its coherence is CopySide so the import fork is never
    // taken; first use emits create + upload, no import.
    let mut cache = ResourceCache::new();
    let k = key(0x1000, 0x100, ResLayout::VertexBuf);
    let (_, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(
        count(&cmds),
        CmdCounts {
            creates: 1,
            uploads: 1,
            imports: 0
        },
        "default policy is copy-side: create + upload, never import"
    );
}

/// A dirty hit whose re-upload read FAILS must leave the entry dirty and emit NO upload
/// command — the next `get` retries once the range is readable again. Guards the
/// stale-clean bug: a failed read would otherwise strand a clean-but-stale buffer.
#[test]
fn dirty_hit_failed_read_stays_dirty_and_retries() {
    let mem = FailMem::new(0x4000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    let k = key(0x1000, 0x100, ResLayout::IndexBuf);

    // First use: clean create + upload while reads succeed.
    let (id, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(count(&cmds).uploads, 1, "first use uploads once");

    // Guest writes → entry marked dirty on drain.
    dirty.stage_write(0x1000, 8);
    cache.drain_dirty(&dirty);

    // The backing range is now unreadable (guest remapped/freed it). The dirty-hit
    // re-upload must emit NO command and must NOT clear dirty.
    mem.set_fail(true);
    let (rid, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id, "same id");
    assert!(
        cmds.is_empty(),
        "failed read → no upload command (backend not told it's clean)"
    );

    // Range readable again: the still-dirty entry retries on the next get.
    mem.set_fail(false);
    let (rid, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id);
    assert_eq!(
        count(&cmds).uploads,
        1,
        "entry stayed dirty → retried and re-uploaded once readable"
    );

    // And that retry cleared dirty: a further clean hit emits nothing.
    let (rid, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id);
    assert!(cmds.is_empty(), "clean after successful retry");
}

/// A dirty hit whose re-upload read SUCCEEDS emits one upload command and clears dirty
/// (existing behavior preserved by the reshape).
#[test]
fn dirty_hit_successful_read_reuploads_and_clears() {
    let mem = FailMem::new(0x4000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    let k = key(0x2000, 0x100, ResLayout::IndexBuf);
    let (id, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(count(&cmds).uploads, 1);

    dirty.stage_write(0x2000, 8);
    cache.drain_dirty(&dirty);

    // Read succeeds → re-upload command emitted and dirty clears.
    let (rid, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id);
    assert_eq!(
        count(&cmds).uploads,
        1,
        "dirty hit with good read re-uploads"
    );
    // Clean hit afterwards: no further command proves dirty was cleared.
    let (rid, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id);
    assert!(cmds.is_empty(), "dirty cleared after successful re-upload");
}

/// First use whose INITIAL upload read fails must emit the create but NO upload command,
/// and insert the entry dirty so a later `get` retries once the range is readable —
/// rather than handing out a ready-looking buffer holding no data.
#[test]
fn first_use_failed_read_inserts_dirty_and_retries() {
    let mem = FailMem::new(0x4000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    let k = key(0x1000, 0x100, ResLayout::IndexBuf);

    // Initial read fails: the buffer create is emitted but no upload.
    mem.set_fail(true);
    let (id, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(
        count(&cmds),
        CmdCounts {
            creates: 1,
            uploads: 0,
            imports: 0
        },
        "create emitted, failed initial read → no upload"
    );

    // A follow-up get while still failing must not treat it as a clean ready entry.
    let (rid, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id, "same id");
    assert!(cmds.is_empty(), "still no upload while unreadable");

    // Range readable now: the dirty entry retries and uploads exactly once.
    mem.set_fail(false);
    let (rid, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id);
    assert_eq!(
        count(&cmds).uploads,
        1,
        "dirty-inserted entry retries once readable"
    );
    // And clears: a subsequent clean hit emits nothing.
    let (rid, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id);
    assert!(cmds.is_empty(), "clean after successful first upload");
}

/// AC #1: the upload path is range-validated. A first-use key whose range crosses a
/// mapping boundary (here, past the end of the backing buffer) must NOT over-read: the
/// ranged read `Err`s, so the create is emitted but no upload, and the entry is inserted
/// dirty so a later `get` retries — never a snapshot of raw host memory past the boundary.
#[test]
fn boundary_crossing_key_does_not_over_read() {
    // Backing memory ends at 0x2000; the key spans [0x1F00, 0x2100), crossing the end.
    let mem = BufMem::new(0x2000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    let k = key(0x1F00, 0x200, ResLayout::VertexBuf);
    let (id, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(
        count(&cmds),
        CmdCounts {
            creates: 1,
            uploads: 0,
            imports: 0
        },
        "boundary-crossing range: create emitted, ranged read Err → no over-read upload"
    );

    // Still dirty (not stranded clean): a follow-up while the range stays unreadable
    // emits nothing but the entry is not treated as ready.
    let (rid, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id, "same id");
    assert!(
        cmds.is_empty(),
        "still no upload while range crosses boundary"
    );
}

/// A coherence source reporting every range as a garlic zero-copy candidate, paired with
/// a probe that always declines — models a zero-copy-eligible range on a device without
/// the import extension (the portable default). The cache must take the copy path.
struct AllGarlicNoImport;
impl CoherenceSource for AllGarlicNoImport {
    fn coherence(&self, _a: u64, _s: u64) -> Coherence {
        Coherence::ZeroCopyCandidate
    }
}

/// AC #4: when the import probe declines (copy-side coherence, or a candidate range the
/// probe rejects), first use emits the copy-path command list — CreateBuffer + UploadBuffer
/// — and NEVER an ImportBuffer. Guards the import-decline branch task-order removed.
#[test]
fn probe_declines_emits_copy_path_no_import() {
    let mem = BufMem::new(0x4000);

    // Fill the two guest ranges used below with a recognizable non-zero ramp so the
    // UploadBuffer assertion can distinguish a correct-content upload from a zero-
    // initialized one.
    {
        let mut b = mem.buf.write().unwrap();
        for (i, byte) in b[0x1000..0x1100].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_add(0xA0);
        }
        for (i, byte) in b[0x2000..0x2100].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_add(0xB0);
        }
    }
    let expected_a: Vec<u8> = (0..0x100u16)
        .map(|i| (i as u8).wrapping_add(0xA0))
        .collect();
    let expected_b: Vec<u8> = (0..0x100u16)
        .map(|i| (i as u8).wrapping_add(0xB0))
        .collect();

    let dirty = MockDirty::default();

    // Case A: the default copy-side coherence — the import fork is never even reached.
    let mut copy_cache = ResourceCache::new();
    let k = key(0x1000, 0x100, ResLayout::VertexBuf);
    let (id, cmds) = get_cmds(&mut copy_cache, k, &mem, &dirty);
    assert_eq!(
        count(&cmds),
        CmdCounts {
            creates: 1,
            uploads: 1,
            imports: 0
        },
        "copy-side coherence: create + upload, no import"
    );
    assert_eq!(
        cmds,
        vec![
            BackendCmd::CreateBuffer { id, size: 0x100 },
            BackendCmd::UploadBuffer {
                id,
                offset: 0,
                data: expected_a.into()
            },
        ],
        "copy path emits exactly CreateBuffer then UploadBuffer with correct content"
    );

    // Case B: a garlic zero-copy *candidate* range whose probe still declines (NoImport /
    // no import extension). The import branch is reached but vetoed, so the copy path is
    // taken — proving the probe is the authority and a decline never emits ImportBuffer.
    let mut candidate_cache = ResourceCache::with_policy(CachePolicy::new(
        Box::new(AllGarlicNoImport),
        Box::new(NoImport),
    ));
    let k2 = key(0x2000, 0x100, ResLayout::IndexBuf);
    let (id2, cmds) = get_cmds(&mut candidate_cache, k2, &mem, &dirty);
    assert_eq!(
        count(&cmds),
        CmdCounts {
            creates: 1,
            uploads: 1,
            imports: 0
        },
        "candidate range, probe declines: copy path (create + upload), no import"
    );
    assert_eq!(
        cmds,
        vec![
            BackendCmd::CreateBuffer {
                id: id2,
                size: 0x100
            },
            BackendCmd::UploadBuffer {
                id: id2,
                offset: 0,
                data: expected_b.into()
            },
        ],
        "declined-probe copy path carries the exact guest bytes, not zero-initialized data"
    );
}

/// Collect the ids named by `FreeResource` commands in a drained list.
fn freed_ids(cmds: &[BackendCmd]) -> Vec<u32> {
    cmds.iter()
        .filter_map(|c| match c {
            BackendCmd::FreeResource { id } => Some(id.0),
            _ => None,
        })
        .collect()
}

/// AC #1: freeing a guest range evicts the overlapping copy-path entry — a subsequent
/// `get()` for the SAME key mints a NEW id and re-creates, instead of the stale-id clean
/// hit the insert-only cache used to return on a free+realloc of the same address.
///
/// Independent expected state (not captured from the production path): id1 comes from the
/// first `get`; after a free, a second `get` for the identical key must return an id that
/// is (a) different from id1 and (b) accompanied by a fresh CreateBuffer — a clean hit
/// would return id1 with no commands. The free itself must emit exactly one FreeResource
/// naming id1.
#[test]
fn free_evicts_copy_entry_realloc_mints_new_id() {
    let mem = BufMem::new(0x4000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    let k = key(0x1000, 0x100, ResLayout::IndexBuf);

    // First use: id1, create + upload, and the range is now watched.
    let (id1, cmds1) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(count(&cmds1).creates, 1, "first use creates");
    assert!(
        dirty.is_watched(0x1000, 0x100),
        "first use watches the range"
    );

    // A clean re-get returns id1 with no commands — the stale-clean-hit the free must break.
    let (rid, clean) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id1, "clean hit returns the same id before any free");
    assert!(clean.is_empty(), "clean hit emits nothing");

    // Guest frees the exact backing range.
    let mut free_cmds = Vec::new();
    cache.free_range(0x1000, 0x100, &dirty, &mut free_cmds);
    assert_eq!(
        freed_ids(&free_cmds),
        vec![id1.0],
        "free emits exactly one FreeResource naming the evicted id"
    );
    assert!(
        !dirty.is_watched(0x1000, 0x100),
        "free unwatches the range so the dirty source stops tracking freed pages"
    );

    // Realloc of the SAME key: must mint a NEW id and re-create — no stale-id clean hit.
    let (id2, cmds2) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_ne!(id2, id1, "free+realloc mints a fresh id, not the stale one");
    assert_eq!(
        count(&cmds2).creates,
        1,
        "realloc re-creates the backend buffer (not a clean no-op hit)"
    );
    assert!(
        dirty.is_watched(0x1000, 0x100),
        "realloc re-watches the range"
    );
}

/// AC #2: an imported (zero-copy) entry whose range is freed is REVOKED — a FreeResource is
/// emitted so the backend drops the external-memory buffer, and the cache entry is gone so
/// no dangling import survives. This is the ONLY unimport path: `drain_dirty`/
/// `invalidate_range` both skip imported entries, so without `free_range` a freed garlic
/// import would read freed host pages forever.
///
/// Independent expected state: the import path emits one ImportBuffer under id1 (no
/// create/upload); the free must emit exactly one FreeResource naming id1; and a later
/// `get` for the same key must mint a fresh id + re-emit ImportBuffer (proving the old
/// imported entry was truly dropped, not left clean-forever).
#[test]
fn free_revokes_imported_entry() {
    let mem = BufMem::new(0x4000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::with_policy(zero_copy_policy());

    let k = key(0x1000, 0x100, ResLayout::VertexBuf);

    // Import: one ImportBuffer under id1, no create/upload; range watched.
    let (id1, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(count(&cmds).imports, 1, "import path emits ImportBuffer");
    assert_eq!(count(&cmds).creates, 0, "import path emits no create");

    // A dirtied write never re-uploads an import (existing behavior) — confirms the entry is
    // imported and so the ONLY way to drop it is a free.
    dirty.stage_write(0x1000, 8);
    cache.drain_dirty(&dirty);
    let (rid, clean) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id1, "imported hit returns id1");
    assert!(
        clean.is_empty(),
        "imported entry never re-uploads on a write"
    );

    // Guest frees the imported range → the import is revoked.
    let mut free_cmds = Vec::new();
    cache.free_range(0x1000, 0x100, &dirty, &mut free_cmds);
    assert_eq!(
        freed_ids(&free_cmds),
        vec![id1.0],
        "freeing an imported range emits one FreeResource to revoke the import"
    );
    assert!(
        !dirty.is_watched(0x1000, 0x100),
        "free unwatches the imported range"
    );

    // The imported entry is truly gone: a re-get mints a fresh id and re-imports (a stale
    // dangling import would instead be a clean hit returning id1 with no command).
    let (id2, cmds2) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_ne!(id2, id1, "re-import after free mints a fresh id");
    assert_eq!(
        count(&cmds2).imports,
        1,
        "re-import re-emits ImportBuffer (old imported entry was dropped, not stranded)"
    );
}

/// A free only evicts entries whose backing range OVERLAPS the freed range; unrelated
/// entries (and a partial overlap of a differently-keyed layout at the same base) are
/// handled per the half-open overlap rule. Independent expected state: freeing A's range
/// evicts A (and any entry overlapping it) but leaves a far-away C untouched — C keeps its
/// id on a clean hit, A's realloc mints a new one.
#[test]
fn free_evicts_only_overlapping_entries() {
    let mem = BufMem::new(0x8000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    let k_a = key(0x1000, 0x100, ResLayout::VertexBuf); // [0x1000, 0x1100)
    let k_c = key(0x5000, 0x100, ResLayout::IndexBuf); // far away

    let (id_a, _) = get_cmds(&mut cache, k_a, &mem, &dirty);
    let (id_c, _) = get_cmds(&mut cache, k_c, &mem, &dirty);

    // Free A's range only.
    let mut free_cmds = Vec::new();
    cache.free_range(0x1000, 0x100, &dirty, &mut free_cmds);
    assert_eq!(freed_ids(&free_cmds), vec![id_a.0], "only A is freed");

    // C survives as a clean hit with its original id; A's realloc mints a new id.
    let (rid_c, cmds_c) = get_cmds(&mut cache, k_c, &mem, &dirty);
    assert_eq!(rid_c, id_c, "non-overlapping C keeps its id");
    assert!(cmds_c.is_empty(), "C was not freed → clean hit, no command");

    let (rid_a, cmds_a) = get_cmds(&mut cache, k_a, &mem, &dirty);
    assert_ne!(rid_a, id_a, "A was freed → realloc mints a new id");
    assert_eq!(count(&cmds_a).creates, 1, "A re-creates");
}

/// Freeing a range with no cached entry (and a zero-length free) is a no-op: no
/// FreeResource, no panic. Guards the idempotent free contract.
#[test]
fn free_unmatched_or_empty_range_is_noop() {
    let mem = BufMem::new(0x4000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    let k = key(0x1000, 0x100, ResLayout::IndexBuf);
    let (_, _) = get_cmds(&mut cache, k, &mem, &dirty);

    // A free of a disjoint range touches nothing.
    let mut cmds = Vec::new();
    cache.free_range(0x9000, 0x100, &dirty, &mut cmds);
    assert!(cmds.is_empty(), "free of an unmapped range emits nothing");
    assert!(
        dirty.is_watched(0x1000, 0x100),
        "the live entry's range stays watched"
    );

    // A zero-length free overlaps nothing (half-open rule).
    cache.free_range(0x1000, 0, &dirty, &mut cmds);
    assert!(cmds.is_empty(), "zero-length free evicts nothing");
    assert!(
        dirty.is_watched(0x1000, 0x100),
        "zero-length free does not unwatch"
    );
}

/// task-227: the write barrier is charged per guest STORE, so a layout whose entries never
/// produce a CLEAN hit must not be watched at all — a clean hit is the only thing dirty
/// state can buy. Vertex ranges (a ring whose key moves with the write cursor) and constant
/// buffers (rewritten every frame) measure at zero clean hits; index buffers and textures
/// are hit repeatedly and keep their tracking.
///
/// The pairing is what keeps it correct: an unwatched entry is never reported dirty, so it
/// must never be served as a clean hit either. Asserted here as one property, because
/// splitting them is exactly how a stale-content regression gets in.
#[test]
fn unwatched_layouts_reupload_instead_of_serving_a_clean_hit() {
    let mem = BufMem::new(0x4000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    for (layout, addr) in [
        (ResLayout::VertexBuf, 0x1000u64),
        (ResLayout::ConstBuf, 0x2000),
    ] {
        let k = key(addr, 0x100, layout);
        let (id, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
        assert_eq!(count(&cmds).creates, 1, "{layout:?}: first use creates");
        assert!(
            !dirty.is_watched(addr, 0x100),
            "{layout:?}: the barrier buys nothing here — the range must stay unwatched"
        );

        // No dirty report can ever arrive for this range, so the hit re-uploads blind.
        let (rid, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
        assert_eq!(rid, id, "{layout:?}: same entry, not a re-create");
        assert_eq!(
            count(&cmds),
            CmdCounts {
                creates: 0,
                uploads: 1,
                imports: 0
            },
            "{layout:?}: unwatched hit re-uploads rather than trusting a stale clean flag"
        );

        // Freeing must not unwatch what was never watched: tracking is page-granular, and
        // a spurious unwatch drops protection for whatever live entry shares the page.
        let mut free_cmds = Vec::new();
        cache.free_range(addr, 0x100, &dirty, &mut free_cmds);
        assert_eq!(
            freed_ids(&free_cmds),
            vec![id.0],
            "{layout:?}: free still evicts the entry"
        );
    }

    // A watched layout at a range the loop above freed keeps the clean-hit behaviour.
    let k = key(0x1000, 0x100, ResLayout::IndexBuf);
    let (id, _) = get_cmds(&mut cache, k, &mem, &dirty);
    assert!(
        dirty.is_watched(0x1000, 0x100),
        "index buffers are hit repeatedly and rarely rewritten — tracking earns its keep"
    );
    let (rid, cmds) = get_cmds(&mut cache, k, &mem, &dirty);
    assert_eq!(rid, id);
    assert!(cmds.is_empty(), "watched clean hit still emits nothing");
}

// ---- sampled-texture cache path (doc-2 §C3/§C4) ----------------------------

use ps4_core::gpu::{ColorFormat, SamplerAddressMode, SamplerDesc, SamplerFilter, TextureFormat};

/// A 2×2 linear R8G8B8A8 texture key. Linear tiling → detile is an identity copy, so the
/// uploaded bytes equal the guest bytes exactly (asserted below).
fn tex_key(addr: u64, w: u32, h: u32, tiling: Tiling) -> (ResourceKey, SurfaceLayout) {
    let surface = SurfaceLayout {
        texel: TexelSize::Bpp32,
        extent: Extent {
            width: w,
            height: h,
        },
        tiling,
        compression: Compression::Off,
        pitch: 0,
    };
    let key = ResourceKey {
        addr,
        size: surface.linear_size() as u64, // 2*2*4 = 16 for a 2×2 linear texture
        layout: ResLayout::Texture {
            format: SurfaceFormat { dfmt: 10, nfmt: 0 },
            surface,
        },
    };
    (key, surface)
}

/// AC #2: a textured draw's texture cache emits CreateImage + UploadImage (detiled) on
/// first use; a clean reuse emits nothing; a guest write to the texel range re-uploads
/// EXACTLY once on next use. The command shapes are hand-reasoned literals, not read back
/// from a production builder.
#[test]
fn texture_first_use_clean_reuse_single_reupload() {
    let mem = BufMem::new(0x4000);
    // Write a distinct 2×2 RGBA texture at 0x1000: four texels, row-major.
    let texels: [u8; 16] = [
        1, 2, 3, 4, // (0,0)
        5, 6, 7, 8, // (1,0)
        9, 10, 11, 12, // (0,1)
        13, 14, 15, 16, // (1,1)
    ];
    mem.buf.write().unwrap()[0x1000..0x1010].copy_from_slice(&texels);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    let (k, surface) = tex_key(0x1000, 2, 2, Tiling::LinearGeneral);

    // First use: exactly CreateImage(2×2, RGBA8) then UploadImage with the linear bytes
    // (identity detile of the guest texels — hand-reasoned equal to the input).
    let mut cmds = Vec::new();
    let id = cache.get_texture(
        k,
        surface,
        TextureFormat::R8G8B8A8Unorm,
        &mem,
        &dirty,
        &mut cmds,
    );
    assert_eq!(
        cmds,
        vec![
            BackendCmd::CreateImage {
                id,
                width: 2,
                height: 2,
                format: TextureFormat::R8G8B8A8Unorm,
            },
            BackendCmd::UploadImage {
                id,
                data: std::sync::Arc::from(&texels[..]),
            },
        ],
        "first use: create-image then upload the detiled linear texels"
    );
    // The texel range is watched for dirty tracking.
    assert!(dirty.is_watched(0x1000, 0x10), "texel range watched");

    // Clean reuse: no commands.
    let mut cmds = Vec::new();
    let id2 = cache.get_texture(
        k,
        surface,
        TextureFormat::R8G8B8A8Unorm,
        &mem,
        &dirty,
        &mut cmds,
    );
    assert_eq!(id2, id, "same id on reuse");
    assert!(cmds.is_empty(), "clean reuse emits no commands");

    // Guest write to the texel range → mark dirty via drain, then next use re-uploads
    // EXACTLY once (no second CreateImage — the image already exists).
    dirty.stage_write(0x1004, 4); // overwrite texel (1,0)
    mem.buf.write().unwrap()[0x1004..0x1008].copy_from_slice(&[100, 101, 102, 103]);
    cache.drain_dirty(&dirty);

    let mut cmds = Vec::new();
    let id3 = cache.get_texture(
        k,
        surface,
        TextureFormat::R8G8B8A8Unorm,
        &mem,
        &dirty,
        &mut cmds,
    );
    assert_eq!(id3, id, "same id after re-upload");
    let expected: [u8; 16] = [
        1, 2, 3, 4, 100, 101, 102, 103, 9, 10, 11, 12, 13, 14, 15, 16,
    ];
    assert_eq!(
        cmds,
        vec![BackendCmd::UploadImage {
            id,
            data: std::sync::Arc::from(&expected[..]),
        }],
        "guest write → exactly one re-upload (no second CreateImage)"
    );

    // And immediately after, a clean hit emits nothing (the re-upload cleared dirty).
    let mut cmds = Vec::new();
    cache.get_texture(
        k,
        surface,
        TextureFormat::R8G8B8A8Unorm,
        &mem,
        &dirty,
        &mut cmds,
    );
    assert!(cmds.is_empty(), "clean after re-upload emits nothing");
}

/// The upload path DETILES a tiled surface before upload: a 1D-thin 8×8 tile's texel (1,0)
/// is not at byte offset 4 in the tiled bytes (the Morton swizzle puts it at index 1 too,
/// but texel (0,1) is at swizzled index 2, not the linear index 8). We assert the linear
/// output places (0,1) where the linear layout expects it, proving detile ran.
#[test]
fn texture_upload_detiles_a_tiled_surface() {
    let mem = BufMem::new(0x1000);
    // An 8×8 32bpp tiled surface = 64 texels * 4 bytes = 256 bytes. Fill with a marker at
    // the SWIZZLED offset of texel (0,1): micro_tile_index(0,1) = 2 → byte offset 2*4 = 8.
    {
        let mut b = mem.buf.write().unwrap();
        // Marker bytes at swizzled index 2 (tiled offset 8).
        b[8..12].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
    }
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    let surface = SurfaceLayout {
        texel: TexelSize::Bpp32,
        extent: Extent {
            width: 8,
            height: 8,
        },
        tiling: Tiling::Thin1d,
        compression: Compression::Off,
        pitch: 0,
    };
    let key = ResourceKey {
        addr: 0,
        size: 256,
        layout: ResLayout::Texture {
            format: SurfaceFormat { dfmt: 10, nfmt: 0 },
            surface,
        },
    };
    let mut cmds = Vec::new();
    cache.get_texture(
        key,
        surface,
        TextureFormat::R8G8B8A8Unorm,
        &mem,
        &dirty,
        &mut cmds,
    );
    // The single UploadImage carries the DETILED linear bytes: texel (0,1) is at linear
    // offset (1*8 + 0)*4 = 32, so the marker must have moved from tiled offset 8 → 32.
    let up = cmds
        .iter()
        .find_map(|c| match c {
            BackendCmd::UploadImage { data, .. } => Some(data.clone()),
            _ => None,
        })
        .expect("upload emitted");
    assert_eq!(
        &up[32..36],
        &[0xAA, 0xBB, 0xCC, 0xDD],
        "detile moved texel (0,1) from swizzled offset 8 to linear offset 32"
    );
    assert_ne!(
        &up[8..12],
        &[0xAA, 0xBB, 0xCC, 0xDD],
        "not an identity copy"
    );
}

/// A sampler is created once per distinct SamplerDesc and its id reused (no re-create).
#[test]
fn sampler_created_once_per_desc() {
    let mut cache = ResourceCache::new();
    let desc = SamplerDesc {
        mag_filter: SamplerFilter::Linear,
        min_filter: SamplerFilter::Linear,
        address_mode_u: SamplerAddressMode::Repeat,
        address_mode_v: SamplerAddressMode::Repeat,
    };
    let mut cmds = Vec::new();
    let id = cache.get_sampler(desc, &mut cmds);
    assert_eq!(
        cmds,
        vec![BackendCmd::CreateSampler { id, desc }],
        "first use emits one CreateSampler"
    );
    // Reuse: same id, no command.
    let mut cmds = Vec::new();
    let id2 = cache.get_sampler(desc, &mut cmds);
    assert_eq!(id2, id);
    assert!(cmds.is_empty(), "same desc reuses the sampler id");
    // A different filter mints a new id + emits a create.
    let point = SamplerDesc {
        mag_filter: SamplerFilter::Nearest,
        min_filter: SamplerFilter::Nearest,
        address_mode_u: SamplerAddressMode::Repeat,
        address_mode_v: SamplerAddressMode::Repeat,
    };
    let mut cmds = Vec::new();
    let id3 = cache.get_sampler(point, &mut cmds);
    assert_ne!(id3, id, "distinct desc → distinct id");
    assert_eq!(
        cmds,
        vec![BackendCmd::CreateSampler {
            id: id3,
            desc: point
        }]
    );
}

// ---- render-target cache path (doc-2 §8.5, task-56) ------------------------

/// A 2×2 R8G8B8A8 render-target key over the SAME (addr, size) a `tex_key` would produce,
/// but with a `ResLayout::RenderTarget` layout — the split key that makes a range aliased
/// as both RT and texture yield two independent cache entries (cache/mod.rs:124).
fn rt_key(addr: u64, w: u32, h: u32) -> (ResourceKey, SurfaceLayout) {
    let surface = SurfaceLayout {
        texel: TexelSize::Bpp32,
        extent: Extent {
            width: w,
            height: h,
        },
        tiling: Tiling::LinearGeneral,
        compression: Compression::Off,
        pitch: 0,
    };
    let key = ResourceKey {
        addr,
        size: surface.linear_size() as u64,
        layout: ResLayout::RenderTarget {
            format: SurfaceFormat { dfmt: 10, nfmt: 0 },
            surface,
        },
    };
    (key, surface)
}

#[test]
fn render_target_first_use_emits_one_create_and_no_upload() {
    // First use of a render target emits EXACTLY one CreateRenderTarget and NO upload (the
    // GPU fills it). Every later use is a clean hit that emits nothing — a render target is
    // never dirty-driven. Hand-reasoned expected shape.
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();
    let (k, surface) = rt_key(0x2000, 2, 2);

    let mut cmds = Vec::new();
    let id = cache.get_render_target(k, surface, ColorFormat::R8G8B8A8Unorm, &mut cmds);
    assert_eq!(
        cmds,
        vec![BackendCmd::CreateRenderTarget {
            id,
            width: 2,
            height: 2,
            format: ColorFormat::R8G8B8A8Unorm,
        }],
        "first use: exactly one CreateRenderTarget, and NO upload"
    );
    // A render target is not watched for dirty tracking (the guest never authors its bytes).
    assert!(
        !dirty.is_watched(0x2000, 0x10),
        "render target range is not dirty-watched"
    );

    // Clean reuse: same id, no commands.
    let mut cmds = Vec::new();
    let id2 = cache.get_render_target(k, surface, ColorFormat::R8G8B8A8Unorm, &mut cmds);
    assert_eq!(id2, id, "same id on reuse");
    assert!(cmds.is_empty(), "clean render-target reuse emits nothing");
}

#[test]
fn rt_and_texture_over_same_range_are_two_entries_and_rt_skips_drain_dirty() {
    // AC #1 (cache half): a RenderTarget key and a Texture key over the SAME (addr, size)
    // yield two DISTINCT ids (the split key). A guest write to that range dirties the
    // texture entry (re-uploaded on next use) but NEVER the render target — drain_dirty
    // skips is_rt entries, so the RT stays clean (no create/upload on next use).
    let mem = BufMem::new(0x4000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();

    let (rt_k, rt_surface) = rt_key(0x2000, 2, 2);
    let (tex_k, tex_surface) = tex_key(0x2000, 2, 2, Tiling::LinearGeneral);
    // Same backing (addr, size), different layout kind → distinct keys.
    assert_eq!(rt_k.addr, tex_k.addr);
    assert_eq!(rt_k.size, tex_k.size);
    assert_ne!(rt_k.layout, tex_k.layout, "RT vs Texture layout differ");

    let mut cmds = Vec::new();
    let rt_id = cache.get_render_target(rt_k, rt_surface, ColorFormat::R8G8B8A8Unorm, &mut cmds);
    let tex_id = cache.get_texture(
        tex_k,
        tex_surface,
        TextureFormat::R8G8B8A8Unorm,
        &mem,
        &dirty,
        &mut cmds,
    );
    assert_ne!(
        rt_id, tex_id,
        "RT and texture over the same range get two ids"
    );

    // Guest write to the shared range, then drain: the texture entry goes dirty; the RT
    // does not (is_rt skipped, like imported).
    dirty.stage_write(0x2000, 0x10);
    cache.drain_dirty(&dirty);

    // Next texture use → exactly one UploadImage (dirty re-upload), no CreateImage.
    let mut tex_cmds = Vec::new();
    let tex_id2 = cache.get_texture(
        tex_k,
        tex_surface,
        TextureFormat::R8G8B8A8Unorm,
        &mem,
        &dirty,
        &mut tex_cmds,
    );
    assert_eq!(tex_id2, tex_id);
    let tc = count_images(&tex_cmds);
    assert_eq!(
        tc,
        (0, 1),
        "guest write → texture re-uploads once (0 creates, 1 upload)"
    );

    // Next RT use → clean hit: no commands at all (drain_dirty left it clean).
    let mut rt_cmds = Vec::new();
    let rt_id2 =
        cache.get_render_target(rt_k, rt_surface, ColorFormat::R8G8B8A8Unorm, &mut rt_cmds);
    assert_eq!(rt_id2, rt_id, "RT same id on reuse");
    assert!(
        rt_cmds.is_empty(),
        "a guest write does not invalidate a render target"
    );
}

/// Count `(CreateImage, UploadImage)` commands in a list (the image analogue of `count`).
fn count_images(cmds: &[BackendCmd]) -> (u32, u32) {
    let mut creates = 0;
    let mut uploads = 0;
    for c in cmds {
        match c {
            BackendCmd::CreateImage { .. } => creates += 1,
            BackendCmd::UploadImage { .. } => uploads += 1,
            _ => {}
        }
    }
    (creates, uploads)
}

/// Over-budget eviction (task-223): the least-recently-used linear buffer entry is dropped
/// with a `FreeResource`, the next `get` for its key re-creates and re-uploads, and — the
/// part that matters for correctness — the guest range stays WATCHED.
///
/// Dirty tracking is page-granular. Unwatching an evicted entry's range would drop the
/// write protection for every page it shares with entries that are still live, and a ring's
/// windows all share pages, as do the small per-frame constant buffers. Their rewrites
/// would then go unseen and the cache would serve them as clean, stale hits — which is
/// exactly the frame corruption an earlier revision of this eviction produced.
#[test]
fn over_budget_evict_frees_the_entry_but_keeps_the_range_watched() {
    let mem = BufMem::new(0x2000);
    let dirty = MockDirty::default();
    let mut cache = ResourceCache::new();
    let evicted = ResourceKey {
        addr: 0x100,
        size: 64,
        layout: ResLayout::IndexBuf,
    };
    let (id1, cmds) = get_cmds(&mut cache, evicted, &mem, &dirty);
    assert_eq!(count(&cmds).creates, 1, "first use creates");
    assert!(dirty.is_watched(0x100, 64), "first use watches the range");

    // Put the cache over budget and move the flip counter, so the next `get` trims.
    cache.buffer_bytes = BUFFER_BUDGET_BYTES + 1;
    ps4_core::clock::advance_frame();

    let mut out = Vec::new();
    let other = ResourceKey {
        addr: 0x400,
        size: 64,
        layout: ResLayout::IndexBuf,
    };
    cache.get(other, &mem, &dirty, &mut out);
    assert!(
        out.iter()
            .any(|c| matches!(c, BackendCmd::FreeResource { id } if *id == id1)),
        "the over-budget entry is freed"
    );
    assert!(
        dirty.is_watched(0x100, 64),
        "eviction must NOT unwatch: the guest still owns and writes the range"
    );

    // Re-asking for the evicted key rebuilds it from current guest bytes under a fresh id.
    let (id2, again) = get_cmds(&mut cache, evicted, &mem, &dirty);
    assert_ne!(id2, id1, "an evicted key mints a new id, never a stale one");
    assert_eq!(
        count(&again),
        CmdCounts {
            creates: 1,
            uploads: 1,
            imports: 0
        },
        "re-create re-uploads the current guest bytes"
    );
}
