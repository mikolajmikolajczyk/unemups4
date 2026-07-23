//! ps4-gnm — the Gnm command processor + GPU state model + shader-source
//! abstraction (doc-4 §1). Pure, Vulkan-FREE, headless-testable.
//!
//! Scaffolding only: the module tree from doc-4 §1 exists and compiles,
//! but no PM4 decode, executor, state, cache or shader logic is wired yet. Each
//! module grows in its own phase (doc-4 §7). The `ShaderProvider`/`ResourceCache`/
//! `DirtySource` seams are declared here as trait stubs so `PipelineKey`/
//! `ResourceKey` and the backend's upload/import surface have a shape to target.

pub mod cache;
pub mod derive;
pub mod driver;
pub mod exec;
pub mod free_sink;
pub mod idmem;
pub mod pm4;
pub mod shader;
pub mod state;
pub mod vbuf;
