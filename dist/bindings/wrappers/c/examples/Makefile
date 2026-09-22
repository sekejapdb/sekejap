# Build the sekejap C ABI and run the tour against it.
#
#   make tour      # cargo build + compile tour.c + run it
#   make clean
#
# The header and the library both come from dist/ffi; nothing here builds a
# second copy of either.

CARGO   ?= cargo
CC      ?= cc
# Appended, not assigned: an environment CFLAGS keeps its flags and gets
# these as well.
override CFLAGS += -std=gnu11 -Wall -Wextra -O2

FFI              := ../../../../ffi
CARGO_TARGET_DIR ?= ../../../../../target
TARGET_DIR       := $(CARGO_TARGET_DIR)/release

UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
  LOADER := DYLD_LIBRARY_PATH
else
  LOADER := LD_LIBRARY_PATH
endif

.PHONY: lib tour clean

lib:
	$(CARGO) build --release -p sekejap-capi

tour: lib
	$(CC) $(CFLAGS) tour.c -I $(FFI)/include -L $(TARGET_DIR) -lsekejap -o $(TARGET_DIR)/sekejap_tour
	$(LOADER)=$(TARGET_DIR) $(TARGET_DIR)/sekejap_tour

clean:
	rm -f $(TARGET_DIR)/sekejap_tour
