//! On-demand GPU state snapshot (task-185): the maintainer presses a key, and one complete
//! frame of GPU state lands on disk.
//!
//! ## Why this exists
//!
//! Every retail GPU wall so far was attacked with an env-gated probe bolted onto the draw
//! path for that one investigation. task-179 showed the failure mode: two probe-derived
//! measurements were wrong, and both were wrong in the direction that flattered the
//! hypothesis under test. A probe answers the question you already thought to ask, which is
//! precisely the question a stuck investigation has wrong. This module answers no question:
//! it dumps EVERYTHING the executor holds for a frame, so the evidence exists before the
//! hypothesis does, and so two frames (or ours vs the real-hardware oracle) can be diffed.
//!
//! Three rules follow from that history and are load-bearing:
//!
//! 1. **Nothing derived is presented as observed.** Every field is labelled by where it came
//!    from — a raw guest descriptor is dumped decoded but is still the guest's descriptor,
//!    and a value we computed (the derived target extent, the pipeline key) is named as
//!    ours. A snapshot that quietly substituted a plausible guess for a value it could not
//!    reach would recreate the exact problem this tool exists to eliminate.
//! 2. **Unknown is dumped, not skipped.** Registers with no name in
//!    [`crate::pm4::opcodes::reg_name`] are written with their raw index. Three of the four
//!    registers that mattered in task-179 were ones nothing on our side read.
//! 3. **Capturing never perturbs the CONTENT of the capture.** Everything in the default
//!    capture reads shadow state the executor already holds, plus guest memory through the
//!    bounded seam — nothing is synchronised and no backend command is emitted, so a dumped
//!    frame renders identically to an undumped one.
//!
//!    The opt-in render-target dump (task-187, `UNEMUPS4_SNAPSHOT_RENDER_TARGETS`) does emit
//!    one command, and it is worth being exact about what that costs. Copying an RT makes
//!    the display thread wait on the GPU, so it perturbs frame **TIMING**. It does not
//!    perturb frame **CONTENT**: the copy runs after the frame's passes are recorded and its
//!    fence waited, changes no draw, binding, register or guest byte, and restores the
//!    image's layout. The pixels a dumped frame shows are the pixels it would have shown.
//!
//!    That dump is deliberately NOT `UNEMUPS4_RT_READBACK`, which is a different thing for a
//!    different purpose: the readback writes a target back into GUEST memory in the GUEST's
//!    tiled layout, and since task-181 it correctly REFUSES the 2D macro-tiled surfaces every
//!    Celeste RT uses, because this repo has no macro-tiler. Looking at pixels needs no
//!    tiling at all — the host image is linear RGBA8 — so the diagnostic path takes the host
//!    image and touches guest memory not at all. Keeping them as two paths is what stops the
//!    diagnostic from inheriting a refusal it never needed.
//!
//! ## How a capture is triggered
//!
//! The request crosses threads as a single atomic — see [`ps4_core::snapshot`] for why that
//! is a hard requirement and not a stylistic one (the display thread must never take the
//! `driver()` lock). The display thread deposits a frame budget; [`on_frame_boundary`] runs
//! on the guest submit thread, claims one frame from that budget, and arms the recorder for
//! the frame that follows.
//!
//! **`F10` captures the NEXT complete frame, not the elapsed part of the current one.** By
//! the time a keypress is observed, an arbitrary prefix of the in-flight frame's draws has
//! already been recorded and shipped; there is nowhere to retrieve them from. Arming at the
//! next boundary is the only way to produce a frame that is actually complete, and a
//! half-frame labelled as a frame is precisely the kind of plausible-looking artefact rule 1
//! forbids. In practice the difference is one frame of a 60 Hz game.
//!
//! ## What a capture contains
//!
//! ```text
//! <root>/frame-NNNNN/registers.json   full shadow register file, END OF FRAME
//! <root>/frame-NNNNN/draws.json       per draw: derived state, decoded blend, T#/V#/S#,
//!                                     register DELTA vs the previous draw, and the
//!                                     deferred draws with their reason
//! <root>/frame-NNNNN/summary.txt      one screen, human-readable
//! <root>/frame-NNNNN/render-targets/rt-<base>-<w>x<h>.png   offscreen RT pixels (opt-in)
//! <root>/shaders/<stage>-<hash>.spv   the SPIR-V module handed to Vulkan
//! <root>/shaders/<stage>-<hash>.sb    the raw GCN code it was recompiled from
//! <root>/shaders/<stage>-<hash>.txt   in-tree disassembly (the .spv is authoritative)
//! <root>/textures/<hash>.raw   sampled guest texels, still tiled (opt-in)
//! <root>/textures/<hash>-<layout>.{detiled.raw,png}   detiled per surface layout (opt-in)
//! ```
//!
//! `shaders/` and `textures/` sit at the CAPTURE root, not under a frame, and are deduped
//! across the whole session: an eight-frame burst binds the same handful of shaders every
//! frame, and 23 copies of one module per frame hides the one that differs.
//!
//! `render-targets/` is the opposite and sits UNDER the frame, deduped only within it: a
//! render target's contents are precisely what differs between the frames of a burst, so
//! session-wide dedupe would collapse an eight-frame capture to one picture.
//!
//! ## Zero cost when idle
//!
//! [`Recorder::armed`] is a plain `bool` read on the per-draw path; when it is false the
//! draw path allocates nothing, formats nothing, and reads no memory. The only per-frame
//! cost is the one relaxed atomic load in [`ps4_core::snapshot::take_frame`], at the flip
//! boundary — not per draw.
//!
//! ## Cost while capturing
//!
//! Dumps run on the guest SUBMIT thread, under the `driver()` lock: the guest is stopped for
//! however long a capture takes. Two things follow.
//!
//! * Anything that must observe guest state AT THE MOMENT THE DRAW RAN happens on that
//!   thread and is paid for there — the register copy, the constant-buffer window, and the
//!   texture read. That is unavoidable; a read deferred to another thread would report a
//!   later frame's memory.
//! * Everything else — SPIR-V, `.sb`, disassembly text, and megabytes of texture texels —
//!   is handed to a background writer thread as owned bytes ([`enqueue`]). File I/O and PNG
//!   encoding never run under the lock. [`flush_writes`] waits for the queue when someone
//!   needs the files to exist right now; nothing in the run path waits.
//!
//! Texture dumping is the one part whose submit-thread cost is measured in tens of
//! milliseconds rather than microseconds, which is why it is off by default.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use ps4_core::gpu::{PipelineKey, SamplerDesc, TargetDesc, TargetKind, TextureBinding};
use ps4_core::memory::VirtualMemoryManager;

use crate::cache::SurfaceLayout;
use crate::derive::{DrawState, Scissor, Viewport};
use crate::shader::source::{ShaderRef, Stage};
use crate::state::{GpuState, RegisteredRt};
use crate::vbuf::{BufferDesc, BufferRange, SamplerState, TextureBindingRange, TextureDesc};

/// How many bytes of each constant buffer are dumped. A constant buffer is a guest
/// allocation of arbitrary size, and the thing that goes wrong with one is almost always in
/// its first few vectors (a transform matrix, a texel step, an export colour), so a bounded
/// window keeps a burst capture to a sane size on disk. The record also carries the V#'s
/// FULL span, so a truncated dump is visibly truncated rather than silently mistaken for the
/// whole buffer.
const CONST_BUFFER_DUMP_BYTES: u64 = 512;

/// How many bytes of a shader's `.sb` container are read when its header cannot be parsed.
/// A parseable `.sb` reports its own exact code range and this is unused; a rejected one still
/// wants *something* to look at, and 8 KiB is the window the ad-hoc `UNEMUPS4_DUMP_PS` probes
/// used, which is empirically enough for a retail Celeste shader.
const SHADER_FALLBACK_DUMP_BYTES: usize = 8192;

// ---------------------------------------------------------------------------------------
// Background writer
//
// A capture writes on the guest SUBMIT thread, under the `driver()` lock — the guest is
// stopped for as long as it takes. That was acceptable when a frame was three small JSON
// files (a few ms). It stops being acceptable once a frame can carry several megabytes of
// detiled texture: PNG encoding plus the file write would stall the guest for hundreds of
// milliseconds per frame, and an F9 burst would look like a hang.
//
// So the submit thread does only what MUST happen there — reading guest memory at the
// moment the draw ran — and hands the owned bytes to a single background writer thread. The
// thread is lazily spawned on the first capture and lives for the process; a single thread
// (not one per frame) keeps a burst's writes ordered and its memory bounded by what the
// submit thread has already read.
//
// `flush_writes` exists so tests (and anything that needs the files to be on disk NOW) can
// wait for the queue to drain. Nothing in the run path waits on it.
// ---------------------------------------------------------------------------------------

/// One unit of work for the background writer.
enum WriteJob {
    /// Write `bytes` verbatim to `path`, creating parent directories.
    File { path: PathBuf, bytes: Vec<u8> },
    /// Encode `pixels` (row-major RGBA8, `w * h * 4` bytes) as a PNG at `path`.
    Png {
        path: PathBuf,
        w: u32,
        h: u32,
        pixels: Vec<u8>,
    },
    /// Signal the sender once every job queued before this one has been written.
    Flush(std::sync::mpsc::SyncSender<()>),
}

/// The lazily-spawned writer channel. `OnceLock` rather than a `Mutex<Option<..>>` so the
/// steady-state cost of enqueueing is a load, and so a capture never contends with itself.
static WRITER: std::sync::OnceLock<std::sync::mpsc::Sender<WriteJob>> = std::sync::OnceLock::new();

/// Queue one job for the background writer, spawning it on first use.
///
/// Never blocks: the channel is unbounded, and the only thing bounding memory here is that
/// the submit thread has already paid to read every byte it enqueues (plus the per-texture
/// size cap and the content-hash dedupe upstream).
fn enqueue(job: WriteJob) {
    let tx = WRITER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<WriteJob>();
        // Detached: it must outlive any one capture, and there is no shutdown point in the
        // executor to join it at. It exits when the process does (or when the sender is
        // dropped, which never happens — the sender is in a `static`).
        std::thread::Builder::new()
            .name("gpu-snapshot-writer".into())
            .spawn(move || {
                for job in rx {
                    match job {
                        WriteJob::File { path, bytes } => {
                            if let Some(parent) = path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            if let Err(e) = std::fs::write(&path, &bytes) {
                                tracing::warn!("[SNAPSHOT] write {} failed: {e}", path.display());
                            }
                        }
                        WriteJob::Png { path, w, h, pixels } => {
                            if let Some(parent) = path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            if let Err(e) = crate::texdump::write_rgba_png(&path, w, h, &pixels) {
                                tracing::warn!("[SNAPSHOT] PNG {} failed: {e}", path.display());
                            }
                        }
                        // Rendezvous: the reply lands only after every earlier job in this
                        // FIFO has been written. A dropped receiver (a waiter that gave up)
                        // is not an error.
                        WriteJob::Flush(reply) => {
                            let _ = reply.send(());
                        }
                    }
                }
            })
            // Fire-and-forget (task-66): a spawn failure (near RLIMIT_NPROC / OOM) must NEVER
            // take down the guest submit or display thread — a dump is diagnostic
            // (`on_frame_boundary`: "A dump failure must never take the run down with it"). On
            // failure `.ok()` drops the receiver, so the returned sender is disconnected and every
            // `send` below cleanly logs-and-drops the job instead of panicking.
            .ok();
        tx
    });
    if tx.send(job).is_err() {
        tracing::warn!("[SNAPSHOT] writer thread is gone; dropping a dump");
    }
}

/// Queue a PNG for the background writer from OUTSIDE this crate.
///
/// The one caller is the Vulkan backend's render-target dump (task-187), which runs on the
/// DISPLAY thread and holds a freshly-copied linear RGBA8 image. It exists so that path
/// reuses this module's single writer thread instead of doing file I/O and PNG encoding on
/// the display thread — the thread that must stay responsive and must never block on the
/// guest (task-66).
///
/// `pixels` must be `w * h * 4` row-major RGBA8. Never blocks.
pub fn enqueue_png(path: PathBuf, w: u32, h: u32, pixels: Vec<u8>) {
    enqueue(WriteJob::Png { path, w, h, pixels });
}

/// Block until every dump queued so far has hit disk.
///
/// Not called from the run path — a capture is fire-and-forget so the guest never waits on
/// I/O. It exists for tests, which assert on files, and for any future explicit "flush before
/// you go look at it" step. Returns immediately if no capture has ever run.
pub fn flush_writes() {
    let Some(tx) = WRITER.get() else {
        return;
    };
    let (reply, wait) = std::sync::mpsc::sync_channel(0);
    if tx.send(WriteJob::Flush(reply)).is_ok() {
        let _ = wait.recv();
    }
}

/// FNV-1a over a byte slice — the content hash that dedupes texture dumps.
///
/// Not cryptographic and not trying to be: it names a blob for reuse within one capture
/// session, where a collision costs a wrong filename and nothing else. Chosen to match the
/// hash [`crate::derive`] already uses for shader identity, so the codebase has one shape of
/// "cheap stable id" rather than two.
fn content_hash(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// The four shadow register banks, in a fixed order. The bank tag is part of a register's
/// identity: the same absolute index means different things in different windows.
const BANKS: [&str; 4] = ["context", "sh", "uconfig", "config"];

/// A flat, sorted copy of every register the guest has written, across all four banks.
///
/// Taken PER DRAW when a capture is armed. `registers.json` is written at frame end and so
/// reports only the final value of anything the guest reprograms between draws — which
/// silently misrepresents every draw but the last. task-179 was exactly that: a per-draw
/// `SPI_PS_INPUT_CNTL_n` change that an end-of-frame register file cannot show. The previous
/// round patched around it by copying one register (`ps_input_map`) onto the draw record;
/// this generalises it to the whole file.
///
/// Sorted `(bank, index, value)` so [`RegSnapshot::delta`] is a linear merge and two captures
/// of identical state produce byte-identical output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegSnapshot {
    entries: Vec<(u8, u32, u32)>,
}

impl RegSnapshot {
    /// Copy the live register file. Only ever called behind [`Recorder::armed`].
    pub fn capture(state: &GpuState) -> Self {
        let banks: [&crate::state::RegFile; 4] = [
            &state.ctx_regs,
            &state.sh_regs,
            &state.uconfig_regs,
            &state.config_regs,
        ];
        let mut entries: Vec<(u8, u32, u32)> = banks
            .iter()
            .enumerate()
            .flat_map(|(bank, regs)| regs.iter().map(move |(i, v)| (bank as u8, i, v)))
            .collect();
        entries.sort_unstable_by_key(|&(b, i, _)| (b, i));
        Self { entries }
    }

    /// Every register that differs from `prev`: newly written, or written with a new value.
    ///
    /// A register present in `prev` and absent here cannot happen short of `IT_CLEAR_STATE`;
    /// when it does, it is reported with `value: null` rather than dropped, because "the guest
    /// cleared the bank" is a finding.
    fn delta(&self, prev: &RegSnapshot) -> Vec<RegChange> {
        let mut out = Vec::new();
        let (mut a, mut b) = (0usize, 0usize);
        while a < self.entries.len() || b < prev.entries.len() {
            let cur = self.entries.get(a);
            let old = prev.entries.get(b);
            match (cur, old) {
                (Some(&(cb, ci, cv)), Some(&(pb, pi, pv))) => match (cb, ci).cmp(&(pb, pi)) {
                    std::cmp::Ordering::Less => {
                        out.push(RegChange::new(cb, ci, None, Some(cv)));
                        a += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        out.push(RegChange::new(pb, pi, Some(pv), None));
                        b += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        if cv != pv {
                            out.push(RegChange::new(cb, ci, Some(pv), Some(cv)));
                        }
                        a += 1;
                        b += 1;
                    }
                },
                (Some(&(cb, ci, cv)), None) => {
                    out.push(RegChange::new(cb, ci, None, Some(cv)));
                    a += 1;
                }
                (None, Some(&(pb, pi, pv))) => {
                    out.push(RegChange::new(pb, pi, Some(pv), None));
                    b += 1;
                }
                (None, None) => break,
            }
        }
        out
    }
}

/// One register that changed between two consecutive draws.
#[derive(Debug, Clone)]
struct RegChange {
    /// Index into [`BANKS`].
    bank: u8,
    /// Absolute register index.
    index: u32,
    /// Value at the previous draw, or `None` if this register had never been written.
    from: Option<u32>,
    /// Value at this draw, or `None` if the bank was cleared.
    to: Option<u32>,
}

impl RegChange {
    fn new(bank: u8, index: u32, from: Option<u32>, to: Option<u32>) -> Self {
        Self {
            bank,
            index,
            from,
            to,
        }
    }
}

/// A draw that never reached the backend, and why.
///
/// `setup_draw` has about a dozen clean-defer exits (unsupported shader, unresolvable T#/V#,
/// macro-tiled texture, more than one sampler, …). Before this the snapshot recorded the
/// ABSENCE — a draw that simply is not in `draws.json` — while the reason lived only in a
/// `tracing::debug!` line, so reading a capture meant correlating it against a log. A missing
/// draw is one of the commonest causes of a missing picture, and the correlation is precisely
/// the work this tool exists to remove.
#[derive(Debug, Clone)]
struct DeferredRecord {
    /// Position in the frame's DEFERRED sequence. Deliberately a separate counter from
    /// [`DrawRecord::ordinal`]: a deferred draw was never submitted, so numbering it in the
    /// submitted sequence would misstate what the backend saw.
    ordinal: u32,
    /// Which draw packet would have been issued.
    kind: String,
    /// Vertex/index count the packet carried.
    count: u32,
    /// Stable slug naming the exit that fired (`"macro-tiled-texture"`,
    /// `"unsupported-gcn-shader"`, …) — the coarse, greppable category, kept so the summary
    /// can still group defers by cause. Owned (`String`) so both a `&'static str` slug and a
    /// future owned reason fit the one field (task-195).
    reason: String,
    /// How many submitted draws preceded it, so a reader can place the defer in the frame.
    after_draw: u32,
    /// For a `.sb`-GCN-shader recompile defer: the exact failing instruction, the stage, and
    /// the shader identity — the detail that used to live only in `/tmp/unemups4.log`
    /// (task-195). `None` for coarse defers (unbound shader, vbuf/const-buffer exits, …) that
    /// have no single offending instruction.
    detail: Option<DeferDetail>,
}

/// The per-instruction detail of an unsupported-`.sb`-GCN-shader defer (task-195): parity with
/// the x86jit model where the decoder decodes the whole shader, the lifter handles a subset,
/// and the exact unlifted instruction is visible in the capture rather than only in the log.
#[derive(Debug, Clone)]
struct DeferDetail {
    /// Which HW stage's recompile failed — `"VS"` or `"PS"`.
    stage: &'static str,
    /// Guest `.sb` code address of the failing shader — the value to hand a disassembler.
    shader_addr: u64,
    /// Its identity hash (the same value [`ShaderIdent::hash`] carries for a submitted draw),
    /// so a deferred shader and a later-resolved one can be correlated.
    shader_hash: u64,
    /// The decoded unsupported instruction + its dword offset — the recompiler's
    /// [`ps4_gcn::RecompileError`] `Display` (e.g. `"unsupported instruction at dword offset
    /// 51: …"`). `None` for a coarser recompile-path defer (parse reject, unmodeled stage,
    /// unreadable fetch shader) with no single instruction. Formatted ONLY when armed.
    instruction: Option<String>,
}

/// The per-draw state one draw contributed, captured while the draw was being set up.
///
/// Owned data only: the frame is written after the submit walk finishes, so nothing here may
/// borrow from the executor's per-submit locals.
#[derive(Debug, Clone)]
struct DrawRecord {
    /// Position of this draw within the captured frame, counting from 0 across all submits.
    ordinal: u32,
    /// Which draw packet produced it (`DrawIndexAuto`, `DrawIndex2`, `DrawIndexOffset`).
    kind: String,
    /// Vertex or index count the packet carried.
    count: u32,
    /// The target this draw derives from `CB_COLOR0_*` — OUR derivation, not a guest field.
    target: TargetDesc,
    /// The pipeline key this draw hashes to, including the blend/depth register words.
    pipeline: PipelineKey,
    /// Register-derived viewport (`PA_CL_VPORT_*`).
    viewport: Viewport,
    /// Register-derived screen scissor (`PA_SC_SCREEN_SCISSOR_*`).
    scissor: Scissor,
    /// The bound vertex shader's identity.
    vs: ShaderIdent,
    /// The bound pixel shader's identity.
    ps: ShaderIdent,
    /// Every vertex/index V# range this draw resolved, with the decoded descriptor.
    buffers: Vec<BufferRecord>,
    /// Per-stage scalar constant buffers, with a bounded window of their contents.
    const_buffers: Vec<ConstBufferRecord>,
    /// What became of this draw's TARGET as a picture: the PNG the render-target dump was
    /// asked for, or the named reason there is none (task-187). ALWAYS present — an absent
    /// render-target picture must never be readable as an empty target.
    target_dump: RtDumpOutcome,
    /// EVERY combined image-sampler this draw bound, in the shader's first-sample order
    /// (task-199). A one-texture draw has one entry; a distortion or colour-grade pass has
    /// several, and which resource landed at WHICH binding is the finding.
    sampled: Vec<SampledRecord>,
    /// Registers whose value differs from the PREVIOUS submitted draw's (the first draw of a
    /// frame diffs against an empty file, so it lists everything the guest had written).
    /// A delta rather than a full file per draw because "what changed between these two
    /// draws" is the question actually asked, and answering it directly is cheaper both to
    /// write and to read.
    reg_delta: Vec<RegChange>,
    /// For a draw whose PS declares NO sampler and DOES load constants: the first four floats
    /// of its constant buffer, which for a full-screen fill are the exported RGBA.
    ///
    /// The `(0, 0, 0, 0)` clear on two Celeste menu draws is live evidence in task-184, and it
    /// had to be dug out of the raw constant-buffer dwords by hand. It is still only a
    /// CONVENTION that a samplerless fill exports its CB's first vector — the shader could do
    /// anything — so this is labelled a heuristic in the output and the raw dwords stay
    /// alongside it. `None` when the PS samples a texture or declares no CB.
    fill_color: Option<[f32; 4]>,
}

/// A bound shader's identity as the draw path knows it.
#[derive(Debug, Clone, Default)]
struct ShaderIdent {
    /// How the shader was bound: a real `.sb` GCN binary, an embedded corpus shader, or
    /// nothing at all.
    kind: &'static str,
    /// Guest address of the `.sb` code for a `GcnBinary` bind, else `None`. THIS is the value
    /// to hand a disassembler.
    addr: Option<u64>,
    /// The stable identity hash that keys the host pipeline (`PipelineKey::vs_hash`/`ps_hash`).
    /// Two draws with the same hash resolved the same shader.
    hash: u64,
    /// Size of the recompiled SPIR-V, in 32-bit words.
    spirv_words: usize,
    /// This stage's dumped artefacts, or `None` for an unbound stage. See
    /// [`Recorder::dump_shader`] for what lands there and why the SPIR-V is the point.
    dump: Option<ShaderDumpRef>,
    /// PS only: the `SPI_PS_INPUT_CNTL_n`-derived routing this draw resolved —
    /// `ps_input_map[n]` is the VS export parameter feeding PS attribute slot `attr<n>`
    /// (task-184). Recorded per draw because it is per draw: the same PS binary under a
    /// different routing is a different module, and the routing is INVISIBLE in
    /// `registers.json`, which holds only the end-of-frame register file. A shader that
    /// derives both its sample coordinate and a scalar term from `attr0` reads something
    /// entirely different if this is off by one. `None` for a VS or an unbound stage.
    ps_input_map: Option<[u8; ps4_gcn::PS_INPUT_SLOTS]>,
}

/// Which of a shader's artefacts actually reached disk, and under what key.
///
/// The `.spv` is always written (the module is always in hand). The guest-side ones are not:
/// an embedded corpus shader has no `.sb` at all, and a `.sb` whose bytes could not be read
/// gets a note instead of code. `draws.json` names only files that exist — pointing a reader
/// at a path that was never written is the same class of lie as a plausible stand-in.
#[derive(Debug, Clone)]
struct ShaderDumpRef {
    /// Filename stem under `<root>/shaders/`, e.g. `ps-000000009afae4f0`.
    key: String,
    /// Whether `<key>.sb` (raw GCN machine code) was written.
    sb: bool,
    /// Whether `<key>.txt` (disassembly, or a note explaining its absence) was written.
    disasm: bool,
}

impl ShaderIdent {
    /// Build the identity for one stage from the bind the draw resolved plus its keyed hash.
    fn new(
        stage: Stage,
        bound: Option<ShaderRef>,
        hash: u64,
        spirv_words: usize,
        dump: Option<ShaderDumpRef>,
    ) -> Self {
        let (kind, addr) = match bound {
            Some(ShaderRef::GcnBinary { addr, .. }) => ("GcnBinary", Some(addr)),
            Some(ShaderRef::Embedded { .. }) => ("Embedded", None),
            None => ("unbound", None),
        };
        // Flatten the map to the raw per-slot locations. Only a GCN PS bind carries one; a
        // VS's is the unused default and is written as `null` rather than as a plausible
        // all-identity array nothing derived (module rule 1).
        let ps_input_map = match bound {
            Some(ShaderRef::GcnBinary { ps_input_map, .. }) if matches!(stage, Stage::Pixel) => {
                let mut m = [0u8; ps4_gcn::PS_INPUT_SLOTS];
                for (n, slot) in m.iter_mut().enumerate() {
                    *slot = ps_input_map.location_for(n as u8) as u8;
                }
                Some(m)
            }
            _ => None,
        };
        Self {
            kind,
            addr,
            hash,
            spirv_words,
            dump,
            ps_input_map,
        }
    }
}

/// One V# the draw resolved, decoded.
#[derive(Debug, Clone)]
struct BufferRecord {
    /// Guest base from the V#.
    addr: u64,
    /// Byte span the fetch may touch.
    size: u64,
    /// How the resource cache keys this range.
    layout: String,
    /// The decoded 128-bit V#.
    desc: BufferDesc,
}

/// One scalar constant buffer a stage declared, with a bounded window of its bytes.
#[derive(Debug, Clone)]
struct ConstBufferRecord {
    /// Which stage's `s_buffer_load` declared it.
    stage: &'static str,
    /// Guest base from the inline V#.
    addr: u64,
    /// FULL byte span of the V# — compare against `bytes.len()` to see whether the dump
    /// below is complete or a window onto a larger buffer.
    size: u64,
    /// The decoded inline V#.
    desc: BufferDesc,
    /// Up to [`CONST_BUFFER_DUMP_BYTES`] of contents, read through the bounded seam. Empty if
    /// the read faulted — an empty dump means "we could not read it", never "it was zero".
    bytes: Vec<u8>,
}

/// The sampled texture a draw bound, and which of the two sources it resolved to.
#[derive(Debug, Clone)]
struct SampledRecord {
    /// Descriptor set the combined image-sampler is bound at.
    set: u32,
    /// Binding index within that set.
    binding: u32,
    /// `"Plain"` — a guest-memory texture detiled and uploaded — or `"RenderTarget"` — a
    /// range a prior draw rendered into, bound host-side (task-56 RT-as-texture). Which of
    /// the two a draw takes is a recurring source of wrong pictures, so it is recorded
    /// explicitly rather than inferred from the base address.
    source: &'static str,
    /// Guest base of the sampled surface.
    base: u64,
    /// Sampled width in texels (from the T#, or the RT's derived extent).
    width: u32,
    /// Sampled height in texels.
    height: u32,
    /// The decoded T# the GUEST supplied. Recorded for BOTH sources (task-184). On the
    /// `RenderTarget` path the bind ignores it — the host RT is substituted with a fixed
    /// RGBA8 format — so this field is the only record of what the guest actually asked for.
    /// `width`/`height` above are what we BOUND; this is what was REQUESTED, and the two
    /// disagreeing is a finding, not a formatting detail.
    texture: Option<TextureDesc>,
    /// The decoded S# the guest supplied, for both sources — see [`Self::texture`].
    sampler: Option<SamplerState>,
    /// The sampler the backend was actually told to create and bind (task-201). Recorded
    /// SEPARATELY from [`Self::sampler`] because "what the guest asked for" and "what we
    /// bound" are different facts, and a capture that shows only the request cannot reveal a
    /// bind that ignores it. That is precisely how the RT path's hardcoded linear/repeat
    /// survived: every snapshot faithfully recorded a NEAREST S# while the GPU filtered
    /// bilinearly. If these two disagree, that is the finding.
    sampler_bound: SamplerDesc,
    /// Whether the bind used the guest's T# (`Plain`) or substituted the host render target
    /// image (`RenderTarget`). Stated explicitly so a reader can never mistake a recorded
    /// descriptor for one that was honoured (module rule 1). NOTE this is about the IMAGE:
    /// since task-201 the guest's S# is honoured on both paths, so compare
    /// [`Self::sampler`] with [`Self::sampler_bound`] for the sampler question.
    descriptor_honoured: bool,
    /// What became of this draw's texel dump. ALWAYS present, and always states an outcome:
    /// a texture that was too big, or unreadable, or not dumped because the feature is off,
    /// says so rather than vanishing (module rule 1 — never a silent omission).
    dump: TextureDumpOutcome,
}

/// What happened when the recorder tried to dump a sampled texture's texels.
///
/// Texture dumping reads GUEST memory, so it perturbs nothing — but a 2048×2048 RGBA is 16 MB,
/// a frame samples several, and `F9` defaults to eight frames. Unbounded, one keypress would
/// write hundreds of megabytes. Every way of NOT writing those bytes is a named outcome here,
/// because "no texture file" and "the texture was empty" must never look the same.
#[derive(Debug, Clone)]
enum TextureDumpOutcome {
    /// Texture dumping is off (the default). Turn on with `UNEMUPS4_SNAPSHOT_TEXTURES=1`.
    Disabled,
    /// The texels live on the GPU, not in guest memory: this bind substituted a host render
    /// target (task-56 RT-as-texture). There is nothing for a guest-memory read to find. The
    /// pixels themselves are available separately, as the PRODUCING draw's
    /// [`RtDumpOutcome`] PNG under `render-targets/` (task-187).
    RenderTargetSource,
    /// Over the per-texture byte cap (`UNEMUPS4_SNAPSHOT_TEX_MAX_BYTES`). Carries the span so
    /// the reader can see how far over it was, and raise the cap deliberately.
    TooLarge { span: u64, cap: u64 },
    /// The bounded read faulted. NOT a zero-filled buffer: "we could not read it" and "it was
    /// zeros" are different findings.
    ReadFailed { span: u64 },
    /// The bytes were read and dumped, or matched a blob already dumped this session.
    Dumped {
        /// Key of the RAW (tiled) dump under `<root>/textures/`: the content hash of the RAW
        /// guest bytes (`<key>.raw`).
        key: String,
        /// Stem of the DETILED outputs (`<detiled_key>.detiled.raw` / `.png`) under
        /// `<root>/textures/`. Distinct from `key` because it also folds in the surface LAYOUT
        /// (the same bytes detile differently per tiling/pitch). `None` when the detiler
        /// rejected the buffer (no picture to name).
        detiled_key: Option<String>,
        /// Byte span read from guest memory.
        span: u64,
        /// `Err` if the detiler rejected the buffer — the RAW dump is still written, which is
        /// the point of keeping it: it stays ground truth when the detiler is the suspect.
        detiled: Result<usize, String>,
    },
}

/// What happened when the recorder tried to get a picture of a draw's render target
/// (task-187).
///
/// The dump is fire-and-forget across a thread boundary, so this records what was REQUESTED,
/// never what a file on disk proves. Every way of getting no picture is a named variant,
/// because "no PNG" and "the target was black" are different findings — the same discipline
/// [`TextureDumpOutcome`] follows for sampled texels.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RtDumpOutcome {
    /// Render-target dumping is off (the default). Turn on with
    /// `UNEMUPS4_SNAPSHOT_RENDER_TARGETS=1`.
    Disabled,
    /// The draw renders into the VIDEOOUT target, not an offscreen render target. There is
    /// no `ResourceId` to copy from — what the videoout target ends up holding is the
    /// presented frame, which `UNEMUPS4_DUMP_PNG` already dumps per flip.
    Videoout,
    /// A copy was requested from the display thread. `key` names
    /// `render-targets/<key>.png` inside this frame's directory. The file is written by the
    /// backend after the frame's passes complete; if it is missing, the host-side copy
    /// failed and the display thread logged the reason. Never treat a missing file as an
    /// empty target.
    Requested {
        /// Filename stem under this frame's `render-targets/`.
        key: String,
    },
}

/// Where a sampled bind's texels come from, as the draw path resolved it. Mirrors the
/// executor's private `TextureSource` without exposing it, so the recorder takes plain
/// references and the executor keeps its enum private.
pub enum SampledSource<'a> {
    /// A guest-memory texture: the decoded T#/S# the guest actually supplied.
    Plain(&'a TextureDesc, &'a SamplerState),
    /// A registered offscreen render target bound host-side (no upload), plus the guest's
    /// own T#/S# — which the bind did NOT consult, and which is recorded precisely because
    /// of that (task-184).
    RenderTarget(&'a RegisteredRt, &'a TextureBindingRange),
}

/// One sampled bind as the draw path resolved it: where the descriptor is bound, what it
/// resolved to, and (for a guest-memory texture) the exact layout the upload detiled with.
pub struct SampledInput<'a> {
    /// The `(set, binding)` the combined image-sampler is written at.
    pub binding: TextureBinding,
    /// What this bind resolved to — a guest-memory texture or a host render target.
    pub source: SampledSource<'a>,
    /// For a `Plain` bind: the EXACT [`SurfaceLayout`] the upload path detiled with
    /// (`exec::texture_surface_layout`). Threaded in rather than rebuilt here so the dumped
    /// detiled image is the one the draw sampled, not a second interpretation of the same
    /// T#. `None` for an RT-sourced bind.
    pub surface: Option<SurfaceLayout>,
    /// The sampler the backend was told to bind for this descriptor — the resolved filter
    /// and wrap modes, not the guest's raw request (task-201).
    pub sampler_bound: SamplerDesc,
}

/// Everything the recorder needs from one draw, borrowed from the executor's locals at the
/// point where the draw is fully resolved but has not yet been shipped.
///
/// A struct rather than fifteen positional arguments so that adding a field is a compile
/// error at the one call site instead of a silently-misordered argument.
pub struct DrawInput<'a> {
    /// Which draw packet this is (`"DrawIndexAuto"`, …).
    pub kind: &'a str,
    /// Vertex/index count from the packet.
    pub count: u32,
    /// The derived target/pipeline/viewport/scissor for this draw.
    pub draw: &'a DrawState,
    /// The pipeline key AFTER the resource signature and vertex layout were folded in — the
    /// key the cache is about to be queried with, not the register-only derivation.
    pub key: &'a PipelineKey,
    /// The whole shadow register file as of THIS draw, for the per-draw delta.
    pub regs: RegSnapshot,
    /// The VS bind this draw resolved.
    pub vs: Option<ShaderRef>,
    /// The recompiled VS SPIR-V — the module actually handed to Vulkan.
    pub vs_spirv: &'a [u32],
    /// The PS bind this draw resolved.
    pub ps: Option<ShaderRef>,
    /// The recompiled PS SPIR-V — the module actually handed to Vulkan.
    pub ps_spirv: &'a [u32],
    /// Every V# range the draw resolved (vertex streams and index buffers).
    pub buffers: &'a [BufferRange],
    /// The VS's scalar constant-buffer range, if it declared one.
    pub vs_const: Option<&'a BufferRange>,
    /// The PS's scalar constant-buffer range, if it declared one.
    pub ps_const: Option<&'a BufferRange>,
    /// EVERY combined image-sampler bind the PS declared, in the shader's first-sample
    /// order (task-199). A multi-texture pass — Celeste's distortion pass mixes a
    /// displacement map with the scene, its present pass mixes the frame with a colour-grade
    /// LUT — records them ALL, so a reader can see which resource each sample really reads
    /// instead of only the first. Empty when the PS samples nothing.
    pub sampled: Vec<SampledInput<'a>>,
}

/// The per-frame snapshot recorder. Lives on [`GpuState`] so it spans the several submits a
/// frame is built from; idle (and free) unless the maintainer asked for a capture.
#[derive(Debug, Clone, Default)]
pub struct Recorder {
    /// Whether draws in the current frame are being recorded.
    armed: bool,
    /// Flip index the armed frame will be labelled with.
    frame: u64,
    /// Records appended so far this frame.
    draws: Vec<DrawRecord>,
    /// Draws that bailed out of `setup_draw` this frame, with their reason.
    deferred: Vec<DeferredRecord>,
    /// Register file as of the previous recorded draw, for the per-draw delta. Reset to empty
    /// when a frame is armed, so draw 0's delta is "everything the guest had written".
    prev_regs: RegSnapshot,
    /// Shader hashes already written under `<root>/shaders/`. SESSION-scoped, not per frame:
    /// an 8-frame burst binds the same handful of shaders every frame, and 23 copies of one
    /// module per frame is noise that hides the module that differs. The hash already folds
    /// in the `PsInputMap` (see `derive::shader_hash`), so two routings of one `.sb` are two
    /// keys — which is the whole reason the SPIR-V is dumped at all.
    seen_shaders: HashSet<u64>,
    /// Of [`Self::seen_shaders`], those whose guest `.sb` bytes were actually readable — so a
    /// rebind reports the same file set the first sighting wrote, rather than claiming a `.sb`
    /// that a faulting read never produced.
    shaders_with_sb: HashSet<u64>,
    /// Render targets a PNG dump has already been requested for THIS frame, keyed on guest
    /// base → filename stem (task-187). Per frame, not per session: a render target's
    /// contents are exactly what changes between frames, so deduping one across a burst
    /// would defeat the reason for capturing several. Within a frame the de-dupe is what
    /// keeps one PNG per target rather than one per draw into it, and the file therefore
    /// holds that target's state at the end of the submit the dump was queued in.
    rt_dumps: std::collections::HashMap<u64, String>,
    /// Content hashes of texture blobs already written under `<root>/textures/` as `<hash>.raw`
    /// (the TILED bytes). Session-scoped for the same reason, and keyed on CONTENT rather than
    /// address so a re-uploaded atlas at a recycled address is dumped again rather than mistaken
    /// for the old one. The raw (tiled) bytes are identical across layouts, so content alone is
    /// the right key for them.
    seen_textures: HashSet<u64>,
    /// Stems of the DETILED outputs (`<stem>.detiled.raw` / `<stem>.png`) already written.
    /// Keyed on `(content, layout)`, not content alone: the SAME tiled bytes under a different
    /// T# tiling/pitch (doc-2 §C3) detile to a DIFFERENT image, so a second layout must write
    /// its own picture rather than be deduped onto the first's.
    seen_detiled: HashSet<String>,
}

impl Recorder {
    /// Whether the draw path should build a record for the draw it is setting up.
    ///
    /// **This is the hot-path check** (AC #4): a plain `bool` field read, no atomic, no
    /// allocation, no formatting. Every cost in this module sits behind it.
    #[inline]
    pub fn armed(&self) -> bool {
        self.armed
    }

    /// Number of draws recorded for the in-flight frame. Introspection / tests.
    pub fn recorded_draws(&self) -> usize {
        self.draws.len()
    }

    /// The flip index the in-flight capture is labelled with. Introspection / tests.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Append one draw's state. The caller must have checked [`armed`](Self::armed) — this
    /// is a no-op otherwise, so a missed check costs correctness of the dump, never the run.
    pub fn record_draw(&mut self, input: DrawInput<'_>) {
        if !self.armed {
            return;
        }
        let ordinal = self.draws.len() as u32;
        // One record per declared texture, in shader order — the whole point of task-199
        // is that a draw can sample several DIFFERENT resources, so recording only the
        // first would hide exactly the bug this dump exists to catch.
        let mut sampled: Vec<SampledRecord> = Vec::with_capacity(input.sampled.len());
        for s in input.sampled {
            let SampledInput {
                binding,
                source,
                surface,
                sampler_bound,
            } = s;
            sampled.push(match source {
                SampledSource::Plain(tex, sampler) => SampledRecord {
                    set: binding.set,
                    binding: binding.binding,
                    source: "Plain",
                    base: tex.base,
                    width: tex.width,
                    height: tex.height,
                    texture: Some(*tex),
                    sampler: Some(*sampler),
                    sampler_bound,
                    descriptor_honoured: true,
                    dump: self.dump_texture(tex, surface.as_ref()),
                },
                SampledSource::RenderTarget(rt, req) => SampledRecord {
                    set: binding.set,
                    binding: binding.binding,
                    source: "RenderTarget",
                    base: rt.base,
                    width: rt.desc.width,
                    height: rt.desc.height,
                    texture: Some(req.texture),
                    sampler: Some(req.sampler),
                    sampler_bound,
                    descriptor_honoured: false,
                    dump: TextureDumpOutcome::RenderTargetSource,
                },
            });
        }
        let const_buffers = [
            (Stage::Vertex, input.vs_const),
            (Stage::Pixel, input.ps_const),
        ]
        .into_iter()
        .filter_map(|(stage, range)| range.map(|r| const_buffer_record(stage, r)))
        .collect::<Vec<ConstBufferRecord>>();
        // A samplerless PS that loads constants is, by convention, a full-screen fill
        // exporting its CB's first vector as RGBA (task-184's `(0,0,0,0)` clear). Surfacing it
        // as a named field beats making the next reader convert four hex dwords by hand — but
        // it stays a convention, so the raw dwords are still right there in `const_buffers`.
        let fill_color = if sampled.is_empty() {
            const_buffers
                .iter()
                .find(|c| c.stage == "pixel" && c.bytes.len() >= 16)
                .map(|c| {
                    let f = |i: usize| {
                        f32::from_le_bytes([
                            c.bytes[i],
                            c.bytes[i + 1],
                            c.bytes[i + 2],
                            c.bytes[i + 3],
                        ])
                    };
                    [f(0), f(4), f(8), f(12)]
                })
        } else {
            None
        };
        let target_dump = self.target_dump_outcome(&input.draw.target);
        let reg_delta = input.regs.delta(&self.prev_regs);
        self.prev_regs = input.regs;
        let vs_key = self.dump_shader(Stage::Vertex, input.vs, input.key.vs_hash, input.vs_spirv);
        let ps_key = self.dump_shader(Stage::Pixel, input.ps, input.key.ps_hash, input.ps_spirv);
        self.draws.push(DrawRecord {
            ordinal,
            kind: input.kind.to_string(),
            count: input.count,
            target: input.draw.target,
            pipeline: *input.key,
            viewport: input.draw.viewport,
            scissor: input.draw.scissor,
            reg_delta,
            fill_color,
            vs: ShaderIdent::new(
                Stage::Vertex,
                input.vs,
                input.key.vs_hash,
                input.vs_spirv.len(),
                vs_key,
            ),
            ps: ShaderIdent::new(
                Stage::Pixel,
                input.ps,
                input.key.ps_hash,
                input.ps_spirv.len(),
                ps_key,
            ),
            buffers: input
                .buffers
                .iter()
                .map(|r| BufferRecord {
                    addr: r.addr,
                    size: r.size,
                    layout: format!("{:?}", r.layout),
                    desc: r.desc,
                })
                .collect(),
            const_buffers,
            target_dump,
            sampled,
        });
    }

    /// Record a draw that bailed out of `setup_draw`, with the exit that fired.
    ///
    /// The caller must have checked [`armed`](Self::armed). See [`DeferredRecord`] for why a
    /// defer is worth as much space in the capture as a submitted draw.
    pub fn record_deferred(&mut self, kind: &str, count: u32, reason: &'static str) {
        if !self.armed {
            return;
        }
        self.deferred.push(DeferredRecord {
            ordinal: self.deferred.len() as u32,
            kind: kind.to_string(),
            count,
            reason: reason.to_string(),
            after_draw: self.draws.len() as u32,
            detail: None,
        });
    }

    /// Record an unsupported-`.sb`-GCN-shader defer WITH its per-instruction detail (task-195):
    /// the stage that failed, the shader address/hash, and the decoded unsupported instruction
    /// (`instruction`, already formatted by the armed-gated caller). The `reason` slug stays
    /// the coarse `"unsupported-gcn-shader"` so the summary's by-cause grouping is unchanged;
    /// the exact gap rides in [`DeferDetail`].
    ///
    /// The caller ([`crate::exec`]'s `defer_draw_gcn`) must have checked [`armed`](Self::armed)
    /// AND formatted `instruction` only under that check — this method re-checks defensively but
    /// never formats.
    pub fn record_deferred_gcn(
        &mut self,
        kind: &str,
        count: u32,
        stage: &'static str,
        shader_addr: u64,
        shader_hash: u64,
        instruction: Option<String>,
    ) {
        if !self.armed {
            return;
        }
        // Dump the raw `.sb` (+ in-tree disassembly) the recompiler REJECTED, under the same
        // `shaders/<stage>-<hash>.(sb|txt)` scheme and session dedup set a successful dump uses
        // (task-196). A deferred shader never recompiles, so `dump_shader` never runs for it and
        // its raw GCN would otherwise be invisible — yet a defer is precisely the shader a reader
        // most needs to see. There is deliberately NO `.spv`: nothing was emitted.
        self.dump_deferred_shader(stage, shader_addr, shader_hash);
        self.deferred.push(DeferredRecord {
            ordinal: self.deferred.len() as u32,
            kind: kind.to_string(),
            count,
            reason: "unsupported-gcn-shader".to_string(),
            after_draw: self.draws.len() as u32,
            detail: Some(DeferDetail {
                stage,
                shader_addr,
                shader_hash,
                instruction,
            }),
        });
    }

    /// Dump the raw GCN a DEFERRED (recompile-rejected) shader was given, mirroring the
    /// `.sb`/`.txt` half of [`dump_shader`] but with NO `.spv` (none was emitted). Keyed on the
    /// same stage tag + hash the deferred record carries, so the file name matches
    /// `draws.json`'s `shader_hash`, and deduped on the same session-scoped [`Self::seen_shaders`]
    /// set so a shader that defers every frame writes its `.sb` once.
    ///
    /// The caller ([`Self::record_deferred_gcn`]) has already checked [`armed`](Self::armed);
    /// this re-checks defensively so a missed guard costs a wrong dump, never a wrong run.
    fn dump_deferred_shader(&mut self, stage: &'static str, addr: u64, hash: u64) {
        if !self.armed {
            return;
        }
        // The successful path tags files `vs`/`ps` (from [`Stage`]); the deferred record carries
        // the HW-stage string `"VS"`/`"PS"` — map to the same lowercase tag so both halves of a
        // capture share one naming scheme.
        let tag = match stage {
            "VS" => "vs",
            "PS" => "ps",
            other => other,
        };
        let key = format!("{tag}-{hash:016x}");
        // Same session dedup set as `dump_shader`: a rebind of an already-dumped hash writes
        // nothing (a deferred shader never reaches `dump_shader`, so this cannot collide).
        if !self.seen_shaders.insert(hash) {
            return;
        }
        let dir = ps4_core::snapshot::dump_root().join("shaders");
        let (code, note) = read_shader_code(addr);
        if !code.is_empty() {
            self.shaders_with_sb.insert(hash);
            let mut raw = Vec::with_capacity(code.len() * 4);
            for w in &code {
                raw.extend_from_slice(&w.to_le_bytes());
            }
            enqueue(WriteJob::File {
                path: dir.join(format!("{key}.sb")),
                bytes: raw,
            });
            let mut text = format!(
                "; {key}  addr={addr:#x}  dwords={}  {note}\n\
                 ; DEFERRED: the recompiler rejected this shader — there is no {key}.spv.\n",
                code.len()
            );
            text.push_str(&ps4_gcn::disasm_all(&ps4_gcn::decode_all(&code)));
            enqueue(WriteJob::File {
                path: dir.join(format!("{key}.txt")),
                bytes: text.into_bytes(),
            });
        } else {
            // No zero-filled stand-in: an unreadable shader gets a file saying so.
            enqueue(WriteJob::File {
                path: dir.join(format!("{key}.txt")),
                bytes: format!("; {key}  addr={addr:#x}  READ FAILED: {note}\n").into_bytes(),
            });
        }
    }

    /// Ask for a picture of one offscreen render target, at most once per target per frame.
    ///
    /// Returns the PNG path the caller should attach to a
    /// [`BackendCmd::DumpRenderTargetPng`](ps4_core::gpu::BackendCmd::DumpRenderTargetPng),
    /// or `None` when nothing should be emitted (not armed, the lever is off, or this target
    /// was already requested this frame).
    ///
    /// # Why this is not the guest-memory readback
    ///
    /// The readback ([`BackendCmd::ReadbackRenderTarget`](ps4_core::gpu::BackendCmd::ReadbackRenderTarget),
    /// task-56 step 5, tightened by task-181) exists to put a render target's contents into
    /// GUEST memory in the GUEST's layout, because guest code may read those bytes. It must
    /// therefore reproduce the surface's pitch and tile mode exactly, and REFUSE what it
    /// cannot express — which is every Celeste target, all 2D macro-tiled, a mode this repo
    /// deliberately implements no re-tiler for.
    ///
    /// This path has none of that work to do. The host image is linear RGBA8 already; a PNG
    /// wants linear RGBA8; the guest's tiling is irrelevant to looking at a picture. So it
    /// touches no guest memory, needs no re-tiler, and dumps macro-tiled targets fine. The
    /// two are kept as two functions and two commands precisely so nobody re-fuses them: the
    /// fusion is what made the diagnostic inherit a refusal it never needed (task-187).
    ///
    /// The caller must have checked [`armed`](Self::armed) for the hot path's sake; this
    /// re-checks so a missed guard costs a wrong dump, never a wrong run.
    pub fn request_rt_dump(&mut self, base: u64, desc: &TargetDesc) -> Option<PathBuf> {
        if !self.armed || !ps4_core::snapshot::render_targets_enabled() {
            return None;
        }
        let key = rt_dump_key(base, desc.width, desc.height);
        // Already requested this frame: the same file, so emitting a second copy command
        // would only overwrite it with a later moment of the same frame.
        if self.rt_dumps.contains_key(&base) {
            return None;
        }
        self.rt_dumps.insert(base, key.clone());
        Some(
            frame_dir(&ps4_core::snapshot::dump_root(), self.frame)
                .join("render-targets")
                .join(format!("{key}.png")),
        )
    }

    /// The outcome to record on a draw whose target is `target`. See [`RtDumpOutcome`].
    fn target_dump_outcome(&self, target: &TargetDesc) -> RtDumpOutcome {
        let TargetKind::Offscreen { base, .. } = target.kind else {
            return RtDumpOutcome::Videoout;
        };
        match self.rt_dumps.get(&base) {
            Some(key) => RtDumpOutcome::Requested { key: key.clone() },
            // The only way to be here with the lever on is a target `register_render_target`
            // never reached, so report the lever state rather than inventing a third story.
            None => RtDumpOutcome::Disabled,
        }
    }

    /// Dump one stage's shader artefacts, once per distinct shader hash per session.
    ///
    /// Writes three files under `<root>/shaders/<stage>-<hash>.*`:
    ///
    /// * `.spv` — **the SPIR-V module actually handed to Vulkan.** This is the point of the
    ///   whole function. This session lost hours three times on one Celeste pixel shader, and
    ///   all three would have been prevented by having this file: the in-tree GCN
    ///   disassembler silently drops VOP3 `neg`/`abs` source modifiers (task-182), so its text
    ///   misled a root-cause analysis; an agent then hand-decoded raw dwords to work around
    ///   that; and another agent "verified" our lowering by recompiling OFFLINE under the
    ///   DEFAULT `PsInputMap` while the live draw ran with `ps_input_map[0] = 1` — a different
    ///   module, so it verified code that does not execute. The module is free to obtain
    ///   (an `Arc<[u32]>` already resident) and is the only artefact that cannot be wrong
    ///   about what ran. Emitted as raw binary so `spirv-dis` and `spirv-val` take it
    ///   directly.
    /// * `.sb` — the raw GCN machine code the recompiler consumed.
    /// * `.txt` — the in-tree disassembly. Convenient, but a rendering of the decode rather
    ///   than of what ran: when it disagrees with the `.spv`, the `.spv` is right.
    ///
    /// Deduped on the hash, which folds in the `PsInputMap` — so the two routings that caused
    /// the third failure above land as two files, not one.
    ///
    /// Returns the key `draws.json` references, or `None` for an unbound stage.
    fn dump_shader(
        &mut self,
        stage: Stage,
        bound: Option<ShaderRef>,
        hash: u64,
        spirv: &[u32],
    ) -> Option<ShaderDumpRef> {
        let bound = bound?;
        let tag = match stage {
            Stage::Vertex => "vs",
            Stage::Pixel => "ps",
        };
        let key = format!("{tag}-{hash:016x}");
        // An embedded corpus shader has no guest `.sb`; only a `GcnBinary` bind does.
        let is_gcn = matches!(bound, ShaderRef::GcnBinary { .. });
        if !self.seen_shaders.insert(hash) {
            // A rebind of an already-dumped hash: same files, same names, written once.
            return Some(ShaderDumpRef {
                key,
                sb: is_gcn && self.shaders_with_sb.contains(&hash),
                disasm: is_gcn,
            });
        }
        let dir = ps4_core::snapshot::dump_root().join("shaders");

        // The module Vulkan received, verbatim, little-endian as SPIR-V requires.
        let mut spv = Vec::with_capacity(spirv.len() * 4);
        for w in spirv {
            spv.extend_from_slice(&w.to_le_bytes());
        }
        enqueue(WriteJob::File {
            path: dir.join(format!("{key}.spv")),
            bytes: spv,
        });

        // The guest-side source, for a `GcnBinary` bind only: an embedded corpus shader has no
        // `.sb` and gets no `.sb` file rather than a plausible stand-in.
        let mut wrote_sb = false;
        if let ShaderRef::GcnBinary { addr, .. } = bound {
            let (code, note) = read_shader_code(addr);
            if !code.is_empty() {
                wrote_sb = true;
                self.shaders_with_sb.insert(hash);
                let mut raw = Vec::with_capacity(code.len() * 4);
                for w in &code {
                    raw.extend_from_slice(&w.to_le_bytes());
                }
                enqueue(WriteJob::File {
                    path: dir.join(format!("{key}.sb")),
                    bytes: raw,
                });
                let mut text = format!(
                    "; {key}  addr={addr:#x}  dwords={}  {note}\n\
                     ; When this disagrees with {key}.spv, the .spv is what ran.\n",
                    code.len()
                );
                text.push_str(&ps4_gcn::disasm_all(&ps4_gcn::decode_all(&code)));
                enqueue(WriteJob::File {
                    path: dir.join(format!("{key}.txt")),
                    bytes: text.into_bytes(),
                });
            } else {
                // No zero-filled stand-in: an unreadable shader gets a file saying so.
                enqueue(WriteJob::File {
                    path: dir.join(format!("{key}.txt")),
                    bytes: format!("; {key}  addr={addr:#x}  READ FAILED: {note}\n").into_bytes(),
                });
            }
        }
        Some(ShaderDumpRef {
            key,
            sb: wrote_sb,
            disasm: is_gcn,
        })
    }

    /// Dump one sampled guest texture's texels, once per distinct content hash per session.
    ///
    /// Writes, under `<root>/textures/<content-hash>.*`:
    ///
    /// * `.raw` — the guest bytes exactly as read, still tiled. Kept because it is ground
    ///   truth when the DETILER is the suspect; a detiled image alone cannot tell you whether
    ///   a wrong picture came from the texels or from our reading of them.
    /// * `.detiled.raw` + `.png` — the detiler's output, produced with the SAME
    ///   [`SurfaceLayout`] the upload path used (threaded in as
    ///   [`DrawInput::sampled_surface`], never rebuilt here).
    ///
    /// This reads GUEST memory only, so it perturbs nothing. It is nonetheless the one
    /// genuinely expensive thing a capture does — hence off by default
    /// (`UNEMUPS4_SNAPSHOT_TEXTURES=1`), capped per texture
    /// (`UNEMUPS4_SNAPSHOT_TEX_MAX_BYTES`, 16 MiB), and deduped by content across the whole
    /// session so an F9 burst does not write the atlas eight times. Every path that does NOT
    /// write bytes returns a named [`TextureDumpOutcome`] instead — never a silent omission.
    fn dump_texture(
        &mut self,
        t: &TextureDesc,
        surface: Option<&SurfaceLayout>,
    ) -> TextureDumpOutcome {
        if !ps4_core::snapshot::textures_enabled() {
            return TextureDumpOutcome::Disabled;
        }
        let span = t.byte_span();
        let cap = ps4_core::snapshot::texture_max_bytes();
        if span == 0 || span > cap {
            return TextureDumpOutcome::TooLarge { span, cap };
        }
        let Ok(raw) = crate::idmem::BoundedMem.read_bytes_ranged(t.base, span as usize) else {
            return TextureDumpOutcome::ReadFailed { span };
        };
        let hash = content_hash(&raw);
        let key = format!("{hash:016x}");
        // Detile even on a dedupe hit: it is cheap next to the read, and its Ok/Err outcome
        // belongs on THIS draw's record (the same bytes under a different T# can detile
        // differently — different pitch, different tile mode).
        let detiled = match surface {
            Some(s) => crate::cache::tile::detile(&raw, s).map_err(|e| format!("{e:?}")),
            None => Err("no surface layout for this bind".to_string()),
        };
        // The detiled picture depends on the LAYOUT, not just the bytes, so its stem folds in a
        // hash of the surface. A `None` surface never produced a detile, so it has no stem.
        let detiled_key = surface.map(|s| format!("{key}-{:016x}", layout_hash(s)));
        let outcome = TextureDumpOutcome::Dumped {
            key: key.clone(),
            detiled_key: detiled_key.clone(),
            span,
            detiled: detiled.as_ref().map(|d| d.len()).map_err(|e| e.clone()),
        };
        let dir = ps4_core::snapshot::dump_root().join("textures");
        // RAW (tiled) bytes: identical across layouts, so dedup on content alone.
        if self.seen_textures.insert(hash) {
            enqueue(WriteJob::File {
                path: dir.join(format!("{key}.raw")),
                bytes: raw,
            });
        }
        // Detiled output: keyed on (content, layout), so a second layout of the same bytes writes
        // its own `.detiled.raw`/`.png` instead of being silently deduped onto the first's.
        if let (Ok(linear), Some(dkey)) = (detiled, detiled_key)
            && self.seen_detiled.insert(dkey.clone())
        {
            let (w, h) = (t.width, t.height);
            if linear.len() >= (w as usize) * (h as usize) * 4 && w > 0 && h > 0 {
                enqueue(WriteJob::Png {
                    path: dir.join(format!("{dkey}.png")),
                    w,
                    h,
                    pixels: linear[..(w as usize) * (h as usize) * 4].to_vec(),
                });
            }
            enqueue(WriteJob::File {
                path: dir.join(format!("{dkey}.detiled.raw")),
                bytes: linear,
            });
        }
        outcome
    }
}

/// A deterministic hash of a [`SurfaceLayout`] (tiling / pitch / extent / texel), for keying a
/// detiled dump per layout. `DefaultHasher` is seeded from fixed constants, so the same layout
/// yields the same stem across a session (and across runs of one build) — a `diff -r` of two
/// captures stays meaningful.
fn layout_hash(s: &SurfaceLayout) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Read a shader's GCN machine code through the bounded seam, returning the dwords plus a
/// note on where the extent came from.
///
/// A parseable `.sb` container reports its own header-validated code range, which is exact.
/// When the header does not parse (an encrypted or malformed container), a fixed
/// [`SHADER_FALLBACK_DUMP_BYTES`] window is read INSTEAD — and the note says so, because a
/// fixed window's tail is whatever follows the shader, not shader code. Returns an empty
/// vector and the failure text if nothing could be read at all.
fn read_shader_code(addr: u64) -> (Vec<u32>, String) {
    let words_from = |bytes: Vec<u8>| -> Vec<u32> {
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    };
    if let Some(reader) = ps4_core::bounded_read::bounded_read()
        && let Ok(sb) = crate::shader::sb::parse_sb(addr, &*reader)
    {
        let len = (sb.code_range.end - sb.code_range.start) as usize;
        match crate::idmem::BoundedMem.read_bytes_ranged(sb.code_range.start, len) {
            Ok(b) => {
                return (
                    words_from(b),
                    format!("code_range={len} bytes (.sb header)"),
                );
            }
            Err(e) => return (Vec::new(), format!("code range read faulted: {e}")),
        }
    }
    match crate::idmem::BoundedMem.read_bytes_ranged(addr, SHADER_FALLBACK_DUMP_BYTES) {
        Ok(b) => (
            words_from(b),
            format!(
                ".sb header did not parse — FIXED {SHADER_FALLBACK_DUMP_BYTES}-byte window, tail is not necessarily code"
            ),
        ),
        Err(e) => (Vec::new(), format!("read faulted: {e}")),
    }
}

/// Read a bounded window of a constant buffer's contents through the bounded seam.
///
/// A faulting read yields an EMPTY byte vector, never zeros: "we could not read it" and "it
/// held zeros" are different findings, and conflating them is how a snapshot starts lying.
fn const_buffer_record(stage: Stage, range: &BufferRange) -> ConstBufferRecord {
    let want = range.size.min(CONST_BUFFER_DUMP_BYTES) as usize;
    let bytes = crate::idmem::BoundedMem
        .read_bytes_ranged(range.addr, want)
        .unwrap_or_default();
    ConstBufferRecord {
        stage: match stage {
            Stage::Vertex => "vertex",
            Stage::Pixel => "pixel",
        },
        addr: range.addr,
        size: range.size,
        desc: range.desc,
        bytes,
    }
}

/// Frame boundary hook, called from the executor on the guest submit thread when a submit
/// carries a flip.
///
/// Two things happen, in this order:
///
/// 1. If a capture was armed, the frame just finished is written to disk and the recorder
///    goes idle.
/// 2. One frame is claimed from the cross-thread budget; if there was one, the recorder arms
///    for the frame that is about to begin.
///
/// Doing (1) before (2) is what makes `F9`'s burst capture CONSECUTIVE frames: each boundary
/// closes one capture and opens the next. Writing files here is safe with respect to
/// rendering — it happens between frames, on the submit thread, and touches no GPU state.
///
/// `frame` is the flip index used to label the capture ([`ps4_core::clock::flip_count`]).
pub fn on_frame_boundary(state: &mut GpuState, frame: u64) {
    let wrote = state.snapshot.armed;
    if state.snapshot.armed {
        // End the capture BEFORE deciding whether to start another, so the two never
        // overlap and a burst lands as N distinct, complete frames.
        state.snapshot.armed = false;
        let captured_frame = state.snapshot.frame;
        let draws = std::mem::take(&mut state.snapshot.draws);
        let deferred = std::mem::take(&mut state.snapshot.deferred);
        // `state` is only read from here on, so the register banks can be borrowed alongside
        // the drained draw records.
        if let Err(e) = write_capture(state, captured_frame, &draws, &deferred) {
            // A dump failure must never take the run down with it — the maintainer is
            // mid-session looking at a live window.
            tracing::warn!("[SNAPSHOT] frame {captured_frame} capture failed: {e}");
        }
    }
    if ps4_core::snapshot::take_frame() {
        state.snapshot.armed = true;
        state.snapshot.frame = frame;
        state.snapshot.draws.clear();
        state.snapshot.deferred.clear();
        // Per-frame, not per-session: a render target's CONTENTS are what differ between the
        // frames of a burst, so carrying the de-dupe across frames would give the burst one
        // picture instead of N.
        state.snapshot.rt_dumps.clear();
        // Draw 0's register delta is against an EMPTY file, i.e. "everything the guest had
        // written", rather than against whatever the previous capture happened to end on —
        // which would make the first draw of each burst frame diff against a frame boundary
        // and read as noise.
        state.snapshot.prev_regs = RegSnapshot::default();
    } else if wrote {
        // The capture that just closed was the LAST one owed, so wait here for its background
        // writes to land. This is the only place the submit thread ever waits on I/O, and it
        // is the right one: the maintainer pressed a key and is about to go and look, so
        // "the dump is complete when the burst ends" beats "the dump is complete unless you
        // quit in the next second". It runs once per F9/F10 press, never on an idle frame —
        // and with texture dumping on it can be a visible one-off hitch of up to ~a second.
        flush_writes();
    }
}

/// Write one captured frame's three files into `<root>/frame-NNNNN/`.
///
/// These three are written SYNCHRONOUSLY: they are small (tens of KB), and a capture that
/// returned before its own index existed would be a worse trade than a few milliseconds on
/// the submit thread. The bulk artefacts — shader modules and texture texels — went to the
/// background writer as they were recorded (see [`enqueue`]), so nothing measured in
/// megabytes is written here.
fn write_capture(
    state: &GpuState,
    frame: u64,
    draws: &[DrawRecord],
    deferred: &[DeferredRecord],
) -> std::io::Result<()> {
    let dir = frame_dir(&ps4_core::snapshot::dump_root(), frame);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("registers.json"), registers_json(state))?;
    std::fs::write(dir.join("draws.json"), draws_json(frame, draws, deferred))?;
    std::fs::write(
        dir.join("summary.txt"),
        summary_txt(state, frame, draws, deferred),
    )?;
    // Log the ABSOLUTE path. The dump root defaults to a relative path, so where a capture
    // actually lands depends on the process's working directory — running from a
    // subdirectory silently nests the tree there. Printing the resolved path means the
    // maintainer never has to guess which one of those happened.
    let shown = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    tracing::info!(
        "[SNAPSHOT] frame {frame}: {} draws, {} deferred -> {}",
        draws.len(),
        deferred.len(),
        shown.display()
    );
    Ok(())
}

/// Filename stem for one render target's PNG: `rt-<base>-<w>x<h>`.
///
/// Base-keyed rather than keyed on the backend's `ResourceId`, because the base is what
/// `draws.json`'s `target.base` and the executor's RT registry already name a target by — so
/// the picture and the state that produced it join on a value the reader can see in both.
/// The extent rides along because a target re-created at a new size is a different picture.
fn rt_dump_key(base: u64, w: u32, h: u32) -> String {
    format!("rt-{base:016x}-{w}x{h}")
}

/// `<root>/frame-01734`. Zero-padded so a directory listing sorts chronologically, which is
/// what makes a frame-to-frame diff of a burst capture a `diff -r` away.
fn frame_dir(root: &Path, frame: u64) -> PathBuf {
    root.join(format!("frame-{frame:05}"))
}

// ---------------------------------------------------------------------------------------
// Serialisation
//
// Hand-rolled JSON rather than serde: `ps4-gnm` is deliberately dependency-light (it carries
// its own PNG writer in `texdump` for the same reason), and the shapes here are small and
// fixed. The one thing hand-rolled JSON gets wrong is escaping, so every string goes through
// `json_str`.
// ---------------------------------------------------------------------------------------

/// Emit a JSON string literal with the escapes the spec requires. Values reaching here are
/// mostly `Debug` output, which readily contains quotes and backslashes.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `Debug`-format a value into a JSON string. Used for the plain-data enums (`ColorFormat`,
/// `Tiling`, `ResLayout`, …) whose `Debug` name IS their meaningful identity.
fn json_dbg(v: &impl std::fmt::Debug) -> String {
    json_str(&format!("{v:?}"))
}

/// The full shadow register file: every register the guest has written, in every bank.
///
/// Registers we consume and registers nothing reads are dumped identically, and an unnamed
/// register is emitted with `"name": null` and its raw index rather than dropped (see the
/// module docs, rule 2). Banks are separate objects because the same absolute index means
/// different things in different windows.
fn registers_json(state: &GpuState) -> String {
    let banks: [(&str, u32, &crate::state::RegFile); 4] = [
        (
            "context",
            crate::pm4::opcodes::reg_base::CONTEXT,
            &state.ctx_regs,
        ),
        ("sh", crate::pm4::opcodes::reg_base::SH, &state.sh_regs),
        (
            "uconfig",
            crate::pm4::opcodes::reg_base::UCONFIG,
            &state.uconfig_regs,
        ),
        (
            "config",
            crate::pm4::opcodes::reg_base::CONFIG,
            &state.config_regs,
        ),
    ];
    let mut out = String::from("{\n");
    for (bank_index, (bank_name, base, regs)) in banks.iter().enumerate() {
        // Sorted by index so two captures of the same state produce byte-identical files —
        // a HashMap's iteration order would make every diff noise.
        let mut entries: Vec<(u32, u32)> = regs.iter().collect();
        entries.sort_unstable_by_key(|&(i, _)| i);
        let _ = writeln!(out, "  {}: {{", json_str(bank_name));
        let _ = writeln!(out, "    \"base\": {base},");
        let _ = writeln!(out, "    \"count\": {},", entries.len());
        out.push_str("    \"registers\": [\n");
        for (n, (index, value)) in entries.iter().enumerate() {
            let name = match crate::pm4::opcodes::reg_name(*index) {
                Some(n) => json_str(&n),
                None => "null".to_string(),
            };
            let _ = write!(
                out,
                "      {{\"index\": {index}, \"index_hex\": \"{index:#x}\", \
                 \"offset\": {}, \"name\": {name}, \"value\": {value}, \
                 \"value_hex\": \"{value:#010x}\"}}",
                index.wrapping_sub(*base)
            );
            out.push_str(if n + 1 == entries.len() { "\n" } else { ",\n" });
        }
        out.push_str("    ]\n  }");
        out.push_str(if bank_index + 1 == banks.len() {
            "\n"
        } else {
            ",\n"
        });
    }
    out.push_str("}\n");
    out
}

/// Per-draw state for the captured frame, plus the draws that never made it.
fn draws_json(frame: u64, draws: &[DrawRecord], deferred: &[DeferredRecord]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{{\n  \"frame\": {frame},\n  \"draw_count\": {},\n  \"deferred_count\": {},",
        draws.len(),
        deferred.len()
    );
    out.push_str("  \"draws\": [\n");
    for (n, d) in draws.iter().enumerate() {
        out.push_str(&draw_json(d));
        out.push_str(if n + 1 == draws.len() { "\n" } else { ",\n" });
    }
    out.push_str("  ],\n");
    // Deliberately a SEPARATE array, not draws with a flag: a deferred draw reached the
    // backend zero times, and putting it in `draws` would let a reader (or a `jq` one-liner)
    // count it as something the frame rendered.
    out.push_str("  \"deferred_draws\": [\n");
    for (n, d) in deferred.iter().enumerate() {
        let _ = write!(
            out,
            "    {{\"ordinal\": {}, \"kind\": {}, \"count\": {}, \"reason\": {}, \
             \"after_submitted_draw\": {}",
            d.ordinal,
            json_str(&d.kind),
            d.count,
            json_str(&d.reason),
            d.after_draw
        );
        // For a `.sb`-GCN defer, name the exact gap: the failing stage, the shader identity, and
        // the decoded unsupported instruction + dword offset (task-195). Absent (no extra keys)
        // for coarse defers, so the existing schema for those is unchanged.
        if let Some(det) = &d.detail {
            let instruction = match &det.instruction {
                Some(s) => json_str(s),
                None => "null".to_string(),
            };
            // `shader_hash` is emitted WITHOUT the `0x` prefix (unlike a submitted draw's `hash`,
            // which carries an explicit `dump` path): a deferred draw has no `.spv` and no path
            // pointer, so the only way to find its `.sb`/`.txt` is `dump_deferred_shader`'s
            // on-disk stem `{tag}-{hash:016x}` — this field must match that hash portion
            // verbatim, which the `0x`-prefixed form did not.
            let _ = write!(
                out,
                ", \"stage\": {}, \"shader_addr\": \"{:#x}\", \"shader_hash\": \"{:016x}\", \
                 \"instruction\": {}",
                json_str(det.stage),
                det.shader_addr,
                det.shader_hash,
                instruction
            );
        }
        out.push('}');
        out.push_str(if n + 1 == deferred.len() { "\n" } else { ",\n" });
    }
    out.push_str("  ]\n}\n");
    out
}

/// One draw object. Indented to sit inside `draws.json`'s array.
fn draw_json(d: &DrawRecord) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "    {{\n      \"ordinal\": {},", d.ordinal);
    let _ = writeln!(o, "      \"kind\": {},", json_str(&d.kind));
    let _ = writeln!(o, "      \"count\": {},", d.count);
    let _ = writeln!(o, "      \"target\": {},", target_json(&d.target));
    // The PICTURE of that target, or the named reason there is none. Separate from `target`
    // (which is the DERIVED description) so a reader never confuses "we derived a target" with
    // "we have its pixels".
    let _ = writeln!(
        o,
        "      \"target_dump\": {},",
        rt_dump_json(&d.target_dump)
    );
    let _ = writeln!(o, "      \"pipeline\": {},", pipeline_json(&d.pipeline));
    let _ = writeln!(o, "      \"viewport\": {},", viewport_json(&d.viewport));
    let _ = writeln!(o, "      \"scissor\": {},", scissor_json(&d.scissor));
    let _ = writeln!(o, "      \"vs\": {},", shader_json(&d.vs));
    let _ = writeln!(o, "      \"ps\": {},", shader_json(&d.ps));
    let _ = writeln!(
        o,
        "      \"fill_color_heuristic\": {},",
        match d.fill_color {
            // Named a heuristic in the key itself: it is the PS constant buffer's first
            // vector, which a samplerless full-screen fill exports as RGBA by convention, not
            // by anything we verified about this shader.
            Some(c) => format!(
                "[{}, {}, {}, {}]",
                json_f32(c[0]),
                json_f32(c[1]),
                json_f32(c[2]),
                json_f32(c[3])
            ),
            None => "null".to_string(),
        }
    );

    // Registers this draw changed relative to the previous submitted draw. `registers.json`
    // is the END-OF-FRAME file and cannot show these.
    let _ = write!(o, "      \"register_delta\": [");
    for (n, c) in d.reg_delta.iter().enumerate() {
        if n > 0 {
            o.push(',');
        }
        let name = match crate::pm4::opcodes::reg_name(c.index) {
            Some(n) => json_str(&n),
            None => "null".to_string(),
        };
        let hex = |v: Option<u32>| match v {
            Some(v) => format!("\"{v:#010x}\""),
            None => "null".to_string(),
        };
        let _ = write!(
            o,
            "\n        {{\"bank\": {}, \"index\": {}, \"index_hex\": \"{:#x}\", \"name\": {name}, \
             \"from\": {}, \"to\": {}}}",
            json_str(BANKS[c.bank as usize]),
            c.index,
            c.index,
            hex(c.from),
            hex(c.to)
        );
    }
    o.push_str(if d.reg_delta.is_empty() {
        "],\n"
    } else {
        "\n      ],\n"
    });

    o.push_str("      \"buffers\": [");
    for (n, b) in d.buffers.iter().enumerate() {
        if n > 0 {
            o.push(',');
        }
        let _ = write!(
            o,
            "\n        {{\"addr\": \"{:#x}\", \"size\": {}, \"layout\": {}, \"v_sharp\": {}}}",
            b.addr,
            b.size,
            json_str(&b.layout),
            v_sharp_json(&b.desc)
        );
    }
    o.push_str(if d.buffers.is_empty() {
        "],\n"
    } else {
        "\n      ],\n"
    });

    o.push_str("      \"const_buffers\": [");
    for (n, c) in d.const_buffers.iter().enumerate() {
        if n > 0 {
            o.push(',');
        }
        let _ = write!(
            o,
            "\n        {{\"stage\": {}, \"addr\": \"{:#x}\", \"size\": {}, \
             \"v_sharp\": {}, \"dumped_bytes\": {}, \"truncated\": {}, \
             \"read_failed\": {}, \"dwords\": [{}], \"floats\": [{}]}}",
            json_str(c.stage),
            c.addr,
            c.size,
            v_sharp_json(&c.desc),
            c.bytes.len(),
            c.size > c.bytes.len() as u64,
            // An empty dump on a non-empty buffer means the bounded read faulted. Flagged
            // explicitly so it can never be misread as "the buffer was empty".
            c.bytes.is_empty() && c.size > 0,
            dwords_json(&c.bytes),
            floats_json(&c.bytes)
        );
    }
    o.push_str(if d.const_buffers.is_empty() {
        "],\n"
    } else {
        "\n      ],\n"
    });

    // An ARRAY, one entry per texture the draw sampled (task-199). Always present, `[]`
    // when the PS samples nothing — so "samples nothing" and "we recorded nothing" can
    // never look the same.
    o.push_str("      \"sampled\": [");
    for (i, s) in d.sampled.iter().enumerate() {
        let _ = write!(
            o,
            "{}\n        {{\"set\": {}, \"binding\": {}, \"source\": {}, \
             \"base\": \"{:#x}\", \"width\": {}, \"height\": {}, \
             \"descriptor_honoured\": {}, \"texels\": {}, \"t_sharp\": {}, \
             \"s_sharp\": {}, \"sampler_bound\": {}}}",
            if i == 0 { "" } else { "," },
            s.set,
            s.binding,
            json_str(s.source),
            s.base,
            s.width,
            s.height,
            s.descriptor_honoured,
            texture_dump_json(&s.dump),
            match &s.texture {
                Some(t) => t_sharp_json(t),
                None => "null".to_string(),
            },
            match &s.sampler {
                Some(sm) => s_sharp_json(sm),
                None => "null".to_string(),
            },
            sampler_bound_json(&s.sampler_bound)
        );
    }
    o.push_str(if d.sampled.is_empty() {
        "]\n"
    } else {
        "\n      ]\n"
    });
    o.push_str("    }");
    o
}

/// What became of a draw's render-target picture. Always states an outcome — `"dumped":
/// false` always carries a `reason`, so an absent PNG is never ambiguous (task-187).
fn rt_dump_json(d: &RtDumpOutcome) -> String {
    match d {
        RtDumpOutcome::Disabled => format!(
            "{{\"dumped\": false, \"reason\": \"render-target dumping is off; set {}=1\"}}",
            ps4_core::snapshot::RENDER_TARGETS_ENV
        ),
        RtDumpOutcome::Videoout => "{\"dumped\": false, \"reason\": \"this draw targets \
             VIDEOOUT, not an offscreen render target — the presented frame is what \
             UNEMUPS4_DUMP_PNG dumps per flip\"}"
            .to_string(),
        // "requested", never "written": the copy is fire-and-forget on the display thread,
        // so this file's existence is not something the submit thread can assert.
        RtDumpOutcome::Requested { key } => format!(
            "{{\"dumped\": true, \"key\": {}, \"png\": \"render-targets/{key}.png\", \
             \"source\": \"host RT image copy (linear RGBA8) — NOT a guest-memory readback\", \
             \"note\": \"requested from the display thread; an absent file means the copy \
             failed and was logged, never that the target was empty\"}}",
            json_str(key)
        ),
    }
}

/// What became of a sampled texture's texel dump. Always states an outcome — `"dumped":
/// false` always carries a `reason`, so an absent texture file is never ambiguous.
fn texture_dump_json(d: &TextureDumpOutcome) -> String {
    match d {
        TextureDumpOutcome::Disabled => format!(
            "{{\"dumped\": false, \"reason\": \"texture dumping is off; set {}=1\"}}",
            ps4_core::snapshot::TEXTURES_ENV
        ),
        // Not "out of scope" any more (task-187): the texels are GPU-resident, so THIS dump
        // — which reads guest memory — legitimately has nothing to read, but the producing
        // target's own picture is available under `render-targets/`. Point the reader there
        // rather than at RenderDoc.
        TextureDumpOutcome::RenderTargetSource => format!(
            "{{\"dumped\": false, \"reason\": \"texels are GPU-resident (RT-as-texture), not in \
             guest memory — this dump reads guest memory only. For the pixels, see the \
             producing draw's target_dump PNG under render-targets/ ({}=1)\"}}",
            ps4_core::snapshot::RENDER_TARGETS_ENV
        ),
        TextureDumpOutcome::TooLarge { span, cap } => format!(
            "{{\"dumped\": false, \"reason\": \"byte_span {span} over the {} cap of {cap}\", \
             \"byte_span\": {span}, \"cap_bytes\": {cap}}}",
            ps4_core::snapshot::TEXTURE_MAX_BYTES_ENV
        ),
        // Explicitly not "dumped a buffer of zeros": a faulting bounded read and a
        // legitimately-zero texture are different findings (module rule 1).
        TextureDumpOutcome::ReadFailed { span } => format!(
            "{{\"dumped\": false, \"read_failed\": true, \"reason\": \"bounded read of \
             {span} bytes at the T# base faulted\", \"byte_span\": {span}}}"
        ),
        TextureDumpOutcome::Dumped {
            key,
            detiled_key,
            span,
            detiled,
        } => format!(
            "{{\"dumped\": true, \"key\": {}, \"byte_span\": {span}, \
             \"raw\": \"textures/{key}.raw\", \"detiled\": {}}}",
            json_str(key),
            match (detiled, detiled_key.as_deref()) {
                // The detiled files carry the layout-aware stem (`detiled_key`), NOT `key`, so a
                // second layout of the same bytes points at its own picture.
                (Ok(len), Some(dkey)) => format!(
                    "{{\"ok\": true, \"bytes\": {len}, \"raw\": \"textures/{dkey}.detiled.raw\", \
                     \"png\": \"textures/{dkey}.png\"}}"
                ),
                (Ok(len), None) => format!("{{\"ok\": true, \"bytes\": {len}}}"),
                (Err(e), _) => format!("{{\"ok\": false, \"error\": {}}}", json_str(e)),
            }
        ),
    }
}

/// The decoded V# — the descriptor the GUEST supplied, not a value we chose.
fn v_sharp_json(d: &BufferDesc) -> String {
    format!(
        "{{\"base\": \"{:#x}\", \"stride\": {}, \"num_records\": {}, \"dfmt\": {}, \
         \"nfmt\": {}, \"dst_sel\": [{}, {}, {}, {}], \"byte_span\": {}, \"null\": {}}}",
        d.base,
        d.stride,
        d.num_records,
        json_dbg(&d.dfmt),
        json_dbg(&d.nfmt),
        d.dst_sel[0],
        d.dst_sel[1],
        d.dst_sel[2],
        d.dst_sel[3],
        d.byte_span(),
        d.is_null()
    )
}

/// The decoded T# — the guest's image descriptor.
fn t_sharp_json(t: &TextureDesc) -> String {
    format!(
        "{{\"base\": \"{:#x}\", \"width\": {}, \"height\": {}, \"dfmt\": {}, \"nfmt\": {}, \
         \"tiling_index\": {}, \"tile_kind\": {}, \"pitch\": {}, \"byte_span\": {}}}",
        t.base,
        t.width,
        t.height,
        t.dfmt,
        t.nfmt,
        t.tiling_index,
        json_dbg(&ps4_core::tiling::tile_kind(t.tiling_index)),
        t.pitch,
        t.byte_span()
    )
}

/// The decoded S# — the guest's sampler state (the portable subset we model).
fn s_sharp_json(s: &SamplerState) -> String {
    format!(
        "{{\"bilinear\": {}, \"clamp_x\": {}, \"clamp_y\": {}}}",
        s.bilinear,
        json_dbg(&s.clamp_x),
        json_dbg(&s.clamp_y)
    )
}

/// The sampler the backend was actually told to bind (task-201). Emitted alongside
/// `s_sharp` (the guest's REQUEST) so the two can be compared: they agreeing is the
/// invariant, and them disagreeing is the bug that a request-only capture cannot show.
fn sampler_bound_json(s: &SamplerDesc) -> String {
    format!(
        "{{\"mag_filter\": {}, \"min_filter\": {}, \"address_mode_u\": {}, \
         \"address_mode_v\": {}}}",
        json_dbg(&s.mag_filter),
        json_dbg(&s.min_filter),
        json_dbg(&s.address_mode_u),
        json_dbg(&s.address_mode_v)
    )
}

/// The derived render target. `base`/`size` are present only for an offscreen target — the
/// videoout path names a registered display buffer, which has no `CB_COLOR0`-derived range.
fn target_json(t: &TargetDesc) -> String {
    // The variant NAME plus flat `base`/`size` fields — not `Debug` of the whole enum, whose
    // embedded payload would duplicate those two fields in a form no consumer can index.
    let (kind, base, size) = match t.kind {
        TargetKind::Offscreen { base, size } => {
            ("Offscreen", format!("\"{base:#x}\""), size.to_string())
        }
        TargetKind::Videoout => ("Videoout", "null".to_string(), "null".to_string()),
    };
    format!(
        "{{\"kind\": {}, \"base\": {base}, \"size\": {size}, \"width\": {}, \"height\": {}, \
         \"pitch\": {}, \"format\": {}, \"tiling\": {}}}",
        json_str(kind),
        t.width,
        t.height,
        t.pitch,
        json_dbg(&t.format),
        json_dbg(&t.tiling)
    )
}

/// The pipeline key, with the raw blend/depth register words kept alongside the decoded
/// enable bits — the raw word is what a hardware reference or a RenderDoc capture is
/// comparable against, and task-179 turned on exactly those bits.
fn pipeline_json(p: &PipelineKey) -> String {
    format!(
        "{{\"vs_hash\": \"{:#018x}\", \"ps_hash\": \"{:#018x}\", \"color_format\": {}, \
         \"blend\": {}, \
         \"depth\": {{\"enable\": {}, \"control\": \"{:#010x}\"}}, \
         \"vertex_layout\": {}, \"topology\": {}, \"resources\": {}}}",
        p.vs_hash,
        p.ps_hash,
        json_dbg(&p.color_format),
        blend_json(&p.blend),
        p.depth.enable,
        p.depth.control,
        json_dbg(&p.vertex_layout),
        // `VGT_PRIMITIVE_TYPE`-derived (task-184). Recorded because it is invisible
        // everywhere else a reader would look: it lives in the UCONFIG bank, which
        // `registers.json` only shows as of END OF FRAME, and a rect-list fill draw is
        // otherwise indistinguishable in this file from a triangle-list draw of three
        // vertices — the exact pair that hid Celeste's never-landing bloom clears.
        json_dbg(&p.topology),
        resources_json(&p.resources)
    )
}

/// The blend state, raw word AND decoded.
///
/// The raw `CB_BLEND0_CONTROL` stays because it is what a hardware reference or a RenderDoc
/// capture is comparable against, and task-179 turned on exactly those bits. The decode is
/// here because `"control": "0x45010501"` had to be worked out to ONE /
/// ONE_MINUS_SRC_ALPHA / ADD by hand, mid-investigation, more than once — and the field split
/// comes from [`ps4_core::gpu::BlendKey::fields`], the same one the Vulkan pipeline is built
/// from, so the names cannot drift away from what actually ran.
fn blend_json(b: &ps4_core::gpu::BlendKey) -> String {
    use ps4_core::gpu::{blend_factor_name, blend_op_name};
    let f = b.fields();
    format!(
        "{{\"enable\": {}, \"control\": \"{:#010x}\", \"write_mask\": \"{:#x}\", \
         \"decoded\": {{\"color_src\": {}, \"color_dst\": {}, \"color_op\": {}, \
         \"alpha_src\": {}, \"alpha_dst\": {}, \"alpha_op\": {}, \"separate_alpha\": {}, \
         \"write_rgba\": {}}}}}",
        b.enable,
        b.control,
        b.write_mask,
        json_str(blend_factor_name(f.color_src)),
        json_str(blend_factor_name(f.color_dst)),
        json_str(blend_op_name(f.color_comb)),
        json_str(blend_factor_name(f.alpha_src)),
        json_str(blend_factor_name(f.alpha_dst)),
        json_str(blend_op_name(f.alpha_comb)),
        f.separate_alpha,
        json_str(&write_mask_str(b.write_mask))
    )
}

/// `CB_TARGET_MASK` bits `[3:0]` as `"RGBA"` / `"RGB-"` / … — masking ALPHA off while
/// rendering a premultiplied intermediate is load-bearing (see [`ps4_core::gpu::BlendKey`]),
/// and a hex nibble does not read as such at a glance.
fn write_mask_str(mask: u8) -> String {
    ["R", "G", "B", "A"]
        .iter()
        .enumerate()
        .map(|(i, c)| if mask & (1 << i) != 0 { c } else { &"-" })
        .copied()
        .collect()
}

/// The descriptor provenance folded into the pipeline key: which `(set, binding)` each
/// declared descriptor landed at. Emitted as structured slots rather than `Debug` text so a
/// diff between two frames points at the slot that moved.
fn resources_json(r: &ps4_core::gpu::ResourceSignature) -> String {
    let slot = |s: Option<ps4_core::gpu::ResourceSlot>| match s {
        Some(s) => format!("{{\"set\": {}, \"binding\": {}}}", s.set, s.binding),
        None => "null".to_string(),
    };
    format!(
        "{{\"storage\": {}, \"const_storage\": {}, \"const_storage_fragment\": {}, \
         \"textures\": [{}]}}",
        slot(r.storage),
        slot(r.const_storage),
        slot(r.const_storage_fragment),
        r.textures()
            .map(|s| slot(Some(s)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The register-derived viewport. Height may be NEGATIVE — that is the Vulkan Y-flip, not a
/// decode error (see [`crate::derive::derive_viewport`]).
fn viewport_json(v: &Viewport) -> String {
    format!(
        "{{\"x\": {}, \"y\": {}, \"width\": {}, \"height\": {}}}",
        json_f32(v.x),
        json_f32(v.y),
        json_f32(v.width),
        json_f32(v.height)
    )
}

fn scissor_json(s: &Scissor) -> String {
    format!(
        "{{\"x\": {}, \"y\": {}, \"width\": {}, \"height\": {}}}",
        s.x, s.y, s.width, s.height
    )
}

fn shader_json(s: &ShaderIdent) -> String {
    format!(
        "{{\"kind\": {}, \"addr\": {}, \"hash\": \"{:#018x}\", \"spirv_words\": {}, \
         \"dump\": {}, \"ps_input_map\": {}}}",
        json_str(s.kind),
        match s.addr {
            Some(a) => format!("\"{a:#x}\""),
            None => "null".to_string(),
        },
        s.hash,
        s.spirv_words,
        match &s.dump {
            // The `.spv` is named FIRST because it is the only one of the three that cannot
            // be wrong about what executed (see `Recorder::dump_shader`). `sb`/`disasm` are
            // `null` when that file does not exist — an embedded shader has no `.sb`, and an
            // unreadable one has neither.
            Some(d) => {
                let path = |present: bool, ext: &str| match present {
                    true => format!("\"shaders/{}.{ext}\"", d.key),
                    false => "null".to_string(),
                };
                format!(
                    "{{\"spirv\": \"shaders/{}.spv\", \"sb\": {}, \"disasm\": {}}}",
                    d.key,
                    path(d.sb, "sb"),
                    path(d.disasm, "txt")
                )
            }
            None => "null".to_string(),
        },
        match &s.ps_input_map {
            Some(m) => {
                let slots: Vec<String> = m.iter().map(|o| o.to_string()).collect();
                format!("[{}]", slots.join(", "))
            }
            None => "null".to_string(),
        }
    )
}

/// JSON has no NaN/Infinity literal. Emit those as strings rather than writing invalid JSON
/// — a NaN viewport scale is a real finding and must survive into the file.
fn json_f32(v: f32) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        json_str(&format!("{v}"))
    }
}

/// Constant-buffer bytes as hex dwords. The dword view is what matches a disassembly's
/// `s_buffer_load_dwordx4` offsets.
fn dwords_json(bytes: &[u8]) -> String {
    bytes
        .chunks_exact(4)
        .map(|c| format!("\"{:#010x}\"", u32::from_le_bytes([c[0], c[1], c[2], c[3]])))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The same bytes reinterpreted as `f32`. Constant buffers hold matrices and texel steps, so
/// both views are dumped rather than making the reader convert; neither is authoritative
/// over the other — the bytes are.
fn floats_json(bytes: &[u8]) -> String {
    bytes
        .chunks_exact(4)
        .map(|c| json_f32(f32::from_le_bytes([c[0], c[1], c[2], c[3]])))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Number of draw lines `summary.txt` prints before eliding. Keeps the file to roughly one
/// screen (AC #2) — the full list is always in `draws.json`.
const SUMMARY_DRAW_LINES: usize = 40;

/// One screen of human-readable state, for eyeballing a capture without `jq`.
fn summary_txt(
    state: &GpuState,
    frame: u64,
    draws: &[DrawRecord],
    deferred: &[DeferredRecord],
) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "unemups4 GPU snapshot — frame {frame}");
    let _ = writeln!(
        o,
        "registers: context={} sh={} uconfig={} config={}   draws={} deferred={}",
        state.ctx_regs.len(),
        state.sh_regs.len(),
        state.uconfig_regs.len(),
        state.config_regs.len(),
        draws.len(),
        deferred.len()
    );
    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "  #  kind             count  target                    blend    vs         ps         sampled"
    );
    for d in draws.iter().take(SUMMARY_DRAW_LINES) {
        let target = match d.target.kind {
            TargetKind::Offscreen { base, .. } => {
                format!("rt {base:#x} {}x{}", d.target.width, d.target.height)
            }
            TargetKind::Videoout => format!("videoout {}x{}", d.target.width, d.target.height),
        };
        // Every sampled texture, so a multi-texture pass reads as what it is instead of
        // looking like a single-texture draw (task-199).
        let sampled = if d.sampled.is_empty() {
            "-".to_string()
        } else {
            d.sampled
                .iter()
                .map(|s| format!("{} {:#x} {}x{}", s.source, s.base, s.width, s.height))
                .collect::<Vec<_>>()
                .join(" + ")
        };
        let addr = |i: &ShaderIdent| match i.addr {
            Some(a) => format!("{a:#x}"),
            None => i.kind.to_string(),
        };
        let _ = writeln!(
            o,
            "{:>3}  {:<15} {:>6}  {:<24}  {:<7}  {:<9}  {:<9}  {sampled}",
            d.ordinal,
            d.kind,
            d.count,
            target,
            if d.pipeline.blend.enable {
                format!("{:#x}", d.pipeline.blend.control & 0xFFFF)
            } else {
                "off".to_string()
            },
            addr(&d.vs),
            addr(&d.ps),
        );
    }
    if draws.len() > SUMMARY_DRAW_LINES {
        let _ = writeln!(
            o,
            "... {} more draws (see draws.json)",
            draws.len() - SUMMARY_DRAW_LINES
        );
    }

    // Deferred draws get their own block, and it is worth a screen: a missing draw is one of
    // the commonest causes of a missing picture, and the reason used to live only in a log.
    if !deferred.is_empty() {
        let _ = writeln!(o);
        let _ = writeln!(o, "DEFERRED (never reached the backend):");
        let mut by_reason: Vec<(&str, usize)> = Vec::new();
        for d in deferred {
            match by_reason.iter_mut().find(|(r, _)| *r == d.reason.as_str()) {
                Some((_, n)) => *n += 1,
                None => by_reason.push((d.reason.as_str(), 1)),
            }
        }
        for (reason, n) in &by_reason {
            let _ = writeln!(o, "  {n:>3}x {reason}");
        }
        for d in deferred.iter().take(SUMMARY_DRAW_LINES) {
            let _ = write!(
                o,
                "  after draw {:>3}  {:<15} count={:<6} {}",
                d.after_draw, d.kind, d.count, d.reason
            );
            // Append the exact instruction detail for a `.sb`-GCN defer (task-195) so the gap is
            // legible here too, not only in draws.json: stage, shader identity, and the decoded
            // unsupported instruction + offset.
            if let Some(det) = &d.detail {
                let _ = write!(
                    o,
                    "  [{} @ {:#x} #{:#018x}]",
                    det.stage, det.shader_addr, det.shader_hash
                );
                if let Some(inst) = &det.instruction {
                    let _ = write!(o, " {inst}");
                }
            }
            let _ = writeln!(o);
        }
        if deferred.len() > SUMMARY_DRAW_LINES {
            let _ = writeln!(
                o,
                "  ... {} more (see draws.json deferred_draws)",
                deferred.len() - SUMMARY_DRAW_LINES
            );
        }
    }

    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "registers.json = every register the guest wrote (named where known, raw index otherwise)"
    );
    let _ = writeln!(
        o,
        "                 END-OF-FRAME values. For what a SPECIFIC draw ran with, read that \
         draw's register_delta in draws.json."
    );
    let _ = writeln!(
        o,
        "draws.json     = per-draw derived state, decoded blend, the decoded T#/V#/S# the \
         guest supplied, per-draw register delta,"
    );
    let _ = writeln!(
        o,
        "                 and deferred_draws (draws that bailed out of setup_draw, with the \
         reason)"
    );
    let _ = writeln!(
        o,
        "shaders/       = per bound shader: <key>.spv (THE MODULE HANDED TO VULKAN — \
         spirv-dis/spirv-val take it directly),"
    );
    let _ = writeln!(
        o,
        "                 <key>.sb (raw GCN), <key>.txt (in-tree disassembly —"
    );
    let _ = writeln!(
        o,
        "                 when it disagrees with the .spv, the .spv is what ran). Keyed on a \
         hash that includes the PS input"
    );
    let _ = writeln!(
        o,
        "                 routing, so one .sb under two routings is two modules, as it is at \
         runtime."
    );
    let _ = writeln!(
        o,
        "textures/      = sampled guest texels, raw (tiled) + detiled + PNG. OFF by default: \
         set {}=1.",
        ps4_core::snapshot::TEXTURES_ENV
    );
    let _ = writeln!(
        o,
        "                 Per-texture cap {} bytes ({}); over-cap and unreadable textures are \
         flagged in draws.json, never omitted.",
        ps4_core::snapshot::texture_max_bytes(),
        ps4_core::snapshot::TEXTURE_MAX_BYTES_ENV
    );
    // Which path produced these pixels is the whole point of the paragraph: a host-image
    // copy and a guest-memory readback are NOT interchangeable, and a reader who takes one
    // for the other will draw the wrong conclusion about what the guest can see.
    let n_dumped = draws
        .iter()
        .filter_map(|d| match &d.target_dump {
            RtDumpOutcome::Requested { key } => Some(key.as_str()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let _ = writeln!(
        o,
        "render-targets/= RENDER-TARGET PIXELS as PNG, one per offscreen target of this \
         frame ({n_dumped} this frame). OFF by default: set {}=1.",
        ps4_core::snapshot::RENDER_TARGETS_ENV
    );
    let _ = writeln!(
        o,
        "                 SOURCE: a copy of the HOST render-target image, which is linear \
         RGBA8. This is NOT the guest-memory readback"
    );
    let _ = writeln!(
        o,
        "                 (UNEMUPS4_RT_READBACK) and the two are not interchangeable — that \
         one writes the GUEST's tiled layout back into"
    );
    let _ = writeln!(
        o,
        "                 guest memory and REFUSES the 2D macro-tiled targets this title \
         uses (task-181). Looking at pixels needs no tiling,"
    );
    let _ = writeln!(
        o,
        "                 so this path dumps them anyway, and writes NOTHING to guest \
         memory. What the GUEST can read back is unaffected."
    );
    let _ = writeln!(
        o,
        "                 COST: the copy stalls the GPU, so it perturbs frame TIMING — never \
         frame CONTENT (no draw, binding, register or"
    );
    let _ = writeln!(
        o,
        "                 guest byte changes). A dumped frame renders identically to an \
         undumped one; task-185 AC #5 still holds."
    );
    let _ = writeln!(
        o,
        "                 The PNG carries the target's REAL alpha, so a target rendered with \
         alpha 0 looks transparent — that is data, not an"
    );
    let _ = writeln!(
        o,
        "                 empty file. Each draw's target_dump in draws.json names its PNG or \
         the reason there is none; an absent file means the"
    );
    let _ = writeln!(
        o,
        "                 copy failed and was logged, NEVER \"the target was empty\". \
         VIDEOOUT is not dumped here — that is the presented frame,"
    );
    let _ = writeln!(
        o,
        "                 which UNEMUPS4_DUMP_PNG writes per flip."
    );
    o
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cache::ResLayout;
    use crate::pm4::opcodes::{context_reg, reg_base, sh_reg};

    /// Serialises the tests that redirect the dump root. `UNEMUPS4_SNAPSHOT_DIR` is process-
    /// global, so two tests setting it concurrently would write into each other's scratch
    /// directory and assert on the wrong files. `pub(crate)` because `exec`'s end-to-end
    /// snapshot test sets the same process-global vars and must take the same lock.
    pub(crate) static DUMP_ROOT_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A register the guest wrote but nothing reads must still appear, with a null name and
    /// its raw index. This is rule 2 of the module docs, and the direct lesson of task-179.
    #[test]
    fn registers_json_dumps_named_and_unnamed_registers() {
        let mut s = GpuState::default();
        s.ctx_regs.set(context_reg::CB_COLOR0_BASE, 0x1234);
        // An index this codebase has no constant for.
        s.ctx_regs.set(reg_base::CONTEXT + 0x3FE, 0xDEAD_BEEF);
        s.sh_regs.set(sh_reg::SPI_SHADER_PGM_LO_VS, 0x2000);

        let json = registers_json(&s);
        assert!(json.contains("\"name\": \"CB_COLOR0_BASE\""), "{json}");
        // The unnamed one is present with a null name, NOT skipped.
        assert!(json.contains("\"value_hex\": \"0xdeadbeef\""), "{json}");
        assert!(json.contains("\"name\": null"), "{json}");
        assert!(
            json.contains("\"name\": \"SPI_SHADER_PGM_LO_VS\""),
            "{json}"
        );
        // Every bank is present even when empty, so a diff never shifts.
        for bank in ["\"context\"", "\"sh\"", "\"uconfig\"", "\"config\""] {
            assert!(json.contains(bank), "missing bank {bank} in {json}");
        }
    }

    /// Two captures of identical state must produce identical bytes, or every frame-to-frame
    /// diff is drowned in `HashMap` iteration-order noise — which would defeat the purpose of
    /// the tool.
    #[test]
    fn registers_json_is_deterministic_and_sorted() {
        let mut s = GpuState::default();
        for i in [0x30u32, 0x02, 0x11, 0x01] {
            s.ctx_regs.set(reg_base::CONTEXT + i, i);
        }
        assert_eq!(registers_json(&s), registers_json(&s));

        let json = registers_json(&s);
        let positions: Vec<usize> = [0x01u32, 0x02, 0x11, 0x30]
            .iter()
            .map(|i| {
                let needle = format!("\"index\": {}", reg_base::CONTEXT + i);
                json.find(&needle)
                    .unwrap_or_else(|| panic!("missing {needle} in {json}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "registers must be emitted in ascending index order: {positions:?}"
        );
    }

    /// The whole zero-cost-when-idle contract in one assertion: an unarmed recorder records
    /// nothing, so the draw path's `if armed()` guard is the only cost of an idle frame.
    #[test]
    fn unarmed_recorder_records_nothing() {
        let mut r = Recorder::default();
        assert!(!r.armed());
        r.record_draw(DrawInput {
            kind: "DrawIndexAuto",
            count: 3,
            draw: &DrawState {
                target: TargetDesc::default(),
                pipeline: PipelineKey::default(),
                viewport: Viewport::default(),
                scissor: Scissor::default(),
            },
            key: &PipelineKey::default(),
            regs: RegSnapshot::default(),
            vs: None,
            vs_spirv: &[],
            ps: None,
            ps_spirv: &[],
            buffers: &[],
            vs_const: None,
            ps_const: None,
            sampled: Vec::new(),
        });
        assert_eq!(r.recorded_draws(), 0);
    }

    /// The frame-counter state machine as the executor drives it: idle stays idle, a request
    /// arms exactly one frame, and a burst arms consecutive frames one boundary at a time.
    #[test]
    fn frame_boundary_arms_from_the_cross_thread_budget() {
        let _guard = DUMP_ROOT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        ps4_core::snapshot::clear();
        // Write into a scratch dir so the test never touches the real dump root.
        let tmp =
            std::env::temp_dir().join(format!("unemups4-snapshot-test-{}", std::process::id()));
        // SAFETY-of-behaviour note: this only affects where THIS test's dumps land. The env
        // var is read at write time, not cached.
        unsafe { std::env::set_var(ps4_core::snapshot::DIR_ENV, &tmp) };

        let mut s = GpuState::default();

        // Idle: a frame boundary with no pending request leaves the recorder unarmed.
        on_frame_boundary(&mut s, 10);
        assert!(!s.snapshot.armed());

        // F10: one press arms the NEXT frame (the elapsed part of the current one is gone).
        ps4_core::snapshot::request(1);
        on_frame_boundary(&mut s, 11);
        assert!(s.snapshot.armed());
        assert_eq!(s.snapshot.frame(), 11);

        // The next boundary writes that frame and, with nothing pending, goes idle.
        on_frame_boundary(&mut s, 12);
        assert!(!s.snapshot.armed());
        assert!(tmp.join("frame-00011").join("registers.json").is_file());
        assert!(tmp.join("frame-00011").join("draws.json").is_file());
        assert!(tmp.join("frame-00011").join("summary.txt").is_file());

        // F9: a burst arms consecutive frames, one per boundary, and then stops.
        ps4_core::snapshot::request(2);
        on_frame_boundary(&mut s, 20);
        assert!(s.snapshot.armed() && s.snapshot.frame() == 20);
        on_frame_boundary(&mut s, 21);
        assert!(s.snapshot.armed() && s.snapshot.frame() == 21);
        on_frame_boundary(&mut s, 22);
        assert!(!s.snapshot.armed());
        assert!(tmp.join("frame-00020").is_dir());
        assert!(tmp.join("frame-00021").is_dir());
        assert!(!tmp.join("frame-00022").exists());

        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::remove_var(ps4_core::snapshot::DIR_ENV) };
        ps4_core::snapshot::clear();
    }

    /// An armed recorder captures each draw once, in submission order, and a captured frame's
    /// `draws.json` names the decoded descriptor rather than a derived stand-in.
    #[test]
    fn armed_recorder_captures_draws_in_order() {
        // `record_draw` now dumps shader artefacts as a side effect, so this test must
        // redirect the dump root like the others — otherwise `cargo test` writes a
        // `gpu-snapshots/` tree into whatever directory it was run from.
        let _guard = DUMP_ROOT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp =
            std::env::temp_dir().join(format!("unemups4-snapshot-draws-{}", std::process::id()));
        // SAFETY-of-behaviour: only redirects THIS test's dumps; the var is read at write time.
        unsafe { std::env::set_var(ps4_core::snapshot::DIR_ENV, &tmp) };
        let mut r = Recorder {
            armed: true,
            ..Recorder::default()
        };
        let state = DrawState {
            target: TargetDesc {
                width: 960,
                height: 540,
                pitch: 1024,
                kind: TargetKind::Offscreen {
                    base: 0x4000,
                    size: 0x1000,
                },
                ..TargetDesc::default()
            },
            pipeline: PipelineKey::default(),
            viewport: Viewport::default(),
            scissor: Scissor::default(),
        };
        let range = BufferRange {
            addr: 0x9000,
            size: 96,
            layout: ResLayout::VertexBuf,
            desc: crate::vbuf::decode_v_sharp([0x9000, (24 << 16), 4, 0]),
        };
        for (i, kind) in ["DrawIndexAuto", "DrawIndex2"].iter().enumerate() {
            r.record_draw(DrawInput {
                kind,
                count: 3 + i as u32,
                draw: &state,
                key: &PipelineKey::default(),
                regs: RegSnapshot::default(),
                vs: Some(ShaderRef::GcnBinary {
                    addr: 0x1_2300,
                    ps_input_map: ps4_gcn::PsInputMap::default(),
                    res: Default::default(),
                }),
                vs_spirv: &[0x0723_0203, 7],
                ps: None,
                ps_spirv: &[],
                buffers: std::slice::from_ref(&range),
                vs_const: None,
                ps_const: None,
                sampled: Vec::new(),
            });
        }
        assert_eq!(r.recorded_draws(), 2);

        let json = draws_json(3, &r.draws, &[]);
        assert!(json.contains("\"draw_count\": 2"), "{json}");
        assert!(json.contains("\"ordinal\": 0"), "{json}");
        assert!(json.contains("\"ordinal\": 1"), "{json}");
        // The decoded V# the guest supplied, with its real stride.
        assert!(json.contains("\"stride\": 24"), "{json}");
        assert!(json.contains("\"base\": \"0x9000\""), "{json}");
        // The VS's `.sb` address — what you hand a disassembler.
        assert!(json.contains("\"addr\": \"0x12300\""), "{json}");
        // The offscreen target's guest base, not just its extent.
        assert!(json.contains("\"base\": \"0x4000\""), "{json}");

        let summary = summary_txt(&GpuState::default(), 3, &r.draws, &[]);
        assert!(summary.contains("frame 3"), "{summary}");
        assert!(summary.contains("rt 0x4000 960x540"), "{summary}");

        flush_writes();
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::remove_var(ps4_core::snapshot::DIR_ENV) };
    }

    /// The per-draw register delta is the generalisation of the `ps_input_map` patch: any
    /// register the guest reprograms mid-frame must be attributable to the draw that ran
    /// with it, because `registers.json` only ever shows the LAST draw's value. task-179 was
    /// exactly this shape.
    #[test]
    fn register_delta_reports_what_changed_between_draws() {
        let mut s = GpuState::default();
        s.ctx_regs.set(context_reg::SPI_PS_INPUT_CNTL_0, 0);
        s.ctx_regs.set(context_reg::CB_COLOR0_BASE, 0x1000);
        let first = RegSnapshot::capture(&s);

        // Draw 0 diffs against an empty file: everything the guest had written shows up as a
        // first write (`from: null`), not as "unchanged".
        let initial = first.delta(&RegSnapshot::default());
        assert_eq!(initial.len(), 2);
        assert!(initial.iter().all(|c| c.from.is_none()));

        // The guest reroutes PS attribute 0 between two draws and leaves everything else.
        s.ctx_regs.set(context_reg::SPI_PS_INPUT_CNTL_0, 1);
        let second = RegSnapshot::capture(&s);
        let delta = second.delta(&first);
        assert_eq!(delta.len(), 1, "only the reprogrammed register may appear");
        assert_eq!(delta[0].index, context_reg::SPI_PS_INPUT_CNTL_0);
        assert_eq!(delta[0].from, Some(0));
        assert_eq!(delta[0].to, Some(1));

        // No change between two identical draws — a delta of nothing, not a repeat of the file.
        assert!(second.delta(&second).is_empty());
    }

    /// A draw that bailed out of `setup_draw` must appear WITH ITS REASON, and must not be
    /// counted among the draws the frame submitted.
    #[test]
    fn deferred_draws_are_recorded_with_their_reason() {
        let mut r = Recorder {
            armed: true,
            ..Recorder::default()
        };
        r.record_deferred("DrawIndexAuto", 6, "macro-tiled-texture");
        r.record_deferred("DrawIndex2", 12, "ps-multiple-samplers");
        assert_eq!(r.recorded_draws(), 0, "a defer is not a submitted draw");

        let json = draws_json(7, &[], &r.deferred);
        assert!(json.contains("\"draw_count\": 0"), "{json}");
        assert!(json.contains("\"deferred_count\": 2"), "{json}");
        assert!(
            json.contains("\"reason\": \"macro-tiled-texture\""),
            "{json}"
        );
        assert!(
            json.contains("\"reason\": \"ps-multiple-samplers\""),
            "{json}"
        );

        // ...and it is legible without `jq`, grouped by cause.
        let summary = summary_txt(&GpuState::default(), 7, &[], &r.deferred);
        assert!(summary.contains("DEFERRED"), "{summary}");
        assert!(summary.contains("macro-tiled-texture"), "{summary}");

        // An unarmed recorder records no defers either — the hot path stays free.
        let mut idle = Recorder::default();
        idle.record_deferred("DrawIndexAuto", 6, "macro-tiled-texture");
        assert!(idle.deferred.is_empty());
    }

    /// task-195: an unsupported-`.sb`-GCN-shader defer carries the exact failing instruction
    /// (decoded + dword offset), the stage, and the shader address/hash into draws.json and
    /// summary.txt — not a bare `"unsupported-gcn-shader"` with null detail fields.
    #[test]
    fn gcn_defer_carries_the_instruction_detail_into_the_snapshot() {
        // Recording a GCN defer now also dumps the rejected shader's raw `.sb` (task-196), which
        // resolves a path under the process-global dump root — so redirect it to scratch and take
        // the serialising lock, exactly like the other file-writing snapshot tests.
        let _guard = DUMP_ROOT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "unemups4-gcn-defer-detail-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var(ps4_core::snapshot::DIR_ENV, &tmp) };

        let mut r = Recorder {
            armed: true,
            ..Recorder::default()
        };
        // The instruction text the exec-side, armed-gated caller formats from the recompiler's
        // RecompileError Display — mirrored here so the plumbing is exercised end to end.
        let instruction = "unsupported instruction at dword offset 51: Vop2 { op: 5, .. }";
        r.record_deferred_gcn(
            "DrawIndexOffset",
            6,
            "PS",
            0x0020_0500,
            0xdead_beef_0000_0001,
            Some(instruction.to_string()),
        );

        let json = draws_json(9, &[], &r.deferred);
        // The coarse slug still names the category (summary grouping relies on it)...
        assert!(
            json.contains("\"reason\": \"unsupported-gcn-shader\""),
            "{json}"
        );
        // ...and the detail fields are FILLED, not null: stage, shader address/hash, and the
        // exact decoded instruction + offset.
        assert!(json.contains("\"stage\": \"PS\""), "{json}");
        assert!(json.contains("\"shader_addr\": \"0x200500\""), "{json}");
        // No `0x` prefix: this must match `dump_deferred_shader`'s on-disk stem
        // `{tag}-{hash:016x}` (here `ps-deadbeef00000001`) verbatim, since a deferred draw
        // carries no explicit dump-path pointer.
        assert!(
            json.contains("\"shader_hash\": \"deadbeef00000001\""),
            "{json}"
        );
        assert!(json.contains("dword offset 51"), "{json}");

        // summary.txt names it too, on the deferred line (not just grouped by cause).
        let summary = summary_txt(&GpuState::default(), 9, &[], &r.deferred);
        assert!(summary.contains("[PS @ 0x200500"), "{summary}");
        assert!(summary.contains("dword offset 51"), "{summary}");

        // A coarser recompile-path defer (no single instruction) still carries stage + shader
        // id, with a null instruction — better than nothing, never a fabricated instruction.
        let mut r2 = Recorder {
            armed: true,
            ..Recorder::default()
        };
        r2.record_deferred_gcn("DrawIndexAuto", 3, "VS", 0x0030_0000, 7, None);
        let json2 = draws_json(9, &[], &r2.deferred);
        assert!(json2.contains("\"stage\": \"VS\""), "{json2}");
        assert!(json2.contains("\"instruction\": null"), "{json2}");

        // Armed gate: an unarmed recorder records nothing, so a non-armed run pays no detail.
        let mut idle = Recorder::default();
        idle.record_deferred_gcn(
            "DrawIndexOffset",
            6,
            "PS",
            0x0020_0500,
            1,
            Some(instruction.to_string()),
        );
        assert!(
            idle.deferred.is_empty(),
            "unarmed recorder must record nothing"
        );

        flush_writes();
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::remove_var(ps4_core::snapshot::DIR_ENV) };
    }

    /// task-196 Part A: a shader that DEFERS because the recompiler rejected it is never handed
    /// to `dump_shader` (that runs only for shaders that recompiled), so its raw GCN would be
    /// invisible — yet a defer is exactly the shader a reader most needs. `record_deferred_gcn`
    /// must therefore dump the rejected `.sb` itself, under the same `shaders/<stage>-<hash>.sb`
    /// scheme a successful dump uses, and ONLY when armed.
    #[test]
    fn deferred_shader_dumps_its_raw_sb_when_armed() {
        // A minimal bounded-read seam over a host buffer (host addr == guest ptr), enough for
        // `read_shader_code`'s fallback window to succeed and yield real `.sb` dwords.
        struct BufReader {
            base: u64,
            end: u64,
        }
        impl ps4_core::bounded_read::BoundedRead for BufReader {
            fn read_ranged(&self, addr: u64, size: usize) -> Result<Vec<u8>, &'static str> {
                if size == 0 {
                    return Ok(Vec::new());
                }
                let range_end = addr.checked_add(size as u64).ok_or("overflow")?;
                if addr < self.base || range_end > self.end {
                    return Err("out of region");
                }
                let mut buf = vec![0u8; size];
                unsafe {
                    std::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), size);
                }
                Ok(buf)
            }
        }

        let _guard = DUMP_ROOT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "unemups4-deferred-sb-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::set_var(ps4_core::snapshot::DIR_ENV, &tmp) };

        // A buffer at least the fallback window wide, so `read_shader_code` returns non-empty
        // code (its bytes are not a valid `.sb`, so it takes the fixed-window fallback path).
        let data = vec![0x11u8; SHADER_FALLBACK_DUMP_BYTES];
        let base = data.as_ptr() as u64;
        let reader = BufReader {
            base,
            end: base + data.len() as u64,
        };
        let seam: std::sync::Arc<dyn ps4_core::bounded_read::BoundedRead> =
            std::sync::Arc::new(reader);
        let _seam = ps4_core::bounded_read::registered_source().override_scoped(seam);

        let hash = 0x7220_3976_9396_5fd8u64;

        // Unarmed first: the hot path must pay nothing — no record AND no file.
        let mut idle = Recorder::default();
        idle.record_deferred_gcn("DrawIndexAuto", 3, "PS", base, hash, None);
        flush_writes();
        assert!(idle.deferred.is_empty());
        assert!(
            !tmp.join("shaders")
                .join(format!("ps-{hash:016x}.sb"))
                .exists(),
            "an unarmed recorder must write no shader files"
        );

        // Armed: the rejected shader's raw `.sb` lands under the same scheme as a successful dump,
        // named by the stage tag + hash the deferred record carries.
        let mut r = Recorder {
            armed: true,
            ..Recorder::default()
        };
        r.record_deferred_gcn(
            "DrawIndexAuto",
            3,
            "PS",
            base,
            hash,
            Some("invalid operand at dword offset 28 in Vop3 { .. }: Sgpr(0) (…)".to_string()),
        );
        flush_writes();

        let sb = tmp.join("shaders").join(format!("ps-{hash:016x}.sb"));
        let bytes = std::fs::read(&sb).expect("a deferred shader must dump its raw .sb");
        assert!(!bytes.is_empty(), "the dumped .sb must carry the raw GCN");
        // The dedup set marks it seen with a readable `.sb`, exactly as a successful dump would.
        assert!(r.seen_shaders.contains(&hash));
        assert!(r.shaders_with_sb.contains(&hash));

        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::remove_var(ps4_core::snapshot::DIR_ENV) };
    }

    /// `control: "0x45010501"` had to be decoded by hand mid-investigation more than once.
    /// The decode must be present, correct, and share its field split with the Vulkan
    /// pipeline — and the raw word must survive alongside it.
    #[test]
    fn blend_json_carries_the_decoded_factors_and_the_raw_word() {
        // Celeste's premultiplied-over composite.
        let json = blend_json(&ps4_core::gpu::BlendKey {
            enable: true,
            control: 0x4501_0501,
            write_mask: 0xF,
        });
        assert!(json.contains("\"control\": \"0x45010501\""), "{json}");
        assert!(json.contains("\"color_src\": \"ONE\""), "{json}");
        assert!(
            json.contains("\"color_dst\": \"ONE_MINUS_SRC_ALPHA\""),
            "{json}"
        );
        assert!(json.contains("\"color_op\": \"ADD\""), "{json}");
        assert!(json.contains("\"separate_alpha\": false"), "{json}");
        assert!(json.contains("\"write_rgba\": \"RGBA\""), "{json}");

        // Alpha masked off — the state a premultiplied intermediate relies on, and a hex
        // nibble nobody reads at a glance.
        let masked = blend_json(&ps4_core::gpu::BlendKey {
            enable: true,
            control: 0x4104_0104,
            write_mask: 0x7,
        });
        assert!(masked.contains("\"write_rgba\": \"RGB-\""), "{masked}");
        assert!(masked.contains("\"color_src\": \"SRC_ALPHA\""), "{masked}");
        assert!(masked.contains("\"color_dst\": \"ONE\""), "{masked}");
    }

    /// Every way of NOT dumping a texture must state itself. A missing texture file may never
    /// be confusable with an empty texture (module rule 1).
    #[test]
    fn texture_dump_outcomes_are_never_silent() {
        for (outcome, needle) in [
            (TextureDumpOutcome::Disabled, "texture dumping is off"),
            (
                TextureDumpOutcome::RenderTargetSource,
                "GPU-resident (RT-as-texture)",
            ),
            (
                TextureDumpOutcome::TooLarge {
                    span: 64 << 20,
                    cap: 16 << 20,
                },
                "over the",
            ),
            (
                TextureDumpOutcome::ReadFailed { span: 4096 },
                "\"read_failed\": true",
            ),
        ] {
            let json = texture_dump_json(&outcome);
            assert!(json.contains("\"dumped\": false"), "{json}");
            assert!(json.contains("\"reason\""), "{json}");
            assert!(json.contains(needle), "{needle} missing from {json}");
        }

        // A detiler failure still reports the RAW dump — which is the reason raw bytes are
        // kept at all: they stay ground truth when the detiler is the suspect.
        let json = texture_dump_json(&TextureDumpOutcome::Dumped {
            key: "abc".to_string(),
            detiled_key: None,
            span: 4096,
            detiled: Err("ShortBuffer".to_string()),
        });
        assert!(json.contains("\"dumped\": true"), "{json}");
        assert!(json.contains("textures/abc.raw"), "{json}");
        assert!(json.contains("\"ok\": false"), "{json}");

        // A detiler SUCCESS names the layout-aware detiled stem (not the content `key`), so two
        // layouts of one blob point at different pictures rather than colliding.
        let json = texture_dump_json(&TextureDumpOutcome::Dumped {
            key: "abc".to_string(),
            detiled_key: Some("abc-deadbeef".to_string()),
            span: 4096,
            detiled: Ok(64),
        });
        assert!(json.contains("textures/abc.raw"), "{json}");
        assert!(json.contains("textures/abc-deadbeef.detiled.raw"), "{json}");
        assert!(json.contains("textures/abc-deadbeef.png"), "{json}");
    }

    /// The SPIR-V is the artefact that cannot be wrong about what executed, so `draws.json`
    /// must name it — and it must be deduped on the hash that INCLUDES the PS input routing,
    /// or two routings of one `.sb` collapse to one file and the capture lies about which
    /// module ran (the third of this session's three losses on shader `0x9afae4f00`).
    #[test]
    fn shader_dump_is_named_per_draw_and_deduped_by_routing_aware_hash() {
        let _guard = DUMP_ROOT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "unemups4-snapshot-shaders-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        // SAFETY-of-behaviour: only redirects THIS test's dumps; the var is read at write time.
        unsafe { std::env::set_var(ps4_core::snapshot::DIR_ENV, &tmp) };

        let mut r = Recorder {
            armed: true,
            ..Recorder::default()
        };
        let spirv = [0x0723_0203u32, 0x0001_0000, 0x0000_0008];
        // The same guest `.sb` bound under two different routings hashes differently
        // upstream, so it must produce two keys here.
        let a = r.dump_shader(Stage::Pixel, gcn_ref(0x9afa_e4f0), 0xAAAA, &spirv);
        let b = r.dump_shader(Stage::Pixel, gcn_ref(0x9afa_e4f0), 0xBBBB, &spirv);
        let key = |d: &Option<ShaderDumpRef>| d.as_ref().map(|d| d.key.clone());
        assert_eq!(key(&a).as_deref(), Some("ps-000000000000aaaa"));
        assert_eq!(key(&b).as_deref(), Some("ps-000000000000bbbb"));
        assert_ne!(key(&a), key(&b), "two routings of one .sb are two modules");

        // A rebind of an already-dumped hash reuses the key and writes nothing more.
        assert_eq!(
            key(&r.dump_shader(Stage::Pixel, gcn_ref(0x9afa_e4f0), 0xAAAA, &spirv)),
            key(&a)
        );
        assert_eq!(r.seen_shaders.len(), 2);

        // An unbound stage gets no key rather than a plausible stand-in.
        assert!(r.dump_shader(Stage::Vertex, None, 0, &[]).is_none());

        // Nothing could be read at `0x9afae4f0` in this unit test (no bounded-read seam is
        // wired), so `draws.json` must NOT claim a `.sb` — only files that exist are named.
        assert!(
            !a.as_ref().unwrap().sb,
            "an unreadable .sb must not be advertised"
        );

        flush_writes();
        let spv = tmp.join("shaders").join("ps-000000000000aaaa.spv");
        let bytes = std::fs::read(&spv).expect("the SPIR-V module must be on disk");
        // Byte-for-byte the words Vulkan received, little-endian as the SPIR-V spec requires
        // — so `spirv-dis` / `spirv-val` take the file directly.
        assert_eq!(bytes.len(), spirv.len() * 4);
        assert_eq!(&bytes[..4], &0x0723_0203u32.to_le_bytes());

        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::remove_var(ps4_core::snapshot::DIR_ENV) };
    }

    /// A `GcnBinary` bind at `addr`, with the default routing (the routing that matters is
    /// folded into the hash the caller passes, not into this ref).
    fn gcn_ref(addr: u64) -> Option<ShaderRef> {
        Some(ShaderRef::GcnBinary {
            addr,
            ps_input_map: ps4_gcn::PsInputMap::default(),
            res: Default::default(),
        })
    }

    /// Hand-rolled JSON's one real hazard. Debug output routinely contains quotes.
    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_str("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_str("line\nbreak"), "\"line\\nbreak\"");
        // Non-finite floats become strings rather than invalid JSON literals.
        assert_eq!(json_f32(f32::NAN), "\"NaN\"");
        assert_eq!(json_f32(1.5), "1.5");
    }

    // -----------------------------------------------------------------------------------
    // task-187: the diagnostic render-target dump.
    //
    // What these cover: the request-side decision (opt-in, dedupe, path), the outcome a draw
    // records, and what `draws.json`/`summary.txt` say about it.
    //
    // What they deliberately do NOT cover: the copy itself. `AshBackend::dump_render_target_png`
    // needs a live Vulkan device, and this crate's unit tests are pure-function by convention
    // (there is no device to create one against in CI). The link from
    // `BackendCmd::DumpRenderTargetPng` to a PNG on disk is exercised only at runtime, by
    // running the emulator with the lever on — see the task notes for the command.
    // -----------------------------------------------------------------------------------

    /// A render target whose base and extent are what `draws.json` already names it by.
    fn rt_target(base: u64, w: u32, h: u32) -> TargetDesc {
        TargetDesc {
            width: w,
            height: h,
            kind: TargetKind::Offscreen { base, size: 0x1000 },
            ..TargetDesc::default()
        }
    }

    /// Opt-in, once per target per frame, and a path under THIS frame's directory.
    ///
    /// The dedupe is per frame rather than per session precisely because a render target's
    /// contents are what differ between the frames of an `F9` burst.
    #[test]
    fn request_rt_dump_is_opt_in_deduped_per_frame_and_names_this_frames_directory() {
        let _guard = DUMP_ROOT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("unemups4-rtdump-{}", std::process::id()));
        // SAFETY-of-behaviour: redirects only this test's paths; both vars are read at use.
        unsafe { std::env::set_var(ps4_core::snapshot::DIR_ENV, &tmp) };
        unsafe { std::env::remove_var(ps4_core::snapshot::RENDER_TARGETS_ENV) };

        let target = rt_target(0xD000_0000, 640, 360);

        // Unarmed: nothing, whatever the lever says. This is the zero-cost path.
        let mut idle = Recorder::default();
        assert!(idle.request_rt_dump(0xD000_0000, &target).is_none());

        // Armed but the lever is off (the default): still nothing emitted, and the draw
        // records the reason rather than an absence.
        let mut r = Recorder {
            armed: true,
            frame: 42,
            ..Recorder::default()
        };
        assert!(
            r.request_rt_dump(0xD000_0000, &target).is_none(),
            "render-target dumping must be opt-in"
        );
        assert_eq!(r.target_dump_outcome(&target), RtDumpOutcome::Disabled);

        // Lever on: one request, under this frame's directory, and deduped thereafter.
        unsafe { std::env::set_var(ps4_core::snapshot::RENDER_TARGETS_ENV, "1") };
        let path = r
            .request_rt_dump(0xD000_0000, &target)
            .expect("armed + opted in must request a dump");
        assert_eq!(
            path,
            tmp.join("frame-00042")
                .join("render-targets")
                .join("rt-00000000d0000000-640x360.png"),
            "the PNG lands under the frame it belongs to, keyed on the target's guest base"
        );
        assert!(
            r.request_rt_dump(0xD000_0000, &target).is_none(),
            "a second draw into the same target this frame must not re-request the copy"
        );
        // A different target in the same frame IS its own picture.
        let other = rt_target(0xD100_0000, 320, 180);
        assert!(r.request_rt_dump(0xD100_0000, &other).is_some());

        // The draw's record names the file, and says where the pixels came from — a host
        // image copy, NOT the guest-memory readback the two must never be confused for.
        let outcome = r.target_dump_outcome(&target);
        assert_eq!(
            outcome,
            RtDumpOutcome::Requested {
                key: "rt-00000000d0000000-640x360".to_string()
            }
        );
        let json = rt_dump_json(&outcome);
        assert!(json.contains("\"dumped\": true"), "{json}");
        assert!(
            json.contains("render-targets/rt-00000000d0000000-640x360.png"),
            "{json}"
        );
        assert!(json.contains("NOT a guest-memory readback"), "{json}");

        // A videoout draw is not an offscreen target and gets its own named reason, never a
        // claimed file.
        let vo = TargetDesc {
            kind: TargetKind::Videoout,
            ..TargetDesc::default()
        };
        assert_eq!(r.target_dump_outcome(&vo), RtDumpOutcome::Videoout);

        unsafe { std::env::remove_var(ps4_core::snapshot::RENDER_TARGETS_ENV) };
        unsafe { std::env::remove_var(ps4_core::snapshot::DIR_ENV) };
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Every not-dumped outcome carries an explicit reason — the same discipline
    /// `TextureDumpOutcome` follows. A silently-absent picture is the failure mode this whole
    /// tool exists to prevent.
    #[test]
    fn rt_dump_json_never_omits_a_reason() {
        for outcome in [RtDumpOutcome::Disabled, RtDumpOutcome::Videoout] {
            let json = rt_dump_json(&outcome);
            assert!(json.contains("\"dumped\": false"), "{json}");
            assert!(json.contains("\"reason\":"), "{json}");
        }
        // The `Disabled` reason must name the lever, or the reader cannot act on it.
        assert!(
            rt_dump_json(&RtDumpOutcome::Disabled).contains(ps4_core::snapshot::RENDER_TARGETS_ENV)
        );
    }

    /// `summary.txt` used to send the reader to RenderDoc for render-target pixels. It now
    /// produces them, and must say which path produced them: a HOST image copy, not the
    /// guest-memory readback. It must also state the timing-vs-content cost distinction,
    /// because a careless reader would otherwise take this for a violation of "capturing does
    /// not perturb what is captured".
    #[test]
    fn summary_states_the_render_target_source_and_its_cost() {
        let s = GpuState::default();
        let txt = summary_txt(&s, 7, &[], &[]);
        assert!(
            !txt.contains("USE RENDERDOC"),
            "the RenderDoc deferral is no longer true: {txt}"
        );
        assert!(
            !txt.contains("NOT captured   = RENDER-TARGET PIXELS"),
            "{txt}"
        );
        assert!(txt.contains("render-targets/"), "{txt}");
        assert!(
            txt.contains("HOST render-target image"),
            "the source must be stated: {txt}"
        );
        assert!(
            txt.contains("NOT the guest-memory readback"),
            "the two paths must not be presented as interchangeable: {txt}"
        );
        assert!(
            txt.contains("perturbs frame TIMING — never frame CONTENT"),
            "the cost distinction must be where the reader meets it: {txt}"
        );
        assert!(
            txt.contains(ps4_core::snapshot::RENDER_TARGETS_ENV),
            "the lever must be named: {txt}"
        );
    }

    /// task-201: the snapshot must record what was BOUND, not only what the guest asked
    /// for. `framediff` reads this field to compare our bind against the console's S#, so
    /// its shape is a contract, not an implementation detail.
    #[test]
    fn sampler_bound_json_reports_the_bound_filter_and_wrap() {
        use ps4_core::gpu::{SamplerAddressMode, SamplerDesc, SamplerFilter};
        let j = super::sampler_bound_json(&SamplerDesc {
            mag_filter: SamplerFilter::Nearest,
            min_filter: SamplerFilter::Nearest,
            address_mode_u: SamplerAddressMode::ClampToEdge,
            address_mode_v: SamplerAddressMode::Repeat,
        });
        assert_eq!(
            j,
            "{\"mag_filter\": \"Nearest\", \"min_filter\": \"Nearest\", \
             \"address_mode_u\": \"ClampToEdge\", \"address_mode_v\": \"Repeat\"}"
                .replace("             ", "")
        );
        // The distinction that matters: a NEAREST bind must never render as "Linear".
        assert!(j.contains("Nearest"), "filter must be recorded verbatim");
        assert!(!j.contains("Linear"));
    }
}
