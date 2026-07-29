# Build and run the sekejap C ABI examples.
#
#   make test      build libsekejap + compile/run test.c (asserts the C ABI)
#   make asan      same, under AddressSanitizer (catches C-side memory bugs)
#   make server    build libsekejap w/ engine + compile/run server.c (threads)
#   make install   copy lib + header + sekejap.pc into $(PREFIX) (default /usr/local)
#   make clean
#
# The library is the workspace cargo build output. Static vs dynamic: we link the
# dynamic lib and set the loader path at run time (portable, no install needed).

CARGO      ?= cargo
CC         ?= cc
CFLAGS     ?= -std=gnu11 -Wall -Wextra -O2
PREFIX     ?= /usr/local

# Workspace target dir (three levels up from wrappers/c/examples).
TARGET_DIR ?= ../../../target/release
INCLUDE    := ../include

UNAME := $(shell uname)
ifeq ($(UNAME),Darwin)
  LIBEXT := dylib
  LOADER := DYLD_LIBRARY_PATH
else
  LIBEXT := so
  LOADER := LD_LIBRARY_PATH
endif
LIB := $(TARGET_DIR)/libsekejap.$(LIBEXT)

.PHONY: test asan server lib lib-engine install clean

# ── libsekejap (default surface) ────────────────────────────────────────────
lib:
	$(CARGO) build --release -p sekejap-capi

# ── libsekejap with the concurrent Engine handle ────────────────────────────
lib-engine:
	$(CARGO) build --release -p sekejap-capi --features engine

# ── compile + run the assert-based C test ───────────────────────────────────
test: lib
	$(CC) $(CFLAGS) test.c -I $(INCLUDE) -L $(TARGET_DIR) -lsekejap -o test
	$(LOADER)=$(TARGET_DIR) ./test

# ── same, under AddressSanitizer (instruments the C side) ───────────────────
asan: lib
	$(CC) $(CFLAGS) -fsanitize=address -g test.c -I $(INCLUDE) -L $(TARGET_DIR) -lsekejap -o test_asan
	$(LOADER)=$(TARGET_DIR) ./test_asan

# ── the concurrent server demo (needs the engine feature + -DSEKEJAP_ENGINE) ─
server: lib-engine
	$(CC) $(CFLAGS) -DSEKEJAP_ENGINE server.c -I $(INCLUDE) -L $(TARGET_DIR) -lsekejap -lpthread -o server
	$(LOADER)=$(TARGET_DIR) ./server

# ── install like a system C library ─────────────────────────────────────────
install: lib
	install -d $(PREFIX)/lib $(PREFIX)/include $(PREFIX)/lib/pkgconfig
	install -m 644 $(LIB) $(PREFIX)/lib/
	install -m 644 $(INCLUDE)/sekejap.h $(PREFIX)/include/
	sed 's|@PREFIX@|$(PREFIX)|' sekejap.pc.in > $(PREFIX)/lib/pkgconfig/sekejap.pc
	@echo "installed to $(PREFIX). consumers: cc app.c \$$(pkg-config --cflags --libs sekejap)"

clean:
	rm -f test test_asan server
