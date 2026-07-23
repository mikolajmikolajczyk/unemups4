//! `libSceNet` HLE — thin shims over host BSD sockets.
//!
//! The PS4 `sceNet*` API mirrors BSD sockets almost 1:1 (the address-family,
//! socket-type and protocol constants match Linux: `AF_INET`=2, `SOCK_DGRAM`=2,
//! `SOCK_STREAM`=1, `IPPROTO_UDP`=17), so each call forwards to the matching libc
//! entry point. Homebrew that logs over the network (e.g. a remote-debug UDP sink
//! it uses on real hardware) then sends real datagrams from the emulator, so the
//! same listener receives them.
//!
//! Fd model: `sceNetSocket` returns the raw host fd. Guest file descriptors are a
//! separate virtual table (kernel `next_fd`), and net descriptors are only ever
//! passed back to other `sceNet*` calls, so the two namespaces never cross even if
//! the integers happen to overlap. No `SceNetSockaddr` <-> `sockaddr_in` mismatch is
//! left implicit: the BSD `sockaddr` (leading `sin_len` byte) is translated to the
//! Linux layout (2-byte family, no `sin_len`) in `sceNetSendto`.

use crate::context::NativeContext;
use ps4_core::guest_ptr::{GuestPtr, GuestSlice, read_cstr};
use ps4_macros::ps4_syscall;
use ps4_syscalls::SyscallId;
use std::ffi::c_void;

#[ps4_syscall(id = SyscallId::SCE_NET_INIT, lib = crate::libs::LIB_SCE_NET, name = "sceNetInit")]
pub fn sce_net_init() -> i32 {
    0
}

/// `sceNetSocket(name, family, type, protocol)` — the leading `name` is a debug
/// label BSD `socket()` has no slot for, so it is ignored; the rest forward
/// unchanged (PS4 family/type/protocol constants equal Linux's).
#[ps4_syscall(
    id = SyscallId::SCE_NET_SOCKET,
    lib = crate::libs::LIB_SCE_NET,
    name = "sceNetSocket"
)]
pub fn sce_net_socket(_name: *const u8, family: i32, sock_type: i32, protocol: i32) -> i32 {
    // Returns the host fd (>= 0) or -1 on error; errno is not surfaced to the guest.
    unsafe { libc::socket(family, sock_type, protocol) }
}

/// `sceNetHtons(port)` — host-to-network short. On a little-endian host this is a
/// byte swap; the network-order value is returned in the low 16 bits.
#[ps4_syscall(
    id = SyscallId::SCE_NET_HTONS,
    lib = crate::libs::LIB_SCE_NET,
    name = "sceNetHtons"
)]
pub fn sce_net_htons(port: u32) -> u32 {
    (port as u16).to_be() as u32
}

/// `sceNetInetPton(af, src, dst)` — parse a presentation-form address string
/// (`src`, a guest C string) into the 4-byte network-order address at `dst`.
/// Returns 1 on success, 0 for a malformed address, -1 on error.
#[ps4_syscall(
    id = SyscallId::SCE_NET_INET_PTON,
    lib = crate::libs::LIB_SCE_NET,
    name = "sceNetInetPton"
)]
pub fn sce_net_inet_pton(af: i32, src: *const u8, dst: *mut u8) -> i32 {
    if src.is_null() || dst.is_null() {
        return -1;
    }
    // Only IPv4 is modeled (SCE_NET_AF_INET == Linux AF_INET == 2).
    if af != libc::AF_INET {
        return -1;
    }
    // task-115: bounded scan of the source text through the shared seam instead of a raw
    // `CStr::from_ptr`, which would over-read past an unmapped page on an unterminated string.
    // An IPv4 dotted-quad is at most 15 chars; cap the scan generously.
    let Some(text) = read_cstr(src as u64, 64) else {
        return 0;
    };
    match text.parse::<std::net::Ipv4Addr>() {
        // `octets()` is network byte order (MSB first) — exactly the in_addr layout.
        Ok(ip) => {
            // Write the 4 in_addr bytes through the range-validated, SMC-tracked write seam
            // (task-115): a bad/near-arena-top `dst` fails clean instead of a host store.
            if let Some(gs) = GuestSlice::<u8>::new(dst as u64, 4) {
                let _ = gs.write_slice(&ip.octets());
            }
            1
        }
        Err(_) => 0,
    }
}

/// `sceNetSendto(s, buf, len, flags, to, tolen)` — send a datagram. The destination
/// is a BSD `SceNetSockaddrIn` (leading `sin_len` byte, family at offset 1), which is
/// translated to the Linux `sockaddr_in` (2-byte family, no `sin_len`); the port and
/// address are already network-order and copied verbatim. Returns bytes sent, or -1.
#[ps4_syscall(
    id = SyscallId::SCE_NET_SENDTO,
    lib = crate::libs::LIB_SCE_NET,
    name = "sceNetSendto"
)]
pub fn sce_net_sendto(
    s: i32,
    buf: *const u8,
    len: usize,
    flags: i32,
    to: *const u8,
    _tolen: u32,
) -> i64 {
    if buf.is_null() {
        return -1;
    }
    // Translate the BSD sockaddr the guest supplies into the host layout.
    let mut host_addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let (addr_ptr, addr_len) = if to.is_null() {
        (std::ptr::null(), 0)
    } else {
        // task-115: the `to` sockaddr is a guest-controlled pointer under the identity map, so pull
        // its 8 bytes through the bounded read seam rather than a raw `read_unaligned`. A wild `to`,
        // or one with fewer than 8 mapped bytes (`_tolen` is unenforced by the ABI), then fails
        // clean at -1 instead of segfaulting the emulator or splicing adjacent host bytes into the
        // destination address. BSD `SceNetSockaddrIn`: sin_len[0], sin_family[1], sin_port[2..4],
        // sin_addr[4..8] — port/addr already network-order, loaded verbatim in native byte order to
        // match the prior read.
        let Some(sa) = GuestSlice::<u8>::new(to as u64, 8).and_then(|gs| gs.read_vec()) else {
            return -1;
        };
        host_addr.sin_family = libc::AF_INET as libc::sa_family_t; // guest sin_family at [1] is SCE_NET_AF_INET
        host_addr.sin_port = u16::from_ne_bytes([sa[2], sa[3]]); // network order, verbatim
        host_addr.sin_addr.s_addr = u32::from_ne_bytes([sa[4], sa[5], sa[6], sa[7]]); // network order
        (
            &host_addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    unsafe { libc::sendto(s, buf as *const c_void, len, flags, addr_ptr, addr_len) as i64 }
}

#[ps4_syscall(
    id = SyscallId::SCE_NET_SOCKET_CLOSE,
    lib = crate::libs::LIB_SCE_NET,
    name = "sceNetSocketClose"
)]
pub fn sce_net_socket_close(s: i32) -> i32 {
    unsafe { libc::close(s) }
}

/// `pthread_sigmask` — the guest runtime installs signal masks the emulator has no
/// signal delivery for; accept and ignore (report success). Imported from libkernel.
#[ps4_syscall(
    id = SyscallId::SYS_PTHREAD_SIGMASK,
    lib = crate::libs::LIB_KERNEL,
    name = "pthread_sigmask"
)]
pub fn pthread_sigmask_stub(_how: i32, _set: *const u8, _oldset: *mut u8) -> i32 {
    0
}

// -- Net init/teardown surface the Mono runtime touches during startup ---------------
// The goal here is to let network INITIALIZATION succeed while reporting NO connectivity,
// so the title runs offline (Celeste's online features degrade gracefully). Memory pools
// and resolvers hand back a non-zero handle; the link is reported disconnected; DNS
// resolution fails. Real socket I/O stays on the host-backed path (sceNetSocket/sendto).

#[ps4_syscall(id = SyscallId::SCE_NET_POOL_CREATE, lib = crate::libs::LIB_SCE_NET, name = "sceNetPoolCreate")]
pub fn sce_net_pool_create(_name: *const u8, _size: i32, _flags: i32) -> i32 {
    1 // a non-zero memory-pool id
}

#[ps4_syscall(id = SyscallId::SCE_NET_POOL_DESTROY, lib = crate::libs::LIB_SCE_NET, name = "sceNetPoolDestroy")]
pub fn sce_net_pool_destroy(_id: i32) -> i32 {
    0
}

#[ps4_syscall(id = SyscallId::SCE_NET_RESOLVER_CREATE, lib = crate::libs::LIB_SCE_NET, name = "sceNetResolverCreate")]
pub fn sce_net_resolver_create(_name: *const u8, _pool: i32, _flags: i32) -> i32 {
    1 // a non-zero resolver id
}

#[ps4_syscall(id = SyscallId::SCE_NET_RESOLVER_DESTROY, lib = crate::libs::LIB_SCE_NET, name = "sceNetResolverDestroy")]
pub fn sce_net_resolver_destroy(_id: i32) -> i32 {
    0
}

// DNS resolution fails (no connectivity) — the title falls back to offline.
#[ps4_syscall(id = SyscallId::SCE_NET_RESOLVER_START_NTOA, lib = crate::libs::LIB_SCE_NET, name = "sceNetResolverStartNtoa")]
pub fn sce_net_resolver_start_ntoa(
    _rid: i32,
    _hostname: *const u8,
    _addr: *mut u8,
    _timeout: i32,
    _retry: i32,
    _flags: i32,
) -> i32 {
    -1
}

// Reverse DNS also fails (no connectivity). Only the first args matter for a stub; the
// trailing timeout/retry/flags live on the stack beyond our 6-register ABI and are unused.
#[ps4_syscall(id = SyscallId::SCE_NET_RESOLVER_START_ATON, lib = crate::libs::LIB_SCE_NET, name = "sceNetResolverStartAton")]
pub fn sce_net_resolver_start_aton(
    _rid: i32,
    _addr: *const u8,
    _hostname: *mut u8,
    _len: i32,
    _timeout: i32,
) -> i32 {
    -1
}

// Link state: report disconnected (0 = SCE_NET_CTL_STATE_DISCONNECTED).
#[ps4_syscall(id = SyscallId::SCE_NET_CTL_GET_STATE, lib = crate::libs::LIB_SCE_NET, name = "sceNetCtlGetState")]
pub fn sce_net_ctl_get_state(state: *mut i32) -> i32 {
    // task-115: validated GuestPtr write; junk pointer = clean no-op instead of a segfault.
    if let Some(gp) = GuestPtr::<i32>::new(state as u64) {
        let _ = gp.write(0); // SCE_NET_CTL_STATE_DISCONNECTED
    }
    0
}

#[ps4_syscall(id = SyscallId::SCE_NET_TERM, lib = crate::libs::LIB_SCE_NET, name = "sceNetTerm")]
pub fn sce_net_term(_memid: i32) -> i32 {
    0
}

// sceNetErrnoLoc: a pointer to the per-thread net errno. Reuse the thread's errno slot
// (guest-resident) so a deref is always valid.
#[ps4_syscall(id = SyscallId::SCE_NET_ERRNO_LOC, lib = crate::libs::LIB_SCE_NET, name = "sceNetErrnoLoc")]
pub fn sce_net_errno_loc() -> u64 {
    ps4_cpu::current_errno_addr().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps4_core::bounded_read::{BoundedRead, registered_source as read_source};
    use ps4_core::kernel::set_arena_bounds;
    use ps4_core::write_guest::{WriteGuest, registered_source as write_source};
    use std::ffi::CString;
    use std::sync::{Arc, Mutex};

    /// A single host-backed region (host addr == guest ptr) implementing both the read and
    /// write seam over `[start, end)` — a read/write wholly inside succeeds, one that crosses
    /// the end (or starts below) is rejected. task-115 routes `sceNetInetPton` through the
    /// seams, so a unit test that backs `src`/`dst` with host memory must wire them.
    struct RegionMem {
        start: u64,
        end: u64,
        lock: Mutex<()>,
    }

    impl BoundedRead for RegionMem {
        fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
            if size == 0 {
                return Ok(Vec::new());
            }
            let range_end = addr.checked_add(size as u64).ok_or("overflow")?;
            if addr < self.start || addr >= self.end || range_end > self.end {
                return Err("out of region");
            }
            let _g = self.lock.lock().unwrap();
            let mut buf = vec![0u8; size];
            unsafe { std::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), size) };
            Ok(buf)
        }
    }

    impl WriteGuest for RegionMem {
        fn write_bytes(&self, addr: u64, data: &[u8]) -> Result<(), &'static str> {
            if data.is_empty() {
                return Ok(());
            }
            let range_end = addr.checked_add(data.len() as u64).ok_or("overflow")?;
            if addr < self.start || addr >= self.end || range_end > self.end {
                return Err("out of region");
            }
            let _g = self.lock.lock().unwrap();
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
            Ok(())
        }
    }

    /// Wire a `RegionMem` over `buf` into both seams and set the arena bounds to the buffer,
    /// returning the two scoped-seam restore guards. The caller must hold [`NET_TEST_GUARD`].
    fn wire<'a>(buf: &[u8]) -> (impl Drop + 'a, impl Drop + 'a) {
        let base = buf.as_ptr() as u64;
        let end = base + buf.len() as u64;
        set_arena_bounds(base, buf.len() as u64);
        let mem = Arc::new(RegionMem {
            start: base,
            end,
            lock: Mutex::new(()),
        });
        let rg = read_source().override_scoped(mem.clone() as Arc<dyn BoundedRead>);
        let wg = write_source().override_scoped(mem as Arc<dyn WriteGuest>);
        (rg, wg)
    }

    #[test]
    fn htons_produces_network_order() {
        // 80 = 0x0050 host; network order is bytes 00 50, which read back as a
        // little-endian u16 is 0x5000.
        assert_eq!(sce_net_htons(80), 0x5000);
        assert_eq!(sce_net_htons(0x1234), 0x3412);
    }

    #[test]
    fn inet_pton_writes_network_order_octets() {
        let _t = crate::arena_test_lock();
        // task-115: `sceNetInetPton` reads `src` via the bounded seam and writes `dst` via the
        // SMC-tracked seam, so back both with one host region and wire the seams over it. Layout:
        // the NUL-terminated source text, then the 4-byte destination.
        let mut buf = [0u8; 32];
        let text = b"127.0.0.1\0";
        buf[..text.len()].copy_from_slice(text);
        let base = buf.as_ptr() as u64;
        let src_ptr = base;
        let dst_ptr = base + 16;
        let (_rg, _wg) = wire(&buf);
        let rc = sce_net_inet_pton(libc::AF_INET, src_ptr as *const u8, dst_ptr as *mut u8);
        assert_eq!(rc, 1);
        assert_eq!(&buf[16..20], &[127, 0, 0, 1]);
    }

    #[test]
    fn inet_pton_rejects_malformed_and_wrong_family() {
        let bad = CString::new("not.an.ip").unwrap();
        let mut dst = [0u8; 4];
        assert_eq!(
            sce_net_inet_pton(libc::AF_INET, bad.as_ptr() as *const u8, dst.as_mut_ptr()),
            0
        );
        let ok = CString::new("1.2.3.4").unwrap();
        assert_eq!(
            sce_net_inet_pton(libc::AF_INET6, ok.as_ptr() as *const u8, dst.as_mut_ptr()),
            -1
        );
    }

    #[test]
    fn null_pointers_fault_cleanly() {
        let mut dst = [0u8; 4];
        assert_eq!(
            sce_net_inet_pton(libc::AF_INET, std::ptr::null(), dst.as_mut_ptr()),
            -1
        );
        assert_eq!(
            sce_net_sendto(3, std::ptr::null(), 0, 0, std::ptr::null(), 0),
            -1
        );
    }

    #[test]
    fn sendto_rejects_unbounded_to_pointer() {
        // task-115: a non-null `to` the bounded read seam can't validate must fail clean at -1,
        // never a raw 8-byte read off an untrusted pointer. Pin the arena to a fake region that
        // excludes the (real, on-stack) sockaddr so `GuestSlice::new` rejects it before any read.
        let _t = crate::arena_test_lock();
        set_arena_bounds(0x1000, 0x1000); // arena [0x1000, 0x2000): far from any real address
        let sockaddr = [0u8; 8];
        let buf = [0u8; 4];
        let rc = sce_net_sendto(
            3,
            buf.as_ptr(),
            buf.len(),
            0,
            sockaddr.as_ptr(),
            sockaddr.len() as u32,
        );
        assert_eq!(rc, -1);
    }
}

/// The MAC address we report for the console's wired NIC. A **locally administered**
/// address (bit 1 of the first octet set), which is precisely the standard's way of saying
/// "this was not assigned by a hardware vendor" — so it can never collide with a real
/// console's address, and anything that inspects it can tell it is synthetic.
///
/// Stable across runs on purpose: titles use the MAC as a machine identity for save-data
/// ownership and local-cache keys, and an address that changed every boot would look like a
/// different console each time.
const SYNTHETIC_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x75, 0x6e, 0x34];

/// `sceNetGetMacAddress(SceNetEtherAddr *addr, int flags)` — the NIC's hardware address.
///
/// Answered, not refused, even though the console is modelled as having no link: a MAC is a
/// property of the hardware, present whether or not a cable is plugged in. Refusing would be
/// a different claim — "this machine has no network interface" — and a title that reads the
/// MAC as its machine id would then have none.
#[ps4_syscall(
    id = SyscallId::SCE_NET_GET_MAC_ADDRESS,
    lib = crate::libs::LIB_SCE_NET,
    name = "sceNetGetMacAddress"
)]
pub fn sce_net_get_mac_address(addr: u64, _flags: i32) -> i32 {
    let Some(gs) = ps4_core::guest_ptr::GuestSlice::<u8>::new(addr, SYNTHETIC_MAC.len()) else {
        return -1;
    };
    let _ = gs.write_slice(&SYNTHETIC_MAC);
    0
}
