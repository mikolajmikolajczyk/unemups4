use crate::process::Process;
use ps4_core::memory::{MemoryAccessExt, MemoryProtection, VirtualMemoryManager};
use ps4_cpu::{GuestExit, run_guest_call};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use tracing::{debug, error, info};

const DTOR_ITERS: usize = 4;
const STACK_SIZE: usize = 0x200_000;

#[derive(Debug)]
pub struct Thread {
    pub id: u32,
    pub stack_base: u64,
    pub stack_size: usize,
    pub tls_base: u64,
    /// Guest address of this thread's errno slot (inside the TLS allocation, past the
    /// TCB). Handed to `run_guest_call` so `__error`/`errno` returns a guest pointer.
    pub errno_base: u64,
    pub entry_point: u64,
    pub entry_argument: u64,
    pub host_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    pub is_main: bool,
    pub process: std::sync::Weak<Process>,

    pub exited: AtomicBool,
    pub exit_value: AtomicU64,
    pub exit_mu: Mutex<()>,
    pub exit_cv: Condvar,

    pub tls_specific: Mutex<Vec<u64>>,

    pub detached: AtomicBool,
    pub name: Mutex<String>,
}

impl Thread {
    pub fn execute(self: &Arc<Self>) -> std::io::Result<()> {
        let this = self.clone();
        let builder = std::thread::Builder::new().name(format!("PS4_Thread_{}", this.id));

        let tid = this.id;
        let handle = builder.spawn(move || {
            // Per-guest-thread span: a long-lived parent for every zone this
            // thread emits (syscalls, nested calls). Cheap callsite check with no
            // span-consuming layer; under Tracy it gives each guest thread its own lane.
            let _guest_thread = tracing::info_span!("guest_thread", tid).entered();
            ps4_core::kernel::set_current_tid(this.id);
            ps4_core::kernel::set_current_stack(this.stack_base, this.stack_size as u64);
            info!(
                "Kernel: current_tid set to {}",
                ps4_core::kernel::current_tid()
            );

            let Some(proc) = this.process.upgrade() else {
                error!("Kernel: Thread {} started but Process is gone", this.id);
                return;
            };
            let vm = Arc::clone(&proc.guest_vm);
            // Dependency `module_start` addresses (leaves-first) to run before the eboot
            // entry — only the main thread bootstraps modules. Snapshot now, then drop the
            // Process ref so guest execution below holds no lock.
            let module_inits: Vec<u64> = if this.is_main {
                proc.module_inits.read().unwrap().clone()
            } else {
                Vec::new()
            };
            // Main-thread pthread pointer passed as module_start's arg0 (see Process).
            let main_pthread = proc
                .main_thread_pthread
                .load(std::sync::atomic::Ordering::Relaxed);
            drop(proc);

            // Compute the initial guest stack pointer. `run_guest_call` pushes the gadget
            // return address at rsp-8, so the callee sees a 16-aligned frame per SysV; we
            // hand it a 16-aligned `start_rsp` (leaving a 128-byte red-zone gap below the
            // top).
            let stack_top = this.stack_base + this.stack_size as u64;
            let mut rsp = stack_top;
            rsp &= !0xF;
            rsp -= 8;
            rsp -= 128;
            rsp &= !0xF;
            let start_rsp = rsp;

            // Main thread: the guest gets RDI = RSP (a pointer to the on-stack argv/env
            // frame). Worker threads: RDI = the entry argument. FS base = the thread's
            // guest TLS base, installed directly into the Vcpu's `Reg::FsBase` — the host
            // FS is untouched, so Rust TLS keeps working (doc-1 dec 4).
            let rdi = if this.is_main {
                start_rsp
            } else {
                this.entry_argument
            };

            // Bootstrap each dependency module before the eboot entry: call its
            // `module_start(main_pthread, ...)` on this (main) thread's stack, leaves-first.
            // PS4 modules initialize their own globals here; skipping this leaves e.g. libc
            // globals null and the eboot CRT faults on first use. Each call is a self-contained
            // `run_guest_call` (returns via the HLT gadget), so the same stack is reused.
            // KNOWN LIMITATION (task-117): this runs inside the thread run-loop. A
            // module_start that spawns a worker (scePthreadCreate) races: the worker runs
            // before later leaves-first module_starts; and a Fatal here is logged, not fatal
            // to bring-up. Move module init to an explicit process bootstrap phase.
            for (i, &module_start) in module_inits.iter().enumerate() {
                info!(
                    "Kernel: module_start[{}/{}] -> {:#x}",
                    i + 1,
                    module_inits.len(),
                    module_start
                );
                match run_guest_call(
                    &vm,
                    module_start,
                    start_rsp,
                    main_pthread, // module_start(main-thread pthread ptr, ...)
                    this.tls_base,
                    this.errno_base,
                ) {
                    GuestExit::Returned(rax) => {
                        info!("Kernel: module_start {:#x} returned {}", module_start, rax)
                    }
                    GuestExit::ThreadExit(v) => {
                        info!("Kernel: module_start {:#x} thread-exit {}", module_start, v)
                    }
                    GuestExit::Fatal(msg) => {
                        error!("Kernel: module_start {:#x} fatal: {}", module_start, msg)
                    }
                }
            }

            info!("Kernel: Thread {} jumping...", this.id);

            let real_exit_value = match run_guest_call(
                &vm,
                this.entry_point,
                start_rsp,
                rdi,
                this.tls_base,
                this.errno_base,
            ) {
                GuestExit::Returned(rax) => rax,
                GuestExit::ThreadExit(value) => value,
                GuestExit::Fatal(msg) => {
                    error!("Kernel: Thread {} guest execution fatal: {}", this.id, msg);
                    0
                }
            };

            this.run_tls_destructors_on_exit(&vm, start_rsp);

            {
                let _g = this.exit_mu.lock().unwrap();
                this.exit_value.store(real_exit_value, Ordering::Release);
                this.exited.store(true, Ordering::Release);
            }
            this.exit_cv.notify_all();

            if this.detached.load(Ordering::Acquire)
                && let Some(proc) = this.process.upgrade()
            {
                let mut threads = proc.thread_manager.threads.write().unwrap();
                threads.remove(&this.id);
                info!("Kernel: Detached Thread {} cleaned up.", this.id);
            }
        })?;
        *self.host_thread.lock().unwrap() = Some(handle);
        Ok(())
    }

    pub fn tls_set_specific(&self, key: u32, value: u64) -> Result<(), u64> {
        let mut v = self.tls_specific.lock().unwrap();
        let idx = key as usize;
        if idx >= v.len() {
            v.resize(idx + 1, 0);
        }
        v[idx] = value;
        Ok(())
    }

    pub fn tls_get_specific(&self, key: u32) -> u64 {
        let v = self.tls_specific.lock().unwrap();
        v.get(key as usize).copied().unwrap_or(0)
    }

    /// Run pending TLS key destructors on thread exit. Each destructor is a guest
    /// function `dtor(value)`; we invoke it as its own top-level [`run_guest_call`] on the
    /// thread's stack (there is no live guest frame to nest under here — the thread's main
    /// call already returned). Full validation is deferred; this keeps the path exercised
    /// for the simple examples without regressing compilation.
    fn run_tls_destructors_on_exit(&self, vm: &Arc<ps4_cpu::GuestVm>, start_rsp: u64) {
        let Some(proc) = self.process.upgrade() else {
            return;
        };

        for _ in 0..DTOR_ITERS {
            let mut did_work = false;
            let max = proc.tls_keys_max();

            // Collect the (dtor_ptr, value) pairs to run, then drop the slots lock before
            // re-entering guest code (a destructor may touch TLS again).
            let mut to_run: Vec<(u64, u64)> = Vec::new();
            {
                let mut slots = self.tls_specific.lock().unwrap();
                if slots.len() < max {
                    slots.resize(max, 0);
                }
                for key in 0..max {
                    let val = slots[key];
                    if val == 0 {
                        continue;
                    }
                    let Some(dtor_ptr) = proc.tls_key_destructor(key as u32) else {
                        continue;
                    };
                    slots[key] = 0;
                    to_run.push((dtor_ptr, val));
                }
            }

            for (dtor_ptr, val) in to_run {
                did_work = true;
                debug!(
                    "Kernel: Thread {} running TLS destructor {:#x} with value {:#x}",
                    self.id, dtor_ptr, val
                );
                match run_guest_call(vm, dtor_ptr, start_rsp, val, self.tls_base, self.errno_base) {
                    GuestExit::Returned(_) | GuestExit::ThreadExit(_) => {}
                    GuestExit::Fatal(msg) => {
                        error!("Kernel: Thread {} TLS destructor fatal: {}", self.id, msg);
                    }
                }
            }

            if !did_work {
                break;
            }
        }
    }
}

pub struct ThreadManager {
    pub threads: RwLock<HashMap<u32, Arc<Thread>>>,
    next_tid: AtomicU32,
}

impl ThreadManager {
    pub fn new() -> Self {
        Self {
            next_tid: AtomicU32::new(1),
            threads: RwLock::new(HashMap::new()),
        }
    }

    pub fn create_thread(
        &self,
        entry_point: u64,
        arg: u64,
        process_weak: std::sync::Weak<Process>,
        memory: &mut dyn VirtualMemoryManager,
    ) -> Result<Arc<Thread>, Box<dyn std::error::Error>> {
        let tid = self.next_tid.fetch_add(1, Ordering::Relaxed);
        info!("Kernel: Creating Thread {} entry={:#x}", tid, entry_point);

        let stack_size: usize = STACK_SIZE;
        let stack_base = memory.map(
            0,
            stack_size,
            MemoryProtection::READ | MemoryProtection::WRITE,
            Some(&format!("Stack_TID_{}", tid)),
        )?;

        // tls size and content
        let (tls_data_size, tls_init_data, tls_align) = if let Some(proc) = process_weak.upgrade() {
            let lock = proc.tls_template.read().unwrap();
            if let Some(info) = lock.as_ref() {
                (info.mem_size, Some(info.data.clone()), info.align)
            } else {
                (0, None, 16)
            }
        } else {
            (0, None, 16)
        };

        // FreeBSD/PS4 variant II TLS layout
        // Memory: | ... TLS Data ... | ... Padding ... | TCB |
        //         ^                  ^                 ^
        //         |                  |                 FS_BASE
        //      tls_base          aligned_end

        let align_mask = if tls_align > 0 { tls_align - 1 } else { 15 };
        let aligned_tls_size = (tls_data_size as u64 + align_mask) & !align_mask;

        // TCB struct lives at FS:0; 0x200 covers self/dtv pointers
        let tcb_size = 0x200;

        // A guest-resident errno slot sits just past the TCB, inside the same mapped
        // allocation, so `__error`/`errno` can hand the guest a dereferenceable pointer
        // (a host static pointer would trap UnmappedMemory under x86jit).
        // 16 bytes keeps the layout 16-aligned; only the first u64 is used.
        let errno_slot_size = 16;

        let total_size = aligned_tls_size as usize + tcb_size + errno_slot_size;

        // page align
        let alloc_size = (total_size + 0x3FFF) & !0x3FFF;

        let allocation_base = memory.map(
            0,
            alloc_size,
            MemoryProtection::READ | MemoryProtection::WRITE,
            Some(&format!("TLS_TID_{}", tid)),
        )?;

        // init tls data at start of allocation
        if let Some(data) = tls_init_data {
            memory
                .write_bytes(allocation_base, &data)
                .map_err(|e| format!("TLS Write failed: {}", e))?;

            // zero tbss (after data, before padding)
            let tbss_size = tls_data_size - data.len();
            if tbss_size > 0 {
                memory
                    .zero_memory(allocation_base + data.len() as u64, tbss_size)
                    .map_err(|e| format!("TLS Zero failed: {}", e))?;
            }
        }

        // TCB sits after tls data; FS points to its start
        let fs_base = allocation_base + aligned_tls_size;

        // FS:[0] must point to itself (t_self)
        memory
            .write::<u64>(fs_base, fs_base)
            .map_err(|e| format!("TCB Init failed: {}", e))?;

        // Guest errno slot immediately after the TCB; zero-initialized (nothing writes
        // errno today — handlers return negative errno as their value — so the CRT reads 0).
        let errno_base = fs_base + tcb_size as u64;
        memory
            .write::<u64>(errno_base, 0u64)
            .map_err(|e| format!("errno slot init failed: {}", e))?;

        tracing::debug!(
            "Kernel: Thread {} TLS Setup: Alloc {:#x}, Size {:#x}, FS_BASE {:#x}",
            tid,
            allocation_base,
            alloc_size,
            fs_base
        );

        let is_main = tid == 1;
        let thread = Arc::new(Thread {
            id: tid,
            stack_base,
            stack_size,
            tls_base: fs_base,
            errno_base,
            entry_point,
            entry_argument: arg,
            host_thread: Mutex::new(None),
            is_main,
            exited: AtomicBool::new(false),
            exit_value: AtomicU64::new(0),
            exit_mu: Mutex::new(()),
            exit_cv: Condvar::new(),
            tls_specific: Mutex::new(Vec::new()),
            process: process_weak,
            detached: AtomicBool::new(false),
            name: Mutex::new(format!("Thread-{}", tid)),
        });
        self.threads.write().unwrap().insert(tid, thread.clone());
        Ok(thread)
    }

    pub fn join_thread(&self, tid: u32) -> Result<u64, u64> {
        let thread = {
            let threads = self.threads.read().map_err(|_| 0x80020001u64)?;
            threads.get(&tid).cloned()
        }
        .ok_or(0x80020002u64)?;
        info!("Kernel: Joining thread {}...", tid);

        if !thread.exited.load(Ordering::Acquire) {
            let mut g = thread.exit_mu.lock().map_err(|_| 0x80020001u64)?;
            while !thread.exited.load(Ordering::Acquire) {
                g = thread.exit_cv.wait(g).map_err(|_| 0x80020001u64)?;
            }
        }

        let exit_value = thread.exit_value.load(Ordering::Acquire);
        let handle_opt = {
            let mut guard = thread.host_thread.lock().map_err(|_| 0x80020001u64)?;
            guard.take()
        };

        if let Some(handle) = handle_opt
            && handle.join().is_err()
        {
            error!("Kernel: Thread {} panicked!", tid);
            return Err(0x80020001u64);
        }

        info!("Kernel: Thread {} joined successfully.", tid);
        Ok(exit_value)
    }

    pub fn current_thread(&self) -> Result<Arc<Thread>, u64> {
        let tid = ps4_core::kernel::current_tid();
        let threads = self.threads.read().map_err(|_| 0x80020001u64)?;
        threads.get(&tid).cloned().ok_or(0x80020002u64)
    }
    pub fn thread_detach(&self, tid: u32) -> Result<i32, u64> {
        let thread = {
            let threads = self.threads.read().unwrap();
            threads.get(&tid).cloned().ok_or(3u64)? // ESRCH (No such process)
        };

        thread.detached.store(true, Ordering::Release);

        let mut handle_opt = thread.host_thread.lock().unwrap();
        *handle_opt = None;

        Ok(0)
    }

    pub fn thread_yield(&self) {
        std::thread::yield_now();
    }

    pub fn thread_self(&self) -> u32 {
        ps4_core::kernel::current_tid()
    }

    pub fn thread_equal(&self, t1: u32, t2: u32) -> i32 {
        if t1 == t2 { 1 } else { 0 }
    }

    pub fn thread_set_name(&self, tid: u32, name: &str) -> Result<i32, u64> {
        let target_tid = if tid == 0 { self.thread_self() } else { tid };

        let thread = {
            let threads = self.threads.read().unwrap();
            threads.get(&target_tid).cloned().ok_or(3u64)?
        };

        *thread.name.lock().unwrap() = name.to_string();
        Ok(0)
    }
    pub fn thread_get_name(
        &self,
        tid: u32,
        out_ptr: u64,
        len: usize,
        memory: &dyn VirtualMemoryManager,
    ) -> Result<i32, u64> {
        let target_tid = if tid == 0 { self.thread_self() } else { tid };

        let thread = {
            let threads = self.threads.read().unwrap();
            threads.get(&target_tid).cloned().ok_or(3u64)?
        };

        let name = thread.name.lock().unwrap();
        let bytes = name.as_bytes();

        let copy_len = std::cmp::min(len - 1, bytes.len());

        memory
            .write_bytes(out_ptr, &bytes[..copy_len])
            .map_err(|_| 14u64)?; // EFAULT
        memory
            .write(out_ptr + copy_len as u64, 0u8)
            .map_err(|_| 14u64)?;

        Ok(0)
    }

    pub fn thread_cancel(&self, _tid: u32) -> Result<i32, u64> {
        // no safe forced cancellation in rust; return ENOTSUP
        Ok(95)
    }
}
