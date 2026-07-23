// TEMP (task-152): disassemble a dumped Celeste GCN shader window to inspect the
// vertex fetch component count. Run: cargo run -p ps4-gcn --example dump_disasm -- <file.bin>
use std::io::Read;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: dump_disasm <file.bin>");
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    // Find OrbShdr header to bound the VS code length.
    let hdr = bytes
        .windows(7)
        .position(|w| w == b"OrbShdr")
        .expect("no OrbShdr");
    let word = u32::from_le_bytes([
        bytes[hdr + 8],
        bytes[hdr + 9],
        bytes[hdr + 10],
        bytes[hdr + 11],
    ]);
    let code_len = ((word >> 8) & 0x00FF_FFFF) as usize;
    println!(
        "OrbShdr@{hdr} code_len={code_len} bytes m_type={}",
        (word >> 2) & 0xF
    );
    let code_bytes = &bytes[..code_len.min(hdr)];
    let words: Vec<u32> = code_bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    println!("=== VS main ({} dwords) ===", words.len());
    let decoded = ps4_gcn::decode_all(&words);
    for d in &decoded {
        println!("{}", ps4_gcn::disasm(d));
    }
    // If there's a fetch call, resolve + splice it and disassemble the fetch body too.
    if ps4_gcn::has_fetch_call(&decoded) {
        println!("\n=== HAS FETCH CALL — resolving ===");
        // The fetch shader body lives at the s_swappc target; the dump window may not
        // contain it. Try resolve_fetch_call_from_code over the whole 64KiB window.
        match ps4_gcn::resolve_fetch_call_from_code(&decoded, &{
            let all: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            all
        }) {
            Ok(spliced) => {
                println!("=== spliced VS+fetch ({} insts) ===", spliced.len());
                for d in &spliced {
                    println!("{}", ps4_gcn::disasm(d));
                }
            }
            Err(e) => println!("resolve_fetch_call_from_code failed: {e:?}"),
        }
    } else {
        println!("\n=== NO fetch call detected ===");
    }

    // Scan the WHOLE 64KiB window for any buffer_load_format (the fetch shader may sit
    // after the VS main in the same window). Print each with its component width.
    println!("\n=== scan whole window for buffer_load_format ===");
    let all: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let decoded_all = ps4_gcn::decode_all(&all);
    for d in &decoded_all {
        let s = ps4_gcn::disasm(d);
        if s.contains("buffer_load_format") {
            println!("{s}");
        }
    }
}
