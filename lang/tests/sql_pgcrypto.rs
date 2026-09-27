//! pgcrypto-compatible functions (`lang/src/pgcrypto.rs`), each answer
//! compared with what PostgreSQL 16 with the pgcrypto extension returns for
//! the same call -- the expected strings below were produced by it.
//!
//! What is at risk, one test each:
//!
//! * the six digests and two HMACs, and encode/decode, byte for byte
//!   (`digest_hmac_encode_and_decode_answer_as_pgcrypto`);
//! * crypt with bcrypt and md5-crypt salts, and a stored hash verifying the
//!   password it was made from and no other
//!   (`crypt_answers_as_pgcrypto_and_verifies`);
//! * gen_salt and gen_random_bytes: the shape and the randomness
//!   (`gen_salt_and_gen_random_bytes_are_well_formed_and_random`);
//! * a password stored at INSERT and checked at login, the account-table
//!   pattern (`a_password_is_stored_hashed_and_checked`);
//! * the errors PostgreSQL raises, with its SQLSTATEs, and DES refused by name
//!   (`errors_carry_postgresqls_sqlstates`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{Param, SqlDatabase, SqlError, SqlResult, SqlValue};
use tempfile::TempDir;

fn db(dir: &TempDir) -> Database {
    Database::create(
        dir.path().join("crypto.sekejap"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap()
}

fn value(db: &mut Database, sql: &str, params: &[Param]) -> SqlValue {
    match db.sql(sql, params).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { mut rows, .. } => rows.remove(0).values.remove(0),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn text(db: &mut Database, sql: &str) -> String {
    match value(db, sql, &[]) {
        SqlValue::Text(t) => t,
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn sqlstate(result: Result<SqlResult, SqlError>) -> &'static str {
    match result {
        Err(SqlError::Coded { sqlstate, .. }) => sqlstate,
        other => panic!("a coded error, not {other:?}"),
    }
}

#[test]
fn digest_hmac_encode_and_decode_answer_as_pgcrypto() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    for (kind, hex) in [
        ("md5", "900150983cd24fb0d6963f7d28e17f72"),
        ("sha1", "a9993e364706816aba3e25717850c26c9cd0d89d"),
        ("sha224", "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"),
        ("sha256", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        ("sha384", "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"),
        ("sha512", "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"),
    ] {
        assert_eq!(text(&mut db, &format!("SELECT encode(digest('abc', '{kind}'), 'hex')")), hex, "{kind}");
    }
    // A bytea result prints as PostgreSQL prints one, and a bytea argument is
    // read as its bytes.
    assert_eq!(text(&mut db, "SELECT digest('abc', 'md5')"), "\\x900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(
        text(&mut db, "SELECT encode(digest(decode('00ff', 'hex'), 'sha256'), 'hex')"),
        "06eb7d6a69ee19e5fbdf749018d3d2abfa04bcbd1365db312eb86dc7169389b8"
    );
    assert_eq!(
        text(&mut db, "SELECT encode(hmac('what do ya want for nothing?', 'Jefe', 'md5'), 'hex')"),
        "750c783e6ab0b503eaa86e310a5db738"
    );
    assert_eq!(
        text(&mut db, "SELECT encode(hmac('what do ya want for nothing?', 'Jefe', 'sha256'), 'hex')"),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    assert_eq!(text(&mut db, "SELECT encode('hello world', 'base64')"), "aGVsbG8gd29ybGQ=");
    assert_eq!(text(&mut db, "SELECT decode('00ff10', 'hex')"), "\\x00ff10");
    assert_eq!(
        text(&mut db, "SELECT encode(decode('aGVsbG8gd29ybGQ=', 'base64'), 'hex')"),
        "68656c6c6f20776f726c64"
    );
}

#[test]
fn crypt_answers_as_pgcrypto_and_verifies() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    assert_eq!(
        text(&mut db, "SELECT crypt('U*U', '$2a$05$CCCCCCCCCCCCCCCCCCCCC.')"),
        "$2a$05$CCCCCCCCCCCCCCCCCCCCC.E5YPO9kmyuRGyh0XouQYb4YMJKvyOeW"
    );
    assert_eq!(
        text(&mut db, "SELECT crypt('secret', '$2a$06$abcdefghijklmnopqrstuu')"),
        "$2a$06$abcdefghijklmnopqrstuuxLa0AkDDSrQ9VwNnETzOsObiucpMYgC"
    );
    assert_eq!(
        text(&mut db, "SELECT crypt('password', '$1$saltsalt')"),
        "$1$saltsalt$qjXMvbEw8oaL.CzflDtaK/"
    );
    assert_eq!(
        text(&mut db, "SELECT crypt('secret', '$1$abcdefgh$')"),
        "$1$abcdefgh$cHJi5PXp/ki/ktXzqlk6I1"
    );
    // The stored hash is its own salt: crypt(input, hash) = hash exactly when
    // the input is the password.
    let hash = "$2a$06$abcdefghijklmnopqrstuuxLa0AkDDSrQ9VwNnETzOsObiucpMYgC";
    for (input, ok) in [("secret", true), ("Secret", false), ("", false)] {
        let got = value(
            &mut db,
            "SELECT crypt($1, $2) = $2 AS ok",
            &[Param::Text(input.into()), Param::Text(hash.into())],
        );
        assert_eq!(got, SqlValue::Bool(ok), "{input:?}");
    }
}

#[test]
fn gen_salt_and_gen_random_bytes_are_well_formed_and_random() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    let a = text(&mut db, "SELECT gen_salt('bf')");
    let b = text(&mut db, "SELECT gen_salt('bf')");
    assert!(a.starts_with("$2a$06$") && a.len() == 29, "{a}");
    assert_ne!(a, b, "two salts differ");
    assert!(text(&mut db, "SELECT gen_salt('bf', 10)").starts_with("$2a$10$"));
    let md5 = text(&mut db, "SELECT gen_salt('md5')");
    assert!(md5.starts_with("$1$") && md5.len() == 11, "{md5}");
    let hash = text(&mut db, "SELECT crypt('secret', gen_salt('bf', 4))");
    assert!(hash.starts_with("$2a$04$") && hash.len() == 60, "{hash}");
    let r = text(&mut db, "SELECT gen_random_bytes(16)");
    assert!(r.starts_with("\\x") && r.len() == 2 + 32, "{r}");
    assert_ne!(r, text(&mut db, "SELECT gen_random_bytes(16)"));
}

#[test]
fn a_password_is_stored_hashed_and_checked() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    db.sql("CREATE TABLE account (_key TEXT PRIMARY KEY, email TEXT UNIQUE, password_hash TEXT)", &[])
        .unwrap();
    db.sql(
        "INSERT INTO account (_key, email, password_hash) VALUES ($1, $2, crypt($3, gen_salt('bf', 4)))",
        &[Param::Text("u1".into()), Param::Text("ayu@example.com".into()), Param::Text("correct horse".into())],
    )
    .unwrap();
    db.sql("COMMIT", &[]).unwrap();
    let stored = match value(&mut db, "SELECT password_hash FROM account WHERE email = 'ayu@example.com'", &[]) {
        SqlValue::Text(t) => t,
        other => panic!("{other:?}"),
    };
    assert!(stored.starts_with("$2a$04$"), "the password is not stored as written: {stored}");
    for (attempt, ok) in [("correct horse", true), ("wrong horse", false)] {
        let got = value(
            &mut db,
            "SELECT crypt($1, $2) = $2 AS ok",
            &[Param::Text(attempt.into()), Param::Text(stored.clone())],
        );
        assert_eq!(got, SqlValue::Bool(ok), "{attempt}");
    }
    // UPDATE SET takes the same call: a password change.
    db.sql(
        "UPDATE account SET password_hash = crypt($1, gen_salt('bf', 4)) WHERE _key = 'u1'",
        &[Param::Text("battery staple".into())],
    )
    .unwrap();
    let changed = match value(&mut db, "SELECT password_hash FROM account WHERE _key = 'u1'", &[]) {
        SqlValue::Text(t) => t,
        other => panic!("{other:?}"),
    };
    assert_ne!(changed, stored);
    let got = value(
        &mut db,
        "SELECT crypt($1, $2) = $2 AS ok",
        &[Param::Text("battery staple".into()), Param::Text(changed)],
    );
    assert_eq!(got, SqlValue::Bool(true));
}

#[test]
fn errors_carry_postgresqls_sqlstates() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    assert_eq!(sqlstate(db.sql("SELECT gen_salt('bf', 3)", &[])), "22023");
    assert_eq!(sqlstate(db.sql("SELECT gen_salt('nope')", &[])), "22023");
    assert_eq!(sqlstate(db.sql("SELECT gen_random_bytes(2000)", &[])), "39000");
    assert_eq!(sqlstate(db.sql("SELECT digest('a', 'sha3')", &[])), "22023");
    assert_eq!(sqlstate(db.sql("SELECT decode('zz', 'hex')", &[])), "22023");
    // DES is refused by name, where PostgreSQL would fall back to it.
    assert_eq!(sqlstate(db.sql("SELECT crypt('a', 'xx')", &[])), "0A000");
    assert_eq!(sqlstate(db.sql("SELECT gen_salt('des')", &[])), "0A000");
    // NULL in, NULL out: every pgcrypto function is STRICT.
    assert_eq!(value(&mut db, "SELECT digest(NULL, 'md5')", &[]), SqlValue::Null);
}
