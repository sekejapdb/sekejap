# sekejap for Python. No Rust is built here: the binding is ctypes over the
# library `dist/ffi` produces, and every target below either runs Python
# against that library or packages the two together.
#
#   make test                  pytest against $(SEKEJAP_LIBRARY)
#   make example               examples/tour.py
#   make bench                 bench.py
#   make sdist                 the pure-Python source distribution
#   make wheel                 a platform wheel with the library inside it
#   make clean
#
# Point SEKEJAP_LIBRARY at the file, or SEKEJAP_LIB_DIR at the directory that
# holds it. The default is what `cargo build --release -p sekejap-capi` leaves
# in the workspace target directory.

PYTHON       ?= python3
ROOT         := $(abspath $(CURDIR)/../../../..)
TARGET_DIR   ?= $(ROOT)/target/release
UNAME        := $(shell uname -s)
ifeq ($(UNAME),Darwin)
LIB_NAME     := libsekejap.dylib
else
LIB_NAME     := libsekejap.so
endif
SEKEJAP_LIB_DIR ?= $(TARGET_DIR)
SEKEJAP_LIBRARY ?= $(SEKEJAP_LIB_DIR)/$(LIB_NAME)

export SEKEJAP_LIBRARY
PY := PYTHONPATH=$(CURDIR)/python $(PYTHON)

.PHONY: all test example bench sdist wheel stage-library clean

all: test

test:
	@test -f "$(SEKEJAP_LIBRARY)" || { \
	  echo "no library at $(SEKEJAP_LIBRARY) -- build it with"; \
	  echo "  cargo build --release -p sekejap-capi"; \
	  echo "or set SEKEJAP_LIBRARY to one"; exit 1; }
	$(PY) -m pytest tests -v

example:
	$(PY) examples/tour.py

bench:
	$(PY) bench.py

# An sdist is pure Python. A library staged for a wheel is removed first, so
# a source distribution can never carry one built for another machine.
sdist:
	rm -rf python/sekejap/_lib
	$(PYTHON) -m build --sdist

# The library goes INSIDE the package, so the wheel that carries it needs no
# loader path on the machine that installs it.
stage-library:
	@test -f "$(SEKEJAP_LIBRARY)" || { echo "no library at $(SEKEJAP_LIBRARY)"; exit 1; }
	mkdir -p python/sekejap/_lib
	cp "$(SEKEJAP_LIBRARY)" python/sekejap/_lib/

wheel: stage-library
	$(PYTHON) -m build --wheel
	@echo "the wheel is tagged py3-none-any -- retag it for the platform the"
	@echo "library was built for:  python3 -m wheel tags --remove \\"
	@echo "  --platform-tag=<tag> dist/*.whl"

clean:
	rm -rf build dist *.egg-info python/sekejap/_lib .pytest_cache
	find . -name __pycache__ -type d -prune -exec rm -rf {} +
