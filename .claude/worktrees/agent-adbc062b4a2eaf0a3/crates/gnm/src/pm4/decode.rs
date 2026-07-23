//! PM4 header walk → [`Pm4Packet`] stream (doc-4 §1, §3). Decode-only, no
//! execution, no Vulkan.
//!
//! A PM4 command buffer is a stream of 32-bit little-endian dwords. Each dword
//! that begins a packet is a header whose top two bits select the packet type:
//!
//! * **Type-3** (`0b11`) — the interesting one: opcode in [15:8], count in
//!   [29:16]; the body is `count + 1` dwords.
//! * **Type-0** (`0b00`) — a register write run: count in [29:16], base register
//!   index in [15:0]; the body is `count + 1` dwords.
//! * **Type-2** (`0b10`) — a filler NOP; header only, no body.
//! * **Type-1** (`0b01`) — reserved / unused on GFX; treated as an opaque stop.
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

/// Extract the type field (bits [31:30]) of a header dword.
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
                // Filler NOP: header only, advance one dword.
                self.pos = body_start;
                Some(Pm4Packet::Type2)
            }
            Pm4Type::Type1 => {
                // Reserved / unused on GFX — nothing sane to skip by. Stop.
                self.done = true;
                Some(Pm4Packet::Truncated { header })
            }
            ty => {
                // Type-0 and Type-3 share the count field: bits [29:16], body = count + 1.
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
                        let opcode = ((header >> 8) & 0xFF) as u8;
                        Some(Pm4Packet::Type3 {
                            opcode,
                            count: body_len as u16,
                            body,
                        })
                    }
                    _ => {
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
