# Build the sekejap Lua C module (sekejap.so) linking libsekejap.
#
#   cargo build --release -p sekejap-capi   # build libsekejap once
#   make            # build sekejap.so
#   make test       # run test.lua against it
#
# A Lua loadable module resolves the lua_* symbols from the host interpreter at
# load time, so we use the Lua headers (pkg-config) but do NOT link liblua.

LUA_PKG   := $(shell pkg-config --exists lua5.4 && echo lua5.4 || echo lua)
LUA_CFLAGS := $(shell pkg-config --cflags $(LUA_PKG))
CABI      := ../c/include
LIBDIR    := ../../target/release

UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
  SOFLAGS := -bundle -undefined dynamic_lookup
  RPATH   := -Wl,-rpath,@loader_path/$(LIBDIR)
else
  SOFLAGS := -shared -fPIC
  RPATH   := -Wl,-rpath,'$$ORIGIN/$(LIBDIR)'
endif

sekejap.so: sekejap.c
	$(CC) $(SOFLAGS) $(LUA_CFLAGS) -I$(CABI) sekejap.c \
	  -L$(LIBDIR) -lsekejap $(RPATH) -o sekejap.so

.PHONY: test clean
test: sekejap.so
	lua test.lua

clean:
	rm -f sekejap.so
