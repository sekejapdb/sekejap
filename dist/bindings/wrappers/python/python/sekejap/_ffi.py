"""The ctypes layer: find `libsekejap`, declare its 59 entry points, own its strings.

Nothing in this module knows what a collection is. It loads the shared library
that `dist/ffi` builds (contract: `docs/dist/C_ABI.md`), gives every function
the argument and return types the header states, and carries the two ownership
rules across the boundary:

* a ``const char *`` going IN is borrowed UTF-8 that the library never frees;
* a ``char *`` coming OUT was allocated there and is freed ONCE with
  ``sekejap_string_free`` -- except ``sekejap_version``, which is static.

Every returned string is therefore declared ``c_void_p`` rather than
``c_char_p``: ctypes copies a ``c_char_p`` result into ``bytes`` and loses the
pointer, which would leak the allocation. :func:`take_string` does the copy and
the free together.

Where the library is looked for, in order:

1. ``$SEKEJAP_LIBRARY`` -- a full path to the file;
2. ``sekejap/_lib/`` inside this package -- what a platform wheel carries;
3. the platform loader's own search path -- ``DYLD_LIBRARY_PATH`` on macOS,
   ``LD_LIBRARY_PATH`` on Linux, ``PATH`` on Windows, and the system
   directories a ``make install`` of ``dist/ffi`` writes to.
"""

from __future__ import annotations

import ctypes
import os
import sys
import threading
from ctypes import c_char_p, c_int32, c_long, c_size_t, c_uint64, c_void_p
from pathlib import Path
from typing import Iterator, List, Optional

__all__ = [
    "LibraryNotFound",
    "library",
    "library_path",
    "take_string",
    "encode",
    "SEKEJAP_LIBRARY_ENV",
    "REBIND_UNBOUND",
]

SEKEJAP_LIBRARY_ENV = "SEKEJAP_LIBRARY"

# `sekejap_stmt_rebindable`: bound to nothing yet, so there is nothing to
# answer. The header's SEKEJAP_REBIND_UNBOUND. Not a failure; -1 is.
REBIND_UNBOUND = 2


class LibraryNotFound(Exception):
    """`libsekejap` is not on this machine, or not where the loader looked."""


def _file_names() -> List[str]:
    if sys.platform == "darwin":
        return ["libsekejap.dylib"]
    if sys.platform == "win32":
        return ["sekejap.dll", "libsekejap.dll"]
    return ["libsekejap.so"]


def _candidates() -> Iterator[str]:
    explicit = os.environ.get(SEKEJAP_LIBRARY_ENV)
    if explicit:
        yield explicit
    bundled = Path(__file__).resolve().parent / "_lib"
    for name in _file_names():
        yield str(bundled / name)
    # A bare name hands the search to the platform loader.
    for name in _file_names():
        yield name


_lock = threading.Lock()
_library = None  # type: Optional[ctypes.CDLL]
_library_path = None  # type: Optional[str]


def library():
    """The loaded library, declared and cached. Loads it on first use."""
    global _library, _library_path
    if _library is not None:
        return _library
    with _lock:
        if _library is not None:
            return _library
        tried = []  # type: List[str]
        for candidate in _candidates():
            try:
                loaded = ctypes.CDLL(candidate)
            except OSError as failure:
                tried.append("%s: %s" % (candidate, failure))
                continue
            _declare(loaded)
            _library = loaded
            _library_path = candidate
            return loaded
        raise LibraryNotFound(
            "libsekejap was not found. Set %s to the file, install it with "
            "`make install` in dist/ffi, or put it on the loader path. Tried:\n  %s"
            % (SEKEJAP_LIBRARY_ENV, "\n  ".join(tried))
        )


def library_path() -> str:
    """The file the wrapper loaded, for a report or a bug."""
    library()
    return _library_path or ""


def encode(text: Optional[str]) -> Optional[bytes]:
    """A borrowed UTF-8 argument, or NULL for ``None``."""
    if text is None:
        return None
    return text.encode("utf-8")


def take_string(pointer) -> Optional[str]:
    """Copy a returned ``char *`` into a ``str`` and free it exactly once."""
    if not pointer:
        return None
    value = ctypes.cast(pointer, c_char_p).value
    library().sekejap_string_free(c_void_p(pointer))
    if value is None:
        return None
    return value.decode("utf-8", "replace")


# The header, one row per function: name, argument types, return type.
# `c_void_p` as a return type is either an opaque handle or an OWNED string;
# `c_char_p` appears once, for the static `sekejap_version`.
_SIGNATURES = (
    # -- opening and identity ------------------------------------------------
    ("sekejap_open", [c_char_p], c_void_p),
    ("sekejap_open_with_config", [c_char_p, c_char_p], c_void_p),
    ("sekejap_open_service", [c_char_p], c_void_p),
    ("sekejap_close", [c_void_p], None),
    ("sekejap_version", [], c_char_p),
    ("sekejap_format_version", [], c_int32),
    # -- errors and memory ---------------------------------------------------
    ("sekejap_last_error", [c_void_p], c_void_p),
    ("sekejap_last_error_code", [c_void_p], c_int32),
    ("sekejap_string_free", [c_void_p], None),
    # -- documents -----------------------------------------------------------
    ("sekejap_put", [c_void_p, c_char_p, c_char_p, c_char_p], c_int32),
    ("sekejap_put_many", [c_void_p, c_char_p, c_char_p], c_long),
    ("sekejap_get", [c_void_p, c_char_p, c_char_p], c_void_p),
    ("sekejap_exists", [c_void_p, c_char_p, c_char_p], c_int32),
    ("sekejap_delete", [c_void_p, c_char_p, c_char_p], c_int32),
    ("sekejap_scan_open", [c_void_p, c_char_p, c_size_t], c_void_p),
    ("sekejap_scan_next", [c_void_p], c_void_p),
    ("sekejap_scan_close", [c_void_p], None),
    # -- SQL -----------------------------------------------------------------
    ("sekejap_execute", [c_void_p, c_char_p, c_char_p], c_long),
    ("sekejap_query", [c_void_p, c_char_p, c_char_p], c_void_p),
    ("sekejap_explain", [c_void_p, c_char_p, c_char_p], c_void_p),
    ("sekejap_prepare", [c_void_p, c_char_p], c_void_p),
    ("sekejap_stmt_query", [c_void_p, c_char_p], c_void_p),
    ("sekejap_stmt_execute", [c_void_p, c_char_p], c_long),
    ("sekejap_stmt_rebindable", [c_void_p], c_int32),
    ("sekejap_stmt_free", [c_void_p], None),
    ("sekejap_query_open", [c_void_p, c_char_p, c_char_p, c_size_t], c_void_p),
    ("sekejap_query_next", [c_void_p], c_void_p),
    ("sekejap_query_close", [c_void_p], None),
    # -- edges ---------------------------------------------------------------
    (
        "sekejap_link",
        [c_void_p, c_char_p, c_char_p, c_char_p, c_char_p, c_char_p],
        c_int32,
    ),
    (
        "sekejap_link_with",
        [c_void_p, c_char_p, c_char_p, c_char_p, c_char_p, c_char_p, c_char_p],
        c_int32,
    ),
    (
        "sekejap_unlink",
        [c_void_p, c_char_p, c_char_p, c_char_p, c_char_p, c_char_p],
        c_int32,
    ),
    (
        "sekejap_neighbours",
        [c_void_p, c_char_p, c_char_p, c_char_p, c_int32, c_size_t],
        c_void_p,
    ),
    # -- the catalog ---------------------------------------------------------
    ("sekejap_create_collection", [c_void_p, c_char_p, c_char_p], c_int32),
    ("sekejap_drop_collection", [c_void_p, c_char_p], c_int32),
    ("sekejap_collections", [c_void_p], c_void_p),
    ("sekejap_describe", [c_void_p, c_char_p], c_void_p),
    ("sekejap_count_rows", [c_void_p, c_char_p], c_long),
    ("sekejap_scan_count_rows", [c_void_p, c_char_p], c_long),
    ("sekejap_scan_count_edges", [c_void_p], c_long),
    # -- transactions --------------------------------------------------------
    ("sekejap_tx_begin", [c_void_p], c_void_p),
    ("sekejap_tx_put", [c_void_p, c_char_p, c_char_p, c_char_p], c_int32),
    ("sekejap_tx_delete", [c_void_p, c_char_p, c_char_p], c_int32),
    (
        "sekejap_tx_link",
        [c_void_p, c_char_p, c_char_p, c_char_p, c_char_p, c_char_p],
        c_int32,
    ),
    ("sekejap_tx_execute", [c_void_p, c_char_p, c_char_p], c_long),
    ("sekejap_tx_commit", [c_void_p], c_int32),
    ("sekejap_tx_rollback", [c_void_p], c_int32),
    # -- maintenance ---------------------------------------------------------
    ("sekejap_checkpoint", [c_void_p], c_int32),
    ("sekejap_publish", [c_void_p], c_int32),
    ("sekejap_storage", [c_void_p], c_void_p),
    # -- service mode --------------------------------------------------------
    ("sekejap_statement_timeout_ms", [c_void_p, c_uint64], c_int32),
    ("sekejap_cancel", [c_void_p], c_int32),
    ("sekejap_clear_interrupt", [c_void_p], c_int32),
    ("sekejap_subscribe", [c_void_p], c_long),
    ("sekejap_next_change", [c_void_p, c_long, c_uint64], c_void_p),
    ("sekejap_unsubscribe", [c_void_p, c_long], c_int32),
    # -- refused by name -----------------------------------------------------
    ("sekejap_open_memory", [], c_void_p),
    ("sekejap_trim_memory", [c_void_p], c_int32),
    ("sekejap_compact", [c_void_p], c_int32),
    ("sekejap_show", [c_void_p, c_char_p], c_void_p),
)


def _declare(loaded) -> None:
    """Give every entry point its types, so ctypes never guesses one."""
    for name, arguments, result in _SIGNATURES:
        try:
            function = getattr(loaded, name)
        except AttributeError as missing:
            raise LibraryNotFound(
                "the library at this path has no %s: it is not libsekejap "
                "0.17, or it is an older build (%s)" % (name, missing)
            ) from missing
        function.argtypes = arguments
        function.restype = result
