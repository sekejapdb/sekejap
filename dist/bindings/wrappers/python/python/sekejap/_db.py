"""The handles: :class:`Db`, :class:`Statement`, :class:`Scan`, :class:`Tx`.

One class per opaque handle in the C ABI, one method per C function, and
Python's own idioms over the top: a failure is an exception rather than a
sentinel, a document is a ``dict``, an answer is a ``list`` of ``dict``, and
every handle is a context manager that frees itself.

The sentinels of `docs/dist/C_ABI.md` §1 are read here and nowhere else:

* ``NULL`` for a failed pointer return, ``-1`` for a failed integer return;
* a MISS is ``NULL`` with :attr:`Status.OK`, and becomes ``None``, not an
  exception -- ``get``, ``describe``, the end of a walk, an empty change feed;
* anything else raises the :class:`SekejapError` subclass its
  ``sekejap_last_error_code`` names, carrying ``sekejap_last_error``'s
  sentence.

Ownership, as the header states it: a derived handle borrows its database, so
:meth:`Db.close` closes every live :class:`Statement`, :class:`Scan` and
:class:`Tx` taken from it FIRST.
"""

from __future__ import annotations

import enum
import json
import weakref
from typing import Any, Dict, Iterable, Iterator, List, Mapping, Optional, Sequence, Union

from . import _ffi

__all__ = [
    "Db",
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
    "version",
    "format_version",
    "library_path",
    "open_memory",
]

Json = Any
Document = Union[Mapping[str, Json], str]
Params = Union[None, str, Sequence[Json]]


# ── the two closed enumerations ───────────────────────────────────────────────


class Status(enum.IntEnum):
    """Why the last call on this thread failed. `C_ABI.md` §1.1."""

    OK = 0
    REFUSED = 1
    CORRUPT = 2
    UNSUPPORTED = 3
    IO = 4
    INVALID = 5
    BUSY = 6
    UNKNOWN_ROW = 7
    UNKNOWN = 8


class Direction(enum.IntEnum):
    """Which way an edge points, for :meth:`Db.neighbours`. `C_ABI.md` §1.2."""

    OUTGOING = 0
    INCOMING = 1
    BOTH = 2


# ── the exceptions ────────────────────────────────────────────────────────────


class SekejapError(Exception):
    """A failure the library reported, with its code and its sentence.

    ``code`` is a :class:`Status`, so a caller branches on the enumeration
    rather than on the text of ``message``.
    """

    def __init__(self, operation: str, code: Status, message: Optional[str]):
        self.operation = operation
        self.code = code
        self.message = message or "no message"
        super().__init__("%s: %s [%s]" % (operation, self.message, code.name))


class Refused(SekejapError):
    """A construct sekejap has no atomic for, refused by name with a reason."""


class Corrupt(SekejapError):
    """A page or a log failed verification. Nothing was changed."""


class Unsupported(SekejapError):
    """A format, policy or configuration this build does not implement."""


class IoFailure(SekejapError):
    """The directory, the file or the medium refused."""


class Invalid(SekejapError):
    """The caller's arguments are wrong, including JSON that does not parse."""


class Busy(SekejapError):
    """A bound refused rather than waiting: a budget, a deadline, a cancel."""


class UnknownRow(SekejapError):
    """The named row is not in the collection, on a call that needs it."""


_EXCEPTIONS = {
    Status.REFUSED: Refused,
    Status.CORRUPT: Corrupt,
    Status.UNSUPPORTED: Unsupported,
    Status.IO: IoFailure,
    Status.INVALID: Invalid,
    Status.BUSY: Busy,
    Status.UNKNOWN_ROW: UnknownRow,
}


def last_error() -> Optional[str]:
    """The message for the last failure ON THIS THREAD, or ``None``.

    The handle the C function takes is accepted and ignored -- the slot is
    thread-local -- so the wrapper passes ``NULL`` and a failed open still
    reports.
    """
    library = _ffi.library()
    return _ffi.take_string(library.sekejap_last_error(None))


def last_error_code() -> Status:
    """The code for the same failure, :attr:`Status.OK` after a success."""
    library = _ffi.library()
    return _status(library.sekejap_last_error_code(None))


def _status(value: int) -> Status:
    try:
        return Status(value)
    except ValueError:
        return Status.UNKNOWN


def _fail(operation: str) -> "SekejapError":
    code = last_error_code()
    message = last_error()
    return _EXCEPTIONS.get(code, SekejapError)(operation, code, message)


# ── JSON at the boundary ──────────────────────────────────────────────────────


def _document(document: Document) -> str:
    if isinstance(document, str):
        return document
    return json.dumps(document)


def _params(params: Params) -> Optional[str]:
    """A parameter list is a JSON ARRAY; ``None`` is no parameters."""
    if params is None:
        return None
    if isinstance(params, str):
        return params
    return json.dumps(list(params))


def _rows(rows: Union[str, Mapping[str, Document], Iterable[Any]]) -> str:
    """``put_many``'s array of ``{"key": ..., "doc": ...}``.

    A mapping of key to document, a sequence of ``(key, document)`` pairs and
    a sequence of those objects are all accepted and all become the one shape.
    """
    if isinstance(rows, str):
        return rows
    prepared: List[Dict[str, Json]] = []
    items: Iterable[Any]
    if isinstance(rows, Mapping):
        items = rows.items()
    else:
        items = rows
    for item in items:
        if isinstance(item, Mapping) and "key" in item and "doc" in item:
            prepared.append({"key": item["key"], "doc": item["doc"]})
            continue
        key, document = item
        if isinstance(document, str):
            document = json.loads(document)
        prepared.append({"key": key, "doc": document})
    return json.dumps(prepared)


def _parse(text: Optional[str]) -> Json:
    if text is None:
        return None
    return json.loads(text)


# ── library identity ──────────────────────────────────────────────────────────


def version() -> str:
    """The library version, ``MAJOR.MINOR.PATCH``. Static: never freed."""
    value = _ffi.library().sekejap_version()
    return value.decode("utf-8") if value else ""


def format_version() -> int:
    """The disk format this build reads and writes."""
    return int(_ffi.library().sekejap_format_version())


def library_path() -> str:
    """The `libsekejap` file this process loaded."""
    return _ffi.library_path()


def open_memory() -> "Db":
    """REFUSED: sekejap is disk-first and has no in-memory store.

    The symbol is kept so the refusal arrives by name with a reason rather
    than as an ``AttributeError``. Give :class:`Db` a directory.
    """
    _ffi.library().sekejap_open_memory()
    raise _fail("open_memory")


# ── the handles ───────────────────────────────────────────────────────────────


class _Handle:
    """A pointer this wrapper frees exactly once."""

    __slots__ = ("_pointer", "__weakref__")

    def __init__(self, pointer: int):
        self._pointer = pointer

    @property
    def closed(self) -> bool:
        return self._pointer is None

    def _live(self, operation: str) -> int:
        if self._pointer is None:
            raise ValueError("%s on a handle that is already closed" % operation)
        return self._pointer

    def __enter__(self):
        return self

    def __exit__(self, kind, value, traceback):
        self.close()
        return False

    def close(self) -> None:  # pragma: no cover - overridden
        raise NotImplementedError


class Scan(_Handle):
    """A paged walk: of one collection, or of one statement's answer.

    Iterating a scan yields DOCUMENTS; :meth:`pages` yields the pages the C
    ABI actually hands over, which is what bounds the string per call.
    """

    __slots__ = ("_next", "_close", "_what")

    def __init__(self, pointer: int, what: str, paged_query: bool):
        super().__init__(pointer)
        library = _ffi.library()
        # `sekejap_query_next`/`_close` are the same operations as
        # `sekejap_scan_next`/`_close` under the names that match the open.
        self._next = library.sekejap_query_next if paged_query else library.sekejap_scan_next
        self._close = library.sekejap_query_close if paged_query else library.sekejap_scan_close
        self._what = what

    def next_page(self) -> Optional[List[Json]]:
        """The next page, or ``None`` at the END of the walk (not a failure)."""
        pointer = self._next(self._live("next_page"))
        if not pointer:
            if last_error_code() is Status.OK:
                return None
            raise _fail("scan_next(%s)" % self._what)
        return _parse(_ffi.take_string(pointer))

    def pages(self) -> Iterator[List[Json]]:
        while True:
            page = self.next_page()
            if page is None:
                return
            yield page

    def __iter__(self) -> Iterator[Json]:
        for page in self.pages():
            for row in page:
                yield row

    def close(self) -> None:
        if self._pointer is not None:
            pointer, self._pointer = self._pointer, None
            self._close(pointer)

    def __del__(self):  # pragma: no cover - interpreter teardown
        try:
            self.close()
        except Exception:
            pass


class Statement(_Handle):
    """One statement, parsed at :meth:`Db.prepare` and compiled by its first bind."""

    __slots__ = ("_sql",)

    def __init__(self, pointer: int, sql: str):
        super().__init__(pointer)
        self._sql = sql

    @property
    def sql(self) -> str:
        return self._sql

    def query(self, params: Params = None) -> List[Dict[str, Json]]:
        """Run it as a row-returning statement."""
        library = _ffi.library()
        pointer = library.sekejap_stmt_query(
            self._live("query"), _ffi.encode(_params(params))
        )
        if not pointer:
            raise _fail("stmt_query")
        return _parse(_ffi.take_string(pointer))

    def execute(self, params: Params = None) -> int:
        """Run it as a writing statement and commit. Returns the rows it moved."""
        library = _ffi.library()
        moved = library.sekejap_stmt_execute(
            self._live("execute"), _ffi.encode(_params(params))
        )
        if moved == -1:
            raise _fail("stmt_execute")
        return int(moved)

    @property
    def rebindable(self) -> Optional[bool]:
        """Whether a further bind compiles nothing; ``None`` until first bound.

        A WRITING statement is never rebindable -- its document is folded at
        compile -- and answers ``False`` rather than pretending.
        """
        answer = _ffi.library().sekejap_stmt_rebindable(self._live("rebindable"))
        if answer == -1:
            raise _fail("stmt_rebindable")
        if answer == _ffi.REBIND_UNBOUND:
            return None
        return answer == 1

    def close(self) -> None:
        if self._pointer is not None:
            pointer, self._pointer = self._pointer, None
            _ffi.library().sekejap_stmt_free(pointer)

    def __del__(self):  # pragma: no cover - interpreter teardown
        try:
            self.close()
        except Exception:
            pass


class Tx(_Handle):
    """The writer, held across many writes under ONE barrier.

    While it is open it HOLDS the writer: a call on the same :class:`Db` that
    needs the writer waits for it. Used as a context manager, a clean exit
    commits and an exception rolls back.
    """

    __slots__ = ()

    def put(self, collection: str, key: str, document: Document) -> None:
        """Write one document, with NO commit."""
        library = _ffi.library()
        if library.sekejap_tx_put(
            self._live("put"),
            _ffi.encode(collection),
            _ffi.encode(key),
            _ffi.encode(_document(document)),
        ) != 0:
            raise _fail("tx_put(%s/%s)" % (collection, key))

    def delete(self, collection: str, key: str) -> bool:
        """Delete one row, with NO commit. ``True`` if it was there."""
        answer = _ffi.library().sekejap_tx_delete(
            self._live("delete"), _ffi.encode(collection), _ffi.encode(key)
        )
        if answer == -1:
            raise _fail("tx_delete(%s/%s)" % (collection, key))
        return answer == 1

    def link(
        self,
        from_collection: str,
        from_key: str,
        edge_type: str,
        to_collection: str,
        to_key: str,
    ) -> None:
        """Link two rows with a typed edge, with NO commit."""
        if _ffi.library().sekejap_tx_link(
            self._live("link"),
            _ffi.encode(from_collection),
            _ffi.encode(from_key),
            _ffi.encode(edge_type),
            _ffi.encode(to_collection),
            _ffi.encode(to_key),
        ) != 0:
            raise _fail("tx_link(%s)" % edge_type)

    def execute(self, sql: str, params: Params = None) -> int:
        """One writing statement, with NO commit. Returns the rows it moved."""
        moved = _ffi.library().sekejap_tx_execute(
            self._live("execute"), _ffi.encode(sql), _ffi.encode(_params(params))
        )
        if moved == -1:
            raise _fail("tx_execute")
        return int(moved)

    def commit(self) -> None:
        """Commit and FREE the handle, whether or not the commit succeeded."""
        pointer = self._live("commit")
        self._pointer = None
        if _ffi.library().sekejap_tx_commit(pointer) != 0:
            raise _fail("tx_commit")

    def rollback(self) -> None:
        """Roll back and FREE the handle, whether or not the rollback succeeded."""
        pointer = self._live("rollback")
        self._pointer = None
        if _ffi.library().sekejap_tx_rollback(pointer) != 0:
            raise _fail("tx_rollback")

    def close(self) -> None:
        """Roll back if the transaction is still open. A close is not a commit."""
        if self._pointer is not None:
            self.rollback()

    def __exit__(self, kind, value, traceback):
        if self._pointer is None:
            return False
        if kind is None:
            self.commit()
        else:
            self.rollback()
        return False

    def __del__(self):  # pragma: no cover - interpreter teardown
        try:
            self.close()
        except Exception:
            pass


class Db(_Handle):
    """An open sekejap database.

    ::

        from sekejap import Db

        with Db("./data") as db:
            db.create_collection("venues", [{"name": "suburb", "kind": "text"}])
            db.put("venues", "fitzroy_town_hall", {"suburb": "Fitzroy"})
            rows = db.query("SELECT _key FROM venues WHERE suburb = $1", ["Fitzroy"])

    The handle is ``Send + Sync`` on the Rust side, so it MAY be shared across
    threads; a :class:`Statement`, :class:`Scan` or :class:`Tx` taken from it
    is used from one thread at a time.
    """

    __slots__ = ("_path", "_service", "_children")

    def __init__(
        self,
        path: str,
        config: Optional[Mapping[str, Json]] = None,
        service: bool = False,
    ):
        library = _ffi.library()
        if service:
            if config is not None:
                raise ValueError(
                    "open_service takes no store configuration; open the "
                    "directory with a config or in service mode, not both"
                )
            pointer = library.sekejap_open_service(_ffi.encode(path))
        elif config is None:
            pointer = library.sekejap_open(_ffi.encode(path))
        else:
            pointer = library.sekejap_open_with_config(
                _ffi.encode(path), _ffi.encode(json.dumps(dict(config)))
            )
        if not pointer:
            raise _fail("open(%s)" % path)
        super().__init__(pointer)
        self._path = path
        self._service = service
        self._children: "weakref.WeakSet" = weakref.WeakSet()

    # -- opening -------------------------------------------------------------

    @classmethod
    def open(cls, path: str, config: Optional[Mapping[str, Json]] = None) -> "Db":
        """Open the directory, creating it when it holds none."""
        return cls(path, config=config)

    @classmethod
    def open_service(cls, path: str) -> "Db":
        """Open in SERVICE mode: one writer, parallel readers, the change feed."""
        return cls(path, service=True)

    @property
    def path(self) -> str:
        return self._path

    @property
    def service(self) -> bool:
        """Whether this handle was opened with :meth:`open_service`."""
        return self._service

    def close(self) -> None:
        """Close and free. Uncommitted work is discarded: a close is not a commit.

        Every live statement, scan and transaction taken from this handle is
        freed FIRST, because each of them borrows it.
        """
        if self._pointer is None:
            return
        for child in list(self._children):
            try:
                child.close()
            except Exception:
                pass
        self._children.clear()
        pointer, self._pointer = self._pointer, None
        _ffi.library().sekejap_close(pointer)

    def __del__(self):  # pragma: no cover - interpreter teardown
        try:
            self.close()
        except Exception:
            pass

    def __repr__(self) -> str:
        mode = "service" if self._service else "single"
        state = "closed" if self._pointer is None else mode
        return "<sekejap.Db %r (%s)>" % (self._path, state)

    def _adopt(self, child):
        self._children.add(child)
        return child

    # -- documents -----------------------------------------------------------

    def put(self, collection: str, key: str, document: Document) -> None:
        """Write one document, committed before this call returns."""
        if _ffi.library().sekejap_put(
            self._live("put"),
            _ffi.encode(collection),
            _ffi.encode(key),
            _ffi.encode(_document(document)),
        ) != 0:
            raise _fail("put(%s/%s)" % (collection, key))

    def put_many(
        self,
        collection: str,
        rows: Union[str, Mapping[str, Document], Iterable[Any]],
    ) -> int:
        """Many documents into one collection under ONE commit.

        A failure stores NONE of the batch. Returns the rows written.
        """
        written = _ffi.library().sekejap_put_many(
            self._live("put_many"), _ffi.encode(collection), _ffi.encode(_rows(rows))
        )
        if written == -1:
            raise _fail("put_many(%s)" % collection)
        return int(written)

    def get(self, collection: str, key: str) -> Optional[Dict[str, Json]]:
        """One document with ``_key`` set, or ``None`` for a MISS."""
        pointer = _ffi.library().sekejap_get(
            self._live("get"), _ffi.encode(collection), _ffi.encode(key)
        )
        if not pointer:
            if last_error_code() is Status.OK:
                return None
            raise _fail("get(%s/%s)" % (collection, key))
        return _parse(_ffi.take_string(pointer))

    def exists(self, collection: str, key: str) -> bool:
        """Whether the row is there."""
        answer = _ffi.library().sekejap_exists(
            self._live("exists"), _ffi.encode(collection), _ffi.encode(key)
        )
        if answer == -1:
            raise _fail("exists(%s/%s)" % (collection, key))
        return answer == 1

    def delete(self, collection: str, key: str) -> bool:
        """Delete one row and every edge that touches it, committed."""
        answer = _ffi.library().sekejap_delete(
            self._live("delete"), _ffi.encode(collection), _ffi.encode(key)
        )
        if answer == -1:
            raise _fail("delete(%s/%s)" % (collection, key))
        return answer == 1

    def scan(self, collection: str, page_rows: int = 0) -> Scan:
        """A walk of one collection in stable id order.

        ``page_rows`` of ``0`` means sekejap's default of 256 rows a page.
        """
        pointer = _ffi.library().sekejap_scan_open(
            self._live("scan"), _ffi.encode(collection), int(page_rows)
        )
        if not pointer:
            raise _fail("scan_open(%s)" % collection)
        return self._adopt(Scan(pointer, collection, paged_query=False))

    # -- SQL -----------------------------------------------------------------

    def execute(self, sql: str, params: Params = None) -> int:
        """One writing statement, committed. Returns the rows it moved."""
        moved = _ffi.library().sekejap_execute(
            self._live("execute"), _ffi.encode(sql), _ffi.encode(_params(params))
        )
        if moved == -1:
            raise _fail("execute")
        return int(moved)

    def query(self, sql: str, params: Params = None) -> List[Dict[str, Json]]:
        """One row-returning statement, as a list of objects keyed by column.

        A column that is MISSING in a row is omitted from that row's dict,
        because missing is not null.
        """
        pointer = _ffi.library().sekejap_query(
            self._live("query"), _ffi.encode(sql), _ffi.encode(_params(params))
        )
        if not pointer:
            raise _fail("query")
        return _parse(_ffi.take_string(pointer))

    def explain(self, sql: str, params: Params = None) -> str:
        """The plan the engine would build for one statement."""
        pointer = _ffi.library().sekejap_explain(
            self._live("explain"), _ffi.encode(sql), _ffi.encode(_params(params))
        )
        if not pointer:
            raise _fail("explain")
        return _ffi.take_string(pointer) or ""

    def prepare(self, sql: str) -> Statement:
        """Parse one statement now; compile it on its first bind."""
        pointer = _ffi.library().sekejap_prepare(self._live("prepare"), _ffi.encode(sql))
        if not pointer:
            raise _fail("prepare")
        return self._adopt(Statement(pointer, sql))

    def stream(self, sql: str, params: Params = None, page_rows: int = 0) -> Scan:
        """A row-returning statement with a PAGED delivery of its answer.

        ``page_rows`` of ``0`` means 4,096. This bounds the string per call
        and lets a caller stop reading; it does not bound the answer.
        """
        pointer = _ffi.library().sekejap_query_open(
            self._live("stream"),
            _ffi.encode(sql),
            _ffi.encode(_params(params)),
            int(page_rows),
        )
        if not pointer:
            raise _fail("query_open")
        return self._adopt(Scan(pointer, sql, paged_query=True))

    # -- edges ---------------------------------------------------------------

    def link(
        self,
        from_collection: str,
        from_key: str,
        edge_type: str,
        to_collection: str,
        to_key: str,
        properties: Optional[Mapping[str, Json]] = None,
    ) -> None:
        """Link two rows with a typed edge, committed.

        BOTH endpoints must already exist: a missing one raises
        :class:`UnknownRow`, never a dangling identity.
        """
        library = _ffi.library()
        handle = self._live("link")
        if properties is None:
            answer = library.sekejap_link(
                handle,
                _ffi.encode(from_collection),
                _ffi.encode(from_key),
                _ffi.encode(edge_type),
                _ffi.encode(to_collection),
                _ffi.encode(to_key),
            )
        else:
            answer = library.sekejap_link_with(
                handle,
                _ffi.encode(from_collection),
                _ffi.encode(from_key),
                _ffi.encode(edge_type),
                _ffi.encode(to_collection),
                _ffi.encode(to_key),
                _ffi.encode(_document(properties)),
            )
        if answer != 0:
            raise _fail("link(%s)" % edge_type)

    def unlink(
        self,
        from_collection: str,
        from_key: str,
        edge_type: str,
        to_collection: str,
        to_key: str,
    ) -> bool:
        """Remove one edge, committed. ``True`` if it was there."""
        answer = _ffi.library().sekejap_unlink(
            self._live("unlink"),
            _ffi.encode(from_collection),
            _ffi.encode(from_key),
            _ffi.encode(edge_type),
            _ffi.encode(to_collection),
            _ffi.encode(to_key),
        )
        if answer == -1:
            raise _fail("unlink(%s)" % edge_type)
        return answer == 1

    def neighbours(
        self,
        collection: str,
        key: str,
        edge_type: Optional[str] = None,
        direction: Direction = Direction.OUTGOING,
        limit: int = 256,
    ) -> List[Dict[str, Json]]:
        """The rows one hop away, as ``{"collection", "key", "document"}``.

        ``edge_type`` of ``None`` is every type. The answer is complete or
        refused, under a bound of 256 edges; a wider walk is ``GRAPH_TABLE``
        in SQL and is refused here by name.
        """
        pointer = _ffi.library().sekejap_neighbours(
            self._live("neighbours"),
            _ffi.encode(collection),
            _ffi.encode(key),
            _ffi.encode(edge_type),
            int(direction),
            int(limit),
        )
        if not pointer:
            raise _fail("neighbours(%s/%s)" % (collection, key))
        return _parse(_ffi.take_string(pointer))

    # -- the catalog ---------------------------------------------------------

    def create_collection(
        self, name: str, fields: Union[str, Sequence[Mapping[str, Json]]] = ()
    ) -> bool:
        """Declare a collection. ``True`` if it was created, ``False`` if it was there.

        A field is ``{"name", "kind", "dimension"?}``, where ``kind`` is one of
        ``text``, ``int``, ``real``, ``bool``, ``json``, ``geo``, ``point``,
        ``vector``, and ``dimension`` is required for ``vector`` only.
        """
        declaration = fields if isinstance(fields, str) else json.dumps(list(fields))
        answer = _ffi.library().sekejap_create_collection(
            self._live("create_collection"), _ffi.encode(name), _ffi.encode(declaration)
        )
        if answer == -1:
            raise _fail("create_collection(%s)" % name)
        return answer == 1

    def drop_collection(self, name: str) -> bool:
        """Remove a collection, its rows, its indexes and its descriptor."""
        answer = _ffi.library().sekejap_drop_collection(
            self._live("drop_collection"), _ffi.encode(name)
        )
        if answer == -1:
            raise _fail("drop_collection(%s)" % name)
        return answer == 1

    def collections(self) -> List[str]:
        """Every collection name in the catalog, in key order."""
        pointer = _ffi.library().sekejap_collections(self._live("collections"))
        if not pointer:
            raise _fail("collections")
        return _parse(_ffi.take_string(pointer))

    def describe(self, collection: str) -> Optional[Dict[str, Json]]:
        """The declared shape of one collection, or ``None`` if there is no such one.

        ``rows`` is the LIVE row count, or ``None`` where this database keeps
        no record for the collection -- which is "no record", not "no rows".
        """
        pointer = _ffi.library().sekejap_describe(
            self._live("describe"), _ffi.encode(collection)
        )
        if not pointer:
            if last_error_code() is Status.OK:
                return None
            raise _fail("describe(%s)" % collection)
        return _parse(_ffi.take_string(pointer))

    def count_rows(self, collection: str) -> int:
        """The rows, from the LIVE record when one is kept and from the walk when not."""
        answer = _ffi.library().sekejap_count_rows(
            self._live("count_rows"), _ffi.encode(collection)
        )
        if answer == -1:
            raise _fail("count_rows(%s)" % collection)
        return int(answer)

    def scan_count_rows(self, collection: str) -> int:
        """The rows BY WALKING them, whether or not a record exists."""
        answer = _ffi.library().sekejap_scan_count_rows(
            self._live("scan_count_rows"), _ffi.encode(collection)
        )
        if answer == -1:
            raise _fail("scan_count_rows(%s)" % collection)
        return int(answer)

    def scan_count_edges(self) -> int:
        """Every edge BY WALKING the primary edge keyspace. sekejap keeps no counter."""
        answer = _ffi.library().sekejap_scan_count_edges(self._live("scan_count_edges"))
        if answer == -1:
            raise _fail("scan_count_edges")
        return int(answer)

    # -- transactions --------------------------------------------------------

    def transaction(self) -> Tx:
        """Take the writer for many writes under ONE barrier."""
        pointer = _ffi.library().sekejap_tx_begin(self._live("transaction"))
        if not pointer:
            raise _fail("tx_begin")
        return self._adopt(Tx(pointer))

    # -- maintenance ---------------------------------------------------------

    def checkpoint(self) -> bool:
        """Fold the committed write-ahead log into the data file.

        ``True`` when it folded, ``False`` when a live reader holds a slot and
        the fold is DEFERRED. Deferred is not a failure.
        """
        answer = _ffi.library().sekejap_checkpoint(self._live("checkpoint"))
        if answer == -1:
            raise _fail("checkpoint")
        return answer == 1

    def publish(self) -> None:
        """Make the newest commit visible to readers now.

        In single mode there is no published view to swap and every commit is
        already visible to this handle, so this succeeds having done nothing.
        """
        if _ffi.library().sekejap_publish(self._live("publish")) != 0:
            raise _fail("publish")

    def storage(self) -> Dict[str, int]:
        """The bytes on disk: ``data_bytes``, ``wal_bytes``, ``total_bytes``."""
        pointer = _ffi.library().sekejap_storage(self._live("storage"))
        if not pointer:
            raise _fail("storage")
        return _parse(_ffi.take_string(pointer))

    # -- service mode --------------------------------------------------------

    def statement_timeout_ms(self, milliseconds: int) -> None:
        """Refuse a statement that runs longer. ``0`` milliseconds CLEARS it.

        Refused by name on a handle that is not in service mode.
        """
        if _ffi.library().sekejap_statement_timeout_ms(
            self._live("statement_timeout_ms"), int(milliseconds)
        ) != 0:
            raise _fail("statement_timeout_ms")

    def cancel(self) -> None:
        """Cancel the work in flight, from any thread. STICKY until cleared."""
        if _ffi.library().sekejap_cancel(self._live("cancel")) != 0:
            raise _fail("cancel")

    def clear_interrupt(self) -> bool:
        """Clear a cancel. ``True`` when one was standing."""
        answer = _ffi.library().sekejap_clear_interrupt(self._live("clear_interrupt"))
        if answer == -1:
            raise _fail("clear_interrupt")
        return answer == 1

    def subscribe(self) -> int:
        """Subscribe to the commit-time change feed. Returns the subscription id."""
        answer = _ffi.library().sekejap_subscribe(self._live("subscribe"))
        if answer == -1:
            raise _fail("subscribe")
        return int(answer)

    def next_change(self, subscription: int, timeout_ms: int = 0) -> Optional[Dict[str, Json]]:
        """The next change event, or ``None`` when none arrived.

        ``timeout_ms`` of ``0`` polls and returns at once.
        """
        pointer = _ffi.library().sekejap_next_change(
            self._live("next_change"), int(subscription), int(timeout_ms)
        )
        if not pointer:
            if last_error_code() is Status.OK:
                return None
            raise _fail("next_change")
        return _parse(_ffi.take_string(pointer))

    def unsubscribe(self, subscription: int) -> bool:
        """Close one subscription. ``True`` when it was open on the service."""
        answer = _ffi.library().sekejap_unsubscribe(
            self._live("unsubscribe"), int(subscription)
        )
        if answer == -1:
            raise _fail("unsubscribe")
        return answer == 1

    # -- refused by name -----------------------------------------------------

    def trim_memory(self) -> None:
        """REFUSED: nothing proportional to rows is held, so there is nothing to trim."""
        _ffi.library().sekejap_trim_memory(self._live("trim_memory"))
        raise _fail("trim_memory")

    def compact(self) -> None:
        """REFUSED: there is no payload-rewriting compaction. :meth:`checkpoint` folds the log."""
        _ffi.library().sekejap_compact(self._live("compact"))
        raise _fail("compact")

    def show(self, statement: str) -> None:
        """REFUSED: the ``SHOW`` family is not in this dialect.

        :meth:`collections` and :meth:`describe` answer the same questions as
        DATA rather than as a result set.
        """
        pointer = _ffi.library().sekejap_show(self._live("show"), _ffi.encode(statement))
        _ffi.take_string(pointer)
        raise _fail("show")
