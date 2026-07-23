//! SCE library/module id encoding (doc-3 K2).
//!
//! Retail PS4 symbol names are `nid#library#module`, where `library` and
//! `module` are small integer ids encoded into the SCE base64 alphabet. The
//! form of a dynamic-symbol NID entry — `[12-byte symbol hash] "#" [library ID
//! encoded] "#" [module ID encoded]` — and the encoding character set are the
//! OpenOrbis OELF spec's "NID Table" section
//! (`oracles/openorbis/.../docs/MD/OELF Specification/PS4 ELF
//! Specification - Dynlib Data.md`). This module carries only that id encoding
//! — [`encode_id`] — because `ps4-syscalls` does not expose it (it maps whole
//! symbol names, not the per-image lib/module ids the `#`-split resolves
//! against).
//!
//! The forward NID hash (`SHA-1(name || salt)` → first 8 bytes byte-reversed →
//! base64 minus the trailing `=`, `/`→`-`) is **not** implemented here: it is
//! the same algorithm `ps4-syscalls` runs at build time. The OpenOrbis OELF
//! spec's "NID Table" section documents that algorithm (its `calculateNID`
//! golang listing) and the salt is `518D64A635DED8C1E6B039B1C3E55230`, verified
//! by the `nid_hash_matches_oo_ground_truth` test below reproducing the exact
//! NIDs the same spec tabulates. So the canonical NID for any HLE export is
//! available via `ps4_syscalls::SyscallId::nid()`, and a retail import's NID
//! resolves through the generated `MAP_BY_NID` (`SyscallId::from_nid`) with no
//! second implementation. It is a hash, not decryption (doc-3 K1): no keys, no
//! SAMU.

/// The SCE base64 alphabet — OpenOrbis OELF spec "NID Table" encoding character
/// set (`PS4 ELF Specification - Dynlib Data.md`): the standard base64 glyphs
/// with the trailing `+-` in place of `+/`.
const CODES: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+-";

/// Encode a small integer library/module id into the SCE base64 alphabet. The
/// `symbol#library#module` name encodes the library and module ids this way
/// (OpenOrbis OELF spec "NID Table", `PS4 ELF Specification - Dynlib Data.md`);
/// matching a decoded id back to a `DT_SCE_*_LIB` / module entry keys on this
/// encoding. The single-char case (id < 0x40, one alphabet glyph) is the one
/// the OO spec's example NID table exercises; the 2- and 3-char branches extend
/// the same 6-bit-group packing to ids that do not fit one glyph.
pub fn encode_id(id: u64) -> String {
    let mut out = String::new();
    if id < 0x40 {
        out.push(CODES[id as usize] as char);
    } else if id < 0x1000 {
        out.push(CODES[((id >> 6) & 0x3f) as usize] as char);
        out.push(CODES[(id & 0x3f) as usize] as char);
    } else {
        out.push(CODES[((id >> 12) & 0x3f) as usize] as char);
        out.push(CODES[((id >> 6) & 0x3f) as usize] as char);
        out.push(CODES[(id & 0x3f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The single-char (id < 0x40) mapping is the SCE base64 alphabet from the
    /// OpenOrbis OELF spec "NID Table" (`PS4 ELF Specification - Dynlib
    /// Data.md`): `A`=0, `B`=1, … `-`=63. The 2-/3-char widths are this file's
    /// extension of the same 6-bit packing (the OO example only tabulates
    /// single-glyph library/module ids).
    #[test]
    fn encode_id_matches_oo_alphabet_widths() {
        // < 0x40 -> 1 char; < 0x1000 -> 2 chars; else 3.
        assert_eq!(encode_id(0), "A");
        assert_eq!(encode_id(1), "B");
        assert_eq!(encode_id(0x3f), "-");
        assert_eq!(encode_id(0x40).len(), 2);
        assert_eq!(encode_id(0xfff).len(), 2);
        assert_eq!(encode_id(0x1000).len(), 3);
    }

    #[test]
    fn encode_id_exact_multichar_values() {
        // 2-char branch: 0x40 = 0b000001_000000 -> CODES[1] CODES[0] = "BA".
        assert_eq!(encode_id(0x40), "BA");
        // top of the 2-char range: 0xfff = 0b111111_111111 -> CODES[63] CODES[63] = "--".
        assert_eq!(encode_id(0xfff), "--");
        // 3-char branch: 0x1000 = 0b000001_000000_000000 -> CODES[1] CODES[0] CODES[0] = "BAA".
        assert_eq!(encode_id(0x1000), "BAA");
        // exercise all three 6-bit groups distinctly: 0b000010_000011_000100.
        let id = (2u64 << 12) | (3u64 << 6) | 4u64;
        assert_eq!(encode_id(id), "CDE"); // CODES[2],CODES[3],CODES[4]
    }

    /// The loader relies on `ps4-syscalls` producing canonical NIDs. Guard that
    /// here so a change to the generated table can't silently break retail
    /// import resolution. `sceKernelUsleep`→`1jfXLRVzisc` is one of the NIDs the
    /// OpenOrbis OELF spec "NID Table" tabulates verbatim (`PS4 ELF
    /// Specification - Dynlib Data.md`, usleep example: `1jfXLRVzisc#A#B`);
    /// `nid_hash_matches_oo_ground_truth` below re-derives both from first
    /// principles.
    #[test]
    fn ps4_syscalls_nids_are_canonical() {
        use ps4_syscalls::SyscallId;
        assert_eq!(
            SyscallId::from_symbol_name("sceKernelAllocateDirectMemory")
                .unwrap()
                .nid(),
            "rTXw65xmLIA"
        );
        assert_eq!(
            SyscallId::from_symbol_name("sceKernelUsleep")
                .unwrap()
                .nid(),
            "1jfXLRVzisc"
        );
    }

    /// Minimal self-contained SHA-1 (FIPS 180-1) — used only to re-derive NIDs
    /// in the witness test below, so the test depends on no crate for its oracle
    /// math. Returns the 20-byte digest.
    fn sha1(msg: &[u8]) -> [u8; 20] {
        let mut h: [u32; 5] = [
            0x6745_2301,
            0xEFCD_AB89,
            0x98BA_DCFE,
            0x1032_5476,
            0xC3D2_E1F0,
        ];
        let ml = (msg.len() as u64) * 8;
        let mut data = msg.to_vec();
        data.push(0x80);
        while data.len() % 64 != 56 {
            data.push(0);
        }
        data.extend_from_slice(&ml.to_be_bytes());

        for chunk in data.chunks_exact(64) {
            let mut w = [0u32; 80];
            for (i, word) in chunk.chunks_exact(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            for i in 16..80 {
                w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
            }
            let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
            for (i, &wi) in w.iter().enumerate() {
                let (f, k) = match i {
                    0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                    20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                    40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                    _ => (b ^ c ^ d, 0xCA62_C1D6),
                };
                let tmp = a
                    .rotate_left(5)
                    .wrapping_add(f)
                    .wrapping_add(e)
                    .wrapping_add(k)
                    .wrapping_add(wi);
                e = d;
                d = c;
                c = b.rotate_left(30);
                b = a;
                a = tmp;
            }
            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
        }

        let mut out = [0u8; 20];
        for (i, word) in h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// URL-safe-ish base64 encoder for the NID trailer: standard base64 with
    /// `/`→`-` and the trailing `=` dropped, matching the OO spec `calculateNID`
    /// listing.
    fn nid_b64(bytes: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for grp in bytes.chunks(3) {
            let b0 = grp[0] as u32;
            let b1 = *grp.get(1).unwrap_or(&0) as u32;
            let b2 = *grp.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(A[((n >> 18) & 0x3f) as usize] as char);
            out.push(A[((n >> 12) & 0x3f) as usize] as char);
            if grp.len() > 1 {
                out.push(A[((n >> 6) & 0x3f) as usize] as char);
            }
            if grp.len() > 2 {
                out.push(A[(n & 0x3f) as usize] as char);
            }
        }
        out.replace('/', "-")
    }

    /// Re-derives NIDs from first principles and asserts they equal the ground
    /// truth the OpenOrbis OELF spec "NID Table" tabulates (`PS4 ELF
    /// Specification - Dynlib Data.md`, usleep example): `printf`→`hcuQgD53UxM`
    /// and `sceKernelUsleep`→`1jfXLRVzisc` are both listed there literally
    /// (`?hcuQgD53UxM#B#C`, `1jfXLRVzisc#A#B`). This pins the salt value
    /// `518D64A635DED8C1E6B039B1C3E55230` and the algorithm the spec's
    /// `calculateNID` listing describes: `SHA-1(name || salt)`, first 8 bytes
    /// byte-reversed, base64 minus the trailing `=`, `/`→`-`.
    #[test]
    fn nid_hash_matches_oo_ground_truth() {
        let salt: [u8; 16] = [
            0x51, 0x8D, 0x64, 0xA6, 0x35, 0xDE, 0xD8, 0xC1, 0xE6, 0xB0, 0x39, 0xB1, 0xC3, 0xE5,
            0x52, 0x30,
        ];
        let nid = |name: &str| -> String {
            let mut msg = name.as_bytes().to_vec();
            msg.extend_from_slice(&salt);
            let digest = sha1(&msg);
            // First 8 bytes, byte-reversed (spec: big-endian read, little-endian write).
            let mut first8 = [0u8; 8];
            first8.copy_from_slice(&digest[..8]);
            first8.reverse();
            nid_b64(&first8)
        };
        // OO spec "NID Table" usleep-example ground truth.
        assert_eq!(nid("printf"), "hcuQgD53UxM");
        assert_eq!(nid("sceKernelUsleep"), "1jfXLRVzisc");
    }
}
