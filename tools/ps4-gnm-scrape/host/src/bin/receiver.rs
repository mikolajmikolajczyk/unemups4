//! TCP receiver for the real-PS4 GNM scraper (task-168).
//!
//! Listens on `0.0.0.0:9010` (the PC = 192.168.100.1 side of the direct cable),
//! accepts the GoldHEN plugin running inside Celeste, reads the framed +
//! zero-run-RLE'd stream, and writes each command buffer to
//! `dumps/frame%06d_sub%d_<kind>_<dcb|ccb>.bin` with a sidecar `.txt` of the
//! frame metadata.
//!
//! Usage:
//!   receiver [BIND_ADDR] [OUT_DIR] [MAX_FRAME]
//!     BIND_ADDR  default 0.0.0.0:9010
//!     OUT_DIR    default ./dumps
//!     MAX_FRAME  optional; stop + exit once a buffer with frame >= MAX_FRAME
//!                arrives (bounded "first N frames from boot" oracle capture)

use std::fs;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use ps4_gnm_scrape::{Header, Kind, read_frame};

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let bind = args.next().unwrap_or_else(|| "0.0.0.0:9010".to_string());
    let out_dir = PathBuf::from(args.next().unwrap_or_else(|| "dumps".to_string()));
    let max_frame: Option<u32> = args.next().and_then(|s| s.parse().ok());
    fs::create_dir_all(&out_dir)?;

    let listener = TcpListener::bind(&bind)?;
    println!(
        "[receiver] listening on {bind}, writing to {}",
        out_dir.display()
    );
    if let Some(m) = max_frame {
        println!("[receiver] will stop + exit once frame >= {m} is captured");
    }
    println!("[receiver] start this BEFORE launching Celeste; the plugin is the TCP client.");

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                println!("[receiver] connection from {peer}");
                if let Err(e) = handle_conn(s, &out_dir, max_frame) {
                    eprintln!("[receiver] connection ended: {e}");
                }
                println!("[receiver] waiting for next connection (Ctrl-C to stop)...");
            }
            Err(e) => eprintln!("[receiver] accept error: {e}"),
        }
    }
    Ok(())
}

fn handle_conn(stream: TcpStream, out_dir: &Path, max_frame: Option<u32>) -> std::io::Result<()> {
    let mut r = BufReader::new(stream);
    let mut n = 0u64;
    let mut total_bytes = 0u64;
    while let Some((hdr, payload)) = read_frame(&mut r)? {
        write_dump(out_dir, &hdr, &payload)?;
        n += 1;
        total_bytes += payload.len() as u64;
        if n.is_multiple_of(50) {
            println!(
                "[receiver] {n} buffers, {:.1} MiB decoded so far (last frame {})",
                total_bytes as f64 / (1024.0 * 1024.0),
                hdr.frame
            );
        }
        if let Some(m) = max_frame
            && hdr.frame >= m
        {
            println!(
                "[receiver] reached frame {} (>= {m}): {n} buffers, {:.1} MiB total. \
                 Oracle capture complete — exiting.",
                hdr.frame,
                total_bytes as f64 / (1024.0 * 1024.0)
            );
            std::process::exit(0);
        }
    }
    println!("[receiver] stream closed: {n} buffers, {total_bytes} bytes total decoded");
    Ok(())
}

fn write_dump(out_dir: &Path, hdr: &Header, payload: &[u8]) -> std::io::Result<()> {
    // task-172 Phase 2: KIND_VBUF payload is an 8-byte LE base prefix + content.
    // Strip the prefix and name the file by base + span so real vs ours can align.
    if hdr.kind == Kind::Vbuf {
        return write_vbuf_dump(out_dir, hdr, payload);
    }
    let cb = if hdr.is_ccb { "ccb" } else { "dcb" };
    let stem = format!(
        "frame{:06}_sub{}_{}_{}",
        hdr.frame,
        hdr.buf_index,
        hdr.kind.tag(),
        cb
    );
    let bin = out_dir.join(format!("{stem}.bin"));
    fs::write(&bin, payload)?;

    let meta = out_dir.join(format!("{stem}.txt"));
    let mut f = fs::File::create(&meta)?;
    writeln!(f, "frame      {}", hdr.frame)?;
    writeln!(f, "kind       {:?}", hdr.kind)?;
    writeln!(f, "buf_index  {}", hdr.buf_index)?;
    writeln!(f, "is_ccb     {}", hdr.is_ccb)?;
    writeln!(f, "flip       {}", hdr.flip)?;
    writeln!(f, "raw_size   {} bytes", hdr.raw_size)?;
    writeln!(f, "bin        {}", bin.display())?;
    Ok(())
}

/// Save a KIND_VBUF frame: `payload` = 8-byte LE guest base + content bytes. Writes
/// `frameNNNNNN_buf_<base_hex>_<span>.bin` (content only) next to the DCBs.
fn write_vbuf_dump(out_dir: &Path, hdr: &Header, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() < 8 {
        eprintln!(
            "[receiver] KIND_VBUF frame {} payload too short ({} bytes)",
            hdr.frame,
            payload.len()
        );
        return Ok(());
    }
    let base = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let content = &payload[8..];
    let stem = format!(
        "frame{:06}_buf{:02}_{:#016x}_{}",
        hdr.frame,
        hdr.buf_index,
        base,
        content.len()
    );
    let bin = out_dir.join(format!("{stem}.bin"));
    fs::write(&bin, content)?;
    Ok(())
}
