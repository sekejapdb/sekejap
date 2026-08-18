//! SQL robustness + safety: the parser must never PANIC on adversarial input
//! (only return Err), the resource caps must REJECT pathological queries, and —
//! crucially — must NOT false-reject legitimate ones.

use sekejap::{Config, CoreDB};

fn db_with_rows(n: usize) -> (CoreDB, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = CoreDB::open_with_config(dir.path(), Config::default()).unwrap();
    for i in 0..n {
        db.put(&format!("t/k{i}"), &format!(r#"{{"_collection":"t","_key":"k{i}","v":{i}}}"#)).unwrap();
    }
    (db, dir)
}

fn xorshift(s: &mut u64) -> u64 {
    let mut x = *s;
    x ^= x << 13; x ^= x >> 7; x ^= x << 17; *s = x; x
}

/// Feed a large volume of random SQL token-soup at query()/execute(). Success =
/// the process does NOT panic (Ok or Err are both acceptable outcomes).
#[test]
fn parser_never_panics_on_random_sql() {
    let (mut db, _dir) = db_with_rows(5);
    const TOKENS: &[&str] = &[
        "SELECT","*","FROM","t","WHERE","MATCH","(",")","[","]","-",">","<","=","!=","<=",">=",
        "AND","OR","NOT","IN","1",",","2","3","VECTOR_NEAR","emb","0.5","..","*","1..9",
        "CASE","WHEN","THEN","ELSE","END","GROUP","BY","ORDER","ASC","DESC","LIMIT","OFFSET",
        "INSERT","INTO","VALUES","'x'","$1","$99","ILIKE","LIKE","'%a%'","999999999","-5",
        "SUBSTRING","f","COUNT","SUM","ST_DWithin","POINT","geometry","UNION","DELETE","UPDATE","SET",
        "COLLECT","_key","v","0.0","AS","k","->","<-","BETWEEN","DISTINCT",";","::","{","}",
    ];
    let mut s = 0x1234_9abc_def0_1111u64;
    for _ in 0..60_000 {
        let n = (xorshift(&mut s) % 22) as usize;
        let mut q = String::with_capacity(64);
        for _ in 0..n {
            q.push_str(TOKENS[(xorshift(&mut s) as usize) % TOKENS.len()]);
            q.push(' ');
        }
        let _ = db.query(&q);   // must not panic
        let _ = db.execute(&q); // must not panic
    }
}

/// Every resource cap must reject its pathological query with an Err (no OOM,
/// no stack overflow, no hang).
#[test]
fn guardrails_reject_pathological_queries() {
    let (db, _dir) = db_with_rows(3);

    // Parser recursion: deeply nested parens must be rejected up front.
    let nested = format!("SELECT * FROM t WHERE {}v = 1{}", "(".repeat(500), ")".repeat(500));
    assert!(db.query(&nested).is_err(), "deeply nested query must be rejected");

    // Graph traversal: unbounded hop depth.
    assert!(
        db.query("SELECT b._key AS k FROM MATCH (a:t)-[r*1..999999999]->(b:t) WHERE a._key = 'k0'").is_err(),
        "huge hop depth must be rejected"
    );
    // Graph traversal: inverted range.
    assert!(
        db.query("SELECT b._key AS k FROM MATCH (a:t)-[r*100..10]->(b:t) WHERE a._key = 'k0'").is_err(),
        "min > max hop range must be rejected"
    );
    // Vector KNN: absurd k (rejected at parse, before touching any index).
    assert!(
        db.query("SELECT _key FROM t WHERE VECTOR_NEAR(emb, [1.0, 2.0], 999999999)").is_err(),
        "huge VECTOR_NEAR k must be rejected"
    );
    // SUBSTRING: length beyond the cap.
    assert!(
        db.query("SELECT SUBSTRING(v, 0, 999999999999) AS s FROM t").is_err(),
        "huge SUBSTRING length must be rejected"
    );
}

/// The caps must NOT reject ordinary queries — no false positives.
#[test]
fn guardrails_allow_normal_queries() {
    let (db, _dir) = db_with_rows(50);
    assert!(db.query("SELECT * FROM t WHERE v IN (1, 2, 3, 4, 5)").unwrap().collect().len() == 5);
    assert!(db.query("SELECT * FROM t WHERE ((v = 1) OR (v = 2))").is_ok(), "moderate nesting is fine");
    assert!(db.query("SELECT b._key AS k FROM MATCH (a:t)-[r*1..3]->(b:t) WHERE a._key = 'k0'").is_ok(), "reasonable hop range is fine");
    assert!(db.query("SELECT COUNT(*) AS n FROM t").is_ok());
    assert!(db.query("SELECT * FROM t WHERE v NOT IN (0, 1)").unwrap().collect().len() == 48);
}

/// **A query string may not abort the process.**
///
/// `max_nesting` bounds parentheses before parsing, and its comment states the
/// assumption that broke: "the recursion consumes an opening token per level".
/// So does `NOT`, and so does unary `-`, and neither is a parenthesis. A `WHERE`
/// with 200 000 leading `NOT`s walked straight past the guard and overflowed the
/// stack — and a Rust stack overflow is `SIGABRT`, not an error: not catchable,
/// not recoverable, the process is simply gone.
///
/// Verified before the fix by running each case in its own process: the paren
/// case exited 0 with a refusal, the other two exited 134 with
/// "fatal runtime error: stack overflow, aborting". Reachable by anyone able to
/// send SQL, which includes every client of the PostgreSQL wire server.
///
/// The bound is now on recursion frames rather than on tokens, so it holds for
/// paths nobody thought to pre-scan for.
#[test]
fn deeply_nested_expressions_are_refused_not_fatal() {
    let (mut db, _dir) = db_with_rows(3);

    for (what, sql) in [
        ("parentheses", format!("SELECT _key FROM t WHERE {}v = 1{}",
                                "(".repeat(5_000), ")".repeat(5_000))),
        ("NOT prefixes", format!("SELECT _key FROM t WHERE {} v = 1", "NOT ".repeat(5_000))),
        ("unary minus",  format!("SELECT _key FROM t ORDER BY {} 1", "- ".repeat(5_000))),
        ("NOT in HAVING", format!("SELECT COUNT(*) FROM t GROUP BY v HAVING {} COUNT(*) > 0",
                                  "NOT ".repeat(5_000))),
    ] {
        // The assertion is that we get here at all: an abort would take the test
        // binary with it. Either answer is acceptable, a dead process is not.
        let refused = db.query(&sql).is_err();
        assert!(refused, "`{what}` was accepted; it should be refused as too deep");
    }

    // …and the depth counter must not reject ordinary queries. It is restored on
    // the way out of every frame, including the erroring ones above, so these run
    // after the refusals on purpose.
    for sql in [
        "SELECT _key FROM t WHERE v = 1",
        "SELECT _key FROM t WHERE NOT v = 1",
        "SELECT _key FROM t WHERE NOT (v = 1 AND v = 2)",
        "SELECT _key FROM t WHERE ((v = 1))",
        "SELECT _key FROM t ORDER BY v DESC LIMIT 2",
        "SELECT COUNT(*) FROM t",
    ] {
        assert!(db.query(sql).is_ok(), "`{sql}` is ordinary SQL and was refused");
    }
}
