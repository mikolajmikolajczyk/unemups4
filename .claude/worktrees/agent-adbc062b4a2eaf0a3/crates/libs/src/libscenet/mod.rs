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
    let text = match unsafe { std::ffi::CStr::from_ptr(src as *const libc::c_char) }.to_str() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    match text.parse::<std::net::Ipv4Addr>() {
        // `octets()` is network byte order (MSB first) — exactly the in_addr layout.
        Ok(ip) => {
            unsafe { std::ptr::copy_nonoverlapping(ip.octets().as_ptr(), dst, 4) };
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
        unsafe {
            host_addr.sin_family = libc::AF_INET as libc::sa_family_t; // guest sin_family at [1] is SCE_NET_AF_INET
            host_addr.sin_port = std::ptr::read_unaligned(to.add(2) as *const u16); // network order, verbatim
            host_addr.sin_addr.s_addr = std::ptr::read_unaligned(to.add(4) as *const u32); // network order
        }
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
    if crate::is_guest_ptr(state) {
        unsafe { *state = 0 };
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
    use std::ffi::CString;

    #[test]
    fn htons_produces_network_order() {
        // 80 = 0x0050 host; network order is bytes 00 50, which read back as a
        // little-endian u16 is 0x5000.
        assert_eq!(sce_net_htons(80), 0x5000);
        assert_eq!(sce_net_htons(0x1234), 0x3412);
    }

    #[test]
    fn inet_pton_writes_network_order_octets() {
        let src = CString::new("127.0.0.1").unwrap();
        let mut dst = [0u8; 4];
        let rc = sce_net_inet_pton(libc::AF_INET, src.as_ptr() as *const u8, dst.as_mut_ptr());
        assert_eq!(rc, 1);
        assert_eq!(dst, [127, 0, 0, 1]);
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
}
