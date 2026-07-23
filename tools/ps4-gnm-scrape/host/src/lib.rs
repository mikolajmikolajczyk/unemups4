//! Wire framing + zero-run RLE shared by the receiver and (mirrored in C) the
//! GoldHEN plugin. See `SETUP.md` for the on-the-wire contract.
//!
//! ## Frame layout (all fields little-endian)
//!
//! ```text
//! Header (20 bytes, packed):
//!   magic     u32   = 0x344D4E47  (wire bytes "GNM4")
//!   frame     u32   monotonic flip/submit counter (increments per submit call)
//!   kind      u8    0=submit 1=flip 2=submit_workload 3=flip_workload 4=vbuf
//!   buf_index u8    index of this buffer within the submit batch (0..count)
//!   is_ccb    u8    0=DCB 1=CCB
//!   flip      u8    1 if this submit call carries a flip (present_flip)
//!   raw_size  u32   decompressed payload length in bytes
//!   comp_size u32   RLE payload length in bytes (the bytes that follow)
//! then `comp_size` bytes of RLE payload.
//! ```
//!
//! ## KIND_VBUF (task-172 Phase 2 — referenced dynamic-buffer content)
//!
//! `kind == 4` (`Kind::Vbuf`) reuses the SAME 20-byte header and the SAME RLE payload
//! encoding as a DCB/CCB — the only difference is the payload SEMANTICS: for a VBUF the
//! de-RLE'd payload begins with an **8-byte little-endian u64 = the guest base address**
//! of the buffer, followed by the buffer content. So `raw_size == 8 + span`, `is_ccb == 0`,
//! `buf_index` is a per-flip buffer counter. The plugin's `send_vbuf` writes this prefix;
//! the receiver strips it. DCB/CCB framing is unchanged. Plugin side:
//! `tools/ps4-gnm-scrape/plugin/source/main.c` (`KIND_VBUF`, `send_vbuf`).
//!
//! ## RLE payload (zero-run RLE)
//!
//! A sequence of chunks. Each chunk:
//! ```text
//!   op  u8    0 = literal, 1 = zero-run
//!   len u32   number of bytes
//!   if op==0 (literal): `len` literal bytes follow
//!   if op==1 (zero-run): nothing follows (len implied zeros)
//! ```
//! The encoder only emits a zero-run for a maximal run of >= [`MIN_ZERO_RUN`]
//! zeros; shorter zero runs stay inside a literal. This collapses Celeste's
//! ~4 MB mostly-zero DCB to a few bytes while faithfully preserving every
//! non-zero byte, and bounds worst-case expansion on non-zero data.

/// A dependency-free JSON reader for the artefacts we ourselves write (the GPU
/// snapshot `draws.json` that `framediff` diffs against a console capture).
pub mod json;

use std::io::{self, Read};

/// Wire magic, little-endian bytes spell "GNM4".
pub const MAGIC: u32 = 0x344D_4E47;

/// The fixed header size in bytes.
pub const HEADER_LEN: usize = 20;

/// Minimum consecutive-zero run the encoder collapses into a zero-run chunk.
/// Must match `MIN_ZERO_RUN` in the plugin's C encoder.
pub const MIN_ZERO_RUN: usize = 8;

/// The submit call a frame was captured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `sceGnmSubmitCommandBuffers`
    Submit,
    /// `sceGnmSubmitAndFlipCommandBuffers`
    Flip,
    /// `sceGnmSubmitCommandBuffersForWorkload`
    SubmitWorkload,
    /// `sceGnmSubmitAndFlipCommandBuffersForWorkload`
    FlipWorkload,
    /// A referenced dynamic-buffer content dump (task-172 Phase 2). The payload's first
    /// 8 bytes are the guest base address (LE u64); the rest is the buffer content.
    Vbuf,
    /// Any other value seen on the wire.
    Unknown(u8),
}

impl Kind {
    /// Decode the wire `kind` byte.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Kind::Submit,
            1 => Kind::Flip,
            2 => Kind::SubmitWorkload,
            3 => Kind::FlipWorkload,
            4 => Kind::Vbuf,
            other => Kind::Unknown(other),
        }
    }

    /// Short tag used in dump file names.
    pub fn tag(&self) -> &'static str {
        match self {
            Kind::Submit => "submit",
            Kind::Flip => "flip",
            Kind::SubmitWorkload => "submitwl",
            Kind::FlipWorkload => "flipwl",
            Kind::Vbuf => "vbuf",
            Kind::Unknown(_) => "unknown",
        }
    }
}

/// The decoded frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Monotonic counter the plugin stamps on every submit call.
    pub frame: u32,
    /// Which submit entry point produced this buffer.
    pub kind: Kind,
    /// Index of this buffer within the submit batch (`count`).
    pub buf_index: u8,
    /// `true` for a CCB, `false` for a DCB.
    pub is_ccb: bool,
    /// `true` if the submit call carried a flip.
    pub flip: bool,
    /// Decompressed payload length in bytes.
    pub raw_size: u32,
}

/// Zero-run-RLE encode `data`. Mirrors the plugin's C encoder byte-for-byte.
pub fn rle_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut lit_start = 0usize;

    let flush_lit = |out: &mut Vec<u8>, data: &[u8], start: usize, end: usize| {
        if end > start {
            let len = (end - start) as u32;
            out.push(0u8); // literal op
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&data[start..end]);
        }
    };

    while i < data.len() {
        if data[i] == 0 {
            // Measure the zero run.
            let run_start = i;
            while i < data.len() && data[i] == 0 {
                i += 1;
            }
            let run = i - run_start;
            if run >= MIN_ZERO_RUN {
                // Emit any pending literal up to the run, then the zero-run.
                flush_lit(&mut out, data, lit_start, run_start);
                out.push(1u8); // zero-run op
                out.extend_from_slice(&(run as u32).to_le_bytes());
                lit_start = i;
            }
            // else: short zero run stays inside the current literal.
        } else {
            i += 1;
        }
    }
    flush_lit(&mut out, data, lit_start, data.len());
    out
}

/// Decode an RLE payload produced by [`rle_encode`]. `raw_size` is the expected
/// decompressed length (used to pre-size and to validate).
pub fn rle_decode(payload: &[u8], raw_size: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(raw_size);
    let mut p = 0usize;
    while p < payload.len() {
        let op = payload[p];
        p += 1;
        if p + 4 > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "RLE chunk truncated (length)",
            ));
        }
        let len = u32::from_le_bytes([payload[p], payload[p + 1], payload[p + 2], payload[p + 3]])
            as usize;
        p += 4;
        match op {
            0 => {
                if p + len > payload.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "RLE literal chunk truncated (body)",
                    ));
                }
                out.extend_from_slice(&payload[p..p + len]);
                p += len;
            }
            1 => out.resize(out.len() + len, 0),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad RLE op byte {other}"),
                ));
            }
        }
    }
    if out.len() != raw_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "RLE decoded {} bytes, header claimed {}",
                out.len(),
                raw_size
            ),
        ));
    }
    Ok(out)
}

/// Serialize a whole frame (header + RLE payload) for a raw buffer. Used by the
/// round-trip test; the plugin does the equivalent in C.
pub fn encode_frame(
    frame: u32,
    kind: Kind,
    buf_index: u8,
    is_ccb: bool,
    flip: bool,
    raw: &[u8],
) -> Vec<u8> {
    let comp = rle_encode(raw);
    let kind_byte = match kind {
        Kind::Submit => 0,
        Kind::Flip => 1,
        Kind::SubmitWorkload => 2,
        Kind::FlipWorkload => 3,
        Kind::Vbuf => 4,
        Kind::Unknown(v) => v,
    };
    let mut out = Vec::with_capacity(HEADER_LEN + comp.len());
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&frame.to_le_bytes());
    out.push(kind_byte);
    out.push(buf_index);
    out.push(is_ccb as u8);
    out.push(flip as u8);
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
    out.extend_from_slice(&comp);
    out
}

/// Read one frame off a stream, returning the header and the de-RLE'd payload.
/// Returns `Ok(None)` on a clean EOF at a frame boundary.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<(Header, Vec<u8>)>> {
    let mut hdr = [0u8; HEADER_LEN];
    // Read the header, tolerating a clean EOF before the first byte.
    if !read_exact_or_eof(r, &mut hdr)? {
        return Ok(None);
    }
    let magic = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    if magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad frame magic {magic:#010x} (expected {MAGIC:#010x})"),
        ));
    }
    let frame = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let kind = Kind::from_u8(hdr[8]);
    let buf_index = hdr[9];
    let is_ccb = hdr[10] != 0;
    let flip = hdr[11] != 0;
    let raw_size = u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]);
    let comp_size = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]) as usize;

    let mut comp = vec![0u8; comp_size];
    r.read_exact(&mut comp)?;
    let raw = rle_decode(&comp, raw_size as usize)?;
    Ok(Some((
        Header {
            frame,
            kind,
            buf_index,
            is_ccb,
            flip,
            raw_size,
        },
        raw,
    )))
}

/// `read_exact` that returns `Ok(false)` on a clean EOF before any byte was
/// read (end of stream at a frame boundary), `Ok(true)` on a full read.
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF in the middle of a frame header",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_roundtrip_big_zero_run() {
        // A synthetic buffer shaped like Celeste's 4 MB DCB: a small non-zero
        // head, a huge zero body, a small non-zero tail.
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x11, 0x22]);
        buf.resize(buf.len() + 4 * 1024 * 1024, 0); // 4 MiB of zeros
        buf.extend_from_slice(&[0x00, 0x28, 0xFC, 0x09]); // atlas-base-like tail
        buf.extend_from_slice(&[0x00, 0x00, 0x00]); // short zero run (< MIN), kept literal

        let comp = rle_encode(&buf);
        // The big zero run must make the compressed form tiny vs the raw size.
        assert!(
            comp.len() < 1024,
            "expected strong compression, got {} bytes",
            comp.len()
        );
        let back = rle_decode(&comp, buf.len()).unwrap();
        assert_eq!(back, buf);
    }

    #[test]
    fn rle_roundtrip_all_nonzero() {
        let buf: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        let comp = rle_encode(&buf);
        let back = rle_decode(&comp, buf.len()).unwrap();
        assert_eq!(back, buf);
    }

    #[test]
    fn rle_roundtrip_empty_and_all_zero() {
        assert_eq!(rle_decode(&rle_encode(&[]), 0).unwrap(), Vec::<u8>::new());
        let zeros = vec![0u8; 100_000];
        assert_eq!(rle_decode(&rle_encode(&zeros), zeros.len()).unwrap(), zeros);
    }

    #[test]
    fn short_zero_run_stays_literal() {
        // A 3-zero run (< MIN_ZERO_RUN) between non-zero bytes must NOT become a
        // zero-run chunk — it round-trips inside a single literal.
        let buf = vec![1u8, 0, 0, 0, 2u8];
        let comp = rle_encode(&buf);
        // One literal chunk: op(1) + len(4) + 5 body bytes = 10.
        assert_eq!(comp.len(), 10);
        assert_eq!(comp[0], 0); // literal op
        assert_eq!(rle_decode(&comp, buf.len()).unwrap(), buf);
    }

    #[test]
    fn frame_roundtrip_via_reader() {
        let mut raw = vec![0x37u8, 0x13];
        raw.resize(1 << 20, 0); // 1 MiB zeros
        raw.push(0x99);
        let bytes = encode_frame(42, Kind::Flip, 1, false, true, &raw);
        let mut cursor = std::io::Cursor::new(bytes);
        let (hdr, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(hdr.frame, 42);
        assert_eq!(hdr.kind, Kind::Flip);
        assert_eq!(hdr.buf_index, 1);
        assert!(!hdr.is_ccb);
        assert!(hdr.flip);
        assert_eq!(hdr.raw_size as usize, raw.len());
        assert_eq!(payload, raw);
        // Clean EOF after the single frame.
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }
}
