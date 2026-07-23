//! Shared errno converter for the two SCE ABIs (task-191).
//!
//! A syscall handler that fails must return the error code in the convention the
//! *importing symbol* expects, and the two caller families want OPPOSITE encodings
//! of the same POSIX errno:
//!
//! * **sce** names (`sceKernelStat`, `sceKernelOpen`, …): the SCE runtime expects a
//!   POSITIVE code `0x8002_0000 | posix_errno` (e.g. `ORBIS_KERNEL_ERROR_ENOENT ==
//!   0x8002_0002`). Retail Mono runs `sce_to_errno(ret)` on it; a raw `-2` is not a
//!   valid SCE code, so it hits an "unknown error" formatter that feeds NULL to `%s`
//!   and crashes.
//! * **posix** names (`stat`, `open`, …): the OpenOrbis libc wrappers check the SIGN
//!   of the return — a NEGATIVE errno is the error path. A positive errno reads as a
//!   valid fd and corrupts the guest's stdio (task-101).
//!
//! [`Errno`] holds a single internal representation — an ALWAYS-positive POSIX errno —
//! and [`Errno::to_sce`] / [`Errno::to_posix`] project it into whichever ABI a given
//! handler serves.
//!
//! The numeric POSIX errnos are FreeBSD's (Orbis OS is FreeBSD 9-based): FreeBSD 9
//! `sys/sys/errno.h` — `EPERM 1`, `ENOENT 2`, `EIO 5`, `EBADF 9`, `EACCES 13`,
//! `EFAULT 14`, `EEXIST 17`, `EINVAL 22`. The SCE codes are `0x8002_0000 | posix`:
//! OpenOrbis `orbis/_types/errors.h` `ORBIS_KERNEL_ERROR_*` (e.g.
//! `ORBIS_KERNEL_ERROR_ENOENT == 0x8002_0002`), vendored in this repo at
//! `data/oo_sdk/include/orbis/_types/errors.h`. Both are pinned by
//! `errno_values_match_freebsd_and_orbis_oracle` below. The two-ABI projection itself
//! (which caller family wants which sign/encoding) is this emulator's HLE design.

/// A POSIX errno. The inner value is ALWAYS the positive POSIX number (e.g. `2` for
/// ENOENT); the ABI projection happens in [`to_sce`](Errno::to_sce) /
/// [`to_posix`](Errno::to_posix).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Errno(pub i32);

impl Errno {
    // Inner value = FreeBSD 9 `sys/sys/errno.h` POSIX number; trailing hex = the
    // OpenOrbis `orbis/_types/errors.h` `ORBIS_KERNEL_ERROR_*` code (`0x8002_0000 | posix`).
    pub const EPERM: Errno = Errno(1); // 0x8002_0001
    pub const ENOENT: Errno = Errno(2); // 0x8002_0002
    pub const EIO: Errno = Errno(5); // 0x8002_0005
    pub const EBADF: Errno = Errno(9); // 0x8002_0009
    pub const EACCES: Errno = Errno(13); // 0x8002_000d
    pub const EFAULT: Errno = Errno(14); // 0x8002_000e
    pub const EEXIST: Errno = Errno(17); // 0x8002_0011
    pub const EINVAL: Errno = Errno(22); // 0x8002_0016

    /// The high half of every libkernel SCE error code: `0x8002_0000`.
    const SCE_KERNEL_ERROR_BASE: u32 = 0x8002_0000;

    /// Project to the SCE ABI: a POSITIVE `0x8002_0000 | posix_errno`.
    pub fn to_sce(self) -> i32 {
        (Self::SCE_KERNEL_ERROR_BASE | self.0 as u32) as i32
    }

    /// Project to the POSIX ABI: the NEGATED errno (the libc-wrapper error path).
    pub fn to_posix(self) -> i32 {
        -self.0
    }

    /// Reverse of [`to_sce`](Errno::to_sce): recover the POSIX errno from an SCE code,
    /// or `None` if `code` is not in the `0x8002_00xx` libkernel error range.
    pub fn from_sce(code: i32) -> Option<Errno> {
        if (code as u32) & 0xffff_0000 == Self::SCE_KERNEL_ERROR_BASE {
            Some(Errno((code as u32 & 0xffff) as i32))
        } else {
            None
        }
    }
}

impl From<std::io::Error> for Errno {
    fn from(e: std::io::Error) -> Self {
        // raw_os_error() is the host errno; if absent (synthetic errors) fall back to
        // EIO. Numeric divergence between host Linux and FreeBSD errno is not remapped
        // here — the fs backend already maps io::Error -> FreeBSD errno before it ever
        // reaches this converter; this impl exists for completeness.
        Errno(e.raw_os_error().unwrap_or(Errno::EIO.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins each POSIX errno to its FreeBSD 9 `sys/sys/errno.h` number and each SCE
    /// projection to its OpenOrbis `orbis/_types/errors.h` `ORBIS_KERNEL_ERROR_*` code.
    /// The right-hand literals are those oracle values; this test fails if ours drift.
    #[test]
    fn errno_values_match_freebsd_and_orbis_oracle() {
        // (our const, FreeBSD 9 errno.h POSIX number, OpenOrbis errors.h ORBIS_KERNEL_ERROR_* code).
        let oracle: [(Errno, i32, i32); 8] = [
            (Errno::EPERM, 1, 0x8002_0001u32 as i32),
            (Errno::ENOENT, 2, 0x8002_0002u32 as i32),
            (Errno::EIO, 5, 0x8002_0005u32 as i32),
            (Errno::EBADF, 9, 0x8002_0009u32 as i32),
            (Errno::EACCES, 13, 0x8002_000Du32 as i32),
            (Errno::EFAULT, 14, 0x8002_000Eu32 as i32),
            (Errno::EEXIST, 17, 0x8002_0011u32 as i32),
            (Errno::EINVAL, 22, 0x8002_0016u32 as i32),
        ];
        for (e, posix, sce) in oracle {
            assert_eq!(e.0, posix, "POSIX errno {e:?} != FreeBSD {posix}");
            assert_eq!(e.to_sce(), sce, "SCE code {e:?} != Orbis {sce:#010x}");
        }
    }

    #[test]
    fn to_sce_encodes_high_half() {
        assert_eq!(Errno::ENOENT.to_sce(), 0x8002_0002u32 as i32);
        assert_eq!(Errno::EPERM.to_sce(), 0x8002_0001u32 as i32);
        assert_eq!(Errno::EINVAL.to_sce(), 0x8002_0016u32 as i32);
    }

    #[test]
    fn from_sce_recovers_posix() {
        assert_eq!(Errno::from_sce(0x8002_0002u32 as i32), Some(Errno::ENOENT));
        assert_eq!(Errno::from_sce(0x8002_0016u32 as i32), Some(Errno::EINVAL));
        // Not an SCE libkernel code -> None.
        assert_eq!(Errno::from_sce(-2), None);
        assert_eq!(Errno::from_sce(0), None);
    }

    #[test]
    fn to_posix_negates() {
        assert_eq!(Errno::ENOENT.to_posix(), -2);
        assert_eq!(Errno::EBADF.to_posix(), -9);
    }

    #[test]
    fn round_trip_sce() {
        for e in [
            Errno::EPERM,
            Errno::ENOENT,
            Errno::EIO,
            Errno::EBADF,
            Errno::EACCES,
            Errno::EFAULT,
            Errno::EEXIST,
            Errno::EINVAL,
        ] {
            assert_eq!(Errno::from_sce(e.to_sce()), Some(e));
        }
    }
}
