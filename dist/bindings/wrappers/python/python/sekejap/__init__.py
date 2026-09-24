"""sekejap for Python -- the `libsekejap` C ABI, bound with ctypes.

sekejap is an embedded, disk-first, multi-model database: documents addressed
by COLLECTION and KEY, SQL with ``$n`` parameters over the same rows, typed
edges between them, and vector and spatial fields in the one store.

This package is pure Python. It carries no compiled extension of its own: it
loads ``libsekejap`` -- the shared library built from ``dist/ffi``, whose
contract is ``docs/dist/C_ABI.md`` -- and calls its 59 entry points through
``ctypes``. A platform wheel ships that library inside the package; an
install from the source distribution finds one through ``$SEKEJAP_LIBRARY``
or on the platform loader path.

::

    from sekejap import Db

    with Db("./data") as db:
        db.create_collection("venues", [{"name": "suburb", "kind": "text"}])
        db.put("venues", "fitzroy_town_hall", {"suburb": "Fitzroy"})
        for row in db.query("SELECT _key FROM venues WHERE suburb = $1", ["Fitzroy"]):
            print(row["_key"])

Handles, one per opaque pointer in the ABI: :class:`Db`, :class:`Statement`,
:class:`Scan`, :class:`Tx`. Each is a context manager, and closing a
:class:`Db` closes every handle taken from it first.

This is NOT the 0.16 Python API. ``DB``, ``Hit``, ``EdgeHit`` and the
slug-addressed ``put``/``link`` of the PyO3 extension are gone, because the
0.17 surface addresses a row by collection and key and answers a query with
plain ``dict`` rows. Code written against 0.16 fails at import rather than
silently meaning something else.
"""

from ._db import (
    Busy,
    Corrupt,
    Db,
    Direction,
    Invalid,
    IoFailure,
    Refused,
    Scan,
    SekejapError,
    Statement,
    Status,
    Tx,
    UnknownRow,
    Unsupported,
    format_version,
    last_error,
    last_error_code,
    library_path,
    open_memory,
    version,
)
from ._ffi import SEKEJAP_LIBRARY_ENV, LibraryNotFound

__all__ = [
    "Db",
    "DB",
    "Statement",
    "Scan",
    "Tx",
    "Direction",
    "Status",
    "SekejapError",
    "Refused",
    "Corrupt",
    "Unsupported",
    "IoFailure",
    "Invalid",
    "Busy",
    "UnknownRow",
    "LibraryNotFound",
    "SEKEJAP_LIBRARY_ENV",
    "version",
    "format_version",
    "library_path",
    "last_error",
    "last_error_code",
    "open_memory",
    "DataFrameAccessor",
]

__version__ = "0.17.3"


def _dataframe_accessor(self):
    """Pandas integration namespace (``db.df``). pandas is imported only here."""
    from ._dataframe import DataFrameAccessor

    return DataFrameAccessor(self)


Db.df = property(_dataframe_accessor)

# e1 exported the handle as ``DB``, and its README taught ``from sekejap import
# DB``. The same class under both names, so code written for e1 imports here.
DB = Db


def __getattr__(name):
    # `DataFrameAccessor` is advertised but not imported until it is asked
    # for, so `import sekejap` never reaches for pandas.
    if name == "DataFrameAccessor":
        from ._dataframe import DataFrameAccessor

        return DataFrameAccessor
    raise AttributeError("module %r has no attribute %r" % (__name__, name))
