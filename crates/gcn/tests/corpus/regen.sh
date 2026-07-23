#!/usr/bin/env bash
# Regenerate the committed GCN corpus from the .s sources.
#
# For each <name>.s this assembles GFX7 / Sea Islands (bonaire) GCN with llvm-mc,
# extracts the raw instruction bytes (the `; encoding: [..]` bytes llvm-mc emits),
# and writes <name>.code.bin (the raw GCN machine code, no wrapper). The OrbShdr
# .sb blobs are then produced by the committed Rust builder:
#
#     cargo test -p ps4-gcn --test corpus -- --ignored regen_sb_blobs
#
# which reads each .code.bin, stamps a 28-byte ShaderBinaryInfo header + semantic
# tables, and writes <name>.sb. Run this script first, then that test, then commit
# the updated .code.bin / .sb next to the .s source.
#
# Toolchain: any llvm-mc with the amdgcn target (verified against LLVM 22). If
# llvm-mc is unavailable the bytes in the committed .code.bin can be hand-encoded
# instead — they are real GCN instruction encodings either way.
#
# ZERO copyrighted assets: every shader here is self-authored. Never derive corpus
# bytes from a game eboot, a dumped .sb, or any Sony/OpenOrbis SDK artifact.
#
# The committed .dis goldens are hand-verified against the .s source (the mnemonics
# and operands read off the assembly), NOT blessed from the decoder under test. The
# decode test additionally cross-checks the corpus bytes against `llvm-mc
# -disassemble` when llvm-mc is on PATH, so a decoder bug cannot silently rewrite a
# golden.
set -euo pipefail
cd "$(dirname "$0")"

MC=${LLVM_MC:-llvm-mc}
CPU=bonaire

for src in *.s; do
	name=${src%.s}
	# llvm-mc prints one `; encoding: [0x..,0x..,...]` per instruction. Collect the
	# hex bytes in order and pack them into <name>.code.bin.
	"$MC" -triple amdgcn -mcpu="$CPU" -filetype=asm -show-encoding "$src" \
		| sed -n 's/.*encoding: \[\(.*\)\].*/\1/p' \
		| tr ',' '\n' \
		| tr -d ' ' \
		| grep -E '^0x[0-9a-fA-F]{2}$' \
		| sed 's/^0x//' \
		| xxd -r -p \
		> "$name.code.bin"
	echo "assembled $src -> $name.code.bin ($(wc -c < "$name.code.bin") bytes)"
done
