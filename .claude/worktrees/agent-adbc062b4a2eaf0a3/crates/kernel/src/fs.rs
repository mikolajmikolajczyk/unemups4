// crates/kernel/src/fs.rs

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use ps4_core::fs::{O_APPEND, O_CREAT, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
use tracing::{error, info};

/// Represents an open file description.
/// Shared because multiple FDs (dup) can point to the same file.
pub struct FileDescription {
    pub file: Mutex<File>,
    pub path: String,
    pub flags: i32,
}

/// Maps a host `io::Error` to a PS4/FreeBSD errno for the fs-mutation syscalls.
fn io_errno(e: &std::io::Error) -> i32 {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => 2,          // ENOENT
        ErrorKind::PermissionDenied => 13, // EACCES
        ErrorKind::AlreadyExists => 17,    // EEXIST
        // raw_os_error is a Linux errno on the host; only pass through the ones
        // whose numeric value matches FreeBSD, and remap the ones that diverge.
        _ => match e.raw_os_error() {
            Some(20) => 20, // ENOTDIR (same on both)
            Some(21) => 21, // EISDIR (same on both)
            Some(36) => 63, // ENAMETOOLONG (Linux 36 -> FreeBSD 63)
            Some(39) => 66, // ENOTEMPTY (Linux 39 -> FreeBSD 66)
            Some(40) => 62, // ELOOP (Linux 40 -> FreeBSD 62)
            _ => 5,         // EIO — avoid leaking an unmapped Linux errno
        },
    }
}

/// Verify `host_path` stays within `host_root` after host symlinks are resolved, and
/// return its canonical form. The textual `..` rejection in [`FileSystem::translate`]
/// does not catch a symlink placed under the mount that points outside it; canonicalizing
/// and prefix-checking against the (canonical) mount root does (task-102). A path whose
/// tail does not exist yet (a file being created) can't itself be a symlink, so only the
/// deepest existing ancestor is canonicalized and the remaining components are re-appended
/// lexically — otherwise creating a new in-sandbox file would fail. Returns `None`
/// (rejected) on escape or if the mount root can't be canonicalized.
fn contain(host_path: &Path, host_root: &Path) -> Option<PathBuf> {
    let root = host_root.canonicalize().ok()?;
    let canon = canonicalize_existing_prefix(host_path)?;
    if canon.starts_with(&root) {
        Some(canon)
    } else {
        None
    }
}

/// Canonicalize `path` if it exists; otherwise canonicalize its deepest existing ancestor
/// (resolving any symlinks there) and re-append the not-yet-existing tail components. This
/// lets containment checking work for a file that is about to be created.
fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    if let Ok(c) = path.canonicalize() {
        return Some(c);
    }
    let parent = path.parent()?;
    let file = path.file_name()?;
    Some(canonicalize_existing_prefix(parent)?.join(file))
}

pub struct FileSystem {
    /// Maps guest paths prefixes to host paths.
    /// Example: "/app0" -> "./data/app0"
    mounts: Mutex<Vec<(String, PathBuf)>>,

    /// The File Descriptor Table.
    /// Maps integer FD -> Open File.
    files: Mutex<HashMap<i32, Arc<FileDescription>>>,

    /// Simple counter for next FD (starts at 3, leaving 0,1,2 for stdio).
    next_fd: AtomicI32,
}

impl Default for FileSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem {
    pub fn new() -> Self {
        FileSystem {
            mounts: Mutex::new(Vec::new()),
            files: Mutex::new(HashMap::new()),
            next_fd: AtomicI32::new(3), // 0=stdin, 1=stdout, 2=stderr
        }
    }

    /// Mounts a host directory to a guest path prefix.
    pub fn mount(&self, guest_path: &str, host_path: PathBuf) {
        let mut mounts = self.mounts.lock().unwrap();
        // Sort by length descending to match longest prefix first
        mounts.push((guest_path.to_string(), host_path));
        mounts.sort_by_key(|m| std::cmp::Reverse(m.0.len()));
    }

    /// Translates a guest path (e.g., "/app0/eboot.bin") to a host path.
    fn translate(&self, guest_path: &str) -> Option<PathBuf> {
        // Reject directory traversal: no path component may be "..".
        if guest_path.split('/').any(|c| c == "..") {
            return None;
        }

        let mounts = self.mounts.lock().unwrap();

        // Union mounts sharing a prefix (e.g. two "/app0" roots — a dev game_data dir
        // and the loaded title's directory): a file that EXISTS in any of them resolves
        // there; a not-yet-existing path (a file being created) resolves under the FIRST
        // matching mount. This lets a retail title read its assemblies from its own dir
        // while examples keep creating files under game_data/app0.
        // KNOWN LIMITATION (task-116): resolution is flag-blind — an O_CREAT|O_TRUNC of a
        // name that exists only in a later mount resolves THERE and truncates it, instead
        // of creating a scratch file in the write layer. Needs an explicit write layer.
        let mut create_fallback = None;
        for (prefix, host_root) in mounts.iter() {
            if guest_path.starts_with(prefix) {
                // Remove prefix ("/app0/foo" -> "/foo"), clean the leading slash.
                let relative = guest_path[prefix.len()..].trim_start_matches('/');
                // Construct host path, then verify it stays inside the mount root even
                // after host symlinks are resolved (the `..` check above is textual only).
                let Some(contained) = contain(&host_root.join(relative), host_root) else {
                    continue;
                };
                if contained.exists() {
                    return Some(contained);
                }
                if create_fallback.is_none() {
                    create_fallback = Some(contained);
                }
            }
        }

        // No existing file in any union mount; return the first mount's path (for
        // creation), or None if no mount matched the prefix at all.
        create_fallback
    }

    /// Resolves a guest path to a host path, translating mounted prefixes and
    /// falling back to the first mount's host root for CWD-relative paths that
    /// match no mount (the guest CWD is the game's `/app0` sandbox). The `..`
    /// rejection in `translate` still applies to every component. Note this is a
    /// textual check only: it does not resolve host symlinks, so a symlink placed
    /// under the mount could still point outside it (acceptable under the
    /// trusted-homebrew threat model).
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        if let Some(host) = self.translate(path) {
            return Some(host);
        }

        // Relative path (no leading '/') with no matching mount: anchor it under the
        // /app0 sandbox and re-run through translate, so it unions across ALL /app0
        // mounts (dev game_data + the title dir + framework aliases) rather than only
        // the first — a Mono title opens its assemblies by paths like
        // `lib/mono/4.5/mscorlib.dll` relative to /app0.
        if path.starts_with('/') {
            return None;
        }
        let relative = path.trim_start_matches("./").trim_start_matches('/');
        self.translate(&format!("/app0/{relative}"))
    }

    pub fn mkdir(&self, path: &str, _mode: i32) -> Result<i32, i32> {
        if path.is_empty() {
            return Err(2); // ENOENT — empty path is not a directory
        }
        // Doom's M_MakeDirectory calls mkdir(".") for the config/save dir. The
        // sandbox root / "." always exists, so report EEXIST (caller tolerates it).
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() || trimmed == "." {
            return Err(17); // EEXIST
        }

        let host_path = self.resolve(path).ok_or(2)?; // ENOENT
        info!("[FS] Mkdir: '{}' -> {:?}", path, host_path);
        match std::fs::create_dir(&host_path) {
            Ok(_) => Ok(0),
            Err(e) => Err(io_errno(&e)),
        }
    }

    pub fn rmdir(&self, path: &str) -> Result<i32, i32> {
        let host_path = self.resolve(path).ok_or(2)?; // ENOENT
        info!("[FS] Rmdir: '{}' -> {:?}", path, host_path);
        match std::fs::remove_dir(&host_path) {
            Ok(_) => Ok(0),
            Err(e) => Err(io_errno(&e)),
        }
    }

    pub fn unlink(&self, path: &str) -> Result<i32, i32> {
        let host_path = self.resolve(path).ok_or(2)?; // ENOENT
        info!("[FS] Unlink: '{}' -> {:?}", path, host_path);
        match std::fs::remove_file(&host_path) {
            Ok(_) => Ok(0),
            Err(e) => Err(io_errno(&e)),
        }
    }

    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<i32, i32> {
        let host_old = self.resolve(old_path).ok_or(2)?; // ENOENT
        let host_new = self.resolve(new_path).ok_or(2)?; // ENOENT
        info!(
            "[FS] Rename: '{}' -> '{}' ({:?} -> {:?})",
            old_path, new_path, host_old, host_new
        );
        match std::fs::rename(&host_old, &host_new) {
            Ok(_) => Ok(0),
            Err(e) => Err(io_errno(&e)),
        }
    }

    /// Stat a guest path: resolve it to a host file and report `(is_dir, size_bytes)`.
    /// `Err(2)` (ENOENT) when it does not resolve or does not exist.
    pub fn stat(&self, path: &str) -> Result<(bool, u64), i32> {
        let host_path = self.resolve(path).ok_or(2)?;
        let md = std::fs::metadata(&host_path).map_err(|_| 2)?;
        info!(
            "[FS] Stat: '{}' -> {:?} (dir={}, size={})",
            path,
            host_path,
            md.is_dir(),
            md.len()
        );
        Ok((md.is_dir(), md.len()))
    }

    /// Read `buf.len()` bytes at absolute `offset` from `fd` WITHOUT moving the fd's
    /// position (positional read). Used by file-backed mmap. `Err` is a PS4 errno.
    pub fn pread(&self, fd: i32, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        use std::os::unix::fs::FileExt;
        let files = self.files.lock().unwrap();
        let fd_entry = files.get(&fd).ok_or(9)?; // EBADF
        let file = fd_entry.file.lock().unwrap();
        file.read_at(buf, offset).map_err(|e| io_errno(&e))
    }

    /// Stat an open fd: `(is_dir, size_bytes)` from the underlying host file's metadata.
    pub fn fstat(&self, fd: i32) -> Result<(bool, u64), i32> {
        let files = self.files.lock().unwrap();
        let fd_entry = files.get(&fd).ok_or(9)?; // EBADF
        let file = fd_entry.file.lock().unwrap();
        let md = file.metadata().map_err(|e| io_errno(&e))?;
        Ok((md.is_dir(), md.len()))
    }

    pub fn open(&self, path: &str, flags: i32, _mode: i32) -> Result<i32, i32> {
        let host_path = self.resolve(path).ok_or(2)?; // ENOENT (2)

        info!("[FS] Open: '{}' -> {:?}", path, host_path);

        // Convert PS4 flags to Rust OpenOptions
        let mut opts = OpenOptions::new();

        let access_mode = flags & 3;
        match access_mode {
            O_RDONLY => {
                opts.read(true);
            }
            O_WRONLY => {
                opts.write(true);
            }
            O_RDWR => {
                opts.read(true).write(true);
            }
            _ => return Err(22), // EINVAL
        }

        if flags & O_CREAT != 0 {
            opts.create(true);
        }
        if flags & O_TRUNC != 0 {
            opts.truncate(true);
        }
        if flags & O_APPEND != 0 {
            opts.append(true);
        }
        // O_EXCL is not implemented.

        let file = opts.open(&host_path).map_err(|e| {
            error!("[FS] Host open failed: {}", e);
            2 // ENOENT (Generic fallback)
        })?;

        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(FileDescription {
            file: Mutex::new(file),
            path: path.to_string(),
            flags,
        });

        self.files.lock().unwrap().insert(fd, entry);
        Ok(fd)
    }

    pub fn close(&self, fd: i32) -> Result<i32, i32> {
        if self.files.lock().unwrap().remove(&fd).is_some() {
            Ok(0)
        } else {
            Err(9) // EBADF
        }
    }

    pub fn read(&self, fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
        let map = self.files.lock().unwrap();
        let entry = map.get(&fd).ok_or(9)?; // EBADF

        // Check permissions
        if (entry.flags & 3) == O_WRONLY {
            return Err(9); // EBADF (Not open for reading)
        }

        let mut file = entry.file.lock().unwrap();
        file.read(buf).map_err(|_| 5) // EIO
    }

    pub fn write(&self, fd: i32, buf: &[u8]) -> Result<usize, i32> {
        if fd == 1 || fd == 2 {
            std::io::stdout().write_all(buf).ok();
            return Ok(buf.len());
        }

        let map = self.files.lock().unwrap();
        let entry = map.get(&fd).ok_or(9)?;

        if (entry.flags & 3) == O_RDONLY {
            return Err(9);
        }

        let mut file = entry.file.lock().unwrap();
        file.write(buf).map_err(|_| 5)
    }

    pub fn lseek(&self, fd: i32, offset: i64, whence: i32) -> Result<u64, i32> {
        let map = self.files.lock().unwrap();
        let entry = map.get(&fd).ok_or(9)?;
        let mut file = entry.file.lock().unwrap();

        let seek_from = match whence {
            0 => SeekFrom::Start(offset as u64), // SEEK_SET
            1 => SeekFrom::Current(offset),      // SEEK_CUR
            2 => SeekFrom::End(offset),          // SEEK_END
            _ => return Err(22),                 // EINVAL
        };

        file.seek(seek_from).map_err(|_| 22)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ps4_core::fs::O_RDONLY;

    // The fs backend returns a POSITIVE errno on the error path; the syscall
    // handlers (sce_kernel_open / sce_kernel_close) negate it so the guest's
    // POSIX wrapper sees ret<0. Guard that contract — a positive value here
    // would read as a valid fd and corrupt the guest's stdio (task-101).

    #[test]
    fn open_missing_file_is_enoent() {
        let fs = FileSystem::new();
        fs.mount("/app0", std::env::temp_dir());
        // Absent file, read-only (no O_CREAT) — must not look like a fd.
        assert_eq!(
            fs.open("/app0/task101-definitely-absent.xyz", O_RDONLY, 0),
            Err(2) // ENOENT
        );
    }

    #[test]
    fn open_unmounted_path_is_enoent() {
        let fs = FileSystem::new(); // no mounts
        assert_eq!(fs.open("/app0/anything", O_RDONLY, 0), Err(2));
    }

    #[test]
    fn open_rejects_dotdot_traversal() {
        let fs = FileSystem::new();
        fs.mount("/app0", std::env::temp_dir());
        assert_eq!(fs.open("/app0/../etc/passwd", O_RDONLY, 0), Err(2));
    }

    #[test]
    fn close_bad_fd_is_ebadf() {
        let fs = FileSystem::new();
        assert_eq!(fs.close(999), Err(9)); // EBADF
    }

    /// Create a unique temp directory for a test (no tempfile dep; PID + a counter keep
    /// parallel test threads from colliding).
    fn unique_tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "unemups4-fs-test-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn symlink_escaping_sandbox_is_rejected() {
        use ps4_core::fs::{O_CREAT, O_WRONLY};

        let sandbox = unique_tmp("sandbox");
        let outside = unique_tmp("outside");
        std::fs::write(outside.join("secret.txt"), b"nope").unwrap();
        // A symlink INSIDE the sandbox pointing at the outside dir — the textual `..`
        // check can't catch this; canonicalize + prefix-check must (task-102 AC#1).
        std::os::unix::fs::symlink(&outside, sandbox.join("escape")).unwrap();

        let fs = FileSystem::new();
        fs.mount("/app0", sandbox.clone());

        // Following the symlink out of the sandbox is rejected, not opened.
        assert!(
            fs.open("/app0/escape/secret.txt", O_RDONLY, 0).is_err(),
            "a symlink escaping the sandbox must be rejected"
        );

        // AC#2: a legitimate in-sandbox file still opens.
        std::fs::write(sandbox.join("ok.txt"), b"hi").unwrap();
        assert!(fs.open("/app0/ok.txt", O_RDONLY, 0).is_ok());

        // AC#2: a not-yet-existing in-sandbox file (being created) still resolves.
        assert!(
            fs.open("/app0/new.txt", O_WRONLY | O_CREAT, 0o644).is_ok(),
            "creating a new in-sandbox file must work"
        );

        std::fs::remove_dir_all(&sandbox).ok();
        std::fs::remove_dir_all(&outside).ok();
    }
}
