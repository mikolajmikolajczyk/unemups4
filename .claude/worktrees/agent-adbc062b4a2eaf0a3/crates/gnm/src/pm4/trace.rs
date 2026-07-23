//! `Pm4Packet` → human-readable trace line, and an env-gated emitter that walks a
//! guest submission and logs it (doc-4 §1, §3).
//!
//! Emission is gated on `UNEMUPS4_PM4_TRACE=1`: default runs stay silent (AC #2).
//! Unknown opcodes render with their raw hex value and are skipped, never fatal —
//! the guest keeps running (AC #3). This is decode + trace only; nothing here
//! touches the GPU, applies state, or presents.

use crate::pm4::decode::{OwnedPacket, Pm4Packet};
use crate::pm4::opcodes;

/// Env var that turns PM4 tracing on. Any value other than an unset/empty/`0`
/// string enables it.
pub const TRACE_ENV: &str = "UNEMUPS4_PM4_TRACE";

/// Whether PM4 tracing is enabled for this process (checks `UNEMUPS4_PM4_TRACE`).
pub fn enabled() -> bool {
    match std::env::var(TRACE_ENV) {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    }
}

/// Render a borrowed decoded packet to a single trace line (no newline).
pub fn trace(packet: &Pm4Packet<'_>) -> String {
    match *packet {
        Pm4Packet::Type3 {
            opcode,
            count,
            body,
        } => trace_type3(opcode, count, body),
        Pm4Packet::Type0 {
            base_index,
            count,
            body,
        } => trace_type0(base_index, count, body),
        Pm4Packet::Type2 => "TYPE2 NOP".to_string(),
        Pm4Packet::Truncated { header } => {
            format!("TRUNCATED header={header:#010x} (buffer ended mid-packet)")
        }
    }
}

/// Render an owned decoded packet (see [`OwnedPacket`]) to a single trace line.
pub fn trace_owned(packet: &OwnedPacket) -> String {
    match packet {
        OwnedPacket::Type3 {
            opcode,
            count,
            body,
        } => trace_type3(*opcode, *count, body),
        OwnedPacket::Type0 {
            base_index,
            count,
            body,
        } => trace_type0(*base_index, *count, body),
        OwnedPacket::Type2 => "TYPE2 NOP".to_string(),
        OwnedPacket::Truncated { header } => {
            format!("TRUNCATED header={header:#010x} (buffer ended mid-packet)")
        }
    }
}

fn trace_type3(opcode: u8, count: u16, body: &[u32]) -> String {
    let name = match opcodes::name(opcode) {
        Some(n) => n.to_string(),
        None => format!("UNKNOWN({opcode:#04x})"),
    };
    let mut line = format!("T3 {name} count={count}");
    if let (Some(base), Some(&reg_off)) = (opcodes::set_reg_base(opcode), body.first()) {
        // SET_*_REG: first body dword is the register offset; the rest are values.
        let abs = base + reg_off;
        line.push_str(&format!(
            " reg={:#x} (base+{:#x}) values={}",
            abs,
            reg_off,
            body.len().saturating_sub(1)
        ));
    }
    line
}

fn trace_type0(base_index: u16, count: u16, _body: &[u32]) -> String {
    format!("T0 REG_WRITE base={base_index:#x} count={count}")
}

/// Decode a guest submission and, when tracing is enabled, log each packet.
/// No-op (aside from the cheap env check) when tracing is off, so it is safe to
/// call on every submit. Never fatal: a truncated buffer is logged and the walk
/// stops; unknown opcodes are logged and skipped.
///
/// # Safety
/// Same contract as [`crate::pm4::decode::decode_submit_range`]: the range's
/// DCB/CCB pointers must reference readable guest command-buffer memory.
pub unsafe fn trace_submit_range(range: &crate::driver::SubmitRange) {
    if !enabled() {
        return;
    }
    let packets = unsafe { crate::pm4::decode::decode_submit_range(range) };
    tracing::info!(
        "[PM4] submit dcb={:#x} ({} B) ccb={:#x} ({} B){} — {} packet(s)",
        range.dcb_ptr,
        range.dcb_size,
        range.ccb_ptr,
        range.ccb_size,
        if range.flip { " +flip" } else { "" },
        packets.len(),
    );
    for (i, p) in packets.iter().enumerate() {
        tracing::info!("[PM4]   [{}] {}", i, trace_owned(p));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm4::decode::{OwnedPacket, decode};
    use crate::pm4::opcodes::op;
    use crate::pm4::opcodes::t3_header;

    #[test]
    fn renders_known_buffer_lines() {
        let mut buf = Vec::new();
        buf.push(t3_header(op::IT_NOP, 1));
        buf.push(0x0);
        buf.push(t3_header(op::IT_SET_CONTEXT_REG, 2));
        buf.extend([0x0000_0010, 0xABCD]); // reg offset 0x10, value
        buf.push(t3_header(op::IT_DRAW_INDEX_AUTO, 2));
        buf.extend([3, 0]);

        let lines: Vec<String> = decode(&buf).map(|p| trace(&p)).collect();
        assert_eq!(
            lines,
            vec![
                "T3 IT_NOP count=1".to_string(),
                // CONTEXT base 0xA000 + reg offset 0x10 = 0xA010, one value.
                "T3 IT_SET_CONTEXT_REG count=2 reg=0xa010 (base+0x10) values=1".to_string(),
                "T3 IT_DRAW_INDEX_AUTO count=2".to_string(),
            ]
        );
    }

    #[test]
    fn unknown_opcode_renders_raw_hex() {
        let line = trace_owned(&OwnedPacket::Type3 {
            opcode: 0xEE,
            count: 1,
            body: vec![0xDEAD_BEEF],
        });
        assert_eq!(line, "T3 UNKNOWN(0xee) count=1");
    }

    #[test]
    fn truncated_renders() {
        let line = trace_owned(&OwnedPacket::Truncated {
            header: 0xC00F_0000,
        });
        assert!(line.starts_with("TRUNCATED header=0xc00f0000"));
    }

    #[test]
    fn env_gate_toggles_enabled() {
        // Single test owns the env var to avoid cross-test races.
        unsafe { std::env::remove_var(TRACE_ENV) };
        assert!(!enabled());
        unsafe { std::env::set_var(TRACE_ENV, "0") };
        assert!(!enabled());
        unsafe { std::env::set_var(TRACE_ENV, "1") };
        assert!(enabled());
        unsafe { std::env::remove_var(TRACE_ENV) };
        assert!(!enabled());
    }
}
