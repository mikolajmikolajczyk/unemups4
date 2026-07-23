//! Guest-side host-pipeline cache (doc-2 §4, decision-7): maps a [`PipelineKey`] to a
//! guest-minted [`PipelineId`], mirroring the resource cache's id-ownership model.
//!
//! The executor runs on the guest thread (only a `&dyn PresentSink` reaches the
//! display side) while the sole `GpuBackend` lives on the display thread across a
//! one-way channel (doc-2 §3). A fire-and-forget `BackendCmd` cannot round-trip a
//! backend-minted handle back, so — exactly as for [`ResourceId`](ps4_core::gpu::ResourceId)
//! — pipeline ids are minted **guest-side** here from a monotonic counter and handed
//! into the backend via [`BackendCmd::CreatePipeline`]. The display thread records
//! `id -> vk::Pipeline`.
//!
//! On a **miss** the caller emits `CreatePipeline { id, .. }` (carrying the recompiled
//! SPIR-V once) then `BindPipeline { id }`; on a **hit** only `BindPipeline { id }`, so
//! the SPIR-V never crosses the channel twice for one pipeline.

use std::collections::HashMap;

use ps4_core::gpu::{PipelineId, PipelineKey};

/// A get-or-mint map from [`PipelineKey`] to guest-minted [`PipelineId`]. Owned by the
/// PM4 executor across submits so a re-bound pipeline resolves to the same id (and so
/// emits no second `CreatePipeline`).
#[derive(Debug, Default)]
pub struct PipelineCache {
    ids: HashMap<PipelineKey, PipelineId>,
    /// Guest-side monotonic id allocator: ids mint here, no backend round-trip.
    next_id: u32,
    /// How many distinct pipelines have been minted (== `CreatePipeline`s the caller
    /// emits). A test hook for the cache-keying AC — a re-bind of the same key must not
    /// bump this.
    created: u32,
}

/// The outcome of a [`PipelineCache::get_or_mint`] lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineLookup {
    /// The key was already cached; bind this id, emit no `CreatePipeline`.
    Hit(PipelineId),
    /// A freshly minted id; the caller must emit `CreatePipeline` before binding it.
    Miss(PipelineId),
}

impl PipelineCache {
    /// A fresh, empty cache. Ids start at 1 so 0 stays available as a sentinel, matching
    /// the resource cache.
    pub fn new() -> PipelineCache {
        PipelineCache {
            ids: HashMap::new(),
            next_id: 1,
            created: 0,
        }
    }

    /// Get the id for `key`, minting a new one on a miss. On a miss the returned
    /// [`PipelineLookup::Miss`] tells the caller to emit `CreatePipeline` before the
    /// bind; a hit reuses the recorded id and emits only the bind.
    pub fn get_or_mint(&mut self, key: PipelineKey) -> PipelineLookup {
        if let Some(&id) = self.ids.get(&key) {
            return PipelineLookup::Hit(id);
        }
        let id = PipelineId(self.next_id);
        self.next_id += 1;
        self.created += 1;
        self.ids.insert(key, id);
        PipelineLookup::Miss(id)
    }

    /// How many distinct pipelines have been minted (one `CreatePipeline` per). A test
    /// hook for the cache-keying AC (same [`PipelineKey`] → no second create).
    pub fn created_count(&self) -> u32 {
        self.created
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps4_core::gpu::{ColorFormat, PipelineKey};

    fn key(vs: u64, ps: u64) -> PipelineKey {
        PipelineKey {
            vs_hash: vs,
            ps_hash: ps,
            vertex_layout: None,
            color_format: ColorFormat::B8G8R8A8Unorm,
            ..Default::default()
        }
    }

    #[test]
    fn first_lookup_is_a_miss_with_id_1() {
        let mut cache = PipelineCache::new();
        // Independently reasoned expectation: the first mint hands out id 1 (ids start
        // at 1) and reports a miss so the caller emits CreatePipeline.
        assert_eq!(
            cache.get_or_mint(key(10, 20)),
            PipelineLookup::Miss(PipelineId(1))
        );
        assert_eq!(cache.created_count(), 1);
    }

    #[test]
    fn same_key_hits_without_a_second_create() {
        let mut cache = PipelineCache::new();
        let first = cache.get_or_mint(key(10, 20));
        assert_eq!(first, PipelineLookup::Miss(PipelineId(1)));
        // The same key returns the SAME id as a hit — and the create counter does NOT
        // move (the AC's no-second-create invariant, asserted against the reasoned
        // value 1, not a value the production path re-derived).
        assert_eq!(
            cache.get_or_mint(key(10, 20)),
            PipelineLookup::Hit(PipelineId(1))
        );
        assert_eq!(cache.created_count(), 1);
    }

    #[test]
    fn distinct_keys_mint_distinct_ids() {
        let mut cache = PipelineCache::new();
        assert_eq!(
            cache.get_or_mint(key(10, 20)),
            PipelineLookup::Miss(PipelineId(1))
        );
        // A different key mints the next id (2) and bumps the counter to 2.
        assert_eq!(
            cache.get_or_mint(key(11, 20)),
            PipelineLookup::Miss(PipelineId(2))
        );
        assert_eq!(cache.created_count(), 2);
    }

    #[test]
    fn same_shaders_different_resource_signature_both_miss() {
        use ps4_core::gpu::{ResourceSignature, ResourceSlot};
        let mut cache = PipelineCache::new();
        // Two draws with byte-identical shader hashes (and every other field) but a
        // DIFFERENT bound-resource layout: the vertex-fetch SSBO at a different binding.
        // Before slice 6 these produced a byte-identical key → the second would HIT the
        // first's (wrong-layout) pipeline and created_count would stay 1 (silent
        // wrong-reuse). With the ResourceSignature in the key they MUST both miss.
        let mut a = key(10, 20);
        a.resources = ResourceSignature {
            storage: Some(ResourceSlot { set: 0, binding: 0 }),
            ..Default::default()
        };
        let mut b = key(10, 20);
        b.resources = ResourceSignature {
            storage: Some(ResourceSlot { set: 0, binding: 1 }),
            ..Default::default()
        };
        assert_eq!(cache.get_or_mint(a), PipelineLookup::Miss(PipelineId(1)));
        assert_eq!(cache.get_or_mint(b), PipelineLookup::Miss(PipelineId(2)));
        assert_eq!(cache.created_count(), 2);
        // And each re-binds to its OWN id (no cross-contamination).
        assert_eq!(cache.get_or_mint(a), PipelineLookup::Hit(PipelineId(1)));
        assert_eq!(cache.get_or_mint(b), PipelineLookup::Hit(PipelineId(2)));
        assert_eq!(cache.created_count(), 2);
    }

    #[test]
    fn stride_is_out_of_the_key_one_pipeline_serves_all_strides() {
        // Stride lives in a SPIR-V push constant pushed per draw (task-140), so it is NOT
        // part of the key: the same shaders + same layout must HIT regardless of stride;
        // one pipeline serves every stride with no re-specialization. The key
        // carries only set/binding provenance (ResourceSlot has no stride), so two draws
        // that differ ONLY in vertex stride resolve to the SAME pipeline — asserted here
        // by construction: identical keys can't encode a stride difference.
        use ps4_core::gpu::{ResourceSignature, ResourceSlot};
        let mut cache = PipelineCache::new();
        let mut k = key(10, 20);
        k.resources = ResourceSignature {
            storage: Some(ResourceSlot { set: 0, binding: 0 }),
            ..Default::default()
        };
        assert_eq!(cache.get_or_mint(k), PipelineLookup::Miss(PipelineId(1)));
        // A second draw with a different vertex stride but the same layout: same key
        // (stride is not in it) → a HIT, one pipeline, created stays 1.
        assert_eq!(cache.get_or_mint(k), PipelineLookup::Hit(PipelineId(1)));
        assert_eq!(cache.created_count(), 1);
    }
}
