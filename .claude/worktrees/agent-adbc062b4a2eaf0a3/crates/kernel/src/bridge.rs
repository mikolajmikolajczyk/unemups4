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

    fn mutex_init(&self, ptr: u64, recursive: bool) -> Result<i32, u64> {
        self.process.sync_manager.mutex_init(ptr, recursive)
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
    fn mutex_timedlock(&self, mutex: u64, abs: u64) -> Result<i32, u64> {
        self.process.mutex_timedlock(mutex, abs)
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
        let mut buf = vec![0u8; len];
        let bytes_read = self.process.fs.read(fd, &mut buf)?;
        let memory = self.process.memory.write().unwrap();
        match memory.write_bytes(ptr, &buf[0..bytes_read]) {
            Ok(_) => Ok(bytes_read),
            Err(_) => Err(14),
        }
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

    fn munmap(&self, addr: u64, len: usize) -> Result<i32, i64> {
        self.process.munmap(addr, len)
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
        _count: i32,
        _attr_ptr: u64,
    ) -> Result<i32, i32> {
        if let Some(gpu) = &self.process.gpu_manager {
            // ptr is 'void* list[]'; read the first pointer (PS4 pointers are 64-bit)
            let memory = self.process.memory.read().unwrap();

            // little-endian (x86)
            let actual_buffer_addr = match memory.read::<u64>(ptr) {
                Ok(addr) => addr,
                Err(_) => return Err(14), // EFAULT
            };

            info!(
                "[KERNEL] RegisterBuffer: ArrayPtr={:#x} -> FrameBuffer={:#x}",
                ptr, actual_buffer_addr
            );

            gpu.register_buffer(actual_buffer_addr, 1920, 1080, handle, start_index as u32);
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
        if let Some(gpu) = &self.process.gpu_manager {
            // blocks the guest thread until the window thread catches up
            gpu.submit_flip(handle, index as u32);
        }
        Ok(0)
    }
    fn pad_get_state(&self, _handle: i32) -> ps4_core::pad::PadState {
        *self.process.input_manager.state.read().unwrap()
    }
}
