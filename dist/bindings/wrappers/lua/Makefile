# Build the sekejap Lua C module (sekejap.so) against a PREBUILT libsekejap.
#
#   make                # build sekejap.so
#   make test           # run test.lua against it
#
# This directory builds NO Rust. libsekejap.{dylib,so,a} and
# include/sekejap.h come from SEKEJAP_PREFIX -- the sekejap-capi build
# (`cargo build --release -p sekejap-capi`) or, in this tree, the prebuilt
# copy the coordinator publishes at <scratch>
# `sekejap.pc` in that prefix predates the flat layout and points at a
# nonexistent lib/ subdirectory, so this Makefile does not use pkg-config
# for sekejap itself -- only for the Lua headers.
#
# A Lua loadable module resolves the lua_* symbols from the host interpreter
# at load time, so we use the Lua headers (pkg-config) but do NOT link
# liblua.

SEKEJAP_PREFIX ?= <scratch>
CABI           := $(SEKEJAP_PREFIX)/include
LIBDIR         := $(SEKEJAP_PREFIX)

LUA_PKG    := $(shell pkg-config --exists lua5.4 && echo lua5.4 || echo lua)
LUA_CFLAGS := $(shell pkg-config --cflags $(LUA_PKG))

UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
  SOFLAGS := -bundle -undefined dynamic_lookup
  RPATH   := -Wl,-rpath,$(LIBDIR)
else
  SOFLAGS := -shared -fPIC
  RPATH   := -Wl,-rpath,'$(LIBDIR)'
endif

sekejap.so: sekejap.c
	$(CC) $(SOFLAGS) $(LUA_CFLAGS) -I$(CABI) sekejap.c \
	  -L$(LIBDIR) -lsekejap $(RPATH) -o sekejap.so

.PHONY: test clean
test: sekejap.so
	lua5.4 test.lua

clean:
	rm -f sekejap.so
