use crate::context::NativeContext;
use ps4_syscalls::SyscallId;
use std::sync::RwLock;
use tracing::error;

pub type SyscallHandler = fn(&mut NativeContext) -> u64;

#[derive(Clone, Copy)]
pub struct HleSyscallDef {
    pub id: SyscallId,
    pub lib_name: &'static str,
    pub names: &'static [&'static str],
    pub handler: SyscallHandler,
}

inventory::collect!(HleSyscallDef);

const MAX_SYSCALLS: usize = 128_000;

pub static SYSCALL_TABLE: RwLock<[Option<SyscallHandler>; MAX_SYSCALLS]> =
    RwLock::new([None; MAX_SYSCALLS]);

pub fn register_handler(id: u64, handler: SyscallHandler) {
    match SYSCALL_TABLE.write() {
        Ok(mut table) => {
            if (id as usize) < table.len() {
                table[id as usize] = Some(handler);
            } else {
                error!("Warning: Syscall ID {} out of bounds!", id);
            }
        }
        Err(poisoned) => {
            let mut table = poisoned.into_inner();
            error!("[SYSCALL] WARNING: SYSCALL_TABLE poisoned, recovering.");
            if (id as usize) < table.len() {
                table[id as usize] = Some(handler);
            }
        }
    }
}

pub fn get_handler(id: u64) -> Option<SyscallHandler> {
    let table = SYSCALL_TABLE.read().ok()?; // if poisoned, return None
    if (id as usize) < table.len() {
        table[id as usize]
    } else {
        None
    }
}
