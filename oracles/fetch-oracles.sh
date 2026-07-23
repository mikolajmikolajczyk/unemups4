#!/usr/bin/env bash
# Reconstruct the unemups4 clean-oracle stash (see MANIFEST.md).
# Every source is a CLEAN oracle (AMD ISA, Mesa, Linux kernel, FreeBSD, OpenOrbis,
# llvm-mc) — pinned + checksummed. shadPS4/GPCS4/fpPS4 are NOT sources.
#
# Usage:
#   ./fetch-oracles.sh fetch     # download everything to pinned refs, (re)write SHA256SUMS
#   ./fetch-oracles.sh verify    # check current files against SHA256SUMS
#   ./fetch-oracles.sh           # = fetch
#
# AMD ISA PDFs are © AMD (kept local, cite-only, NOT redistributed). This script only
# fetches them from public archives for local reference; do not commit the PDFs.
set -euo pipefail
cd "$(dirname "$0")"

# --- pinned refs (change deliberately; then re-run fetch to refresh SHA256SUMS) ---
KERNEL_REF="v6.12"                                   # torvalds/linux tag
MESA_COMMIT="42f591b906b7e8a966cc339f84d2671f423d48c4"
OPENORBIS_COMMIT="0a1aaf9dd4a92695538bdeb09fb056d06dd11725"
FREEBSD_REF="release/9.0.0"                           # PS4 Orbis OS base

RAW_KERNEL="https://raw.githubusercontent.com/torvalds/linux/${KERNEL_REF}"
RAW_FREEBSD="https://raw.githubusercontent.com/freebsd/freebsd-src/${FREEBSD_REF}"
export GIT_TERMINAL_PROMPT=0

log(){ printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn(){ printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }

fetch_file(){ # url dest [min_bytes] — writes to a temp, keeps existing file on failure
  local url="$1" dest="$2" min="${3:-500}" tmp
  mkdir -p "$(dirname "$dest")"
  tmp="$(mktemp)"
  local code; code=$(curl -sL -o "$tmp" -w '%{http_code}' --max-time 90 "$url" 2>/dev/null || echo 000)
  local sz; sz=$(stat -c%s "$tmp" 2>/dev/null || echo 0)
  if [ "$code" != "200" ] || [ "${sz:-0}" -lt "$min" ]; then
    rm -f "$tmp"
    warn "FAILED $dest (http=$code size=$sz) <- $url ${dest:+(kept existing if any)}"
    return 1
  fi
  mv "$tmp" "$dest"
  printf '   %-40s http=%s size=%s\n' "$dest" "$code" "$sz"
}

fetch_all(){
  # --- AMD ISA manuals (archive.org; amd.com/docs.amd.com block scripted fetch) ---
  # Sea/Southern Islands = PS4 Liverpool (GCN2/GFX6). RDNA2 = PS5 Oberon (GFX10.3).
  log "AMD ISA manuals (Sea Islands = PS4/Liverpool/GCN2, Southern Islands = GFX6, RDNA2 = PS5/Oberon/GFX10.3)"
  fetch_file "https://web.archive.org/web/2id_/http://developer.amd.com/wordpress/media/2013/07/AMD_Sea_Islands_Instruction_Set_Architecture.pdf" amd/ci-isa.pdf 500000 || warn "grab amd/ci-isa.pdf manually (docs.amd.com) if archive is down"
  fetch_file "https://web.archive.org/web/2id_/http://developer.amd.com/wordpress/media/2012/12/AMD_Southern_Islands_Instruction_Set_Architecture.pdf" amd/si-isa.pdf 500000 || warn "grab amd/si-isa.pdf manually if archive is down"
  # RDNA2 ISA ("RDNA 2" Instruction Set Architecture Reference Guide, Nov 2020, 291pp).
  # docs.amd.com serves only an HTML viewer + gpuopen download 404s; the wayback raw
  # snapshot of the retired developer.amd.com PDF is the reachable clean copy.
  fetch_file "https://web.archive.org/web/20230214072857id_/https://developer.amd.com/wp-content/resources/RDNA2_Shader_ISA_November2020.pdf" amd/rdna2-isa.pdf 500000 || warn "grab amd/rdna2-isa.pdf manually (docs.amd.com 'RDNA2 shader ISA') if archive is down"

  # --- Linux kernel radeon/amdgpu register + PM4 headers (canonical AMD MMIO/PM4) ---
  log "Linux kernel AMD headers @ ${KERNEL_REF}"
  fetch_file "${RAW_KERNEL}/drivers/gpu/drm/radeon/sid.h"        amd/kernel-sid.h        2000
  fetch_file "${RAW_KERNEL}/drivers/gpu/drm/radeon/cikd.h"       amd/kernel-cikd.h       2000
  fetch_file "${RAW_KERNEL}/drivers/gpu/drm/radeon/evergreend.h" amd/kernel-evergreend.h 2000
  fetch_file "${RAW_KERNEL}/drivers/gpu/drm/amd/amdgpu/soc15d.h" amd/kernel-soc15d.h     2000
  fetch_file "${RAW_KERNEL}/drivers/gpu/drm/amd/amdgpu/nvd.h"    amd/kernel-nvd.h        2000

  # --- Mesa src/amd (sparse, blobless) — PM4/registers/tiling/descriptors ---
  log "Mesa src/amd @ ${MESA_COMMIT:0:10}"
  if [ ! -d mesa/mesa/.git ]; then
    git clone --filter=blob:none --no-checkout https://gitlab.freedesktop.org/mesa/mesa.git mesa/mesa
    git -C mesa/mesa sparse-checkout set src/amd/common src/amd/registers
  fi
  git -C mesa/mesa fetch --depth 1 origin "$MESA_COMMIT" 2>/dev/null || git -C mesa/mesa fetch origin
  git -C mesa/mesa checkout -q "$MESA_COMMIT"

  # --- OpenOrbis SDK (headers + OELF/SELF spec + make-fself) ---
  log "OpenOrbis toolchain @ ${OPENORBIS_COMMIT:0:10}"
  if [ ! -d openorbis/OpenOrbis-PS4-Toolchain/.git ]; then
    git clone --filter=blob:none https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain.git openorbis/OpenOrbis-PS4-Toolchain
  fi
  git -C openorbis/OpenOrbis-PS4-Toolchain fetch --depth 1 origin "$OPENORBIS_COMMIT" 2>/dev/null || git -C openorbis/OpenOrbis-PS4-Toolchain fetch origin
  git -C openorbis/OpenOrbis-PS4-Toolchain checkout -q "$OPENORBIS_COMMIT"

  # --- FreeBSD 9.0 (Orbis OS base): syscall table, errno, ELF, mman, fcntl ---
  log "FreeBSD ${FREEBSD_REF}"
  fetch_file "${RAW_FREEBSD}/sys/kern/syscalls.master" freebsd9/sys_kern_syscalls.master 500
  fetch_file "${RAW_FREEBSD}/sys/sys/errno.h"          freebsd9/sys_sys_errno.h          500
  fetch_file "${RAW_FREEBSD}/sys/sys/elf_common.h"     freebsd9/sys_sys_elf_common.h     500
  fetch_file "${RAW_FREEBSD}/sys/sys/elf64.h"          freebsd9/sys_sys_elf64.h          500
  fetch_file "${RAW_FREEBSD}/sys/sys/mman.h"           freebsd9/sys_sys_mman.h           500
  fetch_file "${RAW_FREEBSD}/sys/sys/fcntl.h"          freebsd9/sys_sys_fcntl.h          500
  fetch_file "${RAW_FREEBSD}/sys/sys/signal.h"         freebsd9/sys_sys_signal.h         500
  fetch_file "${RAW_FREEBSD}/sys/sys/unistd.h"         freebsd9/sys_sys_unistd.h         500

  # --- llvm-mc (witness tool; record version) ---
  log "llvm-mc witness"
  if command -v llvm-mc >/dev/null; then
    llvm-mc --version | sed -n 's/.*LLVM version \(.*\)/llvm-mc \1/p' > llvm-mc.version
    printf '   llvm-mc: %s\n' "$(cat llvm-mc.version)"
  else
    warn "llvm-mc not installed — needed as GCN encoding witness (#6). Install LLVM."
  fi

  write_sums
  log "Done. Stash rebuilt; SHA256SUMS refreshed."
}

# Files whose bytes are stable enough to checksum (git-clone trees excluded — pinned by commit).
sum_targets(){ ls amd/*.pdf amd/*.h freebsd9/* 2>/dev/null || true; }

write_sums(){
  log "Writing SHA256SUMS (fixed files; git trees pinned by commit above)"
  # shellcheck disable=SC2046
  sha256sum $(sum_targets) > SHA256SUMS
  {
    echo "# git-pinned (verify via commit, not sha):"
    echo "# mesa/mesa            $MESA_COMMIT"
    echo "# openorbis/...        $OPENORBIS_COMMIT"
    echo "# freebsd ref          $FREEBSD_REF ; kernel ref $KERNEL_REF"
  } >> SHA256SUMS
}

verify(){
  [ -f SHA256SUMS ] || { warn "no SHA256SUMS — run 'fetch' first"; exit 1; }
  log "Verifying fixed files against SHA256SUMS"
  grep -v '^#' SHA256SUMS | sha256sum -c -
  log "git-pinned trees (check HEAD == pinned commit):"
  printf '   mesa      %s (want %s)\n' "$(git -C mesa/mesa rev-parse HEAD 2>/dev/null || echo MISSING)" "$MESA_COMMIT"
  printf '   openorbis %s (want %s)\n' "$(git -C openorbis/OpenOrbis-PS4-Toolchain rev-parse HEAD 2>/dev/null || echo MISSING)" "$OPENORBIS_COMMIT"
}

case "${1:-fetch}" in
  fetch)  fetch_all ;;
  verify) verify ;;
  sums)   write_sums ;;
  *) echo "usage: $0 [fetch|verify|sums]"; exit 2 ;;
esac
