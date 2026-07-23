use crate::process::Process;
use ps4_core::kernel::KernelInterface;
use ps4_core::memory::MemoryAccessExt;
use std::sync::Arc;
use tracing::{error, info};

pub struct KernelBridge {
    process: Arc<Process>,
}

impl KernelBridge {
    pub fn new(process: Arc<Process>) -> Arc<Self> {
        Arc::new(KernelBridge { process })
    }

    /// Parse the `SceVideoOutBufferAttribute` a `sceVideoOutRegisterBuffers` call carries into
    /// the seam's geometry and scanout format (pixelFormat @+0, width @+12, height @+16, all
    /// `u32`). A null pointer, an unreadable pointer, or a zero-sized attribute yields the
    /// historical 1080p default (which carries the A8R8G8B8_SRGB default `pixel_format`), so
    /// the live present path is unchanged when the guest omits explicit dimensions. The
    /// `pixel_format` is threaded on to the present shader so it can R↔B-swap the BGRA scanout
    /// formats without hardcoding (task-154 residual #2).
    fn read_videoout_attr(
        &self,
        memory: &dyn ps4_core::memory::VirtualMemoryManager,
        attr_ptr: u64,
    ) -> ps4_core::videoout::VideoOutBufferAttribute {
        use ps4_core::memory::MemoryAccessExt;
        use ps4_core::videoout::VideoOutBufferAttribute;

        if attr_ptr == 0 {
            return VideoOutBufferAttribute::DEFAULT;
        }
        let pixel_format = memory.read::<u32>(attr_ptr);
        let width = memory.read::<u32>(attr_ptr + 12);
        let height = memory.read::<u32>(attr_ptr + 16);
        match (width, height) {
            (Ok(w), Ok(h)) if w != 0 && h != 0 => VideoOutBufferAttribute {
                width: w,
                height: h,
                // Fall back to the default scanout format if the format read faults.
                pixel_format: pixel_format.unwrap_or(VideoOutBufferAttribute::DEFAULT.pixel_format),
            },
            _ => VideoOutBufferAttribute::DEFAULT,
        }
    }
}

/// Allocate a zeroed host scratch buffer of `len` bytes for a guest read-side
/// I/O transfer, failing gracefully instead of aborting when the guest supplies
/// a bogus/huge length. `vec![0u8; len]` routes the request through the global
/// allocator's *infallible* path, so a length such as `0x0FFF_FFFF_FFFF_FFFF`
/// (well below `SSIZE_MAX`, so it clears FreeBSD's `uio_resid < 0` EINVAL guard
/// in `sys_generic.c`) fails to allocate ~1 EiB and trips `handle_alloc_error`,
/// SIGABRTing the whole emulator. `try_reserve_exact` surfaces that allocation
/// failure as an error we turn into `ENOMEM` (12, per FreeBSD `sys/errno.h` —
/// the Orbis OS base). For any length the host can actually satisfy the buffer
/// is identical to the old `vec![0u8; len]`, so valid reads are unchanged.
fn alloc_io_scratch(len: usize) -> Result<Vec<u8>, i32> {
    let mut buf: Vec<u8> = Vec::new();
    buf.try_reserve_exact(len).map_err(|_| 12i32)?; // ENOMEM
    buf.resize(len, 0);
    Ok(buf)
}

impl KernelInterface for KernelBridge {
    fn create_thread(&self, entry: u64, arg: u64) -> Result<u32, i64> {
        info!("[KERNEL BRIDGE] Request to spawn thread at {:#x}", entry);
        match self
            .process
            .create_thread(entry, arg, Arc::downgrade(&self.process))
        {
            Ok(thread) => {
                if let Err(e) = thread.execute() {
                    error!("KernelBridge Error: {}", e);
                    return Err(0x80020001); // Generic Error
                }
                Ok(thread.id)
            }
            Err(e) => {
                error!("KernelBridge Allocation Error: {}", e);
                Err(0x80020001)
            }
        }
    }
    fn join_thread(&self, tid: u32) -> Result<u64, u64> {
        self.process.thread_manager.join_thread(tid)
    }

    fn tls_key_create(&self, destructor: u64) -> Result<u32, u64> {
        Ok(self.process.tls_keys.create_key(destructor))
    }

    fn tls_set_specific(&self, key: u32, value: u64) -> Result<(), u64> {
        let tid = ps4_core::kernel::current_tid();
        info!(
            "Kernel: tls_set_specific tid={} key={} val={:#x}",
            tid, key, value
        );
        self.process.tls_set_specific(key, value)
    }

    fn tls_get_specific(&self, key: u32) -> Result<u64, u64> {
        self.process.tls_get_specific(key)
    }

    fn mutex_init(&self, ptr: u64, mtype: ps4_core::kernel::MutexType) -> Result<i32, u64> {
        self.process.sync_manager.mutex_init(ptr, mtype)
    }

    fn mutex_destroy(&self, ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.mutex_destroy(ptr)
    }

    fn mutex_lock(&self, ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.mutex_lock(ptr)
    }

    fn mutex_unlock(&self, ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.mutex_unlock(ptr)
    }
    fn cond_init(&self, ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.cond_init(ptr)
    }
    fn cond_destroy(&self, ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.cond_destroy(ptr)
    }
    fn cond_wait(&self, cond: u64, mutex: u64) -> Result<i32, u64> {
        self.process.sync_manager.cond_wait(cond, mutex)
    }
    fn cond_signal(&self, ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.cond_signal(ptr)
    }
    fn cond_broadcast(&self, ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.cond_broadcast(ptr)
    }
    fn cond_timedwait(&self, cond: u64, mutex: u64, micros: u32) -> Result<i32, u64> {
        self.process.cond_timedwait(cond, mutex, micros)
    }
    fn mutex_timedlock(&self, mutex: u64, micros: u32) -> Result<i32, u64> {
        self.process.mutex_timedlock(mutex, micros)
    }
    fn thread_detach(&self, tid: u32) -> Result<i32, u64> {
        self.process.thread_manager.thread_detach(tid)
    }
    fn thread_yield(&self) {
        self.process.thread_manager.thread_yield()
    }
    fn thread_self(&self) -> u32 {
        self.process.thread_manager.thread_self()
    }
    fn thread_equal(&self, t1: u32, t2: u32) -> i32 {
        self.process.thread_manager.thread_equal(t1, t2)
    }
    fn thread_set_name(&self, tid: u32, name: &str) -> Result<i32, u64> {
        self.process.thread_manager.thread_set_name(tid, name)
    }
    fn thread_get_name(&self, tid: u32, out_buf: u64, len: usize) -> Result<i32, u64> {
        self.process.thread_get_name(tid, out_buf, len)
    }
    fn thread_name_of(&self, tid: u32) -> Option<String> {
        self.process.thread_manager.thread_name_of(tid)
    }
    fn thread_cancel(&self, tid: u32) -> Result<i32, u64> {
        self.process.thread_manager.thread_cancel(tid)
    }

    fn mutex_trylock(&self, mutex_ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.mutex_trylock(mutex_ptr)
    }

    fn rwlock_init(&self, ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.rwlock_init(ptr)
    }

    fn rwlock_destroy(&self, ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.rwlock_destroy(ptr)
    }

    fn rwlock_rdlock(&self, ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.rwlock_rdlock(ptr)
    }

    fn rwlock_tryrdlock(&self, ptr: u64) -> Result<i32, u64> {
        // Treat as exclusive try-lock
        self.mutex_trylock(ptr)
    }

    fn rwlock_wrlock(&self, ptr: u64) -> Result<i32, u64> {
        // Treat as exclusive lock
        self.mutex_lock(ptr)
    }

    fn rwlock_trywrlock(&self, ptr: u64) -> Result<i32, u64> {
        // Treat as exclusive try-lock
        self.mutex_trylock(ptr)
    }

    fn rwlock_unlock(&self, ptr: u64) -> Result<i32, u64> {
        self.process.sync_manager.rwlock_unlock(ptr)
    }

    fn file_open(&self, path: &str, flags: i32, mode: i32) -> Result<i32, i32> {
        self.process.fs.open(path, flags, mode)
    }

    fn file_stat(&self, path: &str) -> Result<(bool, u64), i32> {
        self.process.fs.stat(path)
    }

    fn file_fstat(&self, fd: i32) -> Result<(bool, u64), i32> {
        self.process.fs.fstat(fd)
    }

    fn file_close(&self, fd: i32) -> Result<i32, i32> {
        self.process.fs.close(fd)
    }

    fn file_read(&self, fd: i32, ptr: u64, len: usize) -> Result<usize, i32> {
        let mut buf = alloc_io_scratch(len)?;
        let bytes_read = self.process.fs.read(fd, &mut buf)?;
        let memory = self.process.memory.write().unwrap();
        match memory.write_bytes(ptr, &buf[0..bytes_read]) {
            Ok(_) => Ok(bytes_read),
            Err(_) => Err(14),
        }
    }

    fn file_pread(&self, fd: i32, ptr: u64, len: usize, offset: u64) -> Result<usize, i32> {
        let mut buf = alloc_io_scratch(len)?;
        let n = self.process.fs.pread(fd, offset, &mut buf)?;
        let memory = self.process.memory.write().unwrap();
        match memory.write_bytes(ptr, &buf[0..n]) {
            Ok(_) => Ok(n),
            Err(_) => Err(14), // EFAULT
        }
    }

    fn file_pwrite(&self, fd: i32, ptr: u64, len: usize, offset: u64) -> Result<usize, i32> {
        let memory = self.process.memory.read().unwrap();
        let buf = memory.read_bytes(ptr, len).map_err(|_| 14i32)?;
        drop(memory);
        self.process.fs.pwrite(fd, &buf, offset)
    }

    fn file_write(&self, fd: i32, ptr: u64, len: usize) -> Result<usize, i32> {
        let memory = self.process.memory.read().unwrap();
        let buf = match memory.read_bytes(ptr, len) {
            Ok(b) => b,
            Err(_) => return Err(14),
        };
        self.process.fs.write(fd, &buf)
    }

    fn file_lseek(&self, fd: i32, offset: i64, whence: i32) -> Result<u64, i32> {
        self.process.fs.lseek(fd, offset, whence)
    }

    fn file_getdents(&self, fd: i32, ptr: u64, len: usize) -> Result<usize, i32> {
        let mut buf = alloc_io_scratch(len)?;
        let written = self.process.fs.getdents(fd, &mut buf)?;
        let memory = self.process.memory.write().unwrap();
        match memory.write_bytes(ptr, &buf[..written]) {
            Ok(_) => Ok(written),
            Err(_) => Err(14), // EFAULT
        }
    }

    fn file_mkdir(&self, path: &str, mode: i32) -> Result<i32, i32> {
        self.process.fs.mkdir(path, mode)
    }

    fn file_rmdir(&self, path: &str) -> Result<i32, i32> {
        self.process.fs.rmdir(path)
    }

    fn file_unlink(&self, path: &str) -> Result<i32, i32> {
        self.process.fs.unlink(path)
    }

    fn file_rename(&self, old_path: &str, new_path: &str) -> Result<i32, i32> {
        self.process.fs.rename(old_path, new_path)
    }
    fn mmap(
        &self,
        addr: u64,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> Result<u64, i64> {
        self.process.mmap(addr, len, prot, flags, fd, offset)
    }

    fn mmap_aligned(
        &self,
        addr: u64,
        len: usize,
        align: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> Result<u64, i64> {
        self.process
            .mmap_aligned(addr, len, align, prot, flags, fd, offset)
    }

    fn munmap(&self, addr: u64, len: usize) -> Result<i32, i64> {
        self.process.munmap(addr, len)
    }

    fn allocate_direct_memory(&self, len: usize, align: usize) -> Result<u64, i64> {
        self.process.allocate_direct_memory(len, align)
    }

    fn available_direct_memory(
        &self,
        search_start: u64,
        search_end: u64,
        align: u64,
    ) -> Option<(u64, u64)> {
        self.process
            .available_direct_memory(search_start, search_end, align)
    }

    fn map_direct_memory(&self, phys_off: u64, len: usize) -> u64 {
        self.process.map_direct_memory(phys_off, len)
    }

    fn release_direct_memory(&self, phys_off: u64, len: usize) -> i32 {
        self.process.release_direct_memory(phys_off, len)
    }

    fn tls_arena_base(&self) -> Result<u64, u64> {
        self.process.tls_arena_base()
    }
    fn video_out_open(
        &self,
        _user_id: i32,
        _bus_type: i32,
        _index: i32,
        _param: u64,
    ) -> Result<i32, i32> {
        // Return a dummy display handle (0).
        Ok(0)
    }

    fn video_out_register_buffers(
        &self,
        handle: i32,
        start_index: i32,
        ptr: u64,
        count: i32,
        attr_ptr: u64,
    ) -> Result<i32, i32> {
        if let Some(sink) = ps4_core::videoout::video_out_sink() {
            // `ptr` is a `void* list[count]` of 64-bit GPU addresses; `attr_ptr` is one
            // `SceVideoOutBufferAttribute*` describing all of them.
            let memory = self.process.memory.read().unwrap();

            // Thread the real framebuffer geometry from the guest attribute instead of
            // hardcoding 1080p. `SceVideoOutBufferAttribute`: width @+12, height @+16
            // (both u32). A null attr pointer (or a zero-sized attribute) falls back to
            // the historical 1080p default, so behavior is unchanged for callers that
            // omit explicit dimensions.
            let attr = self.read_videoout_attr(&**memory, attr_ptr);

            // Register EVERY buffer in the array at its display index `start_index + i`.
            // A double-buffered title (e.g. Celeste: count=2) registers both scanout
            // targets; reading only `list[0]` misclassified the second (index 1) as an
            // offscreen render target. Bound `count` to a sane max so a garbage count
            // can't spin. A zero / EFAULT entry is skipped cleanly, not aborted.
            const MAX_BUFFERS: i32 = 16;
            let n = count.clamp(0, MAX_BUFFERS);
            for i in 0..n {
                let elem_ptr = ptr + (i as u64) * 8; // little-endian (x86) 64-bit ptrs
                let buffer_addr = match memory.read::<u64>(elem_ptr) {
                    Ok(addr) if addr != 0 => addr,
                    _ => continue,
                };
                let index = (start_index + i) as u32;
                info!(
                    "[KERNEL] RegisterBuffer: idx={} ArrayPtr={:#x} -> FrameBuffer={:#x} ({}x{})",
                    index, elem_ptr, buffer_addr, attr.width, attr.height
                );
                sink.register_buffer(buffer_addr, attr, handle, index);
            }
        }
        Ok(0)
    }

    fn video_out_submit_flip(
        &self,
        handle: i32,
        index: i32,
        _flip_mode: i32,
        _arg: i64,
    ) -> Result<i32, i32> {
        if let Some(sink) = ps4_core::videoout::video_out_sink() {
            // blocks the guest thread until the window thread catches up
            sink.submit_flip(handle, index as u32);
        }
        Ok(0)
    }
    fn pad_get_state(&self, _handle: i32) -> ps4_core::pad::PadState {
        *self.process.input_manager.state.read().unwrap()
    }

    fn load_start_module(&self, guest_path: &str) -> Result<(i32, Vec<u64>), i32> {
        self.process.load_start_module(guest_path)
    }

    fn module_dlsym(&self, handle: i32, name: &str) -> Option<u64> {
        self.process.module_dlsym(handle, name)
    }

    fn tempdata_mount(&self) -> Result<String, i32> {
        self.process.tempdata_mount()
    }

    fn savedata_mount(
        &self,
        user_id: u32,
        dir_name: &str,
        requested_blocks: u64,
        mount_mode: u32,
    ) -> Result<(String, u32, u64), i32> {
        self.process
            .savedata_mount(user_id, dir_name, requested_blocks, mount_mode)
    }

    fn savedata_umount(&self, mount_point: &str) -> Result<(), i32> {
        self.process.savedata_umount(mount_point)
    }

    fn savedata_dir_count(&self) -> u32 {
        self.process.savedata_dir_count()
    }

    fn virtual_query(&self, addr: u64, find_next: bool) -> Option<ps4_core::kernel::VqRegion> {
        self.process.virtual_query(addr, find_next)
    }
}

#[cfg(test)]
mod tests {
    use super::alloc_io_scratch;

    #[test]
    fn scratch_of_reasonable_len_is_zeroed() {
        let buf = alloc_io_scratch(4096).expect("small allocation must succeed");
        assert_eq!(buf.len(), 4096);
        assert!(buf.iter().all(|&b| b == 0));
        assert_eq!(alloc_io_scratch(0).expect("zero-len must succeed").len(), 0);
    }

    #[test]
    fn scratch_of_bogus_len_errors_instead_of_aborting() {
        // A guest read(fd, ptr, 0x0FFF_FFFF_FFFF_FFFF) previously reached
        // `vec![0u8; len]` and SIGABRTed via handle_alloc_error; now it must
        // return ENOMEM (12) without allocating ~1 EiB.
        assert_eq!(alloc_io_scratch(0x0FFF_FFFF_FFFF_FFFF), Err(12));
        assert_eq!(alloc_io_scratch(usize::MAX), Err(12));
    }
}
