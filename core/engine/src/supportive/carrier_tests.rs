//! The carrier's byte formats (`docs/core/SUPPORTIVE.md` 2.0). What is at
//! risk, one test each:
//!
//! * a kind's class is its first letter's case, and only well-formed codes
//!   exist (`a_kinds_class_is_its_first_letters_case`);
//! * keys round-trip in every item grammar and sort node first, then owner,
//!   then kind, with ids in numeric order (`keys_round_trip_and_sort_node_first`);
//! * the checksum covers the key, so a byte moved between key and value, a
//!   flipped byte or a lying length is caught -- the length before any
//!   allocation (`the_value_checksum_catches_a_shifted_or_damaged_value`);
//! * the writer refuses a payload past the limit, and the limit itself fits
//!   (`the_writer_refuses_a_payload_past_the_limit`);
//! * the Anchor round-trips within its 2067 bytes at 220 census lines, and
//!   refuses a 221st line, an unsorted or repeated census, wrong tree roots or
//!   a non-zero reserved byte (`the_anchor_holds_its_census_and_nothing_else`);
//! * admission refuses an unknown critical kind, version or variant by name
//!   and skips an unknown ignorable one
//!   (`admission_refuses_unknown_critical_and_skips_unknown_ignorable`).

use super::carrier::*;
use crate::collections::Error;

fn kind(code: &[u8; 4]) -> Kind {
    Kind::new(code).unwrap()
}

#[test]
fn a_kinds_class_is_its_first_letters_case() {
    assert!(kind(b"COLM").critical());
    assert!(!kind(b"rCNT").critical());
    assert_eq!(kind(b"nIDX").code(), "nIDX");
    for bad in [b"Colm", b"cOLm", b"COL1", b"CO M", b"\xffCOL"] {
        assert!(Kind::new(bad).is_err(), "{bad:?} is not a kind");
    }
}

#[test]
fn keys_round_trip_and_sort_node_first() {
    let keys = [
        Key { node: Node::B, owner_class: OwnerClass::Database, owner_id: 0, kind: kind(b"NEXT"), item: Item::Id(7) },
        Key { node: Node::B, owner_class: OwnerClass::Table, owner_id: 3, kind: kind(b"nIDX"), item: Item::Name { class: 2, name: b"note".to_vec() } },
        Key { node: Node::C, owner_class: OwnerClass::Table, owner_id: 2, kind: kind(b"COLM"), item: Item::Id(7) },
        Key { node: Node::C, owner_class: OwnerClass::Table, owner_id: 10, kind: kind(b"LAYT"), item: Item::Part { id: 5, part: 1 } },
        Key { node: Node::G, owner_class: OwnerClass::Index, owner_id: 1, kind: kind(b"rCNT"), item: Item::Id(0) },
    ];
    let mut encoded = Vec::new();
    for key in &keys {
        let bytes = key.encode().unwrap();
        assert_eq!(&Key::decode(&bytes).unwrap(), key, "{key:?}");
        encoded.push(bytes);
    }
    let mut sorted = encoded.clone();
    sorted.sort();
    assert_eq!(sorted, encoded, "node first, then owner class and id (numeric), kind, item");
    // A kind this build does not know keeps its item as opaque bytes.
    let unknown = Key { node: Node::C, owner_class: OwnerClass::Table, owner_id: 2, kind: kind(b"ZZZZ"), item: Item::Opaque(vec![1, 2, 3]) };
    assert_eq!(Key::decode(&unknown.encode().unwrap()).unwrap(), unknown);
    // A name past 255 bytes is refused.
    let long = Key { item: Item::Name { class: 2, name: vec![b'x'; MAX_NAME + 1] }, ..keys[1].clone() };
    assert!(long.encode().is_err());
}

#[test]
fn the_value_checksum_catches_a_shifted_or_damaged_value() {
    let key = Key { node: Node::C, owner_class: OwnerClass::Table, owner_id: 2, kind: kind(b"COLM"), item: Item::Id(7) }
        .encode()
        .unwrap();
    let value = encode_value(&key, 1, b"payload").unwrap();
    assert_eq!(decode_value(&key, &value).unwrap(), (1, &b"payload"[..]));
    // The same bytes read under a key one byte shorter or longer.
    let mut longer = key.clone();
    longer.push(value[0]);
    assert!(matches!(decode_value(&longer, &value[1..]), Err(Error::Corrupt(_))));
    assert!(matches!(decode_value(&key[..key.len() - 1], &value), Err(Error::Corrupt(_))));
    // A flipped payload byte.
    let mut flipped = value.clone();
    flipped[6] ^= 1;
    assert!(matches!(decode_value(&key, &flipped), Err(Error::Corrupt(_))));
    // A length that claims 4 GiB is refused before anything is allocated.
    let mut lying = value.clone();
    lying[1..5].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(decode_value(&key, &lying), Err(Error::Corrupt(_))));
}

#[test]
fn the_writer_refuses_a_payload_past_the_limit() {
    let key = [b'c', 2, 0x81, 1];
    let full = encode_value(&key, 1, &vec![0; MAX_PAYLOAD]).unwrap();
    assert_eq!(full.len(), MAX_VALUE);
    assert!(encode_value(&key, 1, &vec![0; MAX_PAYLOAD + 1]).is_err());
}

#[test]
fn the_anchor_holds_its_census_and_nothing_else() {
    let lines: Vec<CensusLine> = (0..MAX_CENSUS as u32)
        .map(|i| CensusLine { kind: kind(b"INDX"), version: 1, variant: i })
        .collect();
    let anchor = Anchor { roots: [10, 11, 12], census: lines.clone() };
    let payload = anchor.encode().unwrap();
    assert!(payload.len() <= ANCHOR_PAYLOAD, "{}", payload.len());
    assert_eq!(Anchor::decode(&payload).unwrap(), anchor);
    // A 221st line does not fit the promise.
    let mut over = lines.clone();
    over.push(CensusLine { kind: kind(b"JOBS"), version: 1, variant: 0 });
    assert!(Anchor { roots: [1, 2, 3], census: over }.encode().is_err());
    // Unsorted or repeated.
    let mut unsorted = lines[..3].to_vec();
    unsorted.swap(0, 2);
    assert!(Anchor { roots: [1, 2, 3], census: unsorted }.encode().is_err());
    let repeated = vec![lines[0], lines[0]];
    assert!(Anchor { roots: [1, 2, 3], census: repeated }.encode().is_err());
    // A non-zero reserved byte is damage.
    let small = Anchor { roots: [1, 2, 3], census: lines[..2].to_vec() }.encode().unwrap();
    let mut padded = small.clone();
    padded.push(0);
    assert!(Anchor::decode(&padded).is_ok(), "zero reserved bytes are fine");
    padded.push(7);
    assert!(matches!(Anchor::decode(&padded), Err(Error::Corrupt(_))));
    // The roots name the fixed Register trees; a different tree id is damage.
    let mut wrong = small.clone();
    wrong[3..5].copy_from_slice(&0x0042u16.to_be_bytes());
    assert!(matches!(Anchor::decode(&wrong), Err(Error::Corrupt(_))));
}

#[test]
fn admission_refuses_unknown_critical_and_skips_unknown_ignorable() {
    let supported = |l: &CensusLine| l.kind == kind(b"COLM") && l.version == 1 && l.variant == 0;
    let known = CensusLine { kind: kind(b"COLM"), version: 1, variant: 0 };
    assert!(admit(&[known], &supported).unwrap().is_empty());
    for (line, says) in [
        (CensusLine { kind: kind(b"COLM"), version: 2, variant: 0 }, "COLM version 2"),
        (CensusLine { kind: kind(b"COLM"), version: 1, variant: 5 }, "variant 5"),
        (CensusLine { kind: kind(b"CAST"), version: 1, variant: 0 }, "CAST"),
    ] {
        match admit(&[known, line], &supported) {
            Err(Error::Unsupported(m)) => {
                assert!(m.contains("needs a newer sekejap") && m.contains(says), "{m}")
            }
            other => panic!("{line:?}: {other:?}"),
        }
    }
    let stat = CensusLine { kind: kind(b"rCNT"), version: 9, variant: 0 };
    assert_eq!(admit(&[known, stat], &supported).unwrap(), vec![stat]);
}
