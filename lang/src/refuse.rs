//! The Tier-2 and Tier-3 table of `docs/lang/QL_CONTRACT.md`, as data.
//!
//! One row per construct the lexer can recognise but this slice does not
//! compile. The reason text is the contract's own: for Tier 2 the atomic that
//! is not built yet, for Tier 3 the atomic that does not exist. Nothing here
//! is emulated -- a construct in this table ends its statement with
//! [`SqlError::Refused`](super::SqlError::Refused), which is the eighth law's
//! "refused with a named reason, never emulated".

use super::{SqlError, Tier};

/// keyword (upper case, as the parser spells it) -> tier -> reason.
///
/// The keyword is what a caller reads back out of the error, so it is written
/// the way the statement writes it (`GROUP BY`, not `GROUP`), and the lookup
/// below tries the two-word forms before the one-word ones.
pub(crate) const TABLE: &[(&str, Tier, &str)] = &[
    // ── §3 predicates and operators ──────────────────────────────────────
    // OR, IN, NOT, `<>`, IS NOT NULL and EXISTS moved from this table to
    // Tier 1 with the membership-set algebra in `src/query/membership.rs`:
    // the union, the intersection and the complement they named exist now,
    // so they are ACCEPTED rather than refused. A boolean LEAF an index
    // cannot answer -- a geometry predicate, a traversal, a JSON equality, a
    // phrase -- is still refused, by `prepare_query`, with that reason.
    ("LIKE", Tier::Two, "QL_CONTRACT §3: LIKE 'abc%' is a text-key prefix range; LIKE '%abc%' needs the trigram index family (pg_trgm-compatible) under a new feature bit. Neither is built."),
    ("ILIKE", Tier::Two, "QL_CONTRACT §3: ILIKE needs the trigram index family (pg_trgm-compatible), a new family under a feature bit."),
    ("SIMILAR TO", Tier::Three, "QL_CONTRACT §3: SIMILAR TO has no index atomic."),
    ("~", Tier::Three, "QL_CONTRACT §3: regex `~` has no index atomic."),
    ("&&", Tier::Two, "QL_CONTRACT §4.4: `&&` with ST_MakeEnvelope is a Bbox filter on the point or geometry index (p3-geometry-io). Not built in this slice; ST_Within against an envelope is the Tier-1 spelling of the same rectangle."),
    ("@>", Tier::Two, "QL_CONTRACT §3: array containment has no Tier-1 atomic in this slice."),
    ("->>", Tier::Two, "QL_CONTRACT §4.1: JSON path extraction is a row function (p3 item 2)."),
    // ── §2 statements ────────────────────────────────────────────────────
    ("JOIN", Tier::Two, "QL_CONTRACT §4.8: INNER/LEFT JOIN on key equality is a key lookup per driving row, scheduled after GROUP BY; a join on a non-key column and FULL OUTER JOIN are Tier 3 (they need a hash join with spill). A pattern is never compiled to a join."),
    ("OFFSET", Tier::Two, "QL_CONTRACT §5 deviation 4: OFFSET is a keyset continuation, never a skip count. The continuation is the prepared query's own next page; a skip count would read and discard rows, which is work proportional to the skip."),
    ("UNION", Tier::Three, "QL_CONTRACT §2: UNION has no atomic; recursion is a GRAPH_TABLE pattern."),
    ("INTERSECT", Tier::Three, "QL_CONTRACT §2: no atomic."),
    ("EXCEPT", Tier::Three, "QL_CONTRACT §2: no atomic."),
    ("WITH", Tier::Two, "QL_CONTRACT §2: a non-recursive WITH is materialised once, bounded by QueryBudget rows; WITH RECURSIVE is Tier 3 (recursion is a GRAPH_TABLE pattern)."),
    ("OVER", Tier::Three, "QL_CONTRACT §2 and §4.7: window functions have no atomic."),
    ("CREATE VIEW", Tier::Three, "QL_CONTRACT §2: a user view has no atomic."),
    ("CREATE SCHEMA", Tier::Two, "QL_CONTRACT §2: CREATE SCHEMA and schema.table are p2-schema-segment."),
    ("CREATE PROPERTY GRAPH", Tier::Two, "QL_CONTRACT §2: CREATE PROPERTY GRAPH optionally names a context and a label map; nothing is built. Not in this slice."),
    ("CREATE TRIGGER", Tier::Three, "QL_CONTRACT §2: triggers have no atomic."),
    ("DECLARE", Tier::Two, "QL_CONTRACT §2: DECLARE ... BINARY CURSOR / FETCH FORWARD / CLOSE are pages over prepare_query (p3-wire)."),
    ("FETCH", Tier::Two, "QL_CONTRACT §2: FETCH FORWARD n is a page over prepare_query (p3-wire)."),
    ("CLOSE", Tier::Two, "QL_CONTRACT §2: CLOSE ends a cursor declared by DECLARE (p3-wire)."),
    ("VALUES", Tier::Two, "QL_CONTRACT §2: a bare VALUES list as a query source has no atomic in this slice."),
    // ── §4.1 string functions ────────────────────────────────────────────
    // lower/upper/length/concat/substring/left/right/trim/split_part/replace/
    // position/starts_with and `||` moved from this table to Tier 1 with
    // `lang/src/functions.rs`: they are row functions over projected values,
    // and `lower(col) = x` / `LIKE 'x%'` / `starts_with` are index ranges.
    // What is left here is what still has no atomic.
    ("REGEXP_REPLACE", Tier::Three, "QL_CONTRACT §4.1: regexp_* has no atomic."),
    ("REGEXP_MATCH", Tier::Three, "QL_CONTRACT §4.1: regexp_* has no atomic."),
    ("COALESCE", Tier::Two, "QL_CONTRACT §4.1: a row function on projected values; a text index spans one declared field, so the concatenation Postgres builds with coalesce is a stored field here (battle50k deviation 1)."),
    // ── §4.2 date and time ───────────────────────────────────────────────
    // EXTRACT, date_trunc, now(), current_date, interval arithmetic, age(),
    // to_char, to_timestamp and to_date moved to Tier 1 with
    // `lang/src/functions.rs`: in a WHERE they fold into ONE scalar range, in
    // a SELECT they are row functions. The MULTI-range forms are refused by
    // `multi_range` below, which is not a keyword row.
    ("AT TIME ZONE", Tier::Three, "QL_CONTRACT §4.2 and §5 deviation 8: a declared TIMESTAMPTZ is stored as UTC microseconds in an Int; there is no time-zone storage, only display conversion."),
    // ── §4.3 graph ───────────────────────────────────────────────────────
    ("ANY SHORTEST", Tier::Two, "QL_CONTRACT §4.3: ANY SHORTEST / ALL SHORTEST need the unweighted shortest-path atomic."),
    ("ALL SHORTEST", Tier::Two, "QL_CONTRACT §4.3: ANY SHORTEST / ALL SHORTEST need the unweighted shortest-path atomic."),
    ("PATH_LENGTH", Tier::Two, "QL_CONTRACT §4.3: path_length() is a frontier depth accumulator."),
    ("PATH_SUM", Tier::Two, "QL_CONTRACT §4.3: path accumulators are GRAPH_CONTRACT 5.1."),
    ("PATH_PRODUCT", Tier::Two, "QL_CONTRACT §4.3: path accumulators are GRAPH_CONTRACT 5.1."),
    ("PATH_MIN", Tier::Two, "QL_CONTRACT §4.3: path accumulators are GRAPH_CONTRACT 5.1."),
    ("PATH_MAX", Tier::Two, "QL_CONTRACT §4.3: path accumulators are GRAPH_CONTRACT 5.1."),
    ("PATH_AVG", Tier::Two, "QL_CONTRACT §4.3: path accumulators are GRAPH_CONTRACT 5.1."),
    ("PATH_FIRST", Tier::Two, "QL_CONTRACT §4.3: path accumulators are GRAPH_CONTRACT 5.1."),
    ("PATH_LAST", Tier::Two, "QL_CONTRACT §4.3: path accumulators are GRAPH_CONTRACT 5.1."),
    ("NODES", Tier::Two, "QL_CONTRACT §4.3: nodes(p) needs the path rebuild for returned rows."),
    ("EDGES", Tier::Two, "QL_CONTRACT §4.3: edges(p) needs the path rebuild for returned rows."),
    ("VERTEX_ID", Tier::Two, "QL_CONTRACT §4.3: VERTEX_ID(v) is the entity id, after element identity."),
    ("EDGE_ID", Tier::Two, "QL_CONTRACT §4.3: EDGE_ID(e) is the edge id, after element identity."),
    ("TRAIL", Tier::Three, "QL_CONTRACT §4.3: IS ACYCLIC is the default and the only mode; TRAIL, WALK and SIMPLE have no atomic."),
    ("WALK", Tier::Three, "QL_CONTRACT §4.3: IS ACYCLIC is the default and the only mode; TRAIL, WALK and SIMPLE have no atomic."),
    ("SIMPLE", Tier::Three, "QL_CONTRACT §4.3: IS ACYCLIC is the default and the only mode; TRAIL, WALK and SIMPLE have no atomic."),
    // ── §4.4 spatial ─────────────────────────────────────────────────────
    ("ST_BUFFER", Tier::Three, "QL_CONTRACT §4.4: GEOS overlay; no pure-Rust substitute accepted."),
    ("ST_UNION", Tier::Three, "QL_CONTRACT §4.4: GEOS overlay; no pure-Rust substitute accepted."),
    ("ST_INTERSECTION", Tier::Three, "QL_CONTRACT §4.4: GEOS overlay; no pure-Rust substitute accepted."),
    ("ST_DIFFERENCE", Tier::Three, "QL_CONTRACT §4.4: GEOS overlay; no pure-Rust substitute accepted."),
    ("ST_SIMPLIFYPRESERVETOPOLOGY", Tier::Three, "QL_CONTRACT §4.4: GEOS overlay; no pure-Rust substitute accepted."),
    ("ST_TRANSFORM", Tier::Two, "QL_CONTRACT §4.4: ST_Transform needs PROJ; storage stays WGS84."),
    ("ST_ASMVT", Tier::Three, "QL_CONTRACT §4.4: raster, topology and ST_AsMVT have no atomic."),
    ("ST_ASTEXT", Tier::Two, "QL_CONTRACT §4.4: the geometry I/O functions are p3-geometry-io."),
    ("ST_ASBINARY", Tier::Two, "QL_CONTRACT §4.4: the geometry I/O functions are p3-geometry-io."),
    ("ST_GEOMFROMTEXT", Tier::Two, "QL_CONTRACT §4.4: the geometry I/O functions are p3-geometry-io."),
    ("ST_GEOMFROMWKB", Tier::Two, "QL_CONTRACT §4.4: the geometry I/O functions are p3-geometry-io."),
    ("ST_X", Tier::Two, "QL_CONTRACT §4.4: coordinate accessors are pure I/O functions (p3-geometry-io); a lon/lat rectangle is ST_Within against an envelope here, which is PointFilter::Bbox."),
    ("ST_Y", Tier::Two, "QL_CONTRACT §4.4: coordinate accessors are pure I/O functions (p3-geometry-io); a lon/lat rectangle is ST_Within against an envelope here, which is PointFilter::Bbox."),
    ("ST_SIMPLIFY", Tier::Two, "QL_CONTRACT §4.4: a pure function on the QGIS render path (p3-geometry-io)."),
    // ── §4.5 vector ──────────────────────────────────────────────────────
    ("<+>", Tier::Three, "QL_CONTRACT §4.5: `<+>` L1, halfvec, sparsevec and binary quantization operators have no atomic."),
    ("VECTOR_DIMS", Tier::Two, "QL_CONTRACT §4.5: vector_dims/vector_norm/l2_normalize are row functions."),
    ("VECTOR_NORM", Tier::Two, "QL_CONTRACT §4.5: vector_dims/vector_norm/l2_normalize are row functions."),
    ("L2_NORMALIZE", Tier::Two, "QL_CONTRACT §4.5: vector_dims/vector_norm/l2_normalize are row functions."),
    // ── §4.6 text ────────────────────────────────────────────────────────
    ("WEBSEARCH_TO_TSQUERY", Tier::Two, "QL_CONTRACT §4.6: websearch_to_tsquery and plainto_tsquery are parsers onto the same Text filter."),
    ("PLAINTO_TSQUERY", Tier::Two, "QL_CONTRACT §4.6: websearch_to_tsquery and plainto_tsquery are parsers onto the same Text filter."),
    ("TS_HEADLINE", Tier::Two, "QL_CONTRACT §4.6: ts_headline is a row function."),
    ("HIGHLIGHT", Tier::Two, "QL_CONTRACT §4.6: highlight is a row function."),
    ("SEARCH", Tier::Two, "QL_CONTRACT §4.6: typo-tolerant search() needs a term-dictionary prefix range plus a bounded Levenshtein automaton over the dictionary -- a new atomic, no format change."),
    // ── §4.7 aggregates ──────────────────────────────────────────────────
    // count/sum/min/max/avg, GROUP BY, HAVING and DISTINCT moved from this
    // table to Tier 1 with `src/query/aggregate.rs`: the atomic they named
    // exists now, so they are ACCEPTED rather than refused. What is left
    // here is what still has no atomic.
    ("ARRAY_AGG", Tier::Two, "QL_CONTRACT §4.7: array_agg/string_agg/json_agg come after the scalar aggregates, bounded by the row budget."),
    ("STRING_AGG", Tier::Two, "QL_CONTRACT §4.7: array_agg/string_agg/json_agg come after the scalar aggregates, bounded by the row budget."),
    ("JSON_AGG", Tier::Two, "QL_CONTRACT §4.7: array_agg/string_agg/json_agg come after the scalar aggregates, bounded by the row budget."),
    ("PERCENTILE_CONT", Tier::Three, "QL_CONTRACT §4.7: percentile_cont, window functions, GROUPING SETS and CUBE have no atomic."),
    ("GROUPING SETS", Tier::Three, "QL_CONTRACT §4.7: GROUPING SETS and CUBE have no atomic."),
    ("CUBE", Tier::Three, "QL_CONTRACT §4.7: GROUPING SETS and CUBE have no atomic."),
    ("ROW_NUMBER", Tier::Three, "QL_CONTRACT §4.7: window functions have no atomic."),
    ("RANK", Tier::Three, "QL_CONTRACT §4.7: window functions have no atomic."),
    ("DENSE_RANK", Tier::Three, "QL_CONTRACT §4.7: window functions have no atomic."),
    ("LAG", Tier::Three, "QL_CONTRACT §4.7: window functions have no atomic."),
    ("LEAD", Tier::Three, "QL_CONTRACT §4.7: window functions have no atomic."),
    ("NTILE", Tier::Three, "QL_CONTRACT §4.7: window functions have no atomic."),
    // ── §2 catalog surface ───────────────────────────────────────────────
    ("VERSION", Tier::Two, "QL_CONTRACT §2: version(), postgis_version() and current_schema() are fixed rows (p3-pg-surface)."),
    ("POSTGIS_VERSION", Tier::Two, "QL_CONTRACT §2: version(), postgis_version() and current_schema() are fixed rows (p3-pg-surface)."),
    ("CURRENT_SCHEMA", Tier::Two, "QL_CONTRACT §2: version(), postgis_version() and current_schema() are fixed rows (p3-pg-surface)."),
];

/// The reason a rewrite whose pre-image is a SET of ranges carries.
///
/// `EXTRACT(MONTH FROM t) = 6` is one interval per year in the corpus and
/// `t <> 'lit'` is the two intervals either side of an instant. A set of
/// ranges IS the membership-set union `OR` compiles to (`docs/lang/QL_CONTRACT.md`
/// §3), so these forms wait on that union rather than being emulated by a
/// scan -- §6 does not allow a scan to be taken silently, and the eighth law
/// does not allow a construct with no atomic to be emulated.
pub const MULTI_RANGE: &str = "QL_CONTRACT §4.2 and §3: this rewrite's pre-image is a SET of scalar ranges over the index, not one, and a set of ranges is exactly the membership-set union `OR` and `IN (list)` compile to. That union is not built in this slice, so the multi-range form is refused here rather than emulated by a scan (QL_CONTRACT §6). The single-range forms -- EXTRACT(YEAR FROM t), date_trunc('unit', t), t::date, t >= lit, t BETWEEN, t > now() - interval -- are accepted.";

/// The error a multi-range rewrite produces, naming the construct.
pub(super) fn multi_range(what: &str) -> SqlError {
    SqlError::Refused {
        keyword: what.to_owned(),
        tier: Tier::Two,
        reason: MULTI_RANGE,
    }
}

/// The reason a keyword carries, if it is in the table.
pub(super) fn lookup(keyword: &str) -> Option<(Tier, &'static str)> {
    let upper = keyword.to_ascii_uppercase();
    TABLE
        .iter()
        .find(|(name, _, _)| *name == upper)
        .map(|(_, tier, reason)| (*tier, *reason))
}

/// The error a keyword in the table produces. Callers that know the construct
/// is refused use this so the reason never has to be repeated at the site.
pub(super) fn refuse(keyword: &str) -> SqlError {
    match lookup(keyword) {
        Some((tier, reason)) => SqlError::Refused {
            keyword: keyword.to_ascii_uppercase(),
            tier,
            reason,
        },
        None => SqlError::Refused {
            keyword: keyword.to_ascii_uppercase(),
            tier: Tier::Three,
            reason: "QL_CONTRACT: not a Tier-1 construct and not in the Tier-2/3 table; no atomic is named for it.",
        },
    }
}
