//! A text-format `bool` parameter is read as PostgreSQL's `boolin` reads it
//! (finding `vuln-a15`, 2026-09-29).
//!
//! The decoder used to accept seven exact spellings of true and read every
//! other text -- `True`, `YES`, ` on `, even `maybe` -- as false, so a true
//! value could be stored as false with no error.
//!
//! What is at risk, one test:
//!
//! * every spelling PostgreSQL accepts, in any case and with surrounding
//!   whitespace, decodes to its value, and anything else is refused
//!   (`bool_text_reads_as_postgresql_reads_it`).

use sekejap_dist::pg::types::{self, oid};
use sekejap_lang::Param;

#[test]
fn bool_text_reads_as_postgresql_reads_it() {
    let decode = |text: &str| types::decode_param(Some(text.as_bytes()), oid::BOOL, 0);
    for (text, want) in [
        ("t", true),
        ("true", true),
        ("True", true),
        ("TRUE", true),
        ("tru", true),
        ("y", true),
        ("YES", true),
        ("on", true),
        ("ON", true),
        ("1", true),
        ("  true  ", true),
        ("f", false),
        ("false", false),
        ("FALSE", false),
        ("fal", false),
        ("n", false),
        ("No", false),
        ("off", false),
        ("of", false),
        ("0", false),
        (" 0 ", false),
    ] {
        match decode(text) {
            Ok(Param::Bool(got)) => assert_eq!(got, want, "`{text}`"),
            other => panic!("`{text}`: {other:?}"),
        }
    }
    for text in ["maybe", "", "o", "tr ue", "10", "yess", "nope", "2"] {
        assert!(decode(text).is_err(), "`{text}` is not a boolean: {:?}", decode(text));
    }
}
