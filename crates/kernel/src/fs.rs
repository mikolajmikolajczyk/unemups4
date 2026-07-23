// crates/kernel/src/fs.rs

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use ps4_core::fs::{O_APPEND, O_CREAT, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
use tracing::{error, info};

/// A directory-read cursor, materialized lazily on the first `getdents` call
/// against a directory fd and advanced across subsequent calls until drained.
/// Snapshotting once (rather than re-reading the host dir per call) gives the
/// stable, repeatable ordering `getdents` promises even if the host dir mutates
/// mid-enumeration.
struct DirCursor {
    entries: Vec<DirentInfo>,
    pos: usize,
}

struct DirentInfo {
    name: String,
    d_type: u8,
    fileno: u32,
}

/// Represents an open file description.
/// Shared because multiple FDs (dup) can point to the same file.
pub struct FileDescription {
    pub file: Mutex<File>,
    pub path: String,
    pub flags: i32,
    /// Lazily-built directory enumeration state (None until first `getdents`).
    dir: Mutex<Option<DirCursor>>,
}

/// Env-gated (`UNEMUPS4_META_TRACE=1`) byte-level trace of reads/seeks on `.meta`
/// atlas-metadata files. Zero cost when unset. Used to verify our FS serves the
/// atlas `.meta` bytes to the guest exactly (task-178).
fn meta_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("UNEMUPS4_META_TRACE").is_ok_and(|v| v != "0" && !v.is_empty())
    })
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
/// return its canonical form. The lexical `..` collapse in [`FileSystem::translate`]
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

/// True when mount `prefix` matches `guest_path` at a path-component boundary: the
/// guest path either equals the prefix or continues with a `/` (or the prefix already
/// ends at a boundary). A bare `str::starts_with` would let `"/app0abc"` match mount
/// `"/app0"`, or `"/app0/mono/4.5.1/x"` match mount `"/app0/mono/4.5"`, stripping a
/// partial component and resolving under the wrong host root.
fn mount_prefix_matches(guest_path: &str, prefix: &str) -> bool {
    match guest_path.strip_prefix(prefix) {
        None => false,
        Some(rest) => rest.is_empty() || rest.starts_with('/') || prefix.ends_with('/'),
    }
}

/// Lexically collapse `.` and `..` in a mount-relative path (`a/b/../c` -> `a/c`),
/// returning the surviving components. `None` if a `..` would pop past the mount
/// root — that is a real traversal attempt, not an in-tree parent reference. Purely
/// textual: it does not touch the disk, so [`contain`] still runs afterwards to catch
/// symlink escapes this can't see.
fn normalize_relative(relative: &str) -> Option<Vec<&str>> {
    let mut out: Vec<&str> = Vec::new();
    for comp in relative.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            other => out.push(other),
        }
    }
    Some(out)
}

/// Resolve `components` under `host_root` the way the PS4's case-insensitive game
/// partition would on our case-sensitive host FS: descend one component at a time,
/// preferring an exact match (no directory scan) and only scanning the parent dir
/// for a case-insensitive match when the exact name is absent. `None` at the first
/// component with no case-insensitive match. When a directory holds two entries
/// differing only in case, the lexicographically-first is chosen deterministically.
fn resolve_case_insensitive(host_root: &Path, components: &[&str]) -> Option<PathBuf> {
    let mut current = host_root.to_path_buf();
    for comp in components {
        let exact = current.join(comp);
        if exact.exists() {
            current = exact;
            continue;
        }
        let mut hits: Vec<PathBuf> = std::fs::read_dir(&current)
            .ok()?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(comp))
            .map(|e| e.path())
            .collect();
        hits.sort();
        current = hits.into_iter().next()?;
    }
    Some(current)
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

    /// Removes every mount registered under `guest_path` (the inverse of [`mount`]).
    pub fn unmount(&self, guest_path: &str) {
        let mut mounts = self.mounts.lock().unwrap();
        mounts.retain(|(prefix, _)| prefix != guest_path);
    }

    /// Translates a guest path (e.g., "/app0/eboot.bin") to a host path.
    ///
    /// `..` is collapsed lexically per mount (`a/b/../c` -> `a/c`) so legitimate
    /// in-tree parent references resolve; only a `..` that would climb above the
    /// mount root is rejected. The PS4 game partition is case-insensitive, so when
    /// the exact host path is absent, resolution retries component-by-component
    /// case-insensitively — an exact match never triggers a directory scan.
    fn translate(&self, guest_path: &str) -> Option<PathBuf> {
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
            if mount_prefix_matches(guest_path, prefix) {
                // Remove prefix ("/app0/foo" -> "/foo"), clean the leading slash.
                let relative = guest_path[prefix.len()..].trim_start_matches('/');
                // Collapse `..`/`.` relative to this mount; a `..` escaping the root
                // is a traversal attempt for this mount — skip it, don't clamp.
                let Some(components) = normalize_relative(relative) else {
                    continue;
                };
                let mut host_candidate = host_root.clone();
                host_candidate.extend(&components);
                // Verify the path stays inside the mount root even after host symlinks
                // are resolved (the `..` collapse above is textual only).
                let Some(contained) = contain(&host_candidate, host_root) else {
                    continue;
                };
                if contained.exists() {
                    return Some(contained);
                }
                // Exact host path absent: retry case-insensitively (the guest's FS is
                // case-insensitive, ours is not), still routing the hit through `contain`.
                if let Some(ci) = resolve_case_insensitive(host_root, &components)
                    && let Some(contained_ci) = contain(&ci, host_root)
                    && contained_ci.exists()
                {
                    return Some(contained_ci);
                }
                // Create-fallback keeps the guest's EXACT requested case — we create a
                // file with the name the guest asked for, never a case-folded one.
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
    /// normalization in `translate` still applies per mount. Note the lexical
    /// collapse does not resolve host symlinks, so a symlink placed under the mount
    /// could point outside it — `contain` (canonicalize + prefix-check) is the
    /// backstop that catches that.
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
        // Clone the Arc and drop the fd-table lock before the blocking read, so
        // unrelated fd ops don't serialize behind this disk I/O (mirrors getdents).
        let fd_entry = {
            let files = self.files.lock().unwrap();
            files.get(&fd).ok_or(9)?.clone() // EBADF
        };
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

    /// Translate a guest path (e.g. `/app0/scePlayStation4.prx`) to its backing host
    /// path, or `None` if no mount matches. The runtime module loader
    /// (`sceKernelLoadStartModule`) reads a `.prx` off disk by guest path, so it needs
    /// the same mount translation `open` uses.
    pub fn host_path(&self, path: &str) -> Option<PathBuf> {
        self.resolve(path)
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
            dir: Mutex::new(None),
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
        // Clone the Arc and drop the fd-table lock before the blocking read, so
        // unrelated fd ops don't serialize behind this disk I/O (mirrors getdents).
        let entry = {
            let map = self.files.lock().unwrap();
            map.get(&fd).ok_or(9)?.clone() // EBADF
        };

        // Check permissions
        if (entry.flags & 3) == O_WRONLY {
            return Err(9); // EBADF (Not open for reading)
        }

        let mut file = entry.file.lock().unwrap();
        let trace = meta_trace_enabled() && entry.path.ends_with(".meta");
        let pos_before = if trace {
            file.stream_position().unwrap_or(u64::MAX)
        } else {
            0
        };
        let res = file.read(buf).map_err(|_| 5); // EIO
        if let (true, Ok(n)) = (trace, &res) {
            let n = *n;
            let head = &buf[..n.min(48)];
            info!(
                "[META_TRACE] read '{}' fd={} pos={} req={} got={} bytes={:02x?}",
                entry.path,
                fd,
                pos_before,
                buf.len(),
                n,
                head
            );
        }
        res
    }

    /// Positional write, the mirror of [`Self::pread`].
    ///
    /// Not seek-write-seek: the fd's cursor is shared by every guest thread holding it, and a
    /// title uses positional I/O precisely to avoid coordinating on that cursor. `write_at`
    /// carries the same guarantee.
    pub fn pwrite(&self, fd: i32, buf: &[u8], offset: u64) -> Result<usize, i32> {
        use std::os::unix::fs::FileExt;
        // Clone the Arc and drop the fd-table lock before the blocking write, so
        // unrelated fd ops don't serialize behind this disk I/O (mirrors getdents).
        let entry = {
            let map = self.files.lock().unwrap();
            map.get(&fd).ok_or(9)?.clone()
        };
        if (entry.flags & 3) == O_RDONLY {
            return Err(9);
        }
        let file = entry.file.lock().unwrap();
        file.write_at(buf, offset).map_err(|_| 5) // EIO
    }

    pub fn write(&self, fd: i32, buf: &[u8]) -> Result<usize, i32> {
        if fd == 1 || fd == 2 {
            std::io::stdout().write_all(buf).ok();
            return Ok(buf.len());
        }

        // Clone the Arc and drop the fd-table lock before the blocking write, so
        // unrelated fd ops don't serialize behind this disk I/O (mirrors getdents).
        let entry = {
            let map = self.files.lock().unwrap();
            map.get(&fd).ok_or(9)?.clone()
        };

        if (entry.flags & 3) == O_RDONLY {
            return Err(9);
        }

        let mut file = entry.file.lock().unwrap();
        file.write(buf).map_err(|_| 5)
    }

    pub fn lseek(&self, fd: i32, offset: i64, whence: i32) -> Result<u64, i32> {
        // Clone the Arc and drop the fd-table lock before the seek, so unrelated
        // fd ops don't serialize behind this I/O (mirrors getdents).
        let entry = {
            let map = self.files.lock().unwrap();
            map.get(&fd).ok_or(9)?.clone()
        };
        let mut file = entry.file.lock().unwrap();

        let seek_from = match whence {
            0 => SeekFrom::Start(offset as u64), // SEEK_SET
            1 => SeekFrom::Current(offset),      // SEEK_CUR
            2 => SeekFrom::End(offset),          // SEEK_END
            _ => return Err(22),                 // EINVAL
        };

        let res = file.seek(seek_from).map_err(|_| 22);
        if meta_trace_enabled() && entry.path.ends_with(".meta") {
            info!(
                "[META_TRACE] lseek '{}' fd={} whence={} off={} -> {:?}",
                entry.path, fd, whence, offset, res
            );
        }
        res
    }

    /// FreeBSD-style `getdents(2)`: pack directory entries from `fd` into `buf` as
    /// a sequence of `struct dirent` records, returning the number of bytes written
    /// (0 == end of directory). The record layout is the vendored Orbis one
    /// (`data/oo_sdk/include/bits/dirent.h`):
    ///
    /// ```text
    /// off 0  u32 d_fileno    off 6  u8  d_type
    /// off 4  u16 d_reclen    off 7  u8  d_namlen
    /// off 8  char d_name[..] (d_namlen bytes, then a NUL)
    /// ```
    ///
    /// `d_reclen` is variable — `roundup8(8 + namlen + 1)` — exactly as the real
    /// kernel packs it, so a FreeBSD-targeting `readdir` advances by `d_reclen`
    /// correctly. Enumeration is snapshotted on the first call (see [`DirCursor`])
    /// and drained across calls; the fd's own position is not touched. `.` and `..`
    /// are synthesized first, matching what the real kernel returns (correct
    /// consumers skip them by name).
    pub fn getdents(&self, fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
        use std::os::unix::fs::DirEntryExt;

        let files = self.files.lock().unwrap();
        let entry = files.get(&fd).ok_or(9)?.clone(); // EBADF
        drop(files); // release the fd table; snapshotting may hit the disk

        let mut cursor_guard = entry.dir.lock().unwrap();
        if cursor_guard.is_none() {
            let host_path = self.resolve(&entry.path).ok_or(9)?; // EBADF
            let md = std::fs::metadata(&host_path).map_err(|e| io_errno(&e))?;
            if !md.is_dir() {
                return Err(20); // ENOTDIR
            }
            let mut entries = vec![
                DirentInfo {
                    name: ".".to_string(),
                    d_type: 4, // DT_DIR
                    fileno: 1,
                },
                DirentInfo {
                    name: "..".to_string(),
                    d_type: 4, // DT_DIR
                    fileno: 1,
                },
            ];
            for rd in std::fs::read_dir(&host_path).map_err(|e| io_errno(&e))? {
                let de = rd.map_err(|e| io_errno(&e))?;
                let ft = de.file_type().map_err(|e| io_errno(&e))?;
                let d_type = if ft.is_dir() {
                    4 // DT_DIR
                } else if ft.is_symlink() {
                    10 // DT_LNK
                } else if ft.is_file() {
                    8 // DT_REG
                } else {
                    0 // DT_UNKNOWN
                };
                // A zero fileno reads as "deleted slot" to some consumers; the host
                // inode is non-zero for real entries, but fall back to a non-zero
                // synthetic just in case.
                let fileno = (de.ino() as u32).max(2);
                entries.push(DirentInfo {
                    name: de.file_name().to_string_lossy().into_owned(),
                    d_type,
                    fileno,
                });
            }
            *cursor_guard = Some(DirCursor { entries, pos: 0 });
        }

        let cursor = cursor_guard.as_mut().unwrap();
        let mut written = 0usize;
        while cursor.pos < cursor.entries.len() {
            let e = &cursor.entries[cursor.pos];
            let name = e.name.as_bytes();
            let namlen = name.len().min(255);
            let reclen = (8 + namlen + 1).div_ceil(8) * 8; // roundup8, room for NUL
            if written + reclen > buf.len() {
                if written == 0 {
                    return Err(22); // EINVAL — buffer too small for even one entry
                }
                break;
            }
            let rec = &mut buf[written..written + reclen];
            rec.fill(0);
            rec[0..4].copy_from_slice(&e.fileno.to_le_bytes());
            rec[4..6].copy_from_slice(&(reclen as u16).to_le_bytes());
            rec[6] = e.d_type;
            rec[7] = namlen as u8;
            rec[8..8 + namlen].copy_from_slice(&name[..namlen]);
            written += reclen;
            cursor.pos += 1;
        }
        Ok(written)
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

    /// Parse a getdents buffer into (d_type, name) pairs by walking d_reclen, the
    /// same way a FreeBSD readdir does. Validates the record layout is self-consistent.
    fn parse_dents(buf: &[u8], total: usize) -> Vec<(u8, String)> {
        let mut out = Vec::new();
        let mut off = 0;
        while off < total {
            let reclen = u16::from_le_bytes([buf[off + 4], buf[off + 5]]) as usize;
            assert!(reclen >= 8 && off + reclen <= total, "reclen sane");
            let d_type = buf[off + 6];
            let namlen = buf[off + 7] as usize;
            let name = String::from_utf8_lossy(&buf[off + 8..off + 8 + namlen]).to_string();
            assert_eq!(buf[off + 8 + namlen], 0, "d_name NUL-terminated");
            out.push((d_type, name));
            off += reclen;
        }
        out
    }

    #[test]
    fn getdents_enumerates_dir_with_dot_entries() {
        let dir = unique_tmp("getdents");
        std::fs::write(dir.join("alpha.txt"), b"a").unwrap();
        std::fs::create_dir(dir.join("subdir")).unwrap();

        let fs = FileSystem::new();
        fs.mount("/app0", dir.clone());
        let fd = fs.open("/app0", O_RDONLY, 0).unwrap();

        let mut buf = vec![0u8; 4096];
        let n = fs.getdents(fd, &mut buf).unwrap();
        assert!(n > 0);
        let ents = parse_dents(&buf, n);

        // "." and ".." synthesized first, both DT_DIR (4).
        assert_eq!(ents[0], (4, ".".to_string()));
        assert_eq!(ents[1], (4, "..".to_string()));
        // The two real entries are present with correct d_type.
        assert!(ents.contains(&(8, "alpha.txt".to_string())), "DT_REG file"); // DT_REG
        assert!(ents.contains(&(4, "subdir".to_string())), "DT_DIR subdir"); // DT_DIR

        // A second call drains to EOF (0 bytes).
        assert_eq!(fs.getdents(fd, &mut buf).unwrap(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn getdents_tiny_buffer_is_einval() {
        let dir = unique_tmp("getdents-tiny");
        let fs = FileSystem::new();
        fs.mount("/app0", dir.clone());
        let fd = fs.open("/app0", O_RDONLY, 0).unwrap();
        // 8 bytes can't hold even the "." record (8 + 1 + NUL -> roundup 16).
        let mut buf = vec![0u8; 8];
        assert_eq!(fs.getdents(fd, &mut buf), Err(22)); // EINVAL
        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn case_mismatch_resolves_to_real_file() {
        let dir = unique_tmp("case-mismatch");
        std::fs::create_dir_all(dir.join("Content/Dialog")).unwrap();
        std::fs::write(dir.join("Content/Dialog/English.txt.export"), b"x").unwrap();

        let fs = FileSystem::new();
        fs.mount("/app0", dir.clone());

        // The guest asks in the wrong case; the PS4 partition would resolve it.
        let host = fs
            .translate("/app0/Content/Dialog/english.txt.export")
            .expect("case-insensitive resolution");
        assert_eq!(
            host,
            dir.join("Content/Dialog/English.txt.export")
                .canonicalize()
                .unwrap()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exact_case_does_not_scan() {
        // A path whose case already matches resolves through the exact-match fast
        // path. We can't observe the absence of a `read_dir` directly, but a
        // symlinked directory would be *followed* by an exact-path lookup and a
        // canonicalize; a fabricated case-insensitive scan would read the parent.
        // Here we simply assert the exact path resolves to itself unchanged.
        let dir = unique_tmp("exact-case");
        std::fs::create_dir_all(dir.join("Sub")).unwrap();
        std::fs::write(dir.join("Sub/File.txt"), b"y").unwrap();

        let fs = FileSystem::new();
        fs.mount("/app0", dir.clone());

        let host = fs.translate("/app0/Sub/File.txt").unwrap();
        assert_eq!(host, dir.join("Sub/File.txt").canonicalize().unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dotdot_inside_tree_resolves_to_parent() {
        let dir = unique_tmp("dotdot-parent");
        std::fs::create_dir_all(dir.join("Content/Dialog")).unwrap();

        let fs = FileSystem::new();
        fs.mount("/app0", dir.clone());

        // `/app0/Content/Dialog/..` is the existing `/app0/Content` directory.
        let host = fs.translate("/app0/Content/Dialog/..").unwrap();
        assert_eq!(host, dir.join("Content").canonicalize().unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dotdot_normalizes_lexically() {
        let dir = unique_tmp("dotdot-lexical");
        std::fs::create_dir_all(dir.join("a/c")).unwrap();
        std::fs::write(dir.join("a/c/leaf.txt"), b"z").unwrap();

        let fs = FileSystem::new();
        fs.mount("/app0", dir.clone());

        // a/b/../c/leaf.txt collapses to a/c/leaf.txt even though a/b never exists.
        let host = fs.translate("/app0/a/b/../c/leaf.txt").unwrap();
        assert_eq!(host, dir.join("a/c/leaf.txt").canonicalize().unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dotdot_escaping_mount_is_rejected() {
        let dir = unique_tmp("dotdot-escape");
        let fs = FileSystem::new();
        fs.mount("/app0", dir.clone());

        // More leading `..` than components consumed climbs above the mount root.
        assert_eq!(fs.translate("/app0/../secret"), None);
        assert_eq!(fs.translate("/app0/a/../../secret"), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mount_prefix_requires_component_boundary() {
        // Two mounts sharing a textual prefix: "/app0/data" and "/app0". The guest
        // path "/app0/database.db" shares the non-boundary substring "/app0/data"
        // with the longer mount but is NOT under it — its real components are under
        // /app0. Bare starts_with would strip "/app0/data" to "base.db" and, because
        // that file exists in the data mount, misroute to the wrong host root; a
        // component-boundary check keeps it under /app0.
        let data_root = unique_tmp("bound-data");
        let app0_root = unique_tmp("bound-app0");
        std::fs::write(data_root.join("base.db"), b"wrong").unwrap();
        std::fs::write(app0_root.join("database.db"), b"right").unwrap();

        let fs = FileSystem::new();
        fs.mount("/app0", app0_root.clone());
        fs.mount("/app0/data", data_root.clone());

        let host = fs
            .translate("/app0/database.db")
            .expect("resolves under /app0");
        assert_eq!(host, app0_root.join("database.db").canonicalize().unwrap());

        std::fs::remove_dir_all(&data_root).ok();
        std::fs::remove_dir_all(&app0_root).ok();
    }

    #[test]
    fn mount_prefix_no_match_on_partial_component() {
        // "/app0abc" must not resolve through mount "/app0" — it is a distinct
        // top-level name, not a child of the mount.
        let app0_root = unique_tmp("partial-app0");
        std::fs::write(app0_root.join("abc"), b"x").unwrap();

        let fs = FileSystem::new();
        fs.mount("/app0", app0_root.clone());

        assert_eq!(fs.translate("/app0abc"), None);

        std::fs::remove_dir_all(&app0_root).ok();
    }

    #[test]
    fn create_fallback_keeps_exact_case() {
        use ps4_core::fs::{O_CREAT, O_WRONLY};

        let dir = unique_tmp("create-case");
        // No case-variant exists, so this is the create-fallback path: the file must
        // be created with the guest's exact mixed case, never a folded name.
        let fs = FileSystem::new();
        fs.mount("/app0", dir.clone());

        let fd = fs
            .open("/app0/NewSave.DAT", O_WRONLY | O_CREAT, 0o644)
            .unwrap();
        fs.write(fd, b"new").unwrap();
        fs.close(fd).unwrap();

        assert!(dir.join("NewSave.DAT").exists(), "exact-case file created");
        assert!(
            !dir.join("newsave.dat").exists(),
            "no folded variant created"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
