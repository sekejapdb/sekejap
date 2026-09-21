//! COLUMN RULES: the per-field `DEFAULT` generator and `NOT NULL` flag that
//! the collection descriptor records and the write path applies.
//!
//! `docs/lang/QL_CONTRACT.md` §2 (`DEFAULT now()`, `DEFAULT uuid4()`,
//! `DEFAULT uuid5(ns, name)`, `NOT NULL`).
//!
//! The oracles here are held in the test process and never read back from the
//! engine's own second reading:
//!
//! * `now()` is bracketed by two clock readings this test takes itself, so
//!   the assertion is that the stored instant lies between two instants the
//!   test observed, not that the engine agrees with the engine.
//! * `uuid5` is compared against the RFC 4122 §4.3 vector for the DNS
//!   namespace and `www.example.org`, a constant written here.
//! * SHA-1 is compared against the FIPS 180-4 vectors, also constants here.
//! * the older-binary refusal puts THIS build's feature word through the
//!   production admission decision carrying a mask with the new bit removed,
//!   which is the mask the previous release actually carried.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        ColumnRule, CollectionOptions, Database, DefaultValue, COLUMN_RULES_FEATURE,
        SUPPORTED_LOGICAL_FEATURES,
    },
    internal::{admit_logical_features, logical_features, parse_uuid, sha1},
    Kind,
};
use serde_json::{json, Value};

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// The instant this process reads, in the units a stored `now()` uses.
fn micros_now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the test host's clock is after 1970")
            .as_micros(),
    )
    .expect("microseconds since 1970 fit in i64 until the year 296000")
}

fn rule(default: Option<DefaultValue>, not_null: bool) -> ColumnRule {
    ColumnRule { default, not_null }
}

/// RFC 4122 Appendix C: the DNS namespace.
const NAMESPACE_DNS: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

// ── the generators ────────────────────────────────────────────────────────

/// A MISSING field with a `DEFAULT` gets it, and `now()` is read once per
/// ROW: two `now()` columns of one row carry the same instant, and that
/// instant lies between the two readings this test took around the write.
#[test]
fn a_missing_field_takes_its_default_and_now_is_one_clock_read_per_row() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection_rules(
            "evt",
            vec![
                ("at".into(), Kind::Int),
                ("also_at".into(), Kind::Int),
                ("id".into(), Kind::Text),
                ("n".into(), Kind::Int),
            ],
            vec![
                ("at".into(), "TIMESTAMPTZ".into()),
                ("also_at".into(), "TIMESTAMPTZ".into()),
            ],
            vec![
                ("at".into(), rule(Some(DefaultValue::Now), false)),
                ("also_at".into(), rule(Some(DefaultValue::Now), false)),
                ("id".into(), rule(Some(DefaultValue::Uuid4), false)),
            ],
            Default::default(),
        )
        .unwrap();
    let before = micros_now();
    db.put(c, "one", &json!({"n": 1})).unwrap();
    let after = micros_now();
    db.commit().unwrap();
    let row = db.get(c, "one").unwrap().unwrap().document;

    let at = row["at"].as_i64().expect("the default filled `at`");
    let also = row["also_at"].as_i64().expect("the default filled `also_at`");
    assert_eq!(at, also, "one clock read per row, shared by both columns");
    assert!(
        before <= at && at <= after,
        "the stored instant {at} us is outside the {before}..={after} us bracket this test read"
    );
    // A uuid4 is sixteen bytes with the version and variant nibbles set, and
    // it is written the way RFC 4122 §3 writes one.
    let id = row["id"].as_str().expect("the default filled `id`").to_owned();
    let bytes = parse_uuid(&id).unwrap();
    assert_eq!(id.len(), 36, "`{id}` is not a hyphenated UUID");
    assert_eq!(bytes[6] & 0xf0, 0x40, "`{id}` is not version 4");
    assert_eq!(bytes[8] & 0xc0, 0x80, "`{id}` is not RFC 4122 variant 2");
    // Two rows take two different random UUIDs, and two different instants
    // are allowed but one shared instant per row is not negotiable.
    db.put(c, "two", &json!({"n": 2})).unwrap();
    db.commit().unwrap();
    let second = db.get(c, "two").unwrap().unwrap().document;
    assert_ne!(second["id"], row["id"], "uuid4 is sixteen random bytes");
    assert_eq!(second["at"], second["also_at"]);
}

/// A field the caller WROTE keeps what the caller wrote, default or not --
/// including an explicit NULL, which defeats the default exactly as it does
/// in Postgres. A default fills a MISSING field, and nothing else.
#[test]
fn a_written_value_defeats_the_default_and_an_explicit_null_defeats_it_too() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection_rules(
            "evt",
            vec![("at".into(), Kind::Int), ("id".into(), Kind::Text)],
            Vec::new(),
            vec![
                ("at".into(), rule(Some(DefaultValue::Now), false)),
                ("id".into(), rule(Some(DefaultValue::Uuid4), false)),
            ],
            Default::default(),
        )
        .unwrap();
    db.put(c, "written", &json!({"at": 7, "id": "mine"})).unwrap();
    db.put(c, "nulled", &json!({"at": Value::Null, "id": Value::Null}))
        .unwrap();
    db.commit().unwrap();
    let written = db.get(c, "written").unwrap().unwrap().document;
    assert_eq!(written["at"], json!(7));
    assert_eq!(written["id"], json!("mine"));
    let nulled = db.get(c, "nulled").unwrap().unwrap().document;
    assert!(
        nulled["at"].is_null() && nulled["id"].is_null(),
        "an explicit NULL is a written value: {nulled}"
    );
}

/// `uuid5` is RFC 4122 §4.3: SHA-1 over the namespace bytes and the name.
/// The oracle is the RFC's own vector, a constant in this file, and the value
/// is DETERMINISTIC -- two rows that take the default take the same UUID.
#[test]
fn uuid5_reproduces_the_rfc_4122_vector_and_is_the_same_for_every_row() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection_rules(
            "site",
            vec![("id".into(), Kind::Text), ("n".into(), Kind::Int)],
            Vec::new(),
            vec![(
                "id".into(),
                rule(
                    Some(DefaultValue::Uuid5 {
                        namespace: parse_uuid(NAMESPACE_DNS).unwrap(),
                        name: "www.example.org".into(),
                    }),
                    false,
                ),
            )],
            Default::default(),
        )
        .unwrap();
    db.put(c, "a", &json!({"n": 1})).unwrap();
    db.put(c, "b", &json!({"n": 2})).unwrap();
    db.commit().unwrap();
    // RFC 4122: uuid5(NAMESPACE_DNS, "www.example.org").
    let expected = "74738ff5-5367-5958-9aee-98fffdcd1876";
    assert_eq!(db.get(c, "a").unwrap().unwrap().document["id"], json!(expected));
    assert_eq!(db.get(c, "b").unwrap().unwrap().document["id"], json!(expected));
}

/// The SHA-1 written for RFC 4122 §4.3, against the FIPS 180-4 vectors. A
/// version-5 UUID is a digest with four nibbles overwritten, so a wrong
/// digest is a wrong UUID everywhere and this is where it is caught.
#[test]
fn the_sha1_in_the_tree_answers_the_fips_180_vectors() {
    let hex = |d: [u8; 20]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(hex(sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
    assert_eq!(
        hex(sha1(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
        "the two-block vector, which is where a wrong padding length shows"
    );
    assert_eq!(
        hex(sha1(&vec![b'a'; 1_000_000])),
        "34aa973cd4c4daa4f61eeb2bdbad27316534016f",
        "the million-a vector, which is where a 32-bit length overflows"
    );
}

// ── NOT NULL ──────────────────────────────────────────────────────────────

/// A NOT NULL column refuses a row that omits it AND a row that writes NULL
/// into it. e4 distinguishes MISSING from NULL, so both refusals are stated
/// and both name the column; neither writes anything.
#[test]
fn not_null_refuses_a_missing_field_and_a_null_one_and_names_the_column() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection_rules(
            "person",
            vec![("email".into(), Kind::Text), ("n".into(), Kind::Int)],
            Vec::new(),
            vec![("email".into(), rule(None, true))],
            Default::default(),
        )
        .unwrap();
    db.put(c, "ok", &json!({"email": "a@b", "n": 1})).unwrap();
    db.commit().unwrap();

    let missing = db.put(c, "bad", &json!({"n": 2})).unwrap_err();
    let missing = format!("{missing:?}");
    assert!(
        missing.contains("email") && missing.contains("MISSING") && missing.contains("NULL"),
        "the MISSING refusal must name the column and say both are refused: {missing}"
    );
    let nulled = db
        .put(c, "bad", &json!({"email": Value::Null, "n": 2}))
        .unwrap_err();
    let nulled = format!("{nulled:?}");
    assert!(
        nulled.contains("email") && nulled.contains("NULL"),
        "the NULL refusal must name the column: {nulled}"
    );
    db.rollback().unwrap();
    // Neither refusal wrote a row.
    assert!(db.get(c, "bad").unwrap().is_none());
    assert!(db.get(c, "ok").unwrap().is_some());
}

/// A DEFAULT satisfies a NOT NULL on the same column, because the default is
/// filled BEFORE the check: the order is the contract's, not an accident.
#[test]
fn a_default_satisfies_the_not_null_on_the_same_column() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection_rules(
            "evt",
            vec![("id".into(), Kind::Text), ("n".into(), Kind::Int)],
            Vec::new(),
            vec![("id".into(), rule(Some(DefaultValue::Uuid4), true))],
            Default::default(),
        )
        .unwrap();
    db.put(c, "a", &json!({"n": 1})).unwrap();
    db.commit().unwrap();
    assert!(db.get(c, "a").unwrap().unwrap().document["id"].is_string());
    // An explicit NULL is not filled by the default, so it is refused.
    assert!(db.put(c, "b", &json!({"id": Value::Null})).is_err());
    db.rollback().unwrap();
}

/// `update` is read-modify-put, so a patch that does not mention a NOT NULL
/// column leaves the stored value in place and is accepted; a patch that
/// NULLS the column is refused.
#[test]
fn an_update_sees_the_merged_row_when_the_rules_are_applied() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection_rules(
            "person",
            vec![("email".into(), Kind::Text), ("n".into(), Kind::Int)],
            Vec::new(),
            vec![("email".into(), rule(None, true))],
            Default::default(),
        )
        .unwrap();
    db.put(c, "a", &json!({"email": "a@b", "n": 1})).unwrap();
    db.commit().unwrap();
    db.update(c, "a", &json!({"n": 2})).unwrap();
    db.commit().unwrap();
    assert_eq!(db.get(c, "a").unwrap().unwrap().document["email"], json!("a@b"));
    assert!(db.update(c, "a", &json!({"email": Value::Null})).is_err());
    db.rollback().unwrap();
    assert_eq!(db.get(c, "a").unwrap().unwrap().document["email"], json!("a@b"));
}

// ── the descriptor ────────────────────────────────────────────────────────

/// A rule is a name beside a FIELD: it survives a rewrite that keeps the
/// field, follows a rename the caller spells out, and is dropped with the
/// column. The feature bit rides the first rule, is never set by opening, and
/// is never cleared once the file has carried a tail.
#[test]
fn a_rule_follows_its_field_through_alter_and_the_bit_is_monotone() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    // A collection with no rule leaves the bit clear.
    let plain = db
        .create_collection("plain", vec![("n".into(), Kind::Int)], Default::default())
        .unwrap();
    db.commit().unwrap();
    assert_eq!(logical_features(&db) & COLUMN_RULES_FEATURE, 0);
    let _ = plain;

    let c = db
        .create_collection_rules(
            "person",
            vec![("email".into(), Kind::Text), ("n".into(), Kind::Int)],
            Vec::new(),
            vec![("email".into(), rule(Some(DefaultValue::Uuid4), true))],
            Default::default(),
        )
        .unwrap();
    db.commit().unwrap();
    assert_eq!(
        logical_features(&db) & COLUMN_RULES_FEATURE,
        COLUMN_RULES_FEATURE,
        "the bit rides the first rule"
    );
    assert_eq!(db.collection_info(c).unwrap().rules.len(), 1);

    // ADD COLUMN: the rule of the surviving field is carried.
    db.alter_collection(
        c,
        vec![
            ("email".into(), Kind::Text),
            ("n".into(), Kind::Int),
            ("note".into(), Kind::Text),
        ],
    )
    .unwrap();
    db.commit().unwrap();
    assert_eq!(db.collection_info(c).unwrap().rules.len(), 1);

    // RENAME COLUMN: the caller spells the rule with the new name and the
    // rule follows the column.
    db.alter_collection_rules(
        c,
        vec![
            ("address".into(), Kind::Text),
            ("n".into(), Kind::Int),
            ("note".into(), Kind::Text),
        ],
        Vec::new(),
        vec![("address".into(), rule(Some(DefaultValue::Uuid4), true))],
    )
    .unwrap();
    db.commit().unwrap();
    let info = db.collection_info(c).unwrap();
    assert_eq!(info.rules.len(), 1);
    assert_eq!(info.rules[0].0, "address");
    // And the renamed column still takes its default on a write.
    db.put(c, "r", &json!({"n": 1})).unwrap();
    db.commit().unwrap();
    assert!(db.get(c, "r").unwrap().unwrap().document["address"].is_string());

    // DROP COLUMN: the rule goes with it, and the rewrite is NOT refused for
    // naming a field the new layout has not got.
    db.alter_collection(c, vec![("n".into(), Kind::Int)]).unwrap();
    db.commit().unwrap();
    assert!(db.collection_info(c).unwrap().rules.is_empty());
    // Monotone: the bit stays set, because a file that ever carried the tail
    // is still a file an older binary must refuse.
    assert_eq!(
        logical_features(&db) & COLUMN_RULES_FEATURE,
        COLUMN_RULES_FEATURE
    );

    // And every rule survives a reopen with its generator intact.
    let d = db
        .create_collection_rules(
            "again",
            vec![("at".into(), Kind::Int), ("id".into(), Kind::Text)],
            Vec::new(),
            vec![
                ("at".into(), rule(Some(DefaultValue::Now), true)),
                (
                    "id".into(),
                    rule(
                        Some(DefaultValue::Uuid5 {
                            namespace: parse_uuid(NAMESPACE_DNS).unwrap(),
                            name: "www.example.org".into(),
                        }),
                        false,
                    ),
                ),
            ],
            Default::default(),
        )
        .unwrap();
    db.commit().unwrap();
    let before = db.collection_info(d).unwrap().rules;
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.collection_info(d).unwrap().rules, before);
    assert_eq!(
        logical_features(&db) & COLUMN_RULES_FEATURE,
        COLUMN_RULES_FEATURE,
        "the bit is read back off the file, not re-derived"
    );
    assert!(db.collection_info(c).unwrap().rules.is_empty());
}

/// A rule names a field of the collection and a `Kind` its generator can
/// produce. Each refusal is raised before anything is written.
#[test]
fn a_rule_that_names_no_field_or_the_wrong_kind_is_refused_by_name() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let fields = || vec![("at".into(), Kind::Int), ("id".into(), Kind::Text)];

    let absent = db
        .create_collection_rules(
            "a",
            fields(),
            Vec::new(),
            vec![("nope".into(), rule(None, true))],
            Default::default(),
        )
        .unwrap_err();
    assert!(format!("{absent:?}").contains("nope"), "{absent:?}");

    let wrong = db
        .create_collection_rules(
            "b",
            fields(),
            Vec::new(),
            vec![("id".into(), rule(Some(DefaultValue::Now), false))],
            Default::default(),
        )
        .unwrap_err();
    let wrong = format!("{wrong:?}");
    assert!(wrong.contains("now()") && wrong.contains("id"), "{wrong}");

    let wrong = db
        .create_collection_rules(
            "c",
            fields(),
            Vec::new(),
            vec![("at".into(), rule(Some(DefaultValue::Uuid4), false))],
            Default::default(),
        )
        .unwrap_err();
    assert!(format!("{wrong:?}").contains("at"), "{wrong:?}");

    let empty = db
        .create_collection_rules(
            "d",
            fields(),
            Vec::new(),
            vec![("at".into(), rule(None, false))],
            Default::default(),
        )
        .unwrap_err();
    assert!(format!("{empty:?}").contains("neither"), "{empty:?}");

    // None of the four wrote a collection.
    db.rollback().unwrap();
    for name in ["a", "b", "c", "d"] {
        assert!(db.collection(name).unwrap().is_none(), "`{name}` was written");
    }
}

/// A collection with rules is still a collection: `rename_collection` moves
/// the name and nothing else, and the rules stay on their fields.
#[test]
fn renaming_a_collection_keeps_its_id_its_rows_and_its_rules() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection_rules(
            "person",
            vec![("email".into(), Kind::Text)],
            Vec::new(),
            vec![("email".into(), rule(Some(DefaultValue::Uuid4), true))],
            Default::default(),
        )
        .unwrap();
    db.put(c, "a", &json!({})).unwrap();
    db.commit().unwrap();
    let before = db.get(c, "a").unwrap().unwrap();

    db.rename_collection(c, "people").unwrap();
    db.commit().unwrap();
    assert_eq!(db.collection("people").unwrap(), Some(c));
    assert!(db.collection("person").unwrap().is_none());
    assert_eq!(db.collection_info(c).unwrap().name, "people");
    assert_eq!(db.collection_info(c).unwrap().rules.len(), 1);
    assert_eq!(db.get(c, "a").unwrap().unwrap(), before);

    // A name already taken is refused, and the rename is not half-applied.
    db.create_collection("other", vec![("n".into(), Kind::Int)], CollectionOptions::default())
        .unwrap();
    db.commit().unwrap();
    assert!(db.rename_collection(c, "other").is_err());
    db.rollback().unwrap();
    assert_eq!(db.collection("people").unwrap(), Some(c));
}

// ── Law 8: the older binary ───────────────────────────────────────────────

/// A file that carries a COLUMN RULES tail is refused at ADMISSION by a
/// binary that predates the bit, as `Unsupported`, not as `Corrupt`.
///
/// The older binary is spelled as the mask it carried -- this build's mask
/// with `COLUMN_RULES_FEATURE` taken out -- and put through the same
/// admission decision `parse_header` makes, so the test exercises the
/// production rule rather than a copy of it. Without the bit an older binary
/// would pass header admission, reach the catalog record, fail the frozen
/// flags byte and report an intact, valid, NEWER file as damage; Law 8
/// separates an unknown format from damage, which is why the bit exists.
#[test]
fn a_column_rules_file_is_unsupported_to_a_binary_that_predates_the_bit() {
    assert_eq!(COLUMN_RULES_FEATURE, 0x1000);
    assert_eq!(
        SUPPORTED_LOGICAL_FEATURES & COLUMN_RULES_FEATURE,
        COLUMN_RULES_FEATURE,
        "the mask this build publishes must contain the bit it writes"
    );
    let older = SUPPORTED_LOGICAL_FEATURES & !COLUMN_RULES_FEATURE;

    // The feature word a file with one rule actually carries, taken off a
    // file this build just wrote rather than assumed.
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    db.create_collection_rules(
        "person",
        vec![("email".into(), Kind::Text)],
        Vec::new(),
        vec![("email".into(), rule(None, true))],
        Default::default(),
    )
    .unwrap();
    db.commit().unwrap();
    let written = logical_features(&db);
    drop(db);
    assert_eq!(written & COLUMN_RULES_FEATURE, COLUMN_RULES_FEATURE);

    // This build opens the file it writes.
    admit_logical_features(written, SUPPORTED_LOGICAL_FEATURES).unwrap();
    Database::open(&path, cfg()).unwrap();

    // The binary that predates the bit refuses it WHOLE, before a record is
    // read, and as Unsupported.
    let refused = admit_logical_features(written, older).unwrap_err();
    assert!(
        matches!(refused, sekejap_core::collections::Error::Unsupported(ref m)
            if m.contains(&format!("{written:#x}"))),
        "an intact newer file must be Unsupported and name its feature word: {refused:?}"
    );
}
