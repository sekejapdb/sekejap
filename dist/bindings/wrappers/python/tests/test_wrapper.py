"""The Python wrapper, end to end against the library it binds.

Every test opens a real directory and closes it: there is no in-memory store
to fake one with, and the wrapper refuses to pretend otherwise. What is
asserted is what `docs/dist/C_ABI.md` promises a caller -- the sentinels, the
difference between a MISS and a FAILURE, the ownership of the strings, and the
refusals arriving by name -- as the Python surface renders it.

Run:

    SEKEJAP_LIBRARY=/path/to/libsekejap.dylib \\
    PYTHONPATH=dist/bindings/wrappers/python/python \\
    python3 -m pytest dist/bindings/wrappers/python/tests -v
"""

import json
import os

import pytest

import sekejap
from sekejap import (
    Db,
    Direction,
    Invalid,
    Refused,
    SekejapError,
    Status,
    UnknownRow,
)

PEOPLE = [{"name": "name", "kind": "text"}, {"name": "age", "kind": "int"}]


@pytest.fixture()
def db(tmp_path):
    """One database per test, in its own directory, closed afterwards."""
    handle = Db(str(tmp_path / "store"))
    try:
        yield handle
    finally:
        handle.close()


@pytest.fixture()
def people(db):
    """`people` declared and two rows written, for the tests that need them."""
    db.create_collection("people", PEOPLE)
    db.put("people", "alice", {"name": "Alice", "age": 34})
    db.put("people", "bob", {"name": "Bob", "age": 41})
    return db


# ── the library itself ───────────────────────────────────────────────────────


def test_the_wrapper_reports_the_library_version_and_disk_format():
    assert sekejap.version() == "0.17.0"
    assert sekejap.format_version() == 2
    assert sekejap.library_path()


def test_the_package_carries_no_compiled_extension_of_its_own():
    # The binding is ctypes over the C ABI: the only native code is the
    # library it loads, which is not part of this package's source.
    directory = os.path.dirname(sekejap.__file__)
    for name in os.listdir(directory):
        assert not name.endswith((".so", ".pyd")), name


# ── opening, the catalog, documents ──────────────────────────────────────────


def test_open_creates_the_directory_and_close_is_idempotent(tmp_path):
    handle = Db(str(tmp_path / "fresh"))
    assert handle.closed is False
    assert (tmp_path / "fresh").is_dir()
    handle.close()
    handle.close()
    assert handle.closed is True


def test_open_with_a_store_configuration_is_accepted(tmp_path):
    configuration = {"budget_bytes": 64 * 1024 * 1024, "io": "buffered"}
    with Db(str(tmp_path / "configured"), config=configuration) as handle:
        assert handle.collections() == []


def test_a_store_configuration_this_build_will_not_honour_is_refused_by_name(tmp_path):
    # `sync: off` is not a knob a page-WAL collection can turn: the refusal
    # arrives as Unsupported with the sentence, never as a silent Full.
    with pytest.raises(SekejapError) as failure:
        Db(str(tmp_path / "unsynced"), config={"sync": "off"})
    assert failure.value.code is Status.UNSUPPORTED
    assert "SyncMode::Full" in failure.value.message


def test_create_collection_answers_created_then_already_there(db):
    assert db.create_collection("people", PEOPLE) is True
    assert db.create_collection("people", []) is False
    assert db.collections() == ["people"]


def test_a_document_read_back_carries_the_key_it_was_written_under(people):
    assert people.get("people", "alice") == {
        "_key": "alice",
        "name": "Alice",
        "age": 34,
    }


def test_a_miss_is_none_and_leaves_the_status_ok(people):
    assert people.get("people", "nobody") is None
    assert sekejap.last_error_code() is Status.OK
    assert sekejap.last_error() is None


def test_exists_and_delete_answer_whether_the_row_was_there(people):
    assert people.exists("people", "alice") is True
    assert people.delete("people", "alice") is True
    assert people.delete("people", "alice") is False
    assert people.exists("people", "alice") is False


def test_put_many_writes_the_whole_batch_under_one_commit(db):
    db.create_collection("people", PEOPLE)
    written = db.put_many(
        "people", {"carol": {"name": "Carol", "age": 29}, "dan": {"name": "Dan"}}
    )
    assert written == 2
    assert db.count_rows("people") == 2


def test_count_rows_and_the_two_walks_agree_on_what_is_there(people):
    assert people.count_rows("people") == 2
    assert people.scan_count_rows("people") == 2
    assert people.scan_count_edges() == 0


def test_describe_names_the_declared_fields_and_answers_none_for_no_such_collection(people):
    shape = people.describe("people")
    assert shape["name"] == "people"
    assert [field["name"] for field in shape["fields"]] == ["_key", "name", "age"]
    assert shape["rows"] == 2
    assert people.describe("absent") is None


def test_storage_answers_the_bytes_on_disk(people):
    storage = people.storage()
    assert storage["total_bytes"] == storage["data_bytes"] + storage["wal_bytes"]
    assert storage["total_bytes"] > 0


def test_drop_collection_answers_whether_it_was_there(people):
    assert people.drop_collection("people") is True
    assert people.drop_collection("people") is False
    assert people.collections() == []


# ── walking ──────────────────────────────────────────────────────────────────


def test_a_scan_walks_the_collection_in_pages_and_ends_with_none(people):
    walk = people.scan("people", page_rows=1)
    first = walk.next_page()
    second = walk.next_page()
    assert [row["_key"] for row in first] == ["alice"]
    assert [row["_key"] for row in second] == ["bob"]
    assert walk.next_page() is None
    walk.close()


def test_iterating_a_scan_yields_the_documents_not_the_pages(people):
    with people.scan("people") as walk:
        assert sorted(row["_key"] for row in walk) == ["alice", "bob"]


# ── SQL ──────────────────────────────────────────────────────────────────────


def test_a_query_with_a_parameter_answers_rows_keyed_by_column_name(people):
    rows = people.query("SELECT _key, name FROM people WHERE _key = $1", ["bob"])
    assert rows == [{"_key": "bob", "name": "Bob"}]


def test_a_column_missing_in_a_row_is_omitted_because_missing_is_not_null(db):
    db.create_collection(
        "singers", [{"name": "name", "kind": "text"}, {"name": "label", "kind": "text"}]
    )
    db.put("singers", "eve", {"name": "Eve"})
    assert db.query("SELECT _key, label FROM singers")[0] == {"_key": "eve"}


def test_execute_answers_the_rows_it_moved(db):
    db.create_collection("people", PEOPLE)
    assert db.execute(
        "INSERT INTO people (_key, name, age) VALUES ($1, $2, $3)", ["fay", "Fay", 22]
    ) == 1
    assert db.count_rows("people") == 1


def test_explain_answers_the_plan_the_engine_would_build(people):
    plan = people.explain("SELECT name FROM people WHERE _key = $1", ["alice"])
    assert "driver:" in plan


def test_a_prepared_statement_is_unbound_until_its_first_bind_then_rebindable(people):
    with people.prepare("SELECT name FROM people WHERE _key = $1") as statement:
        assert statement.rebindable is None
        assert statement.query(["alice"]) == [{"name": "Alice"}]
        assert statement.rebindable is True
        assert statement.query(["bob"]) == [{"name": "Bob"}]


def test_a_writing_prepared_statement_says_it_is_not_rebindable(db):
    db.create_collection("people", PEOPLE)
    with db.prepare(
        "INSERT INTO people (_key, name, age) VALUES ($1, $2, $3)"
    ) as statement:
        assert statement.execute(["gus", "Gus", 60]) == 1
        assert statement.rebindable is False


def test_a_paged_answer_hands_back_one_page_per_call(people):
    with people.stream("SELECT _key FROM people", page_rows=1) as answer:
        pages = list(answer.pages())
    assert [[row["_key"] for row in page] for page in pages] == [["alice"], ["bob"]]


# ── edges ────────────────────────────────────────────────────────────────────


def test_link_and_neighbours_name_the_collection_of_the_row_one_hop_away(people):
    people.link("people", "alice", "knows", "people", "bob")
    neighbours = people.neighbours("people", "alice", "knows", Direction.OUTGOING)
    assert len(neighbours) == 1
    assert neighbours[0]["collection"] == "people"
    assert neighbours[0]["key"] == "bob"
    assert neighbours[0]["document"]["name"] == "Bob"
    assert people.scan_count_edges() == 1


def test_an_edge_reads_back_from_the_other_end_when_the_direction_says_incoming(people):
    people.link("people", "alice", "knows", "people", "bob")
    incoming = people.neighbours("people", "bob", "knows", Direction.INCOMING)
    assert [row["key"] for row in incoming] == ["alice"]


def test_link_with_properties_and_unlink_answer_whether_the_edge_was_there(people):
    people.link("people", "alice", "knows", "people", "bob", properties={"since": 2019})
    assert people.unlink("people", "alice", "knows", "people", "bob") is True
    assert people.unlink("people", "alice", "knows", "people", "bob") is False


def test_an_edge_to_a_row_that_is_not_there_is_refused_never_a_dangling_identity(people):
    with pytest.raises(UnknownRow) as failure:
        people.link("people", "alice", "knows", "people", "ghost")
    assert failure.value.code is Status.UNKNOWN_ROW


# ── transactions ─────────────────────────────────────────────────────────────


def test_a_committed_transaction_makes_every_write_in_it_visible(people):
    with people.transaction() as tx:
        tx.put("people", "hana", {"name": "Hana", "age": 30})
        tx.put("people", "ivan", {"name": "Ivan", "age": 31})
        tx.link("people", "hana", "knows", "people", "ivan")
    assert people.count_rows("people") == 4
    assert people.scan_count_edges() == 1


def test_a_rolled_back_transaction_leaves_nothing_behind(people):
    tx = people.transaction()
    tx.put("people", "jane", {"name": "Jane", "age": 44})
    assert tx.delete("people", "alice") is True
    tx.rollback()
    assert people.exists("people", "jane") is False
    assert people.exists("people", "alice") is True
    assert people.count_rows("people") == 2


def test_an_exception_inside_the_block_rolls_the_transaction_back(people):
    class Deliberate(Exception):
        pass

    with pytest.raises(Deliberate):
        with people.transaction() as tx:
            tx.put("people", "kai", {"name": "Kai", "age": 27})
            raise Deliberate("the block did not finish")
    assert people.exists("people", "kai") is False


def test_a_statement_run_inside_a_transaction_waits_for_the_commit(people):
    with people.transaction() as tx:
        assert tx.execute(
            "INSERT INTO people (_key, name, age) VALUES ($1, $2, $3)",
            ["lena", "Lena", 38],
        ) == 1
    assert people.exists("people", "lena") is True


# ── maintenance ──────────────────────────────────────────────────────────────


def test_checkpoint_folds_the_log_and_publish_succeeds_having_nothing_to_swap(people):
    assert people.checkpoint() is True
    people.publish()


# ── the error channel ────────────────────────────────────────────────────────


def test_a_collection_that_is_not_in_the_catalog_is_a_failure_not_a_silent_zero(db):
    with pytest.raises(Invalid) as failure:
        db.count_rows("absent")
    assert failure.value.code is Status.INVALID
    assert "absent" in failure.value.message


def test_a_syntax_error_is_reported_at_prepare_with_its_sentence(db):
    with pytest.raises(SekejapError) as failure:
        db.prepare("SELCT 1")
    assert failure.value.message
    assert failure.value.code is not Status.OK


def test_a_success_clears_the_error_slot_both_halves(people):
    with pytest.raises(Invalid):
        people.count_rows("absent")
    assert sekejap.last_error_code() is Status.INVALID
    people.count_rows("people")
    assert sekejap.last_error_code() is Status.OK
    assert sekejap.last_error() is None


def test_a_call_on_a_closed_handle_is_refused_by_the_wrapper_not_by_the_library(tmp_path):
    handle = Db(str(tmp_path / "closed"))
    handle.close()
    with pytest.raises(ValueError):
        handle.collections()


# ── refused by name ──────────────────────────────────────────────────────────


def test_an_in_memory_open_is_refused_by_name_with_its_reason():
    with pytest.raises(Refused) as failure:
        sekejap.open_memory()
    assert failure.value.code is Status.REFUSED
    assert "disk-first" in failure.value.message


def test_trim_memory_and_compact_and_show_are_refused_by_name(db):
    for call, argument in ((db.trim_memory, ()), (db.compact, ()), (db.show, ("SHOW TABLES",))):
        with pytest.raises(Refused) as failure:
            call(*argument)
        assert failure.value.code is Status.REFUSED


def test_a_service_call_on_a_single_mode_handle_is_refused_by_name(db):
    with pytest.raises(Refused) as failure:
        db.statement_timeout_ms(50)
    assert "single mode" in failure.value.message


def test_a_neighbour_walk_wider_than_the_bound_is_refused_rather_than_truncated(people):
    people.link("people", "alice", "knows", "people", "bob")
    with pytest.raises(SekejapError) as failure:
        people.neighbours("people", "alice", "knows", Direction.OUTGOING, limit=4096)
    assert "GRAPH_TABLE" in failure.value.message


# ── service mode ─────────────────────────────────────────────────────────────


@pytest.fixture()
def service(tmp_path):
    handle = Db.open_service(str(tmp_path / "service"))
    try:
        yield handle
    finally:
        handle.close()


def test_a_service_handle_takes_a_statement_timeout_and_a_cancel(service):
    service.statement_timeout_ms(1_000)
    service.statement_timeout_ms(0)
    service.cancel()
    assert service.clear_interrupt() is True
    assert service.clear_interrupt() is False


def test_the_change_feed_reports_the_commit_that_wrote_a_document(service):
    subscription = service.subscribe()
    service.create_collection("people", PEOPLE)
    service.put("people", "mira", {"name": "Mira", "age": 51})
    event = service.next_change(subscription, timeout_ms=2_000)
    assert event is not None
    assert event["sequence"] >= 1
    assert [key["key"] for key in event["keys"]] == ["mira"]
    assert event["keys_truncated"] is False
    assert service.next_change(subscription, timeout_ms=0) is None
    assert service.unsubscribe(subscription) is True
    assert service.unsubscribe(subscription) is False


def test_a_checkpoint_in_service_mode_is_deferred_not_failed(service):
    service.create_collection("people", PEOPLE)
    service.put("people", "nadia", {"name": "Nadia", "age": 19})
    assert service.checkpoint() is False


# ── closing order ────────────────────────────────────────────────────────────


def test_closing_the_database_closes_the_handles_taken_from_it_first(people):
    statement = people.prepare("SELECT name FROM people")
    walk = people.scan("people")
    transaction = people.transaction()
    people.close()
    assert statement.closed and walk.closed and transaction.closed
    assert people.closed


def test_documents_survive_a_close_and_a_reopen(tmp_path):
    path = str(tmp_path / "durable")
    with Db(path) as first:
        first.create_collection("people", PEOPLE)
        first.put("people", "olu", {"name": "Olu", "age": 36})
    with Db(path) as second:
        assert second.get("people", "olu")["name"] == "Olu"
        assert json.dumps(second.collections()) == '["people"]'
