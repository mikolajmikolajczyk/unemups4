//! Shader-source abstraction (doc-4 §1, §4): turns a shader reference seen in the
//! PM4 stream into a host pipeline shader, behind the `ShaderProvider` seam.
//!
//! `EmbeddedShaderProvider` (phase 3.5) is wired; `sb` is the phase-4 `.sb`
//! (OrbShdr) container parser — header + semantic tables, no GCN decode; `gcn` is the
//! phase-4 `GcnShaderProvider` (`.sb` → recompiled SPIR-V), chained after embedded.

pub mod embedded;
pub mod fetch;
pub mod gcn;
pub mod pipeline_cache;
pub mod sb;
pub mod source;
