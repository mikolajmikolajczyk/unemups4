/*
 * ps4-gnm-scrape — GoldHEN plugin (task-168)
 *
 * Runs INSIDE the Celeste (CUSA11302) process on a jailbroken PS4. Hooks the
 * sceGnmSubmit* family, memcpy's each submitted DCB/CCB straight out of the
 * process's own address space, and streams every buffer over TCP to the PC
 * receiver (192.168.100.1:9010) as ground-truth PM4 for the task-157 atlas
 * contradiction. The decode happens on the PC (host/ crate) — this plugin only
 * captures + frames + sends, then calls the original submit.
 *
 * Volume guard: Celeste submits count=2 where one DCB is ~4 MB and mostly zeros
 * (task-163). We (a) zero-run-RLE the payload so the big buffer collapses to a
 * few bytes on the wire while every non-zero byte is preserved verbatim, and
 * (b) only capture while armed — the maintainer arms/disarms with an R1+L1+X
 * pad toggle and a hard ~10 s safety cap bounds each run (task-198). Both the
 * RLE and the gate are active at once.
 *
 * Wire framing + RLE contract mirror host/src/lib.rs byte-for-byte — see
 * SETUP.md and that file's module docs.
 */

#include <Common.h>

#include <sys/socket.h>
#include <sys/time.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <unistd.h>
#include <stdint.h>
#include <string.h>
#include <stdarg.h>
#include <stdio.h>

#include <orbis/Pad.h>
#include <orbis/UserService.h>

/* s32/s64 normally come from GoldHEN's plugin_common.h; we stay self-contained. */
typedef int32_t s32;
typedef int64_t s64;

/* ---- plugin metadata (GoldHEN reads these) --------------------------------*/

#define attr_public        __attribute__((visibility("default")))
#define attr_module_hidden __attribute__((weak)) __attribute__((visibility("hidden")))

attr_public const char *g_pluginName = "ps4-gnm-scrape";
attr_public const char *g_pluginDesc = "Streams Celeste DCB/CCB command buffers to the PC for PM4 analysis (task-168)";
attr_public const char *g_pluginAuth = "unemups4";
attr_public uint32_t    g_pluginVersion = 0x00000100; /* 1.00 */

/* klog is GoldHEN's kernel log (printf). Prefix so lines are greppable. */
#define LOG(...) klog("[gnm-scrape] " __VA_ARGS__)

/* ---- config (tweak + rebuild) ---------------------------------------------*/

#define PC_HOST          "192.168.100.1"   /* PC = TCP server */
#define PC_PORT          9010
#define DBG_PORT         18200              /* UDP debug beacon → PC (klog unreachable) */
#define PLUGIN_VERSION   "t198-v3-padhandle"/* bump each rebuild to confirm which build is live */
#define WIRE_MAGIC       0x344D4E47u        /* LE bytes "GNM4" */
#define MIN_ZERO_RUN     8                  /* must match host lib MIN_ZERO_RUN */
#define MAX_BUF_BYTES    (32u * 1024 * 1024)/* sanity cap per buffer */
#define RECONNECT_EVERY  120                /* retry connect every N submit calls */

/* ---- pad-triggered capture control (task-198) -----------------------------
 * Capture is OFF at load. The maintainer drives to the target scene, then
 * presses R1+L1+X (edge-detected toggle) to START, and again to STOP. A hard
 * safety cap auto-stops the capture ~10 s after START regardless of the toggle
 * (wall-clock primary, flip-count backstop) so a forgotten toggle can't flood
 * the wire. A toggle-START resets frame numbering, so the host receiver should
 * be (re)started before each capture. */
#define CAPTURE_COMBO       (ORBIS_PAD_BUTTON_R1 | ORBIS_PAD_BUTTON_L1 | ORBIS_PAD_BUTTON_CROSS) /* 0x4C00 */
#define CAPTURE_MAX_SECONDS 10                 /* wall-clock hard cap after START (primary) */
#define CAPTURE_MAX_FLIPS   700                /* flip backstop in case timing is odd */

/* call-kind byte (must match host lib Kind::from_u8) */
#define KIND_SUBMIT        0
#define KIND_FLIP          1
#define KIND_SUBMIT_WL     2
#define KIND_FLIP_WL       3
/* task-172 Phase 2: referenced dynamic-buffer CONTENT capture. Same 20-byte header
 * as DCB/CCB, same RLE payload — but for KIND_VBUF ONLY the de-RLE'd payload begins
 * with an 8-byte little-endian u64 = guest base address, then `span` content bytes
 * (so raw_size == 8 + span). is_ccb=0, buf_index = per-flip buffer counter, flip
 * mirrors the submit's flip flag. DCB/CCB path is byte-for-byte UNCHANGED. Host side:
 * `host/src/lib.rs` Kind::Vbuf + receiver strips the 8-byte base prefix. */
#define KIND_VBUF          4

/* Referenced-buffer capture tunables. */
#define MAX_VBUF_BYTES     (1u * 1024 * 1024)   /* cap one referenced buffer (temp size) */
#define CB_PROBE_BYTES     256                  /* bytes to dump at a user-data pointer (CB probe) */
#define VSCAN_MAX_DCB      (256u * 1024)        /* skip the whole-DCB V# scan on huge (inert 4MB) DCBs */
#define VBUF_PER_FLIP_MAX  64                   /* dedup / send cap per flip */

/* Real-HW dynamic-buffer heap band (Phase 1: bufs at 0x2xx…, atlas T# at 0x9afc28000);
 * anything outside [4 GB, 64 GB) is not a plausible guest heap pointer -> reject. */
#define HEAP_LO            0x100000000ull
#define HEAP_HI            0x1000000000ull

/* ---- Gnm submit ABI (declared here; symbols resolve from libSceGnmDriver).
 * We do NOT include <orbis/GnmDriver.h> because it declares the *ForWorkload
 * entries as `void f()`, which would clash with the real signatures below. */

typedef int32_t (*submit_flip_fn)(uint32_t count, void **dcb, uint32_t *dcbsz,
                                  void **ccb, uint32_t *ccbsz, uint32_t vhandle,
                                  uint32_t bufidx, uint32_t flipmode, uint64_t fliparg);
typedef int32_t (*submit_fn)(uint32_t count, void **dcb, uint32_t *dcbsz,
                             void **ccb, uint32_t *ccbsz);
typedef int32_t (*submit_flip_wl_fn)(uint32_t workload, uint32_t count, void **dcb,
                                     uint32_t *dcbsz, void **ccb, uint32_t *ccbsz,
                                     uint32_t vhandle, uint32_t bufidx,
                                     uint32_t flipmode, uint64_t fliparg);
typedef int32_t (*submit_wl_fn)(uint32_t workload, uint32_t count, void **dcb,
                                uint32_t *dcbsz, void **ccb, uint32_t *ccbsz);

extern int32_t sceGnmSubmitAndFlipCommandBuffers(uint32_t, void **, uint32_t *,
                                                 void **, uint32_t *, uint32_t,
                                                 uint32_t, uint32_t, uint64_t);
extern int32_t sceGnmSubmitCommandBuffers(uint32_t, void **, uint32_t *, void **,
                                          uint32_t *);
extern int32_t sceGnmSubmitAndFlipCommandBuffersForWorkload(uint32_t, uint32_t,
                                                            void **, uint32_t *,
                                                            void **, uint32_t *,
                                                            uint32_t, uint32_t,
                                                            uint32_t, uint64_t);
extern int32_t sceGnmSubmitCommandBuffersForWorkload(uint32_t, uint32_t, void **,
                                                     uint32_t *, void **,
                                                     uint32_t *);

HOOK_INIT(sceGnmSubmitAndFlipCommandBuffers);
HOOK_INIT(sceGnmSubmitCommandBuffers);
HOOK_INIT(sceGnmSubmitAndFlipCommandBuffersForWorkload);
HOOK_INIT(sceGnmSubmitCommandBuffersForWorkload);

/* sceKernelVirtualQuery + OrbisKernelVirtualQueryInfo come from <orbis/libkernel.h>
 * (pulled in via Common.h). Its layout is { void *unk01 (region start); void *unk02
 * (region end); off_t offset; int unk04; int unk05; unsigned isFlexible/…/isCommitted:1
 * bitfields; char name[32] }. We gate every guest-buffer read on it below. */

/* ---- state ----------------------------------------------------------------*/

static int      g_sock = -1;
static uint32_t g_frame = 0;   /* increments per submit CALL (all bufs share it) */
static uint32_t g_flips = 0;   /* flip-carrying calls; drives the flip backstop cap */
static int      g_capture_done = 0;
static uint32_t g_since_connect_try = 0;

/* pad-triggered capture gate (task-198). Capture is OFF (g_capturing=0) at load;
 * only the R1+L1+X toggle (or a pad-init failure → always-on fallback) starts it. */
static int            g_capturing = 0;    /* 1 = recording this scene */
static int            g_pad_handle = -1;
static int            g_pad_inited = 0;    /* 0=not tried, 1=pad ready, -1=failed→always-on */
static int            g_prev_combo = 0;    /* edge-detect: was the combo down last poll? */
static struct timeval g_cap_start;         /* wall-clock at capture START */

/* ---- TCP ------------------------------------------------------------------*/

static int ensure_connected(void)
{
    if (g_sock >= 0)
        return 0;
    /* Gate reconnect attempts so a down PC listener costs at most one
     * connect-timeout per RECONNECT_EVERY calls (direct cable RSTs fast). */
    if (g_since_connect_try != 0 && g_since_connect_try < RECONNECT_EVERY) {
        g_since_connect_try++;
        return -1;
    }
    g_since_connect_try = 1;

    int s = socket(AF_INET, SOCK_STREAM, 0);
    if (s < 0)
        return -1;

    struct timeval tv;
    tv.tv_sec = 1;
    tv.tv_usec = 0;
    setsockopt(s, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));
    int one = 1;
    setsockopt(s, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(PC_PORT);
    addr.sin_addr.s_addr = inet_addr(PC_HOST);

    if (connect(s, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        close(s);
        return -1;
    }
    g_sock = s;
    g_since_connect_try = 0;
    LOG("connected to %s:%d\n", PC_HOST, PC_PORT);
    return 0;
}

static void drop_connection(void)
{
    if (g_sock >= 0) {
        close(g_sock);
        g_sock = -1;
    }
}

/* Send all bytes; on any error drop the socket and report failure. */
static int send_all(const uint8_t *buf, size_t len)
{
    size_t off = 0;
    while (off < len) {
        int n = send(g_sock, buf + off, len - off, 0);
        if (n <= 0) {
            drop_connection();
            return -1;
        }
        off += (size_t)n;
    }
    return 0;
}

/* ---- zero-run RLE (mirror of host rle_encode) -----------------------------*/

static void put_u32(uint8_t *p, uint32_t v)
{
    p[0] = (uint8_t)(v);
    p[1] = (uint8_t)(v >> 8);
    p[2] = (uint8_t)(v >> 16);
    p[3] = (uint8_t)(v >> 24);
}

static void put_u64(uint8_t *p, uint64_t v)
{
    put_u32(p, (uint32_t)v);
    put_u32(p + 4, (uint32_t)(v >> 32));
}

/* Emit a pending literal chunk [start,end) from data over the socket. */
static int emit_literal(const uint8_t *data, size_t start, size_t end)
{
    if (end <= start)
        return 0;
    uint8_t chunk_hdr[5];
    chunk_hdr[0] = 0; /* literal op */
    put_u32(&chunk_hdr[1], (uint32_t)(end - start));
    if (send_all(chunk_hdr, sizeof(chunk_hdr)) != 0)
        return -1;
    return send_all(data + start, end - start);
}

/* Stream the RLE payload for `data` directly to the socket (no big temp copy;
 * the 4 MB DCB is chunked as it is scanned). Returns compressed byte count via
 * *out_comp, or -1 on socket error. */
static long rle_stream(const uint8_t *data, size_t n)
{
    size_t i = 0, lit_start = 0;
    long comp = 0;
    while (i < n) {
        if (data[i] == 0) {
            size_t run_start = i;
            while (i < n && data[i] == 0)
                i++;
            size_t run = i - run_start;
            if (run >= MIN_ZERO_RUN) {
                if (emit_literal(data, lit_start, run_start) != 0)
                    return -1;
                comp += (long)(run_start - lit_start) + (run_start > lit_start ? 5 : 0);
                uint8_t zr[5];
                zr[0] = 1; /* zero-run op */
                put_u32(&zr[1], (uint32_t)run);
                if (send_all(zr, sizeof(zr)) != 0)
                    return -1;
                comp += 5;
                lit_start = i;
            }
        } else {
            i++;
        }
    }
    if (emit_literal(data, lit_start, n) != 0)
        return -1;
    if (n > lit_start)
        comp += (long)(n - lit_start) + 5;
    return comp;
}

/* rle_stream needs the compressed size in the header, but we stream the payload
 * as we compute it — so we compute the compressed size first (cheap scan), then
 * send header, then stream. Keeps the header honest without buffering 4 MB. */
static long rle_size(const uint8_t *data, size_t n)
{
    size_t i = 0, lit_start = 0;
    long comp = 0;
    while (i < n) {
        if (data[i] == 0) {
            size_t run_start = i;
            while (i < n && data[i] == 0)
                i++;
            size_t run = i - run_start;
            if (run >= MIN_ZERO_RUN) {
                if (run_start > lit_start)
                    comp += (long)(run_start - lit_start) + 5;
                comp += 5;
                lit_start = i;
            }
        } else {
            i++;
        }
    }
    if (n > lit_start)
        comp += (long)(n - lit_start) + 5;
    return comp;
}

/* ---- one buffer -----------------------------------------------------------*/

static void send_buffer(uint32_t frame, uint8_t kind, uint8_t buf_index,
                        uint8_t is_ccb, uint8_t flip, const void *ptr,
                        uint32_t size)
{
    if (ptr == 0 || size == 0 || size > MAX_BUF_BYTES)
        return;
    if (ensure_connected() != 0)
        return;

    const uint8_t *data = (const uint8_t *)ptr;
    long comp = rle_size(data, size);
    if (comp < 0)
        return;

    uint8_t hdr[20];
    put_u32(&hdr[0], WIRE_MAGIC);
    put_u32(&hdr[4], frame);
    hdr[8] = kind;
    hdr[9] = buf_index;
    hdr[10] = is_ccb;
    hdr[11] = flip;
    put_u32(&hdr[12], size);              /* raw_size */
    put_u32(&hdr[16], (uint32_t)comp);    /* comp_size */
    if (send_all(hdr, sizeof(hdr)) != 0)
        return;
    (void)rle_stream(data, size);         /* payload; drops socket on error */
}

/* ---- referenced dynamic-buffer capture (task-172 Phase 2) -----------------
 * Ports the `vref` host-tool parse to on-device C: shadow the VS/PS user-data
 * blocks from the DCB's SET_SH_REG writes, then per draw reconstruct the
 * referenced buffers (inline V# + user-data pointers), and dump each buffer's
 * CONTENT — GUARDED by sceKernelVirtualQuery so a bogus base can never fault. */

/* GFX6 register windows / opcodes (mirror crates/gnm/src/pm4/opcodes.rs). */
#define REG_SH_BASE        0x2C00u
#define SH_USER_DATA_VS_0  (REG_SH_BASE + 0x4C) /* 0x2C4C */
#define SH_USER_DATA_PS_0  (REG_SH_BASE + 0x0C) /* 0x2C0C */
#define IT_SET_SH_REG      0x76
#define IT_DRAW_INDEX_2    0x27
#define IT_DRAW_INDEX_AUTO 0x2D
#define IT_DRAW_INDEX_OFF2 0x35

/* Per-flip dedup + emit budget (base-keyed, first span wins). */
static uint64_t g_vbuf_seen[VBUF_PER_FLIP_MAX];
static uint32_t g_vbuf_count;
static uint32_t g_vbuf_idx;   /* buf_index stamped on the wire, per flip */

/* Static staging buffer: 8-byte base prefix + content (single-threaded submit path). */
static uint8_t g_vbuf_tmp[8 + MAX_VBUF_BYTES];

static int in_heap(uint64_t v) { return v >= HEAP_LO && v < HEAP_HI; }

/* Decode a 4-dword GCN V# (buffer resource) — mirrors vbuf::decode_v_sharp. */
static uint64_t vsharp_base(const uint32_t *w) { return (uint64_t)w[0] | ((uint64_t)(w[1] & 0xFFFF) << 32); }
static uint32_t vsharp_stride(const uint32_t *w) { return (w[1] >> 16) & 0x3FFF; }
static uint32_t vsharp_records(const uint32_t *w) { return w[2]; }
static uint64_t vsharp_span(const uint32_t *w)
{
    uint32_t stride = vsharp_stride(w);
    uint64_t nr = vsharp_records(w);
    uint64_t span = stride ? (uint64_t)stride * nr : nr;  /* stride==0 fallback: nr bytes */
    return span;
}
/* A window is a plausible dynamic-stream V# if its base is in the heap band and the
 * stride/records give a small, sane span. Matches vref's inline-V# filter. */
static int vsharp_plausible(const uint32_t *w)
{
    uint64_t base = vsharp_base(w);
    uint32_t stride = vsharp_stride(w);
    uint32_t nr = vsharp_records(w);
    if (!in_heap(base)) return 0;
    if (stride == 0 || stride > 256) return 0;
    if (nr == 0 || nr >= 1000000u) return 0;
    if (vsharp_span(w) > MAX_VBUF_BYTES) return 0;
    return 1;
}

/* Confirm [addr, addr+len) is entirely inside ONE committed, readable mapping. Any
 * failed check => not safe to touch => 0. This is what makes the in-process memcpy
 * crash-safe against a guest-programmed (untrusted) base/span. */
static int region_readable(uint64_t addr, uint64_t len)
{
    if (addr == 0 || len == 0) return 0;
    if (addr + len < addr) return 0; /* overflow */
    OrbisKernelVirtualQueryInfo info;
    memset(&info, 0, sizeof(info));
    if (sceKernelVirtualQuery((const void *)addr, 0, &info, sizeof(info)) != 0)
        return 0;
    if (!info.isCommitted) return 0;
    uint64_t start = (uint64_t)info.unk01;
    uint64_t end = (uint64_t)info.unk02;
    if (addr < start) return 0;         /* addr fell before the returned region */
    if (addr + len > end) return 0;     /* span must fit wholly inside it */
    return 1;
}

/* Dedup by base (first-seen wins); returns 1 if this base is new (should send). */
static int vbuf_mark(uint64_t base)
{
    for (uint32_t i = 0; i < g_vbuf_count; i++)
        if (g_vbuf_seen[i] == base) return 0;
    if (g_vbuf_count >= VBUF_PER_FLIP_MAX) return 0;
    g_vbuf_seen[g_vbuf_count++] = base;
    return 1;
}

/* Dump one referenced buffer [base, span] as KIND_VBUF (base travels as an 8-byte
 * payload prefix). Guarded + deduped + capped. LOGs and skips on any failed guard. */
static void send_vbuf(uint32_t frame, uint8_t flip, uint64_t base, uint64_t span)
{
    if (span == 0) return;
    if (span > MAX_VBUF_BYTES) span = MAX_VBUF_BYTES;
    if (!in_heap(base)) return;
    if (!vbuf_mark(base)) return;
    if (!region_readable(base, span)) {
        LOG("skip vbuf base=%p span=%llu (not readable)\n", (void *)base,
            (unsigned long long)span);
        return;
    }
    put_u64(&g_vbuf_tmp[0], base);
    memcpy(&g_vbuf_tmp[8], (const void *)base, (size_t)span);
    /* Reuse the DCB/CCB wire path: same header + RLE, kind=KIND_VBUF, is_ccb=0. */
    send_buffer(frame, KIND_VBUF, (uint8_t)g_vbuf_idx, 0, flip, g_vbuf_tmp,
                (uint32_t)(8 + span));
    g_vbuf_idx++;
}

/* Parse one DCB exactly like `vref`: shadow user-data, and per draw emit the inline
 * V# targets + follow user-data pointers (as V#s and as constant-buffer probes). Plus
 * a whole-DCB inline-V# window scan for small DCBs (catches the quads directly). */
static void parse_dcb_vbufs(uint32_t frame, uint8_t flip, const uint32_t *w, uint32_t dwords)
{
    uint32_t vs_ud[16];
    uint32_t ps_ud[16];
    memset(vs_ud, 0, sizeof(vs_ud));
    memset(ps_ud, 0, sizeof(ps_ud));

    uint32_t i = 0;
    while (i < dwords) {
        uint32_t h = w[i];
        uint32_t type = (h >> 30) & 3u;
        if (type == 3) {
            uint32_t count = ((h >> 16) & 0x3FFFu) + 1u;   /* body dwords */
            uint32_t op = (h >> 8) & 0xFFu;
            const uint32_t *body = &w[i + 1];
            if (i + 1 + count > dwords) break;              /* truncated */
            if (op == IT_SET_SH_REG && count >= 1) {
                uint32_t off = body[0];
                for (uint32_t k = 1; k < count; k++) {
                    uint32_t reg = REG_SH_BASE + off + (k - 1);
                    if (reg >= SH_USER_DATA_VS_0 && reg < SH_USER_DATA_VS_0 + 16)
                        vs_ud[reg - SH_USER_DATA_VS_0] = body[k];
                    else if (reg >= SH_USER_DATA_PS_0 && reg < SH_USER_DATA_PS_0 + 16)
                        ps_ud[reg - SH_USER_DATA_PS_0] = body[k];
                }
            } else if (op == IT_DRAW_INDEX_AUTO || op == IT_DRAW_INDEX_2 ||
                       op == IT_DRAW_INDEX_OFF2) {
                /* Inline V#s in the VS user-data block (4-dword windows). */
                for (uint32_t s = 0; s + 4 <= 16; s++) {
                    if (vsharp_plausible(&vs_ud[s])) {
                        send_vbuf(frame, flip, vsharp_base(&vs_ud[s]), vsharp_span(&vs_ud[s]));
                    }
                }
                /* Follow user-data pointer pairs (VS then PS): (a) as a V# whose
                 * target we dump, (b) as a constant/uniform buffer we probe. */
                for (int stg = 0; stg < 2; stg++) {
                    const uint32_t *ud = stg == 0 ? vs_ud : ps_ud;
                    for (uint32_t s = 0; s + 1 < 16; s++) {
                        uint64_t ptr = (uint64_t)ud[s] | ((uint64_t)ud[s + 1] << 32);
                        if (!in_heap(ptr)) continue;
                        /* (a) read 16 bytes as a V# and dump its target if sane. */
                        if (region_readable(ptr, 16)) {
                            uint32_t vw[4];
                            memcpy(vw, (const void *)ptr, 16);
                            if (vsharp_plausible(vw))
                                send_vbuf(frame, flip, vsharp_base(vw), vsharp_span(vw));
                        }
                        /* (b) probe the pointed-to region itself (transform/uniform CB). */
                        send_vbuf(frame, flip, ptr, CB_PROBE_BYTES);
                    }
                }
            }
            i += 1 + count;
        } else if (type == 2) {
            i += 1;                                          /* Type-2 NOP, no body */
        } else {
            uint32_t count = ((h >> 16) & 0x3FFFu) + 1u;     /* Type-0/1 body */
            i += 1 + count;
        }
    }

    /* Whole-DCB inline-V# scan (small DCBs only — the 4 MB inert DCB has no draws). */
    if (dwords <= VSCAN_MAX_DCB / 4) {
        for (uint32_t j = 0; j + 4 <= dwords; j++) {
            if (vsharp_plausible(&w[j]))
                send_vbuf(frame, flip, vsharp_base(&w[j]), vsharp_span(&w[j]));
        }
    }
}

/* Capture the referenced buffers for one flip's DCBs. Resets the per-flip dedup set. */
static void capture_vbufs(uint32_t frame, uint8_t flip, uint32_t count, void **dcb,
                          uint32_t *dcbsz)
{
    g_vbuf_count = 0;
    g_vbuf_idx = 0;
    if (!dcb || !dcbsz) return;
    for (uint32_t i = 0; i < count; i++) {
        const uint32_t *w = (const uint32_t *)dcb[i];
        uint32_t sz = dcbsz[i];
        if (!w || sz < 4) continue;
        /* Only touch the DCB itself once we know it is readable. */
        if (!region_readable((uint64_t)(uintptr_t)w, sz < 16 ? sz : 16))
            continue;
        parse_dcb_vbufs(frame, flip, w, sz / 4);
    }
}

/* ---- pad-triggered capture gate (task-198) --------------------------------*/

/* UDP debug beacon → PC:DBG_PORT. GoldHEN klog is not reachable on this setup,
 * so pad-init results + the button heartbeat are shipped as plain UDP datagrams
 * the PC catches with `nc -u -l 18200`. Fire-and-forget; never blocks capture. */
static int g_dbg_sock = -1;
static void dbg(const char *fmt, ...)
{
    char buf[256];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    if (n <= 0)
        return;
    if (n > (int)sizeof(buf))
        n = (int)sizeof(buf);
    if (g_dbg_sock < 0) {
        g_dbg_sock = socket(AF_INET, SOCK_DGRAM, 0);
        if (g_dbg_sock < 0)
            return;
    }
    struct sockaddr_in a;
    memset(&a, 0, sizeof(a));
    a.sin_family = AF_INET;
    a.sin_port = htons(DBG_PORT);
    a.sin_addr.s_addr = inet_addr(PC_HOST);
    sendto(g_dbg_sock, buf, (size_t)n, 0, (struct sockaddr *)&a, sizeof(a));
}

/* Begin a fresh capture sequence: reset the frame/flip counters and start the
 * safety clock so each triggered capture is an independent numbered stream. */
static void capture_start(void)
{
    g_frame = 0;
    g_flips = 0;
    g_capture_done = 0;
    g_capturing = 1;
    gettimeofday(&g_cap_start, NULL);
    LOG("capture STARTED — (re)start the host receiver now (frame numbering reset)\n");
}

/* Lazily bring the pad up. On ANY failure fall back to the OLD always-on
 * behaviour (capture immediately) so a pad-init error never silently disables
 * the tool — but say so in the log. Runs its body exactly once. */
static void pad_ensure_init(void)
{
    if (g_pad_inited != 0)
        return;

    dbg("[gnm-scrape] %s pad_init begin\n", PLUGIN_VERSION);

    int32_t r = scePadInit();
    /* The user service must be initialised before GetInitialUser returns a real
     * user id; without it scePadOpen rejects the 0xFF "main user" with 0x809b0001.
     * Ordering + necessity mirror the working ps4doom homebrew (its own MIT code,
     * platform/doomgeneric_ps4.c). Already-initialised is a benign non-fatal return. */
    sceUserServiceInitialize(NULL);
    int32_t uid = -1;
    int32_t ur = sceUserServiceGetInitialUser(&uid);
    /* Two ways to get a readable handle: scePadOpen (owns a new handle) or
     * scePadGetHandle (reuses the handle the GAME already opened — the right
     * move inside a plugin, where Celeste may hold the pad). Try open, then
     * fall back to get-handle. */
    int32_t ho = scePadOpen(uid, ORBIS_PAD_PORT_TYPE_STANDARD, 0, NULL);
    int32_t hg = scePadGetHandle(uid, ORBIS_PAD_PORT_TYPE_STANDARD, 0);
    dbg("[gnm-scrape] %s padInit=0x%x usrInit ur=0x%x uid=0x%x padOpen=0x%x getHandle=0x%x\n",
        PLUGIN_VERSION, r, ur, uid, ho, hg);

    int32_t h = ho >= 0 ? ho : hg;
    if (h < 0) {
        LOG("pad init failed (open=0x%x getHandle=0x%x) — FALLBACK: always-on\n", ho, hg);
        dbg("[gnm-scrape] %s FALLBACK always-on (no pad handle)\n", PLUGIN_VERSION);
        g_pad_inited = -1;
        capture_start();
        return;
    }
    g_pad_handle = h;
    g_pad_inited = 1;
    dbg("[gnm-scrape] %s pad ready handle=%d combo=0x%x\n", PLUGIN_VERSION, h, CAPTURE_COMBO);
    LOG("pad ready (handle=%d) — press R1+L1+X to start/stop capture (max %ds)\n",
        h, CAPTURE_MAX_SECONDS);
}

/* Poll the pad, edge-detect the R1+L1+X toggle, and enforce the hard caps.
 * Returns 1 if capture is currently armed (caller records this batch). */
static int capture_gate(void)
{
    pad_ensure_init();

    /* Edge-detected toggle. Skipped when we fell back to always-on (g_pad_inited==-1). */
    if (g_pad_inited == 1) {
        OrbisPadData data;
        memset(&data, 0, sizeof(data));
        int rc = scePadReadState(g_pad_handle, &data);
        static uint32_t hb = 0;
        if ((hb++ % 120) == 0)
            dbg("[gnm-scrape] %s poll rc=0x%x buttons=0x%x conn=%d cap=%d\n",
                PLUGIN_VERSION, (unsigned)rc, (unsigned)data.buttons,
                (int)data.connected, g_capturing);
        if (rc == 0) {
            int now = ((data.buttons & CAPTURE_COMBO) == CAPTURE_COMBO);
            if (now && !g_prev_combo) {          /* fire only on !prev && now */
                if (!g_capturing) {
                    capture_start();
                } else {
                    g_capturing = 0;
                    g_capture_done = 1;
                    LOG("capture STOPPED (manual)\n");
                }
            }
            g_prev_combo = now;
        }
    }

    if (!g_capturing)
        return 0;

    /* Hard safety caps — stop on whichever hits first; wall-clock is primary. */
    struct timeval nowtv;
    gettimeofday(&nowtv, NULL);
    if (nowtv.tv_sec - g_cap_start.tv_sec >= CAPTURE_MAX_SECONDS) {
        g_capturing = 0;
        g_capture_done = 1;
        LOG("capture auto-stopped: %ds cap\n", CAPTURE_MAX_SECONDS);
        return 0;
    }
    if (g_flips >= CAPTURE_MAX_FLIPS) {
        g_capturing = 0;
        g_capture_done = 1;
        LOG("capture auto-stopped: flip cap (%d)\n", CAPTURE_MAX_FLIPS);
        return 0;
    }
    return 1;
}

/* Capture a whole submit batch: DCB[i] then CCB[i] for i in 0..count. */
static void capture_batch(uint8_t kind, uint8_t flip, uint32_t count,
                          void **dcb, uint32_t *dcbsz, void **ccb,
                          uint32_t *ccbsz)
{
    /* Gate on the pad toggle + safety caps. Capture is OFF until triggered. */
    if (!capture_gate())
        return;

    uint32_t frame = g_frame++;
    if (flip)
        g_flips++;

    for (uint32_t i = 0; i < count; i++) {
        if (dcb && dcbsz)
            send_buffer(frame, kind, (uint8_t)i, 0, flip, dcb[i], dcbsz[i]);
        if (ccb && ccbsz)
            send_buffer(frame, kind, (uint8_t)i, 1, flip, ccb[i], ccbsz[i]);
    }

    /* task-172 Phase 2: on a flip, also dump the referenced dynamic-buffer CONTENT
     * (the animation lives there, not in the byte-stable DCB). Same frame counter. */
    if (flip)
        capture_vbufs(frame, flip, count, dcb, dcbsz);
}

/* ---- hooks ----------------------------------------------------------------*/

int32_t sceGnmSubmitAndFlipCommandBuffers_hook(
    uint32_t count, void **dcb, uint32_t *dcbsz, void **ccb, uint32_t *ccbsz,
    uint32_t vhandle, uint32_t bufidx, uint32_t flipmode, uint64_t fliparg)
{
    capture_batch(KIND_FLIP, 1, count, dcb, dcbsz, ccb, ccbsz);
    return HOOK_CONTINUE(sceGnmSubmitAndFlipCommandBuffers, submit_flip_fn,
                         count, dcb, dcbsz, ccb, ccbsz, vhandle, bufidx,
                         flipmode, fliparg);
}

int32_t sceGnmSubmitCommandBuffers_hook(uint32_t count, void **dcb,
                                        uint32_t *dcbsz, void **ccb,
                                        uint32_t *ccbsz)
{
    capture_batch(KIND_SUBMIT, 0, count, dcb, dcbsz, ccb, ccbsz);
    return HOOK_CONTINUE(sceGnmSubmitCommandBuffers, submit_fn, count, dcb,
                         dcbsz, ccb, ccbsz);
}

int32_t sceGnmSubmitAndFlipCommandBuffersForWorkload_hook(
    uint32_t workload, uint32_t count, void **dcb, uint32_t *dcbsz, void **ccb,
    uint32_t *ccbsz, uint32_t vhandle, uint32_t bufidx, uint32_t flipmode,
    uint64_t fliparg)
{
    capture_batch(KIND_FLIP_WL, 1, count, dcb, dcbsz, ccb, ccbsz);
    return HOOK_CONTINUE(sceGnmSubmitAndFlipCommandBuffersForWorkload,
                         submit_flip_wl_fn, workload, count, dcb, dcbsz, ccb,
                         ccbsz, vhandle, bufidx, flipmode, fliparg);
}

int32_t sceGnmSubmitCommandBuffersForWorkload_hook(
    uint32_t workload, uint32_t count, void **dcb, uint32_t *dcbsz, void **ccb,
    uint32_t *ccbsz)
{
    capture_batch(KIND_SUBMIT_WL, 0, count, dcb, dcbsz, ccb, ccbsz);
    return HOOK_CONTINUE(sceGnmSubmitCommandBuffersForWorkload, submit_wl_fn,
                         workload, count, dcb, dcbsz, ccb, ccbsz);
}

/* ---- plugin lifecycle -----------------------------------------------------*/

s32 attr_public plugin_load(s32 argc, const char *argv[])
{
    (void)argc;
    (void)argv;
    LOG("plugin_load v0x%08x — target %s:%d; capture OFF — press R1+L1+X to "
        "start/stop (max %ds), (re)start the host receiver first\n",
        g_pluginVersion, PC_HOST, PC_PORT, CAPTURE_MAX_SECONDS);
    HOOK(sceGnmSubmitAndFlipCommandBuffers);
    HOOK(sceGnmSubmitCommandBuffers);
    HOOK(sceGnmSubmitAndFlipCommandBuffersForWorkload);
    HOOK(sceGnmSubmitCommandBuffersForWorkload);
    return 0;
}

s32 attr_public plugin_unload(s32 argc, const char *argv[])
{
    (void)argc;
    (void)argv;
    UNHOOK(sceGnmSubmitAndFlipCommandBuffers);
    UNHOOK(sceGnmSubmitCommandBuffers);
    UNHOOK(sceGnmSubmitAndFlipCommandBuffersForWorkload);
    UNHOOK(sceGnmSubmitCommandBuffersForWorkload);
    drop_connection();
    LOG("plugin_unload\n");
    return 0;
}

s32 attr_module_hidden module_start(s64 argc, const void *args)
{
    (void)argc;
    (void)args;
    return 0;
}

s32 attr_module_hidden module_stop(s64 argc, const void *args)
{
    (void)argc;
    (void)args;
    return 0;
}
