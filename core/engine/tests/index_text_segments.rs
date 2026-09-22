//! The packed text posting tier (tag `0x7A`, feature bit `0x40`).
//!
//! The head tier writes one B-tree entry per `(term, document)`; the packed
//! tier writes one value per term per segment. These tests hold the second
//! tier to the first: the same corpus, built both ways, must answer with the
//! same documents in the same order with the same scores, and every later
//! mutation must move both representations to the same answer.
use sekejap_core::{
    collections::{
        rebuild::{rebuild_derived_indexes, RebuildLimits},
        verification::{verify_indexed_source, VerificationLimits},
        CollectionId, Database, EntityId, Error, IndexId, TextCandidates, TextHit, TextMatch,
    },
    pagewal::PageWalStore,
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path};

const SEGMENT_TAG: u8 = 0x7a;
const NORM_BLOCK_TAG: u8 = 0x7b;
const TEXT_TAGS: [u8; 6] = [0x75, 0x76, 0x77, 0x78, SEGMENT_TAG, NORM_BLOCK_TAG];

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// `PHASE2_INDEX_FORMAT.md`: `0x80 + width`, then minimal unsigned big-endian.
fn ordered(n: u64) -> Vec<u8> {
    let b = n.to_be_bytes();
    let start = b.iter().position(|x| *x != 0).unwrap_or(7);
    let mut k = vec![0x80 + (8 - start) as u8];
    k.extend_from_slice(&b[start..]);
    k
}

fn index_prefix(tag: u8, id: IndexId) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend(ordered(id.0));
    k
}

/// Every persisted entry the text family owns, in key order.
fn text_entries(path: &Path, id: IndexId) -> Vec<(Vec<u8>, Vec<u8>)> {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let mut out = Vec::new();
    for tag in TEXT_TAGS {
        let prefix = index_prefix(tag, id);
        for row in raw.range(&prefix).unwrap() {
            let (key, value) = row.unwrap();
            if !key.starts_with(&prefix) {
                break;
            }
            out.push((key, value));
        }
    }
    out
}

fn segment_count(path: &Path, id: IndexId) -> usize {
    text_entries(path, id)
        .into_iter()
        .filter(|(key, _)| key[0] == SEGMENT_TAG)
        .count()
}

fn head_count(path: &Path, id: IndexId) -> usize {
    text_entries(path, id)
        .into_iter()
        .filter(|(key, _)| key[0] == 0x75)
        .count()
}

fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            out.insert(
                path.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(&path).unwrap(),
            );
        }
    }
    out
}

/// Deterministic prose. Terms repeat at different rates so the document
/// frequencies -- and therefore the BM25 ordering -- are not all equal, and
/// one term ("river") occurs in every document so its posting list is the
/// long one that exercises a segment split.
fn doc(i: u64) -> String {
    let subject = ["harbour", "mill", "terrace", "orchard", "quarry"][(i % 5) as usize];
    let verb = ["flooded", "settled", "burned", "drained"][(i % 4) as usize];
    let rare = if i % 97 == 0 { " comet" } else { "" };
    format!("the {subject} river {verb} again in the year {i}{rare}")
}

const ROWS: u64 = 1_200;

fn seed_collection(db: &mut Database) -> CollectionId {
    db.create_collection("docs", vec![("body".into(), Kind::Text)], Default::default())
        .unwrap()
}

/// Corpus first, index afterwards: the late build packs into segments.
fn packed(path: &Path) -> (Database, CollectionId, IndexId) {
    let mut db = Database::create(path, cfg()).unwrap();
    let docs = seed_collection(&mut db);
    for i in 0..ROWS {
        db.put(docs, &format!("d{i:05}"), &json!({ "body": doc(i) }))
            .unwrap();
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let index = db.create_text_index(docs, "body_idx", "body").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(index, 256).unwrap();
    (db, docs, index)
}

/// Index first, corpus afterwards: every posting is a head row.
fn heads(path: &Path) -> (Database, CollectionId, IndexId) {
    let mut db = Database::create(path, cfg()).unwrap();
    let docs = seed_collection(&mut db);
    let index = db.create_text_index(docs, "body_idx", "body").unwrap();
    db.commit().unwrap();
    while !db.build_index_step(index, 256).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    for i in 0..ROWS {
        db.put(docs, &format!("d{i:05}"), &json!({ "body": doc(i) }))
            .unwrap();
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    (db, docs, index)
}

fn ask(db: &Database, index: IndexId, text: &str, matching: TextMatch, k: usize) -> Vec<TextHit> {
    db.query_text(
        index,
        text,
        matching,
        k,
        TextCandidates::All,
        1 << 22,
        || false,
    )
    .unwrap()
}

fn ask_filtered(
    db: &Database,
    index: IndexId,
    text: &str,
    matching: TextMatch,
    ids: &[EntityId],
) -> Vec<TextHit> {
    db.query_text(
        index,
        text,
        matching,
        64,
        TextCandidates::SortedUnique(ids),
        1 << 22,
        || false,
    )
    .unwrap()
}

fn same(left: &[TextHit], right: &[TextHit], what: &str) {
    assert_eq!(left.len(), right.len(), "{what}: different hit counts");
    for (a, b) in left.iter().zip(right) {
        assert_eq!(a.id, b.id, "{what}: different document order");
        assert_eq!(
            a.score.to_bits(),
            b.score.to_bits(),
            "{what}: different score for {:?}",
            a.id
        );
    }
}

const QUERIES: [(&str, TextMatch); 7] = [
    ("river", TextMatch::Any),
    ("comet", TextMatch::Any),
    ("harbour river flooded", TextMatch::Any),
    ("harbour river", TextMatch::All),
    ("orchard comet", TextMatch::All),
    ("river flooded again", TextMatch::Phrase),
    ("quarry river", TextMatch::Phrase),
];

/// The property the whole tier stands on: packing changes the bytes, never the
/// answer. Same corpus, same identities, two representations, exact equality
/// of ranked ids AND of scores -- down to the bit.
#[test]
fn a_packed_build_answers_exactly_as_the_head_build_does() {
    let dir = tempfile::tempdir().unwrap();
    let (packed_db, _, packed_index) = packed(&dir.path().join("packed"));
    let (head_db, _, head_index) = heads(&dir.path().join("heads"));

    // The test would prove nothing if the two builds produced the same bytes.
    assert!(
        segment_count(&dir.path().join("packed"), packed_index) > 0,
        "the packed build wrote no segments"
    );
    assert_eq!(
        segment_count(&dir.path().join("heads"), head_index),
        0,
        "the head build must not write segments"
    );
    let packed_entries = text_entries(&dir.path().join("packed"), packed_index).len();
    let head_entries = text_entries(&dir.path().join("heads"), head_index).len();
    assert!(
        packed_entries * 2 < head_entries,
        "packing saved nothing: {packed_entries} entries vs {head_entries}"
    );
    assert_eq!(
        head_count(&dir.path().join("packed"), packed_index),
        0,
        "a packed build must not also write head rows"
    );

    for (text, matching) in QUERIES {
        for k in [1, 5, 50] {
            same(
                &ask(&packed_db, packed_index, text, matching, k),
                &ask(&head_db, head_index, text, matching, k),
                &format!("{text:?}/{matching:?}/k={k}"),
            );
        }
    }

    // The filtered path probes one document at a time and has to find a
    // packed posting by seek rather than by scan.
    let ids: Vec<EntityId> = (1..=ROWS)
        .filter(|sequence| sequence % 7 == 0)
        .map(|sequence| EntityId {
            collection: CollectionId(1),
            sequence,
        })
        .collect();
    for (text, matching) in QUERIES {
        same(
            &ask_filtered(&packed_db, packed_index, text, matching, &ids),
            &ask_filtered(&head_db, head_index, text, matching, &ids),
            &format!("filtered {text:?}/{matching:?}"),
        );
    }
}

/// A folded posting cannot be erased in place without rewriting the packed
/// value it sits in. It is retired by a head tombstone instead, and the two
/// tiers have to agree afterwards -- for a delete, for an update that drops a
/// term, for a reinsertion, and across a reopen.
#[test]
fn mutating_a_folded_document_keeps_both_tiers_agreeing() {
    let dir = tempfile::tempdir().unwrap();
    let packed_path = dir.path().join("packed");
    let head_path = dir.path().join("heads");
    let (mut packed_db, docs, packed_index) = packed(&packed_path);
    let (mut head_db, head_docs, head_index) = heads(&head_path);

    let mutate = |db: &mut Database, collection: CollectionId| {
        // A delete of a folded document.
        db.delete(collection, "d00003").unwrap();
        // An update that drops one term and adds another.
        db.put(
            collection,
            "d00007",
            &json!({"body": "the harbour lantern burned again"}),
        )
        .unwrap();
        // A brand new document indexes into the head tier.
        db.put(
            collection,
            "d99999",
            &json!({"body": "the quarry river flooded again in the year 9999 comet"}),
        )
        .unwrap();
        db.commit().unwrap();
    };
    mutate(&mut packed_db, docs);
    mutate(&mut head_db, head_docs);

    for (text, matching) in QUERIES {
        let got = ask(&packed_db, packed_index, text, matching, 50);
        same(
            &got,
            &ask(&head_db, head_index, text, matching, 50),
            &format!("after mutation {text:?}/{matching:?}"),
        );
        assert!(
            !got.iter().any(|hit| hit.id.sequence == 4),
            "the deleted document still answers {text:?}"
        );
    }
    // "river" left d00007 but its packed posting is still on disk; the
    // tombstone has to hide it.
    assert!(
        !ask(&packed_db, packed_index, "river", TextMatch::Any, 2000)
            .iter()
            .any(|hit| hit.id.sequence == 8),
        "an updated document kept a stale packed posting"
    );

    packed_db.checkpoint().unwrap();
    drop(packed_db);
    let reopened = Database::open(&packed_path, cfg()).unwrap();
    for (text, matching) in QUERIES {
        same(
            &ask(&reopened, packed_index, text, matching, 50),
            &ask(&head_db, head_index, text, matching, 50),
            &format!("after reopen {text:?}/{matching:?}"),
        );
    }
    drop(reopened);
    let report = verify_indexed_source(&packed_path, VerificationLimits::default(), |issue| {
        panic!("verification issue after mutation: {issue:?}");
    })
    .unwrap();
    assert!(report.complete && report.clean);
}

/// Law 8. The packed tier is a new on-disk representation, so it announces
/// itself with a new feature bit that an engine which does not implement it
/// refuses. The bit is only ever set by a build that actually packs something.
#[test]
fn the_packed_tier_is_admitted_only_behind_its_own_feature_bit() {
    let dir = tempfile::tempdir().unwrap();

    // Ordinary use never raises the file's minimum reader.
    let plain = dir.path().join("head-only");
    let (db, _, index) = heads(&plain);
    drop(db);
    // `0x2000` is the live row-count record every `create_collection` writes
    // (`collections/row_count.rs`); the head-only file is asserted to have
    // gained nothing BEYOND it.
    assert_eq!(features(&plain), 0x2011, "a head-only file gained a feature");
    assert_eq!(segment_count(&plain, index), 0);

    // A packed build sets it.
    let path = dir.path().join("packed");
    let (db, _, index) = packed(&path);
    drop(db);
    assert_eq!(features(&path), 0x2051, "the packed build did not set 0x40");
    assert!(segment_count(&path, index) > 0);

    // An engine whose supported mask predates 0x40 sees an unknown bit and
    // refuses the file whole, before anything is normalized or written.
    let older = dir.path().join("older-reader");
    copy_dir(&path, &older);
    // The probe has to name a bit NO released mask implements. This release
    // implements 0x001 through 0x2000 (typed, graph, vector, spatial, text,
    // quantized, segments, per-index tree, geometry, drop, expression,
    // declared spellings, column rules, live row counts:
    // `SUPPORTED_LOGICAL_FEATURES`), and 0x4000 is RESERVED for the parallel
    // semi-join item, so the unknown-bit probe is 0x8000 -- it moves with
    // that mask, as it moved to 0x2000 when the column rules landed.
    set_features(&older, 0x2051 | 0x8000);
    let before = files(&older);
    assert!(matches!(
        Database::open(&older, cfg()),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(files(&older), before, "a refusal changed the source");

    // And the bit cannot be cleared to smuggle the packed keyspace past a
    // reader that would not understand it.
    let hidden = dir.path().join("hidden");
    copy_dir(&path, &hidden);
    set_features(&hidden, 0x2011);
    let before = files(&hidden);
    assert!(matches!(Database::open(&hidden, cfg()), Err(Error::Corrupt(_))));
    assert_eq!(files(&hidden), before, "a refusal changed the source");
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let name = path.file_name().unwrap();
            if name.to_string_lossy().starts_with("reader") {
                continue;
            }
            fs::copy(&path, to.join(name)).unwrap();
        }
    }
}

fn reseal(packet: &mut [u8]) {
    let end = packet.len() - 4;
    let checksum = crc32c::crc32c(&packet[..end]).to_le_bytes();
    packet[end..].copy_from_slice(&checksum);
}

fn features(path: &Path) -> u64 {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let header = raw.get(&[0, 0, 0]).unwrap().unwrap();
    assert_eq!(&header[..8], b"E4COLL2\0");
    u64::from_be_bytes(header[10 + 8..10 + 16].try_into().unwrap())
}

fn set_features(path: &Path, features: u64) {
    let mut raw = PageWalStore::open(path, false, 1 << 20).unwrap();
    for copy in 0..3u8 {
        let key = [0, 0, copy];
        let mut header = raw.get(&key).unwrap().unwrap();
        header[10 + 8..10 + 16].copy_from_slice(&features.to_be_bytes());
        reseal(&mut header);
        raw.put(&key, &header).unwrap();
    }
    raw.commit().unwrap();
}

/// A packed value is a lot of postings behind one checksum, so the verifier
/// has to look inside it, and an explicit rebuild has to be able to put the
/// same bytes back.
#[test]
fn the_verifier_sees_inside_a_segment_and_a_rebuild_reproduces_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("packed");
    let (db, _, index) = packed(&path);
    drop(db);

    let clean = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        panic!("a good packed index reported {issue:?}");
    })
    .unwrap();
    assert!(clean.complete && clean.clean);

    // Rebuild reproduces the packed bytes exactly, not merely an index that
    // answers the same.
    let rebuilt = dir.path().join("rebuilt");
    rebuild_derived_indexes(&path, &rebuilt, RebuildLimits::default()).unwrap();
    assert_eq!(
        text_entries(&path, index),
        text_entries(&rebuilt, index),
        "rebuild did not reproduce the packed text index byte for byte"
    );

    // Now damage one packed value in three different ways and require the
    // verifier to say so each time rather than believe it.
    for damage in 0..3u8 {
        let broken = dir.path().join(format!("broken-{damage}"));
        copy_dir(&path, &broken);
        let (key, value) = {
            let entries = text_entries(&broken, index);
            entries
                .into_iter()
                .filter(|(key, _)| key[0] == SEGMENT_TAG)
                .max_by_key(|(_, value)| value.len())
                .unwrap()
        };
        let mut raw = PageWalStore::open(&broken, false, 1 << 20).unwrap();
        let mut damaged = value.clone();
        match damage {
            // A flipped delta: the postings no longer end where the key says.
            0 => damaged[4] ^= 0x01,
            // A truncated value.
            1 => damaged.truncate(damaged.len() - 1),
            // A count that claims more postings than the bytes hold.
            2 => damaged[1] = damaged[1].wrapping_add(1),
            _ => unreachable!(),
        }
        assert_ne!(damaged, value);
        raw.put(&key, &damaged).unwrap();
        raw.commit().unwrap();
        drop(raw);

        let mut issues = 0usize;
        let report =
            verify_indexed_source(&broken, VerificationLimits::default(), |_| issues += 1).unwrap();
        assert!(
            issues > 0 && !report.clean,
            "damage {damage} went unreported"
        );
    }
}

fn norm_block_count(path: &Path, id: IndexId) -> usize {
    text_entries(path, id)
        .into_iter()
        .filter(|(key, _)| key[0] == NORM_BLOCK_TAG)
        .count()
}

/// Every `0x76` head row and its value, in key order.
fn norm_rows(path: &Path, id: IndexId) -> Vec<(Vec<u8>, Vec<u8>)> {
    text_entries(path, id)
        .into_iter()
        .filter(|(key, _)| key[0] == 0x76)
        .collect()
}

/// The largest surviving per-row write. A 1,200-document packed build wrote
/// 1,200 norm rows; it now writes five blocks and no rows at all, and the
/// answers -- ids AND scores, which is where a wrong length would show --
/// are still exactly the head build's.
#[test]
fn a_packed_build_writes_norm_blocks_instead_of_a_row_per_document() {
    let dir = tempfile::tempdir().unwrap();
    let packed_path = dir.path().join("packed");
    let head_path = dir.path().join("heads");
    let (packed_db, _, packed_index) = packed(&packed_path);
    let (head_db, _, head_index) = heads(&head_path);

    assert_eq!(
        norm_rows(&packed_path, packed_index).len(),
        0,
        "the packed build still writes a norm row per document"
    );
    let blocks = norm_block_count(&packed_path, packed_index);
    assert_eq!(
        blocks,
        (ROWS as usize).div_ceil(256),
        "one block per 256 consecutive sequences"
    );
    // The head build must not grow a second tier.
    assert_eq!(norm_block_count(&head_path, head_index), 0);
    assert_eq!(norm_rows(&head_path, head_index).len(), ROWS as usize);

    // Scores divide by the document length, so an identical ranking with
    // identical score bits is what proves the packed lengths are the same
    // numbers the rows held.
    for (text, matching) in QUERIES {
        for k in [1, 5, 50] {
            same(
                &ask(&packed_db, packed_index, text, matching, k),
                &ask(&head_db, head_index, text, matching, k),
                &format!("packed norms {text:?}/{matching:?}/k={k}"),
            );
        }
    }
}

/// A folded length cannot be erased from the block it sits in. The delete
/// records the absence at the head instead -- an EMPTY `0x76` value, which
/// overrides the block from then on.
///
/// `length = 0` could not be borrowed for this: an explicitly empty string is
/// a PRESENT document with zero tokens, and the two must stay distinguishable,
/// which this checks directly.
#[test]
fn deleting_a_folded_document_leaves_a_norm_tombstone_not_a_zero_length() {
    let dir = tempfile::tempdir().unwrap();
    let packed_path = dir.path().join("packed");
    let head_path = dir.path().join("heads");
    let (mut packed_db, docs, packed_index) = packed(&packed_path);
    let (mut head_db, head_docs, head_index) = heads(&head_path);

    let mutate = |db: &mut Database, collection: CollectionId| {
        // A folded document, deleted.
        db.delete(collection, "d00011").unwrap();
        // A folded document updated to a DIFFERENT length (so a head row
        // covers the block) and then deleted: removing that head row would
        // uncover the stale packed length.
        db.put(collection, "d00012", &json!({"body": "river"}))
            .unwrap();
        db.commit().unwrap();
        db.delete(collection, "d00012").unwrap();
        // A folded document updated to the same length keeps no head row.
        db.put(
            collection,
            "d00013",
            &json!({"body": "the mill river burned again in the year 13"}),
        )
        .unwrap();
        // An explicitly empty document: present, zero tokens, NOT a tombstone.
        db.put(collection, "d00014", &json!({"body": ""})).unwrap();
        db.commit().unwrap();
    };
    mutate(&mut packed_db, docs);
    mutate(&mut head_db, head_docs);

    for (text, matching) in QUERIES {
        let got = ask(&packed_db, packed_index, text, matching, 2000);
        same(
            &got,
            &ask(&head_db, head_index, text, matching, 2000),
            &format!("after norm mutation {text:?}/{matching:?}"),
        );
        for gone in [12u64, 13] {
            assert!(
                !got.iter().any(|hit| hit.id.sequence == gone),
                "deleted document {gone} still answers {text:?}"
            );
        }
    }
    // The filtered path reads the norm FIRST and skips a document it cannot
    // find one for; a deleted document must not come back through it.
    let ids: Vec<EntityId> = (11..=16)
        .map(|sequence| EntityId {
            collection: CollectionId(1),
            sequence,
        })
        .collect();
    for (text, matching) in QUERIES {
        same(
            &ask_filtered(&packed_db, packed_index, text, matching, &ids),
            &ask_filtered(&head_db, head_index, text, matching, &ids),
            &format!("filtered after norm mutation {text:?}/{matching:?}"),
        );
    }

    packed_db.checkpoint().unwrap();
    drop(packed_db);

    // Two tombstones, both EMPTY, and the empty document's own row is a real
    // four-byte zero length: the two shapes are not the same bytes.
    let rows = norm_rows(&packed_path, packed_index);
    let tombstones = rows.iter().filter(|(_, value)| value.is_empty()).count();
    assert_eq!(tombstones, 2, "expected one tombstone per deleted document");
    assert!(
        rows.iter()
            .any(|(_, value)| value.as_slice() == 0u32.to_be_bytes()),
        "the empty document lost its present-with-zero-tokens norm"
    );

    let reopened = Database::open(&packed_path, cfg()).unwrap();
    for (text, matching) in QUERIES {
        same(
            &ask(&reopened, packed_index, text, matching, 2000),
            &ask(&head_db, head_index, text, matching, 2000),
            &format!("after reopen {text:?}/{matching:?}"),
        );
    }
    drop(reopened);

    // Corpus counters are reconstructed from both tiers, so a miscounted
    // tombstone shows up here.
    let report = verify_indexed_source(&packed_path, VerificationLimits::default(), |issue| {
        panic!("verification issue after norm mutation: {issue:?}");
    })
    .unwrap();
    assert!(report.complete && report.clean);
}

/// A block is 256 document lengths behind one checksum, so the verifier has to
/// look inside it rather than trust that it decoded.
#[test]
fn the_verifier_sees_inside_a_norm_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("packed");
    let (db, _, index) = packed(&path);
    drop(db);

    for damage in 0..4u8 {
        let broken = dir.path().join(format!("norm-broken-{damage}"));
        copy_dir(&path, &broken);
        let (key, value) = text_entries(&broken, index)
            .into_iter()
            .filter(|(key, _)| key[0] == NORM_BLOCK_TAG)
            .max_by_key(|(_, value)| value.len())
            .unwrap();
        let mut damaged = value.clone();
        match damage {
            // A length that is no longer the document's token count. Two
            // lengths are changed so a sampled entry is certainly hit.
            0 => {
                let last = damaged.len() - 1;
                damaged[33] = damaged[33].wrapping_add(7);
                damaged[last] = damaged[last].wrapping_add(7);
            }
            // A cleared presence bit: one varint nothing claims.
            1 => damaged[1] &= !0x02,
            // A truncated tail.
            2 => damaged.truncate(damaged.len() - 1),
            // A wholesale rewrite: every length doubled. The corpus token
            // counter is reconstructed from every entry, so this cannot hide
            // between the samples.
            3 => {
                for byte in damaged[1 + 32..].iter_mut() {
                    *byte = byte.saturating_mul(2).max(1) & 0x7f;
                }
            }
            _ => unreachable!(),
        }
        assert_ne!(damaged, value);
        let mut raw = PageWalStore::open(&broken, false, 1 << 20).unwrap();
        raw.put(&key, &damaged).unwrap();
        raw.commit().unwrap();
        drop(raw);

        let mut issues = 0usize;
        let report =
            verify_indexed_source(&broken, VerificationLimits::default(), |_| issues += 1).unwrap();
        assert!(
            issues > 0 && !report.clean,
            "norm block damage {damage} went unreported"
        );
    }
}
