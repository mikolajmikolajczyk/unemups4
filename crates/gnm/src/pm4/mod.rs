//! PM4 command-stream decode + trace (doc-2 §1, §3). Decode-only, no execution;
//! takes the `ps4-core` memory trait so it is unit-testable with mock memory and
//! no GPU (doc-2 §6).
//!
//! TODO phase-2: Type-3 header walk → Packet stream + trace.

pub mod decode;
pub mod emit;
pub mod opcodes;
pub mod trace;
