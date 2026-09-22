"""``db.df`` -- pandas over the rows sekejap answers with.

pandas is optional and is imported only when a method that needs it is
called, so ``import sekejap`` never reaches for it.

A sekejap answer is already a list of objects keyed by column name, and a
scan is already a list of documents, so every method here is one call on
:class:`~sekejap.Db` and one ``DataFrame.from_records``. A column that is
MISSING in a row stays missing: pandas fills it with ``NaN``, which is what a
frame has instead of "not there".
"""

from __future__ import annotations

from typing import Any, Iterable, Optional, Sequence


def _pandas():
    try:
        import pandas
    except ImportError as missing:  # pragma: no cover - depends on the machine
        raise ImportError(
            "pandas is required for db.df -- install it with: pip install pandas"
        ) from missing
    return pandas


class DataFrameAccessor:
    """The pandas namespace of one database. Reached as ``db.df``."""

    __slots__ = ("_db",)

    def __init__(self, db):
        self._db = db

    def __repr__(self):
        return "<sekejap.DataFrameAccessor on %r>" % self._db.path

    # -- out of sekejap ------------------------------------------------------

    def query(self, sql: str, params=None, index_col: Optional[str] = None):
        """Run a row-returning statement and answer a ``DataFrame``."""
        return self._frame(self._db.query(sql, params), index_col)

    def scan(self, collection: str, page_rows: int = 0, index_col: Optional[str] = "_key"):
        """Walk one collection and answer its documents as a ``DataFrame``.

        The walk is paged, so the memory the WALK holds is bounded even
        though the frame it builds is not.
        """
        with self._db.scan(collection, page_rows=page_rows) as walk:
            return self._frame(list(walk), index_col)

    def neighbours(self, collection: str, key: str, edge_type=None, direction=0, limit=256):
        """The rows one hop away, flattened into ``collection``, ``key`` and the document."""
        rows = self._db.neighbours(collection, key, edge_type, direction, limit)
        flattened = []
        for row in rows:
            record = dict(row["document"])
            record["_collection"] = row["collection"]
            record["_key"] = row["key"]
            flattened.append(record)
        return self._frame(flattened, "_key")

    def _frame(self, records, index_col):
        pandas = _pandas()
        frame = pandas.DataFrame.from_records(records)
        if index_col and index_col in frame.columns:
            frame = frame.set_index(index_col)
        return frame

    # -- into sekejap --------------------------------------------------------

    def put(self, frame, collection: str, key_column: Optional[str] = None) -> int:
        """Write every row of a ``DataFrame`` into one collection, under ONE commit.

        The key comes from ``key_column`` when it is given, and from the
        frame's index when it is not. The column that supplied the key is not
        written twice: it is the row's key.
        """
        records = []
        if key_column is None:
            keys = [str(value) for value in frame.index]
            documents = frame.to_dict(orient="records")
        else:
            keys = [str(value) for value in frame[key_column]]
            documents = frame.drop(columns=[key_column]).to_dict(orient="records")
        for key, document in zip(keys, documents):
            records.append({"key": key, "doc": _clean(document)})
        return self._db.put_many(collection, records)


def _clean(document):
    """Drop the cells pandas filled in: a missing column is missing, not null."""
    pandas = _pandas()
    return {
        name: (value.item() if hasattr(value, "item") else value)
        for name, value in document.items()
        if not (value is None or (pandas.api.types.is_scalar(value) and pandas.isna(value)))
    }
