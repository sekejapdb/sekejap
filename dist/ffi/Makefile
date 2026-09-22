# Build, check and install the sekejap C ABI: libsekejap, sekejap.h and the
# pkg-config file a C consumer resolves them with.
#
#   make build                   # cargo build --release -p sekejap-capi
#   make check                   # build, then compile and RUN examples/smoke.c
#   make install                 # into /usr/local (may need sudo)
#   make install PREFIX=~/.local # into another prefix
#
# The version is read from the workspace Cargo.toml, which is the single
# source; the library comes out of the cargo target directory, so
# CARGO_TARGET_DIR is honoured the same way every other build in this
# repository honours it (docs/LAYERS.md).

PREFIX  ?= /usr/local
CARGO   ?= cargo
CC      ?= cc
# Appended, not assigned: an environment CFLAGS (a conda or distribution
# toolchain sets one) keeps its flags and gets these as well.
override CFLAGS += -std=gnu11 -Wall -Wextra -O2
VERSION := $(shell grep -m1 '^version = ' ../../Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')

CARGO_TARGET_DIR ?= ../../target
TARGET_DIR       := $(CARGO_TARGET_DIR)/release
INCLUDE          := include

UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
  LIBEXT := dylib
  LOADER := DYLD_LIBRARY_PATH
else
  LIBEXT := so
  LOADER := LD_LIBRARY_PATH
endif

LIB    := $(TARGET_DIR)/libsekejap.$(LIBEXT)
STATIC := $(TARGET_DIR)/libsekejap.a

.PHONY: build check header install uninstall clean

# ── the library: a cdylib and a staticlib, plus the regenerated header ──────
build:
	$(CARGO) build --release -p sekejap-capi

# ── regenerate include/sekejap.h from the extern "C" surface ────────────────
# build.rs already does this on every build; this target is the manual form,
# for a checkout where the build script was skipped.
header:
	cbindgen --output $(INCLUDE)/sekejap.h

# ── compile examples/smoke.c against the built library and RUN it ───────────
check: build
	$(CC) $(CFLAGS) examples/smoke.c -I $(INCLUDE) -L $(TARGET_DIR) -lsekejap -o $(TARGET_DIR)/sekejap_smoke
	$(LOADER)=$(TARGET_DIR) $(TARGET_DIR)/sekejap_smoke

# ── install like a system C library ─────────────────────────────────────────
install: build
	install -d "$(PREFIX)/lib" "$(PREFIX)/include" "$(PREFIX)/lib/pkgconfig"
	install -m644 "$(LIB)" "$(PREFIX)/lib/"
	install -m644 "$(STATIC)" "$(PREFIX)/lib/"
	install -m644 $(INCLUDE)/sekejap.h "$(PREFIX)/include/"
	sed -e 's|@PREFIX@|$(PREFIX)|g' -e 's|@VERSION@|$(VERSION)|g' \
	    sekejap.pc.in > "$(PREFIX)/lib/pkgconfig/sekejap.pc"
	@echo "installed libsekejap $(VERSION), sekejap.h and sekejap.pc into $(PREFIX)"
	@echo "consumers: cc app.c \$$(pkg-config --cflags --libs sekejap)"

uninstall:
	rm -f "$(PREFIX)/lib/libsekejap.$(LIBEXT)" \
	      "$(PREFIX)/lib/libsekejap.a" \
	      "$(PREFIX)/include/sekejap.h" \
	      "$(PREFIX)/lib/pkgconfig/sekejap.pc"

clean:
	rm -f $(TARGET_DIR)/sekejap_smoke
	$(CARGO) clean -p sekejap-capi
