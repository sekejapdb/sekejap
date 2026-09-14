#![cfg(feature = "compact-cells")]
use kernel::{page::PageKind, verify::{decode_record, DecodedRecord}};

#[test]
fn compact_ordinary_key_is_slot_bounded_and_distinct_from_overflow() {
    for len in [0usize, 1, 8, 255, 256, 2048] {
        let key = vec![0xa5; len];
        let value = [0, 255, 0x40, 0x81, 17];
        let mut record = (0x4000 | len as u16).to_le_bytes().to_vec();
        record.extend(&key); record.extend(value);
        let DecodedRecord::Leaf { key: k, value: v, overflow } =
            decode_record(&record, 2, PageKind::Leaf).unwrap() else { panic!() };
        assert_eq!(k, key); assert_eq!(v, value); assert!(!overflow);
        for cut in 0..2 + len {
            assert!(decode_record(&record[..cut], 2, PageKind::Leaf).is_err());
        }
        assert!(decode_record(&record, 2, PageKind::Interior).is_err());
    }
    let legacy = [1, 0, b'k', 2, 0, 7, 8];
    let DecodedRecord::Leaf { key, value, overflow } =
        decode_record(&legacy, 2, PageKind::Leaf).unwrap() else { panic!() };
    assert_eq!(key, b"k"); assert_eq!(value, [7, 8]); assert!(!overflow);
    let mut marker = vec![1, 0, b'k', 255, 255]; marker.extend([0u8; 12]);
    let DecodedRecord::Leaf { overflow, .. } =
        decode_record(&marker, 2, PageKind::Leaf).unwrap() else { panic!() };
    assert!(overflow);
}
