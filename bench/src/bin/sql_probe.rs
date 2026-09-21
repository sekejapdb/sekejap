//! `sql_probe <db-dir> <sql>`: EXPLAIN one statement against an existing E4
//! database and print the plan and the work the run charged. A diagnostic
//! for the `e4-sql` arm: when its median differs from the `e4` arm's, this is
//! how to see whether the plan differs or only the parse.
use sekejap_lang::SqlDatabase;
use sekejap_core::collections::Database;
use kernel::{io::IoMode, store::{Config, SyncMode}};
fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("db dir");
    let sql = args.next().expect("sql");
    let db = Database::open(
        &dir,
        Config { budget_bytes: 8 << 20, io: IoMode::Buffered, sync: SyncMode::Full },
    )
    .expect("open");
    // Optional: a comma-separated list of float parameters bound as $1.. so
    // the parameterised form the bench arm emits can be timed too.
    let params: Vec<sekejap_lang::Param> = std::env::var("SQL_PROBE_PARAMS")
        .ok()
        .map(|v| v.split(',').map(|f| sekejap_lang::Param::Float(f.parse().expect("float param"))).collect())
        .unwrap_or_default();
    if let Some(n) = args.next().and_then(|n| n.parse::<usize>().ok()) {
        // `--bench`: prepare once, run N times, print median and p90 of the
        // run alone and of prepare alone.
        use sekejap_core::collections::QueryBudget;
        use std::time::Instant;
        let mut runs = Vec::with_capacity(n);
        let mut preps = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            let prepared = sekejap_lang::prepare_sql(&db, &sql, &params).expect("prepare");
            preps.push(t.elapsed().as_secs_f64() * 1e6);
            let t = Instant::now();
            let mut rows = 0usize;
            prepared
                .with_query(&db, &mut |q| {
                    loop {
                        let page = q.next_page(8192, QueryBudget::unlimited(), || false)?;
                        rows += page.rows.len();
                        if page.done || page.rows.is_empty() {
                            break;
                        }
                    }
                    Ok(())
                })
                .expect("run");
            runs.push(t.elapsed().as_secs_f64() * 1e6);
            let _ = rows;
        }
        runs.sort_by(|a, b| a.total_cmp(b));
        preps.sort_by(|a, b| a.total_cmp(b));
        println!(
            "n={n} prepare median {:.1} us | run median {:.1} us p90 {:.1} us min {:.1} us",
            preps[n / 2], runs[n / 2], runs[n * 9 / 10], runs[0]
        );
        return;
    }
    match db.sql_explain(&sql, &params) {
        Ok(text) => println!("{text}"),
        Err(e) => println!("error: {e}"),
    }
}
