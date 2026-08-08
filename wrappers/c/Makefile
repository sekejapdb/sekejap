# Build and install the sekejap C ABI (libsekejap + header + pkg-config file).
#
#   make install                 # into /usr/local (may need sudo)
#   make install PREFIX=~/.local # into a custom prefix
#
# Consumers (C/C++, the Go binding, etc.) then resolve the library via
# pkg-config. Version is read from the workspace Cargo.toml — the single source.

PREFIX  ?= /usr/local
VERSION := $(shell grep -m1 '^version = ' ../../Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')

UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
  LIBEXT := dylib
else
  LIBEXT := so
endif

TARGET := ../../target/release/libsekejap.$(LIBEXT)

.PHONY: build install uninstall clean

build:
	cargo build --release -p sekejap-capi

install: build
	install -d "$(PREFIX)/lib" "$(PREFIX)/include" "$(PREFIX)/lib/pkgconfig"
	install -m644 "$(TARGET)" "$(PREFIX)/lib/"
	install -m644 include/sekejap.h "$(PREFIX)/include/"
	sed -e 's|@PREFIX@|$(PREFIX)|g' -e 's|@VERSION@|$(VERSION)|g' \
	    sekejap.pc.in > "$(PREFIX)/lib/pkgconfig/sekejap.pc"
	@echo "installed libsekejap $(VERSION) + sekejap.pc into $(PREFIX)"

uninstall:
	rm -f "$(PREFIX)/lib/libsekejap.$(LIBEXT)" \
	      "$(PREFIX)/include/sekejap.h" \
	      "$(PREFIX)/lib/pkgconfig/sekejap.pc"

clean:
	cargo clean -p sekejap-capi
