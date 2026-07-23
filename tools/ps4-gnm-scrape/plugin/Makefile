# ps4-gnm-scrape GoldHEN plugin — produces ps4-gnm-scrape.prx
#
# Requires:
#   OO_PS4_TOOLCHAIN  path to the OpenOrbis toolchain (repo: data/oo_sdk)
#   GOLDHEN_SDK       path to a built GoldHEN_Plugins_SDK (defaults to
#                     $(OO_PS4_TOOLCHAIN)/../goldhen_sdk == data/goldhen_sdk).
#                     Build it once: `cd data/goldhen_sdk && OO_PS4_TOOLCHAIN=... make`
#                     which produces libGoldHEN_Hook.a + build/crtprx.o.
#
# Usage (from repo root):
#   OO_PS4_TOOLCHAIN=$PWD/data/oo_sdk make -C tools/ps4-gnm-scrape/plugin

OUTPUT      := ps4-gnm-scrape
TOOLCHAIN   := $(OO_PS4_TOOLCHAIN)
GH_SDK      ?= $(OO_PS4_TOOLCHAIN)/../goldhen_sdk

PROJDIR     := source
INTDIR      := build

CFILES      := $(wildcard $(PROJDIR)/*.c)
OBJS        := $(patsubst $(PROJDIR)/%.c, $(INTDIR)/%.o, $(CFILES))

# GnmDriver provides the sceGnmSubmit* symbols; sockets come from libkernel.
LIBS        := -lSceLibcInternal -lGoldHEN_Hook -lkernel -lSceSysmodule -lSceGnmDriver -lScePad -lSceUserService

CFLAGS      := --target=x86_64-pc-freebsd12-elf -fPIC -funwind-tables -c -Wall \
               -isysroot $(TOOLCHAIN) -isystem $(TOOLCHAIN)/include \
               -I$(GH_SDK)/include
LDFLAGS     := -m elf_x86_64 -pie --script $(TOOLCHAIN)/link.x -e _init \
               --eh-frame-hdr -L$(TOOLCHAIN)/lib -L$(GH_SDK) $(LIBS)

_unused     := $(shell mkdir -p $(INTDIR))

UNAME_S     := $(shell uname -s)
ifeq ($(UNAME_S),Linux)
	CC   := clang
	LD   := ld.lld
	CDIR := linux
endif
ifeq ($(UNAME_S),Darwin)
	CC   := /usr/local/opt/llvm/bin/clang
	LD   := /usr/local/opt/llvm/bin/ld.lld
	CDIR := macos
endif

.DEFAULT_GOAL := all

all: $(OUTPUT).prx

$(OUTPUT).prx: $(OBJS)
	$(LD) $(GH_SDK)/build/crtprx.o $(OBJS) -o $(INTDIR)/$(OUTPUT).elf $(LDFLAGS)
	$(TOOLCHAIN)/bin/$(CDIR)/create-fself \
		-in=$(INTDIR)/$(OUTPUT).elf \
		-out=$(INTDIR)/$(OUTPUT).oelf \
		--lib=$(OUTPUT).prx --paid 0x3800000000000011

$(INTDIR)/%.o: $(PROJDIR)/%.c
	$(CC) $(CFLAGS) -o $@ $<

.PHONY: all clean
clean:
	rm -rf $(INTDIR) $(OUTPUT).prx
