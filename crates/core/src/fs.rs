//! Guest `open(2)` / `fcntl` flag bits.
//!
//! Orbis OS is a FreeBSD 9.0 derivative, so the guest's `O_*` flag values are the
//! FreeBSD `<sys/fcntl.h>` constants. Each value below is the one published in
//! FreeBSD 9.0 `sys/sys/fcntl.h` (BSD-2), pinned by `open_flags_match_freebsd_oracle`.
//! The access-mode trio `O_RDONLY/O_WRONLY/O_RDWR` occupies the low two bits
//! (`O_ACCMODE = 0x0003`); the remaining bits are independent open-behaviour flags.

/// `O_RDONLY` — FreeBSD 9.0 `sys/sys/fcntl.h`.
pub const O_RDONLY: i32 = 0x0000;
/// `O_WRONLY` — FreeBSD 9.0 `sys/sys/fcntl.h`.
pub const O_WRONLY: i32 = 0x0001;
/// `O_RDWR` — FreeBSD 9.0 `sys/sys/fcntl.h`.
pub const O_RDWR: i32 = 0x0002;
/// `O_CREAT` — create if nonexistent (FreeBSD 9.0 `sys/sys/fcntl.h`).
pub const O_CREAT: i32 = 0x0200;
/// `O_TRUNC` — truncate to zero length (FreeBSD 9.0 `sys/sys/fcntl.h`).
pub const O_TRUNC: i32 = 0x0400;
/// `O_EXCL` — error if already exists (FreeBSD 9.0 `sys/sys/fcntl.h`).
pub const O_EXCL: i32 = 0x0800;
/// `O_APPEND` — set append mode (FreeBSD 9.0 `sys/sys/fcntl.h`).
pub const O_APPEND: i32 = 0x0008;

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins every `O_*` flag to its FreeBSD value. The right-hand literals are the
    /// `#define`s in FreeBSD 9.0 `sys/sys/fcntl.h` (Orbis OS is a FreeBSD 9.0
    /// derivative, so the guest's open-flag ABI is these values); this test fails if
    /// ours drift from those definitions.
    #[test]
    fn open_flags_match_freebsd_oracle() {
        // (our const, FreeBSD 9.0 sys/sys/fcntl.h value).
        assert_eq!(O_RDONLY, 0x0000);
        assert_eq!(O_WRONLY, 0x0001);
        assert_eq!(O_RDWR, 0x0002);
        assert_eq!(O_APPEND, 0x0008);
        assert_eq!(O_CREAT, 0x0200);
        assert_eq!(O_TRUNC, 0x0400);
        assert_eq!(O_EXCL, 0x0800);

        // Access-mode trio lives in the low two bits; FreeBSD `O_ACCMODE = 0x0003`.
        assert_eq!(O_RDONLY & 0x3, O_RDONLY);
        assert_eq!(O_WRONLY & 0x3, O_WRONLY);
        assert_eq!(O_RDWR & 0x3, O_RDWR);
    }
}
