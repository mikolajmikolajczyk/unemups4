//! PM4 header walk → [`Pm4Packet`] stream (doc-2 §1, §3). Decode-only, no
//! execution, no Vulkan.
//!
//! A PM4 command buffer is a stream of 32-bit little-endian dwords. Each dword
//! that begins a packet is a header whose top two bits select the packet type.
//! The header field layout is the AMD CP packet format published as the Mesa
//! `src/amd/common/sid.h` `PKT_*` accessors — `PKT_TYPE_G(x) = (x >> 30) & 0x3`,
//! `PKT_COUNT_G(x) = (x >> 16) & 0x3FFF`, `PKT3_IT_OPCODE_G(x) = (x >> 8) & 0xFF`
//! — corroborated by the Linux kernel `drivers/gpu/drm/radeon/cikd.h`
//! (`CP_PACKET_GET_TYPE`/`_COUNT`/`CP_PACKET3_GET_OPCODE`/`CP_PACKET0_GET_REG`).
//! Pinned by `pm4_header_fields_match_amd_oracle` below.
//!
//! * **Type-3** (`0b11`, kernel `PACKET_TYPE3`) — the interesting one: opcode in
//!   [15:8], count in [29:16]; the body is `count + 1` dwords (Mesa `PKT3(op,
//!   count, pred)` packs `count = body_len - 1`, kernel `PACKET3(op, n)`).
//! * **Type-0** (`0b00`, kernel `PACKET_TYPE0`) — a register write run: count in
//!   [29:16], base register index in [15:0]; the body is `count + 1` dwords
//!   (kernel `PACKET0(reg, n)` packs `(reg >> 2) & 0xFFFF` in [15:0]).
//! * **Type-2** (`0b10`, kernel `PACKET_TYPE2`) — a filler NOP; header only, no
//!   body (Mesa `PKT2_NOP_PAD = PKT_TYPE_S(2)`, kernel `CP_PACKET2 = 0x80000000`).
//! * **Type-1** (`0b01`, kernel `PACKET_TYPE1`) — reserved / unused on GFX;
//!   treated as an opaque stop.
//!
//! The guest data is untrusted, so every dword read is bounds-checked against the
//! slice; a header that claims a body longer than what remains stops the walk
//! (recorded as [`Pm4Packet::Truncated`]) rather than panicking.

/// Type field of a PM4 header (bits [31:30]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pm4Type {
    /// Register-write run (base index + `count + 1` dwords).
    Type0,
    /// Reserved / unused on GFX.
    Type1,
    /// Filler NOP (header only).
    Type2,
    /// Command packet (opcode + `count + 1` body dwords).
    Type3,
}

/// One decoded PM4 packet. Bodies borrow the source command buffer (no copy);
/// this is a pure view, nothing here is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pm4Packet<'a> {
    /// A Type-3 command packet.
    Type3 {
        /// IT_* opcode (bits [15:8] of the header).
        opcode: u8,
        /// Body length in dwords (`header count + 1`).
        count: u16,
        /// The `count` body dwords (excludes the header).
        body: &'a [u32],
    },
    /// A Type-0 register-write run.
    Type0 {
        /// Base register index (bits [15:0] of the header).
        base_index: u16,
        /// Body length in dwords (`header count + 1`).
        count: u16,
        /// The `count` register-value dwords (excludes the header).
        body: &'a [u32],
    },
    /// A Type-2 filler NOP (header only, no body).
    Type2,
    /// A malformed or truncated packet: the header claimed more dwords than the
    /// buffer had left. Carries the raw header so a trace can show it. The walk
    /// stops after yielding this (untrusted input → never panic).
    Truncated {
        /// The offending header dword.
        header: u32,
    },
}

/// Extract the type field (bits [31:30]) of a header dword. The `>> 30` and the
/// TYPE0..TYPE3 = 0..3 enumeration are the AMD CP packet-type field: Mesa
/// `src/amd/common/sid.h` `PKT_TYPE_G(x) = (x >> 30) & 0x3` (kernel
/// `drivers/gpu/drm/radeon/cikd.h` `CP_PACKET_GET_TYPE` + `PACKET_TYPE0..3`).
pub fn header_type(header: u32) -> Pm4Type {
    match header >> 30 {
        0 => Pm4Type::Type0,
        1 => Pm4Type::Type1,
        2 => Pm4Type::Type2,
        _ => Pm4Type::Type3,
    }
}

/// A decode-only walk over a PM4 command buffer given as a dword slice.
///
/// The identity mapping (guest ptr == host ptr, doc-2 §1) lets callers build
/// this slice straight from guest memory with no translation. Iteration yields
/// one [`Pm4Packet`] per packet and ends when the buffer is exhausted or a
/// truncated/unusable header is hit (after yielding [`Pm4Packet::Truncated`]).
pub struct Decoder<'a> {
    words: &'a [u32],
    pos: usize,
    done: bool,
}

impl<'a> Decoder<'a> {
    /// Start a walk over `words` (a command buffer as dwords).
    pub fn new(words: &'a [u32]) -> Self {
        Self {
            words,
            pos: 0,
            done: false,
        }
    }
}

impl<'a> Iterator for Decoder<'a> {
    type Item = Pm4Packet<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.pos >= self.words.len() {
            return None;
        }
        let header = self.words[self.pos];
        let body_start = self.pos + 1;

        match header_type(header) {
            Pm4Type::Type2 => {
                // Filler NOP: header only, advance one dword (Mesa sid.h
                // `PKT2_NOP_PAD = PKT_TYPE_S(2)`; kernel cikd.h `CP_PACKET2 = 0x80000000`).
                self.pos = body_start;
                Some(Pm4Packet::Type2)
            }
            Pm4Type::Type1 => {
                // Reserved / unused on GFX — nothing sane to skip by. Stop.
                self.done = true;
                Some(Pm4Packet::Truncated { header })
            }
            ty => {
                // Type-0 and Type-3 share the count field: bits [29:16], body = count + 1
                // (Mesa sid.h `PKT_COUNT_G(x) = (x >> 16) & 0x3FFF`; kernel cikd.h
                // `PACKET3(op, n)` / `PACKET0(reg, n)` pack `n = body_len - 1` at <<16).
                let count = ((header >> 16) & 0x3FFF) as u16;
                let body_len = count as usize + 1;
                let body_end = match body_start.checked_add(body_len) {
                    Some(end) if end <= self.words.len() => end,
                    _ => {
                        self.done = true;
                        return Some(Pm4Packet::Truncated { header });
                    }
                };
                let body = &self.words[body_start..body_end];
                self.pos = body_end;
                match ty {
                    Pm4Type::Type3 => {
                        // Opcode in [15:8] (Mesa sid.h `PKT3_IT_OPCODE_G(x) =
                        // (x >> 8) & 0xFF`; kernel cikd.h `CP_PACKET3_GET_OPCODE`).
                        let opcode = ((header >> 8) & 0xFF) as u8;
                        Some(Pm4Packet::Type3 {
                            opcode,
                            count: body_len as u16,
                            body,
                        })
                    }
                    _ => {
                        // Type-0 register base index in [15:0] (kernel cikd.h
                        // `CP_PACKET0_GET_REG(h) = ((h) & 0xFFFF) << 2`; the field
                        // holds the dword register index, `<< 2` recovers the byte addr).
                        let base_index = (header & 0xFFFF) as u16;
                        Some(Pm4Packet::Type0 {
                            base_index,
                            count: body_len as u16,
                            body,
                        })
                    }
                }
            }
        }
    }
}

/// Decode a command buffer given as a dword slice — the entry used by unit tests
/// and any caller that already has the buffer as `&[u32]`.
pub fn decode(words: &[u32]) -> Decoder<'_> {
    Decoder::new(words)
}

/// Reinterpret a byte slice as dwords (little-endian) and decode it. The tail of
/// any non-multiple-of-4 buffer is ignored. Returns the decoded packets as an
/// owned `Vec` because the transient dword buffer cannot be borrowed out.
pub fn decode_bytes(bytes: &[u8]) -> Vec<OwnedPacket> {
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    decode(&words).map(OwnedPacket::from).collect()
}

/// An owned copy of a [`Pm4Packet`] (body cloned), for callers that cannot hold a
/// borrow of the source buffer — e.g. [`decode_bytes`] or the SubmitRange walk,
/// where the dword buffer is transient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedPacket {
    Type3 {
        opcode: u8,
        count: u16,
        body: Vec<u32>,
    },
    Type0 {
        base_index: u16,
        count: u16,
        body: Vec<u32>,
    },
    Type2,
    Truncated {
        header: u32,
    },
}

impl From<Pm4Packet<'_>> for OwnedPacket {
    fn from(p: Pm4Packet<'_>) -> Self {
        match p {
            Pm4Packet::Type3 {
                opcode,
                count,
                body,
            } => OwnedPacket::Type3 {
                opcode,
                count,
                body: body.to_vec(),
            },
            Pm4Packet::Type0 {
                base_index,
                count,
                body,
            } => OwnedPacket::Type0 {
                base_index,
                count,
                body: body.to_vec(),
            },
            Pm4Packet::Type2 => OwnedPacket::Type2,
            Pm4Packet::Truncated { header } => OwnedPacket::Truncated { header },
        }
    }
}

/// The dwords of one guest command buffer, borrowed in place whenever the buffer
/// permits it.
///
/// A retail submit is megabytes of PM4 per flip, so copying it out is the single most
/// expensive thing that can be done with it. The buffer is identity-mapped (guest ptr ==
/// host ptr, doc-2 §1), so a 4-byte-aligned one can be reinterpreted where it lies. An
/// unaligned pointer falls back to the unaligned dword copy the old path always made —
/// same native-endian reinterpretation, just materialized.
pub enum GuestWords<'a> {
    /// The guest buffer itself, reinterpreted in place.
    InPlace(&'a [u32]),
    /// A dword copy, for buffers that cannot be reinterpreted.
    Copied(Vec<u32>),
}

impl std::ops::Deref for GuestWords<'_> {
    type Target = [u32];

    fn deref(&self) -> &[u32] {
        match self {
            GuestWords::InPlace(w) => w,
            GuestWords::Copied(w) => w,
        }
    }
}

/// Borrow a guest command buffer as dwords. `ptr` is the guest/host address, `size` the
/// buffer size in **bytes**; a non-multiple-of-4 tail is ignored, exactly as the copying
/// path always did. A null pointer or a sub-dword size yields an empty view.
///
/// # Safety
/// `ptr..ptr+size` must be a readable, initialized guest command-buffer region for the
/// lifetime `'a` of the returned view.
pub unsafe fn guest_words<'a>(ptr: u64, size: u32) -> GuestWords<'a> {
    if ptr == 0 || size < 4 {
        return GuestWords::InPlace(&[]);
    }
    let word_count = (size / 4) as usize;
    // The in-place fast path reinterprets guest memory with a bare `from_raw_parts` — no
    // per-element check — so it is sound only when the whole `[ptr, ptr + word_count*4)` span
    // stays inside the mapped guest region. Validate that span the same way `read_array`'s
    // arena guard (2) does (ps4-core `memory.rs`): an over-reported `dcb_size`, or a buffer
    // near the top of an Onion/Garlic mapping, would otherwise let the decoder read past the
    // mapping (OOB host read / SIGSEGV) or over-read adjacent guest memory as PM4. When the
    // start is arena-resident the span end must be at/under the arena top; when no arena is
    // registered or the start lies outside it we defer (headless / unit tests have no arena),
    // exactly as `read_array` guard (2) defers.
    let span_in_bounds = match ps4_core::kernel::arena_bounds() {
        Some((base, end)) if ptr >= base && ptr < end => ptr
            .checked_add(word_count as u64 * 4)
            .is_some_and(|span_end| span_end <= end),
        _ => true,
    };
    if span_in_bounds && ptr.is_multiple_of(4) {
        return GuestWords::InPlace(unsafe {
            std::slice::from_raw_parts(ptr as *const u32, word_count)
        });
    }
    // Unaligned, or an out-of-bounds span the fast path must not reinterpret: fall back to the
    // bounded dword copy. `read_array` re-applies the same arena guard and returns `Err` (→
    // empty) for an over-reported span, so an oversized aligned buffer yields an empty view
    // instead of an unchecked over-read.
    use ps4_core::memory::MemoryAccessExt;
    GuestWords::Copied(
        crate::idmem::IdentityMem
            .read_array::<u32>(ptr, word_count)
            .unwrap_or_default(),
    )
}

/// Read a command buffer straight out of guest memory and decode it (doc-2 §1:
/// identity-mapped, guest ptr == host ptr). `ptr` is the guest/host address and
/// `size` is the buffer size in **bytes** — exactly the pair a
/// [`crate::driver::SubmitRange`] carries.
///
/// # Safety
/// `ptr..ptr+size` must be a readable, initialized guest command-buffer region
/// for the duration of the call. Callers that hold a `SubmitRange` from a live
/// guest submission satisfy this; a null pointer or zero size decodes to nothing.
pub unsafe fn decode_guest(ptr: u64, size: u32) -> Vec<OwnedPacket> {
    use ps4_core::memory::MemoryAccessExt;
    if ptr == 0 || size < 4 {
        return Vec::new();
    }
    let word_count = (size / 4) as usize;
    // Identity-mapped read (guest ptr == host ptr); the array is unaligned-safe.
    let words = crate::idmem::IdentityMem
        .read_array::<u32>(ptr, word_count)
        .unwrap_or_default();
    decode(&words).map(OwnedPacket::from).collect()
}

/// Decode both command buffers a [`crate::driver::SubmitRange`] points at (DCB,
/// then CCB when present). See [`decode_guest`] for the safety contract.
///
/// # Safety
/// Same as [`decode_guest`], for both the DCB and CCB ranges.
pub unsafe fn decode_submit_range(range: &crate::driver::SubmitRange) -> Vec<OwnedPacket> {
    let mut packets = unsafe { decode_guest(range.dcb_ptr, range.dcb_size) };
    if range.ccb_ptr != 0 && range.ccb_size >= 4 {
        packets.extend(unsafe { decode_guest(range.ccb_ptr, range.ccb_size) });
    }
    packets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm4::opcodes;
    use crate::pm4::opcodes::op;
    use crate::pm4::opcodes::t3_header;

    /// A Type-2 filler NOP header (type=2, no body).
    fn t2_header() -> u32 {
        0b10 << 30
    }

    #[test]
    fn header_type_field() {
        assert_eq!(header_type(0x0000_0000), Pm4Type::Type0);
        assert_eq!(header_type(0x4000_0000), Pm4Type::Type1);
        assert_eq!(header_type(0x8000_0000), Pm4Type::Type2);
        assert_eq!(header_type(0xC000_0000), Pm4Type::Type3);
    }

    /// Pins the PM4 header field extraction to its AMD hardware layout. The
    /// right-hand literals are the AMD CP packet accessors: Mesa
    /// `src/amd/common/sid.h` (`PKT_TYPE_G(x) = (x>>30)&0x3`,
    /// `PKT_COUNT_G(x) = (x>>16)&0x3FFF`, `PKT3_IT_OPCODE_G(x) = (x>>8)&0xFF`,
    /// `PKT2_NOP_PAD = PKT_TYPE_S(2)`), corroborated by the Linux kernel
    /// `drivers/gpu/drm/radeon/cikd.h` (`PACKET_TYPE0..3 = 0..3`,
    /// `CP_PACKET_GET_TYPE`/`_COUNT`, `CP_PACKET3_GET_OPCODE`,
    /// `CP_PACKET0_GET_REG(h) = ((h)&0xFFFF)<<2`, `CP_PACKET2 = 0x80000000`).
    /// This test fails if our decode drifts from those AMD definitions.
    #[test]
    // The `0 <<` shifts spell out the zero-valued header fields (PACKET0 type, PACKET3
    // predicate/shader-type) exactly as the AMD PACKET macros write them, so the bit
    // positions stay legible against the cited oracle; keep them literal.
    #[allow(clippy::identity_op)]
    fn pm4_header_fields_match_amd_oracle() {
        // Type field in [31:30]: AMD `PACKET_TYPE0..3` = 0..3.
        assert_eq!(header_type(0 << 30), Pm4Type::Type0);
        assert_eq!(header_type(1 << 30), Pm4Type::Type1);
        assert_eq!(header_type(2 << 30), Pm4Type::Type2);
        assert_eq!(header_type(3 << 30), Pm4Type::Type3);

        // A Type-2 header is header-only: kernel `CP_PACKET2 = 0x80000000`
        // (= PKT_TYPE_S(2)). It must decode to a body-less Type2 packet.
        assert_eq!(header_type(0x8000_0000), Pm4Type::Type2);
        assert_eq!(
            decode(&[0x8000_0000u32]).collect::<Vec<_>>(),
            vec![Pm4Packet::Type2]
        );

        // Type-3: opcode in [15:8], count in [29:16], body = count + 1. Build a
        // header the AMD way — `PACKET3(op, n) = (3<<30) | ((op&0xFF)<<8) | ((n&0x3FFF)<<16)`
        // with op = 0x37 (WRITE_DATA), n = body_len - 1 = 2 → body of 3 dwords.
        let op3 = 0x37u32;
        let n3 = 2u32; // count field = body_len - 1
        let h3 = (3 << 30) | ((op3 & 0xFF) << 8) | ((n3 & 0x3FFF) << 16);
        let buf3 = [h3, 0xA, 0xB, 0xC];
        match decode(&buf3).next().unwrap() {
            Pm4Packet::Type3 {
                opcode,
                count,
                body,
            } => {
                assert_eq!(opcode as u32, op3); // (h>>8)&0xFF
                assert_eq!(count, 3); // body_len = count field + 1
                assert_eq!(body, &[0xA, 0xB, 0xC]);
            }
            other => panic!("expected Type3, got {other:?}"),
        }

        // Type-0: base register index in [15:0], count in [29:16], body = count + 1.
        // Build it the AMD way — `PACKET0(reg, n) = (0<<30) | ((reg>>2)&0xFFFF) | ((n&0x3FFF)<<16)`.
        // Field holds the dword index; kernel `CP_PACKET0_GET_REG` recovers it as `(h&0xFFFF)`.
        let reg_dword = 0x00C0u32; // the [15:0] field value
        let n0 = 1u32; // count field = body_len - 1
        let h0 = (0 << 30) | (reg_dword & 0xFFFF) | ((n0 & 0x3FFF) << 16);
        let buf0 = [h0, 0x1111_1111, 0x2222_2222];
        match decode(&buf0).next().unwrap() {
            Pm4Packet::Type0 {
                base_index,
                count,
                body,
            } => {
                assert_eq!(base_index as u32, reg_dword); // h & 0xFFFF
                assert_eq!(count, 2); // body_len = count field + 1
                assert_eq!(body, &[0x1111_1111, 0x2222_2222]);
            }
            other => panic!("expected Type0, got {other:?}"),
        }
    }

    #[test]
    fn decodes_nop_setctx_draw_sequence() {
        // NOP with a 2-dword body, then SET_CONTEXT_REG (reg off + 1 value),
        // then DRAW_INDEX_AUTO (index_count + flags).
        let mut buf = Vec::new();
        buf.push(t3_header(op::IT_NOP, 2));
        buf.extend([0xAAAA_AAAA, 0xBBBB_BBBB]);
        buf.push(t3_header(op::IT_SET_CONTEXT_REG, 2));
        buf.extend([0x0000_0010, 0x1234_5678]); // reg offset 0x10, value
        buf.push(t3_header(op::IT_DRAW_INDEX_AUTO, 2));
        buf.extend([3, 0]);

        let packets: Vec<_> = decode(&buf).collect();
        assert_eq!(packets.len(), 3);

        match packets[0] {
            Pm4Packet::Type3 {
                opcode,
                count,
                body,
            } => {
                assert_eq!(opcode, op::IT_NOP);
                assert_eq!(count, 2);
                assert_eq!(body, &[0xAAAA_AAAA, 0xBBBB_BBBB]);
            }
            other => panic!("expected NOP, got {other:?}"),
        }
        match packets[1] {
            Pm4Packet::Type3 { opcode, body, .. } => {
                assert_eq!(opcode, op::IT_SET_CONTEXT_REG);
                assert_eq!(body, &[0x0000_0010, 0x1234_5678]);
            }
            other => panic!("expected SET_CONTEXT_REG, got {other:?}"),
        }
        match packets[2] {
            Pm4Packet::Type3 { opcode, count, .. } => {
                assert_eq!(opcode, op::IT_DRAW_INDEX_AUTO);
                assert_eq!(count, 2);
            }
            other => panic!("expected DRAW_INDEX_AUTO, got {other:?}"),
        }
    }

    #[test]
    fn unknown_opcode_is_skipped_not_fatal() {
        // Unknown opcode 0xEE with a 1-dword body, followed by a known NOP.
        // The walk must yield both — the unknown one is skipped by its count.
        let unknown = 0xEE;
        assert!(opcodes::name(unknown).is_none());
        let buf = vec![
            t3_header(unknown, 1),
            0xDEAD_BEEF,
            t3_header(op::IT_NOP, 1),
            0x0000_0001,
        ];

        let packets: Vec<_> = decode(&buf).collect();
        assert_eq!(packets.len(), 2);
        assert!(matches!(
            packets[0],
            Pm4Packet::Type3 {
                opcode: 0xEE,
                count: 1,
                ..
            }
        ));
        assert!(matches!(
            packets[1],
            Pm4Packet::Type3 { opcode, .. } if opcode == op::IT_NOP
        ));
    }

    #[test]
    fn truncated_body_stops_gracefully() {
        // Header claims a 4-dword body but only 1 dword follows: no panic, yields
        // Truncated, and stops.
        let buf = [t3_header(op::IT_SET_SH_REG, 4), 0x0000_0001];
        let packets: Vec<_> = decode(&buf).collect();
        assert_eq!(packets.len(), 1);
        assert!(matches!(packets[0], Pm4Packet::Truncated { .. }));
    }

    #[test]
    fn type2_nop_is_header_only() {
        let buf = [t2_header(), t3_header(op::IT_NOP, 1), 0x42];
        let packets: Vec<_> = decode(&buf).collect();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0], Pm4Packet::Type2);
        assert!(matches!(packets[1], Pm4Packet::Type3 { .. }));
    }

    #[test]
    fn type0_register_run() {
        // Type-0: base_index in [15:0], count in [29:16], body = count + 1.
        let count = 1u32; // body_len 2
        let base_index = 0x00C0u32;
        let header = (count << 16) | base_index;
        let buf = [header, 0x1111_1111, 0x2222_2222];
        let packets: Vec<_> = decode(&buf).collect();
        assert_eq!(packets.len(), 1);
        match packets[0] {
            Pm4Packet::Type0 {
                base_index: b,
                count: c,
                body,
            } => {
                assert_eq!(b, 0x00C0);
                assert_eq!(c, 2);
                assert_eq!(body, &[0x1111_1111, 0x2222_2222]);
            }
            other => panic!("expected Type0, got {other:?}"),
        }
    }

    #[test]
    fn empty_buffer_yields_nothing() {
        assert_eq!(decode(&[]).count(), 0);
    }

    #[test]
    fn decode_bytes_matches_words() {
        let words = [t3_header(op::IT_NOP, 1), 0xCAFE_BABE];
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let packets = decode_bytes(&bytes);
        assert_eq!(packets.len(), 1);
        assert_eq!(
            packets[0],
            OwnedPacket::Type3 {
                opcode: op::IT_NOP,
                count: 1,
                body: vec![0xCAFE_BABE],
            }
        );
    }

    #[test]
    fn decode_guest_reads_identity_mapped_buffer() {
        // Identity mapping: the host slice's address IS the guest pointer.
        let words = [t3_header(op::IT_DRAW_INDEX_AUTO, 1), 3u32];
        let ptr = words.as_ptr() as u64;
        let size = (words.len() * 4) as u32;
        let packets = unsafe { decode_guest(ptr, size) };
        assert_eq!(packets.len(), 1);
        assert!(matches!(
            packets[0],
            OwnedPacket::Type3 { opcode, .. } if opcode == op::IT_DRAW_INDEX_AUTO
        ));
    }

    #[test]
    fn guest_words_borrows_the_buffer_in_place() {
        let words = [t3_header(op::IT_NOP, 1), 0xCAFE_BABE];
        let ptr = words.as_ptr() as u64;
        let view = unsafe { guest_words(ptr, (words.len() * 4) as u32) };
        assert!(matches!(view, GuestWords::InPlace(_)));
        assert_eq!(view.as_ptr() as u64, ptr, "no copy was made");
        assert_eq!(&*view, &words[..]);

        // And the decoded bodies point into the source buffer, not into a clone.
        let packets: Vec<_> = decode(&view).collect();
        match packets[0] {
            Pm4Packet::Type3 { body, .. } => {
                assert_eq!(body.as_ptr() as u64, ptr + 4);
            }
            other => panic!("expected Type3, got {other:?}"),
        }
    }

    #[test]
    fn guest_words_bounds_checks_the_aligned_in_place_span() {
        use ps4_core::kernel::{arena_bounds, set_arena_bounds};

        // A real, 4-aligned host buffer that doubles as the guest command buffer (identity
        // mapping). Register the arena to cover *exactly* this buffer so the in-place fast
        // path's span guard is exercised against a known arena top. Registering an arena that
        // bounds only this still-live allocation is safe under parallel test execution — no
        // other live buffer can occupy an address inside it, so sibling tests' pointers are
        // never seen as arena-resident and their in-place behavior is unchanged.
        let words = [t3_header(op::IT_NOP, 1), 0xCAFE_BABE_u32];
        let base = words.as_ptr() as u64;
        let bytes = (words.len() * 4) as u32;
        set_arena_bounds(base, bytes as u64);
        assert_eq!(arena_bounds(), Some((base, base + bytes as u64)));

        // The honest size — an in-bounds span — is still borrowed in place, zero-copy. The
        // guard must not penalize the valid common case.
        let ok = unsafe { guest_words(base, bytes) };
        assert!(matches!(ok, GuestWords::InPlace(_)));
        assert_eq!(ok.as_ptr() as u64, base, "no copy for the in-bounds span");
        assert_eq!(&*ok, &words[..]);

        // An over-reported size whose span runs one dword past the arena top must NOT be
        // reinterpreted in place (that unchecked `from_raw_parts` would let the decoder
        // over-read past the mapping → OOB host read / adjacent memory parsed as PM4). It falls
        // back to the bounded copy, which rejects the same span and yields an empty view.
        let over = unsafe { guest_words(base, bytes + 4) };
        assert!(
            matches!(over, GuestWords::Copied(_)),
            "an over-reported aligned span must not be borrowed in place"
        );
        assert!(over.is_empty(), "the out-of-bounds span reads nothing");

        // Restore the process-global arena so sibling tests see it unset again.
        set_arena_bounds(0, 0);
    }

    #[test]
    fn guest_words_copies_an_unaligned_buffer() {
        // A misaligned start cannot be reinterpreted, so it falls back to the copy —
        // yielding the same dwords the old path produced.
        let bytes: Vec<u8> = (0u8..=15).collect();
        let ptr = bytes.as_ptr() as u64 + 1;
        let view = unsafe { guest_words(ptr, 8) };
        assert!(matches!(view, GuestWords::Copied(_)));
        assert_eq!(
            &*view,
            &[
                u32::from_le_bytes([1, 2, 3, 4]),
                u32::from_le_bytes([5, 6, 7, 8])
            ]
        );
    }

    #[test]
    fn guest_words_ignores_a_non_multiple_of_four_tail() {
        let words = [0x1111_1111u32, 0x2222_2222, 0x3333_3333];
        let ptr = words.as_ptr() as u64;
        // 11 bytes: two whole dwords plus a 3-byte tail that must be dropped.
        let view = unsafe { guest_words(ptr, 11) };
        assert_eq!(&*view, &words[..2]);
    }

    #[test]
    fn guest_words_null_or_tiny_is_empty() {
        assert!(unsafe { guest_words(0, 64) }.is_empty());
        assert!(unsafe { guest_words(0x1000, 3) }.is_empty());
    }

    #[test]
    fn decode_guest_null_or_tiny_is_empty() {
        assert!(unsafe { decode_guest(0, 64) }.is_empty());
        assert!(unsafe { decode_guest(0x1000, 3) }.is_empty());
    }

    #[test]
    fn decode_submit_range_walks_dcb_and_ccb() {
        let dcb = [t3_header(op::IT_NOP, 1), 0u32];
        let ccb = [t3_header(op::IT_SET_SH_REG, 2), 0x4u32, 0x9u32];
        let range = crate::driver::SubmitRange {
            dcb_ptr: dcb.as_ptr() as u64,
            dcb_size: (dcb.len() * 4) as u32,
            ccb_ptr: ccb.as_ptr() as u64,
            ccb_size: (ccb.len() * 4) as u32,
            flip: false,
            vo_handle: 0,
            buf_idx: 0,
        };
        let packets = unsafe { decode_submit_range(&range) };
        assert_eq!(packets.len(), 2);
        assert!(matches!(
            packets[0],
            OwnedPacket::Type3 { opcode, .. } if opcode == op::IT_NOP
        ));
        assert!(matches!(
            packets[1],
            OwnedPacket::Type3 { opcode, .. } if opcode == op::IT_SET_SH_REG
        ));
    }
}
