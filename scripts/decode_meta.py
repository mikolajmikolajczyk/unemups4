#!/usr/bin/env python3
"""Ground-truth decoder for Monocle/Celeste packed-atlas `.meta` files (format 5).

Prints each sub-texture's ClipRect (x,y,w,h) so an emulator dump can be diffed
against the real sub-rects — used to exonerate the FS/parse path for the Celeste
atlas-splatter (task-178). The sprite UVs are `ClipRect / atlas.Width`; a whole-
texture (corner) UV means a whole-atlas source-rect was authored at draw time,
not a wrong ClipRect here.

Usage: decode_meta.py <path-to-atlas.meta>
"""
import struct, sys
if len(sys.argv) != 2:
    sys.exit("usage: decode_meta.py <path-to-atlas.meta>")
f=open(sys.argv[1],'rb').read()
o=0
def i16():
    global o; v=struct.unpack_from('<h',f,o)[0]; o+=2; return v
def i32():
    global o; v=struct.unpack_from('<i',f,o)[0]; o+=4; return v
def s():
    global o; n=0; sh=0
    while True:
        b=f[o]; o+=1; n|=(b&0x7f)<<sh
        if not (b&0x80): break
        sh+=7
    v=f[o:o+n].decode('utf-8'); o+=n; return v
ver=i32(); args=s(); extra=i32(); ntex=i16()
print("version",ver,"| extra",extra,"| ntex",ntex,"| args",args[:40])
for t in range(ntex):
    src=s(); nsub=i16()
    print(f"  source[{t}]='{src}' nsub={nsub}")
    for k in range(nsub):
        name=s().replace('\\','/'); x=i16();y=i16();w=i16();h=i16();ox=i16();oy=i16();rw=i16();rh=i16()
        if k<5 or name in ('menu/textbg','logo','title','dot'):
            print(f"    [{k}] {name!r} clip=({x},{y},{w},{h}) off=({ox},{oy}) real=({rw},{rh})")
    print(f"    ...consumed to offset {o}")
print("final offset",o,"filelen",len(f))
