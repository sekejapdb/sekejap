//! A page of a query: the prepared query's shape questions, the emit loop,
//! and the resume state a page commits once its rows are final.
use super::*;

pub struct PreparedQuery<'db> {
    pub(super) db: &'db Database,
    pub(super) collection: CollectionId,
    pub(super) filters: Vec<CompiledFilter>,
    pub(super) order: CompiledOrder,
    pub(super) projection: Vec<String>,
    pub(super) driver: DriverPlan,
    pub(super) total_limit: Option<usize>,
    pub(super) emitted: usize,
    pub(super) after: Option<RankKey>,
    /// Rows this query has already ranked but has not handed out yet, in rank
    /// order, LAST FIRST so `pop` takes the next one.
    ///
    /// A driver whose walk order is unrelated to the ranking -- a spatial cell
    /// walk under an entity-id ranking -- cannot stop early and cannot resume:
    /// to know which rows come next it has to see every candidate again. (A
    /// RANGE over a value-ordered index was in this list until the query
    /// asked for it in that index's own order, a SPATIAL walk until
    /// `QueryOrder::Driver` let a query ask for cell order, and a text merge
    /// until it was recognised as already ascending by document; all three
    /// resume now.) So page k+1 re-opened the whole candidate stream and
    /// discarded everything page 1..k had already returned. That is one full
    /// pass PER PAGE, and an answer of R rows in pages of P costs R^2/P --
    /// invisible while the answer fits one page, and the whole of why
    /// `popsim`'s `born_decade` took 1,011 s to return 5.58M rows at 48M
    /// while the same case at 200K took 11 ms.
    ///
    /// A page under such a driver walks to the end of the stream whatever it
    /// does, so the rows past this page are rows it has ALREADY ranked. It
    /// keeps them here instead of dropping them, and the pages after it are
    /// served from here without walking at all.
    pub(super) run: Vec<HeapEntry>,
    /// Whether the walk that filled `run` was cut off by [`RUN_ROWS`]. When it
    /// was, emptying the run is not the end of the answer and the next page
    /// walks again, from the last row handed out.
    pub(super) run_bounded: bool,
    /// One [`MembershipSet`] per filter position, built at most once and
    /// reused by every page and by resume -- see `ensure_membership_sets`.
    pub(super) membership: Vec<MembershipSet>,
    /// The resumable nearest walk, when the driver is [`DriverPlan::Nearest`].
    /// Kept on the prepared query so page N+1 continues the ring the previous
    /// page stopped in rather than re-walking from the centre: a `PreparedQuery`
    /// already owns per-query state (C1's sets), and the walk's `held`/`ready`
    /// buffers and Hilbert cover are the same kind of thing. Re-walking rings
    /// up to the previous page's last `(distance, id)` would be correct and
    /// bounded by that ring, but it would re-examine every posting already
    /// charged on earlier pages.
    pub(super) nearest: Option<crate::index::spatial::point::NearestWalk>,
    /// Sequences a geometry driver has already admitted, carried across
    /// Driver-order pages so a later posting of an already-emitted entity
    /// (at most 8 cells) is not re-yielded after a resume. Cleared each page
    /// under EntityId order, which re-walks from the start.
    pub(super) geometry_seen: HashSet<u64>,
}

impl PreparedQuery<'_> {
    /// True when the driver's own walk hands candidates over in exactly the
    /// order this query ranks them by. Two things follow, and only under this
    /// condition:
    ///
    ///   * a page may STOP the moment its heap is full -- nothing later in the
    ///     walk can outrank what is already there -- which is what turns a
    ///     LIMIT from a filter over N rows into a stop condition;
    ///   * the next page may RESUME at this page's last key instead of
    ///     re-walking everything already emitted.
    ///
    /// The cases are the ones where the key the tree is sorted by and the key
    /// the query sorts by are the same key. The entity cursor walks the
    /// primary tree in id order and `EntityId` ranks by id. An EQUALITY
    /// posting range holds one value for all its entries, so it is ordered by
    /// sequence alone -- also id order. An ascending scalar order driven by
    /// that same index walks `value || sequence`, which is `(value, id)`.
    ///
    /// The text merge is the same statement about a different structure: its
    /// term streams ascend by document, so their merge does, and `EntityId`
    /// ranks by exactly that.
    ///
    /// Everything else is excluded on purpose: a descending order walks
    /// against its ranking, a ranked order (BM25, vector distance) has no
    /// relation to any tree's order and must see every candidate before it
    /// knows its top k, and a range or nullish driver under an id order walks
    /// by value while ranking by id.
    fn driver_walks_in_rank_order(&self) -> RankWalk {
        match (&self.driver, &self.order) {
            (DriverPlan::Entities, CompiledOrder::EntityId) => RankWalk::Exact,
            (
                DriverPlan::Scalar {
                    predicate: EncodedScalarFilter::Eq(_),
                    ..
                },
                CompiledOrder::EntityId,
            ) => RankWalk::Exact,
            // The text merge yields documents in strictly ascending
            // sequence -- every term stream ascends and each round takes the
            // smallest or the common head and steps past it, which
            // `TextCursor::next` asserts and refuses to violate -- and an id
            // ranking is that same order. So a text-driven page ranked by id
            // stops on a full heap like any other, and resumes by seeking
            // every term stream to the document the last page ended on
            // (`TermPostings::open_from`). Spatial is deliberately NOT here:
            // its cells are walked in cell order, which is not id order.
            (DriverPlan::Text { .. }, CompiledOrder::EntityId) => RankWalk::Exact,
            (DriverPlan::Nearest { .. }, CompiledOrder::Distance { .. }) => RankWalk::Exact,
            // Driver order IS the driver's walk order -- that is the whole of
            // what it means -- so every driver that has one walks in rank
            // order by construction. The spatial cell walk is the case that
            // exists for: it ascends by `(cell, sequence)` over a sorted,
            // merged range list, which is exactly the rank key
            // `DriverKey::Cell` produces, and `DriverCursor::new` opens it on
            // the posting the last page stopped on. `driver_key` refused the
            // drivers that have no order of their own before this could be
            // asked.
            (
                DriverPlan::Entities
                | DriverPlan::Scalar { .. }
                | DriverPlan::Spatial { .. }
                | DriverPlan::Geometry { .. }
                | DriverPlan::Text { .. }
                | DriverPlan::Graph { .. }
                | DriverPlan::Keys { .. },
                CompiledOrder::Driver(_),
            ) => RankWalk::Exact,
            (
                DriverPlan::Scalar { info, .. },
                CompiledOrder::Scalar {
                    info: order,
                    direction,
                },
            ) if info.id == order.id => match direction {
                SortDirection::Ascending => RankWalk::Exact,
                SortDirection::Descending => RankWalk::ByValue,
            },
            _ => RankWalk::No,
        }
    }
    /// True when the scalar driver has to be walked backwards: the order is
    /// descending and it is that order's own index doing the driving.
    fn scalar_driver_descends(&self) -> bool {
        matches!(self.driver_walks_in_rank_order(), RankWalk::ByValue)
    }
    /// What the page will actually read off each candidate. A driver holding
    /// borrowed bytes copies them only for something that will be read.
    fn cursor_needs(&self) -> CursorNeeds {
        CursorNeeds {
            // The entity cursor's row bytes save `ensure_row` a point-get, but
            // only if something decodes them. With no filters and an id
            // ranking, nothing does.
            // A text filter that is not a phrase, and a BM25 ranking, are
            // answered from the postings and the norm blocks; only a phrase
            // has to re-read the authoritative text. Copying the row for
            // them cost one row per scored candidate for nothing.
            row: self.filters.iter().any(|filter| {
                !matches!(filter, CompiledFilter::Text(prepared) if prepared.phrase.is_none())
            }) || !matches!(
                self.order,
                CompiledOrder::EntityId
                    | CompiledOrder::Bm25(_)
                    | CompiledOrder::Driver(_)
                    | CompiledOrder::Distance { .. }
                    | CompiledOrder::Score { .. }
            ) || self.distance_order_needs_the_row()
                || matches!(
                    &self.order,
                    CompiledOrder::Score { expr, .. } if expr.needs_row(&self.driver)
                )
                // A projection reads the row as surely as a filter does, and
                // the cursor is standing on it: copying it here costs one
                // allocation, fetching it back costs a whole point-get.
                || !self.projection.is_empty(),
            // The posting's value key is read only by a scalar ranking over
            // the very index that produced it.
            scalar_key: match (&self.driver, &self.order) {
                (DriverPlan::Scalar { info, .. }, CompiledOrder::Scalar { info: order, .. }) => {
                    info.id == order.id
                }
                // A driver-ordered scalar walk ranks by the very key the
                // cursor is standing on.
                (DriverPlan::Scalar { info, .. }, CompiledOrder::Driver(DriverKey::Scalar(order))) => {
                    info.id == *order
                }
                (DriverPlan::Scalar { info, .. }, CompiledOrder::Score { expr, .. }) => {
                    expr.uses_scalar(info.id)
                }
                _ => false,
            },
            // The mapping entry's key bytes are read only by a driver-ordered
            // ranking over the keys walk itself.
            key: matches!(self.order, CompiledOrder::Driver(DriverKey::Key)),
        }
    }
    /// Whether a winner's identity is already established without going back
    /// to the primary tree.
    ///
    /// SQLite answers a key-only query out of a covering index and never
    /// touches the row table. The same holds here when the projection is empty
    /// AND the driver's own stream is the authority for the row: the entity
    /// cursor read the primary record itself, and a scalar posting is the
    /// membership record this engine already trusts elsewhere --
    /// `scalar_eq_posting_matches` answers a non-driving equality filter from
    /// the posting alone, without a row.
    ///
    /// A RANGE walk over that same index reads the same records. The entry
    /// `value || sequence` is written and retired by one maintenance path,
    /// whichever predicate later reads it, so trusting it when the predicate
    /// is `price = 490` and distrusting it when the predicate is
    /// `price >= 490 AND price < 500` would be a distinction in the question,
    /// not in the evidence. The one entry a range walk sees that an equality
    /// walk cannot is the nullish key, which null and missing share: that
    /// candidate is never certified, so `filters_match` opens its row and the
    /// predicate rejects it there. No candidate reaches the heap on a nullish
    /// posting, and every one that does came from a real value entry.
    ///
    /// Every other driver stays as it was, with two more exceptions below the
    /// text and spatial ones just described. An order scalar walk, graph ids,
    /// a vector locator can each name a row that is no longer there, and
    /// those still fetch it and still refuse an orphan -- unless the RANKING
    /// already proved the winner present, which is the BM25 case below, or
    /// the DRIVER's own stream already proved it, which is the text and
    /// spatial cases below that.
    ///
    /// Graph ids stay on the probing side even though an edge cannot outlive
    /// its endpoints: `Database::delete` cascades every incident edge
    /// (`cascade_graph_delete`, both primary and reverse markers) before it
    /// touches the row or the other indexes (`collections.rs:1466-1471`), so
    /// an edge the traversal still walks in this snapshot does prove its far
    /// endpoint alive. But `execute_graph` also seeds its result with
    /// `request.seed` unconditionally when `include_seed && min_depth == 0`
    /// (`query.rs`, the `include_seed` branch near the top of
    /// `execute_graph`) -- no edge is walked to reach that entity, so the
    /// traversal proves nothing about it. A caller can name any entity id as
    /// a seed; the probe is the only thing standing between that and an
    /// orphan reaching the page. So graph keeps the probe for every shape,
    /// not just the ones this function already declined to touch.
    ///
    /// Named sacrifice (Law 4): a store-level orphan -- a primary row removed
    /// behind the scalar index's back, which no supported write can do -- is
    /// no longer refused by a key-only page driven by a range filter, exactly
    /// as it has not been refused by one driven by an equality filter.
    /// `verify_index` refuses it outright either way.
    fn winner_needs_no_row(&self) -> bool {
        if !self.projection.is_empty() {
            return false;
        }
        // A BM25 page has no orphan left to refuse. Every candidate it ranks
        // goes through `text_score`, and the FIRST thing `text_score` does is
        // read the document's `0x76` norm: a document that is not in the text
        // index has no length there, the score is `None`, and the candidate is
        // dropped before it can reach the heap.
        //
        // That lookup is a liveness proof because text maintenance retires a
        // document in the SAME transaction as its row. The head tier deletes
        // the norm row outright; the packed tier cannot cut one document out
        // of a `0x7B` block, so the delete writes the EMPTY head value, which
        // overrides the block and decodes as "not in the index"
        // (`text_indexes::apply_transition`, `decode_norm`). Either way a
        // deleted document scores `None`, so a winner of a ranked text page
        // has already been proved present -- and probing the primary tree for
        // it again was a whole root-to-leaf reach, or a cursor step and a key
        // comparison, per RETURNED row.
        //
        // Named sacrifice (Law 4): a store-level orphan -- a primary row
        // removed behind the index's back, which no supported write can do --
        // is no longer refused by a BM25 page; it is refused by the norm, and
        // `verify_index` still refuses it outright. Every other page shape
        // keeps the probe.
        // A Score page keeps the probe: a Score Bm25 leaf scores a missing
        // document as 0.0 and KEEPS the candidate, so the norm lookup proves
        // nothing about liveness there.
        if matches!(self.order, CompiledOrder::Bm25(_)) {
            return true;
        }
        // A text-driven page has no orphan left to refuse either, ranked by
        // BM25 or not. `TermPostings::next` (text_indexes.rs:850 and 863)
        // never emits a posting whose live term frequency is `0` -- that is
        // exactly the tombstone form a delete or an update writes when it
        // retires a posting that a packed segment will not be rewritten to
        // drop, and it is written in the SAME transaction as the row: the
        // text family of `maintain_indexes` (`indexes.rs:973-975`) runs
        // inside the one `Database::delete` closure that goes on to remove
        // the row itself (`collections.rs:1467,1471`), committed or failed as
        // one frame (`collections.rs:1476`). So a document the merge still
        // hands over in this snapshot is a document whose row was alive when
        // the snapshot was taken -- the same guarantee the BM25 case above
        // reaches through the norm, proved one layer down instead, in the
        // posting stream every matching mode (Any, All, Phrase) reads from.
        // A phrase filter already carries the row forward for its own
        // adjacency check, so this arm changes nothing for phrase; it is
        // Any/All, which certify from the posting alone, that stop paying the
        // probe.
        //
        // Named sacrifice (Law 4): a store-level orphan -- a primary row
        // removed behind the text index's back, which no supported write can
        // do -- is no longer refused by a key-only page driven by the text
        // merge, exactly as it is not refused by a BM25 page. `verify_index`
        // refuses it outright either way.
        if matches!(self.driver, DriverPlan::Text { .. }) {
            return true;
        }
        // A key-driven page reads the same guarantee off the mapping entry
        // itself, unconditionally (no order restriction needed, unlike
        // spatial below): `Database::delete` removes a collection's mapping
        // entry (`self.writer()?.delete(&mapping_key(c, key))?`,
        // `collections.rs:1502`) in the SAME closure that removes its row
        // (`collections.rs:1501`), committed or failed as one frame
        // (`collections.rs:1476`, via `self.finish`). So a mapping entry
        // `KeysCursor` still walks in this snapshot names a row that was
        // alive when the snapshot was taken -- there is no "packed tier"
        // complication here the way there is for text: one entry, one key,
        // retired exactly once.
        //
        // Named sacrifice (Law 4): a store-level orphan -- a primary row
        // removed behind the mapping keyspace's back, which no supported
        // write can do -- is no longer refused by a key-only page. No
        // supported write can produce one; `verify_index`-style consistency
        // checking is out of this item's scope.
        if matches!(self.driver, DriverPlan::Keys { .. }) {
            return true;
        }
        // A spatial-driven page reads the same guarantee off the cell
        // posting. `maintain_point` retires a point's old cell posting in the
        // SAME transaction as the row that carried it: on a delete (or a move
        // to a different cell), `db.index_delete(i, &old.key)`
        // (spatial_indexes.rs:194) runs inside the one `Database::delete`
        // closure that also removes the row (`collections.rs:1467,1471`), and
        // the whole closure commits or fails as one frame
        // (`collections.rs:1476`, via `self.finish`). So a cell posting the
        // spatial cursor still walks in this snapshot is a document whose row
        // was alive when the snapshot was taken.
        //
        // That only covers the shapes where nothing ELSE in the page needs
        // the row either: under `QueryOrder::EntityId` or a driver-ordered
        // walk (`DriverKey::Cell`), the cell the cursor is standing on is the
        // whole of the ranking, the same way the entity cursor and the text
        // merge are the whole of theirs. A spatial driver ranked by anything
        // else already reads the row for the ranking (`order_needs_the_row`),
        // and this function's guard for that is unchanged.
        //
        // Named sacrifice (Law 4): a store-level orphan -- a primary row
        // removed behind the spatial index's back, which no supported write
        // can do -- is no longer refused by a bbox/radius page under those
        // two orders. `verify_index` refuses it outright either way.
        if matches!(self.driver, DriverPlan::Nearest { .. })
            && match &self.order {
                CompiledOrder::Distance { .. }
                | CompiledOrder::EntityId
                | CompiledOrder::Driver(_) => true,
                CompiledOrder::Score { expr, .. } => !expr.needs_row(&self.driver),
                _ => false,
            }
        {
            return true;
        }
        if matches!(self.driver, DriverPlan::Spatial { .. })
            && match &self.order {
                CompiledOrder::EntityId
                | CompiledOrder::Driver(DriverKey::Cell(_))
                | CompiledOrder::Distance { .. } => true,
                CompiledOrder::Score { expr, .. } => !expr.needs_row(&self.driver),
                _ => false,
            }
        {
            return true;
        }
        match &self.driver {
            DriverPlan::Entities => true,
            DriverPlan::Scalar {
                predicate: EncodedScalarFilter::Eq(_) | EncodedScalarFilter::Range { .. },
                position: Some(_),
                ..
            } => true,
            // The ORDER index driving the walk is not itself an authority on
            // membership -- a posting can outlive its row and an orphan must
            // still be refused. But when this plan was chosen precisely
            // BECAUSE an equality filter is riding along as a posting
            // membership probe (`order_index_drives_better`), that probe is
            // the same record the equality DRIVER is trusted for one arm up,
            // and a candidate reached the heap only by passing it. Narrow on
            // purpose: every other driver, and this one without such a filter,
            // still goes back for the row.
            DriverPlan::Scalar { position: None, .. } => self.filters.iter().any(|filter| {
                matches!(
                    filter,
                    CompiledFilter::Scalar {
                        posting_membership: true,
                        ..
                    }
                )
            }),
            _ => false,
        }
    }
    /// True when the driver hands candidates over in ascending entity id, so
    /// the rows they ask for are ascending primary keys and one forward cursor
    /// can serve the whole page. An equality posting is `value || sequence` for
    /// one value, so it IS sequence order; a graph result is sorted before it
    /// leaves the traversal; the entity walk is the primary tree. A range or order walk
    /// is in value order and a spatial or vector walk in neither, so those keep
    /// the point-get.
    fn driver_walks_ids_ascending(&self) -> bool {
        matches!(
            self.driver,
            DriverPlan::Entities
                | DriverPlan::Graph { .. }
                // The text merge emits documents in STRICTLY ascending
                // sequence and refuses a posting that does not advance -- it
                // says so, and returns `text merge did not advance` if it ever
                // stops holding. A phrase re-reads the authoritative text of
                // every document it scores, and was paying a root-to-leaf
                // descent for each: 8.0 pager accesses per candidate on
                // `text/match_phrase`.
                | DriverPlan::Text { .. }
                | DriverPlan::Scalar {
                    predicate: EncodedScalarFilter::Eq(_),
                    ..
                }
        )
    }
    /// True when the RANKING, not a filter, has to read the row: a scalar
    /// order whose key the driver's postings do not carry.
    fn order_needs_the_row(&self) -> bool {
        match &self.order {
            CompiledOrder::Scalar { .. } => !self.cursor_needs().scalar_key,
            CompiledOrder::Distance { .. } => self.distance_order_needs_the_row(),
            CompiledOrder::Score { expr, .. } => expr.needs_row(&self.driver),
            CompiledOrder::EntityId
            | CompiledOrder::ExactVector { .. }
            | CompiledOrder::ApproximateVector { .. }
            | CompiledOrder::Bm25(_)
            // Every driver-order key is carried by the candidate: its id, the
            // scalar posting's value, or the spatial posting's cell.
            | CompiledOrder::Driver(_) => false,
        }
    }
    /// Distance ranking reads a row only when the driver did not already
    /// hand over that index's point (the nearest walk and a spatial cell
    /// walk both do).
    fn distance_order_needs_the_row(&self) -> bool {
        match (&self.order, &self.driver) {
            (
                CompiledOrder::Distance { info, .. },
                DriverPlan::Nearest { info: driving, .. } | DriverPlan::Spatial { info: driving, .. },
            ) if driving.id == info.id => false,
            (CompiledOrder::Distance { .. }, _) => true,
            _ => false,
        }
    }
    /// True when the page should GATHER its candidates and read their rows in
    /// tree order rather than one at a time in driver order.
    ///
    /// Four things have to hold, and each one is a correctness statement:
    ///
    ///   * the walk has no stop condition (`RankWalk::No`), so a batch can
    ///     never read past the point a stopping walk would have reached. This
    ///     is the whole of the bound rule: where a page CAN stop early, its
    ///     driver is already handing candidates over in rank order and the
    ///     row-reading question is a different one;
    ///   * the driver does not already ascend by id -- when it does, the
    ///     lockstep reader serves the page in one pass without gathering
    ///     anything;
    ///   * something actually reads a row per candidate, or there is nothing
    ///     to gather for;
    ///   * and nothing can REJECT a candidate before that read. A batch reads
    ///     rows before any filter runs, so a filter that answers from a
    ///     posting or from a graph set -- and would have rejected the
    ///     candidate for free -- must not be sitting in front of the one that
    ///     needs the row.
    fn batches_row_reads(&self) -> bool {
        if self.driver_walks_in_rank_order() != RankWalk::No || self.driver_walks_ids_ascending() {
            return false;
        }
        if !(self.walk_reads_every_row() || self.order_needs_the_row()) {
            return false;
        }
        self.filters_are_row_pure()
    }
    /// True when every filter this page still has to evaluate is a pure
    /// function of the row -- no posting probe, no graph set, no text merge --
    /// so the whole decision can be made against BORROWED row bytes and
    /// nothing has to be copied out of the leaf to make it.
    ///
    /// The DRIVING filter is judged like any other. It is usually certified
    /// by the driver and skipped per candidate, but whether it is certified is
    /// a property of the CANDIDATE and this is a property of the plan, so
    /// exempting it here would let a phrase -- whose driver certifies nothing
    /// -- through, and a phrase decided against borrowed bytes alone is no
    /// decision at all.
    fn filters_are_row_pure(&self) -> bool {
        self.filters.iter().all(|filter| match filter {
            CompiledFilter::Scalar {
                posting_membership, ..
            } => !*posting_membership,
            CompiledFilter::JsonEq { .. }
            | CompiledFilter::Point { .. }
            | CompiledFilter::Geometry { .. }
            | CompiledFilter::Folded { .. }
            | CompiledFilter::Key { .. } => true,
            // A text filter rejects from its postings before it looks at a
            // row, so it is a cheap refusal standing in front of the expensive
            // one -- and a PHRASE is not a pure function of the row at all: it
            // needs the merge's frequencies first. A graph filter is a
            // membership test over a set the traversal already built.
            CompiledFilter::Text(_) | CompiledFilter::Graph { .. } => false,
        })
    }
    /// True when EVERY candidate that survives to the heap has already had its
    /// primary record read, so the winner stage's existence re-fetch is asking
    /// a question the walk has answered.
    ///
    /// It is a property of the plan, not of the candidate: a candidate reaches
    /// the heap only by passing every filter, so if any filter is one that has
    /// to read the row, every heap candidate's row was read. The entity cursor
    /// is the other case -- it walks the primary tree itself, so a key it
    /// yielded is a record that is there.
    ///
    /// The filter the DRIVER certifies is excluded: that one is skipped
    /// outright and reads nothing.
    fn walk_reads_every_row(&self) -> bool {
        // The entity cursor walks the primary tree itself, so a key it yielded
        // is a record it has already read.
        matches!(self.driver, DriverPlan::Entities) || self.a_filter_reads_the_row()
    }
    /// The half of [`walk_reads_every_row`] that is about the FILTERS: does
    /// one of them have to go to the primary tree for every candidate?
    ///
    /// The entity driver makes `walk_reads_every_row` true without any filter
    /// asking for a row, which is the right answer to "has this candidate's
    /// record been read" and the wrong one to "is there a read here worth
    /// moving".
    fn a_filter_reads_the_row(&self) -> bool {
        let driving = match &self.driver {
            DriverPlan::Scalar { position, .. }
            | DriverPlan::Text { position, .. }
            | DriverPlan::Keys { position, .. } => *position,
            DriverPlan::Spatial { position, .. } | DriverPlan::Graph { position } => Some(*position),
            DriverPlan::Nearest { certifies, .. } => *certifies,
            DriverPlan::Spatial { position, .. }
            | DriverPlan::Geometry { position, .. }
            | DriverPlan::Graph { position } => Some(*position),
            _ => None,
        };
        self.filters.iter().enumerate().any(|(position, filter)| match filter {
            // A phrase is the one filter the DRIVER does not certify: its
            // postings establish all-term candidacy and the ordered adjacency
            // is settled against the authoritative primary text. So every
            // candidate that passes it -- driving or not -- has had its row
            // read, and the winner stage's re-fetch is asking a question this
            // walk has answered.
            CompiledFilter::Text(prepared) => prepared.phrase.is_some(),
            // A geometry posting's BoxF is only a candidate test. Driving or
            // not, the row's geometry is refined through spatial_geometry;
            // T3's no-row rule does not apply.
            CompiledFilter::Geometry { .. } => true,
            _ if Some(position) == driving => false,
            // A non-driving equality answered from its posting reads no row,
            // and neither does a non-driving RANGE once its own posting walk
            // has been collected into a set (`MembershipSet::Ids` or
            // `MembershipSet::Bitmap`); every other scalar predicate does.
            CompiledFilter::Scalar {
                posting_membership, ..
            } => {
                !*posting_membership
                    && !matches!(
                        self.membership[position],
                        MembershipSet::Ids(_) | MembershipSet::Bitmap(_)
                    )
            }
            CompiledFilter::JsonEq { .. } => true,
            // A non-driving point filter whose cover ranges have been walked
            // into a set reads no row either; without one it reads every row.
            CompiledFilter::Point { .. } => !matches!(
                self.membership[position],
                MembershipSet::Ids(_) | MembershipSet::Bitmap(_)
            ),
            CompiledFilter::Graph { .. } | CompiledFilter::Folded { .. } => false,
            // Certified straight from the mapping entry, at the driving
            // position `_ if Some(position) == driving` already caught above;
            // reached only if it were somehow not driving, which
            // `prepare_query` refuses to compile.
            CompiledFilter::Key { .. } => false,
        })
    }
    /// True when this page should HOLD BACK the rows it ranked but could not
    /// return, instead of dropping them and walking for them again.
    ///
    /// Three things have to hold, and each one is a correctness statement:
    ///
    ///   * the walk is not in rank order (`RankWalk::No`), so it has neither a
    ///     stop condition nor a resume: it reads the whole candidate stream
    ///     whatever it does, and everything past this page is something it has
    ///     already ranked. A walk that CAN resume holds nothing, exactly as
    ///     before -- it never ranked those rows in the first place;
    ///   * the page projects no fields, so what is held is a rank key and
    ///     nothing else. A projected page can carry a whole primary record per
    ///     entry, and a bound in rows would not be a bound in bytes. Named
    ///     sacrifice (Law 4): a PROJECTED answer over a value-ordered walk
    ///     still re-walks its driver once per page;
    ///   * the order is not the approximate-vector one, whose `ef` already
    ///     bounds the entire result set to one shortlist, and whose page
    ///     reports approximation diagnostics that a held row does not carry.
    fn keeps_a_run(&self) -> bool {
        self.projection.is_empty()
            && self.driver_walks_in_rank_order() == RankWalk::No
            && !matches!(self.order, CompiledOrder::ApproximateVector { .. })
    }
    /// True when this page hands its winners back in ascending entity id, so
    /// the rows they still owe can be lifted out by one forward cursor rather
    /// than a root-to-leaf descent each.
    ///
    /// An id ranking is the obvious one. Driver order is the other: over the
    /// entity cursor, the text merge or a traversal its key IS the id, and
    /// the page is as ascending as an id-ranked one. Over a scalar or spatial
    /// walk it is not, and those pages lift their rows out in the tree's
    /// order first, as every other ranked page does.
    fn winners_ascend_by_id(&self) -> bool {
        matches!(
            self.order,
            CompiledOrder::EntityId | CompiledOrder::Driver(DriverKey::Entity)
        )
    }
    /// The bookkeeping every page ends with, however its winners were found:
    /// where the next page resumes, how many rows the query has emitted, and
    /// whether there is anything left.
    fn finish_page(
        &mut self,
        rows: Vec<QueryRow>,
        winners: &[HeapEntry],
        has_more: bool,
        approximation: Option<ApproximationDiagnostics>,
        work: QueryWork,
    ) -> QueryResult<QueryPage> {
        let next_after = winners.last().map(|winner| winner.key.clone());
        let next_emitted = self
            .emitted
            .checked_add(rows.len())
            .ok_or_else(|| invalid_query("query total output overflow"))?;
        let hit_total_limit = self.total_limit.is_some_and(|limit| next_emitted >= limit);
        self.after = next_after.or_else(|| self.after.clone());
        self.emitted = next_emitted;
        Ok(QueryPage {
            rows,
            done: hit_total_limit || !has_more,
            driver: self.driver.diagnostic(),
            work,
            approximation,
        })
    }
    /// Turn a page's ranked winners into its rows: the existence proof a
    /// driver that is not its own authority still owes, and the projected
    /// fields. Shared by the page that walked for these winners and the page
    /// that took them out of the held run.
    fn emit_rows<C: FnMut() -> bool>(
        &self,
        winners: &mut [HeapEntry],
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Vec<QueryRow>> {
        let db = self.db;
        // Whether the page will project anything out of the rows it holds.
        let wants_rows = !self.projection.is_empty();
        let winner_needs_no_row = self.winner_needs_no_row();
        let walk_reads_every_row = self.walk_reads_every_row();
        let mut scratch = ProjectionScratch::default();
        // An id ranking returns winners in ascending primary-key order, so the
        // rows they still need can be lifted out by one forward cursor. Any
        // other ranking hands them over in an order the primary tree knows
        // nothing about, and each one is a fresh descent as before.
        let mut winner_rows = PrimaryRows::new(db, self.winners_ascend_by_id());
        // A RANKED page hands its winners over in score order; the primary
        // tree is in id order. Reading them as they are ranked descends from
        // the root once per returned row -- a BM25 page over 900 matching
        // documents paid 900 descents and 900 buffers, where the very same
        // page ranked by id paid one cursor. So a ranked page lifts its rows
        // out FIRST, in the tree's order, and hands them back to the ranked
        // winners by index. Nothing about the answer or its order changes,
        // only the order the rows are read in; an id ranking already ascends
        // and keeps streaming them one at a time, holding none.
        let ranked_rows_read =
            !winner_needs_no_row && !walk_reads_every_row && !self.winners_ascend_by_id();
        if ranked_rows_read {
            let mut ascending = PrimaryRows::new(db, true);
            if wants_rows {
                let mut by_id: Vec<usize> = (0..winners.len())
                    .filter(|at| winners[*at].row.is_none())
                    .collect();
                by_id.sort_unstable_by_key(|at| winners[*at].key.id);
                for at in by_id {
                    meter.charge(WorkResource::PrimaryReads, 1)?;
                    let bytes = ascending
                        .read(winners[at].key.id)?
                        .ok_or_else(|| corrupt_query("query winner is missing its entity"))?;
                    winners[at].row = Some(Box::new(decode_row(db, bytes)?));
                }
            } else {
                // A key-only page wanted this read for one thing: the proof
                // that the entity is still there. It keeps nothing, so it
                // sorts the SEQUENCES and not indices into the winners --
                // every comparison of `sort_unstable_by_key(|at|
                // winners[*at]...)` is an indirect load into a
                // 56-byte-per-entry array, and a 3,716-document BM25 page
                // makes about 44,000 of them. The collection is the same for
                // every candidate (the walk refuses one that crosses), so what
                // is sorted is one `u64` each.
                let mut sequences: Vec<u64> =
                    winners.iter().map(|winner| winner.key.id.sequence).collect();
                sequences.sort_unstable();
                for sequence in sequences {
                    meter.charge(WorkResource::PrimaryReads, 1)?;
                    if !ascending.exists(EntityId {
                        collection: self.collection,
                        sequence,
                    })? {
                        return Err(corrupt_query("query winner is missing its entity"));
                    }
                }
            }
        }
        let mut rows = Vec::with_capacity(winners.len());
        for winner in winners.iter_mut() {
            let order = match (&self.order, &winner.key.value) {
                (CompiledOrder::EntityId, RankValue::Entity) => OrderValue::EntityId,
                (CompiledOrder::Scalar { info, .. }, RankValue::Scalar(key)) => {
                    OrderValue::Scalar(scalar_order_value(info, key)?)
                }
                (CompiledOrder::ExactVector { .. }, RankValue::Score(score)) => {
                    OrderValue::Distance(f64::from_bits(*score))
                }
                (CompiledOrder::ApproximateVector { .. }, RankValue::Score(score)) => {
                    OrderValue::Distance(f64::from_bits(*score))
                }
                (CompiledOrder::Bm25(_), RankValue::Score(score)) => {
                    OrderValue::Bm25(f64::from_bits(*score))
                }
                (CompiledOrder::Distance { .. }, RankValue::Score(score)) => {
                    OrderValue::Distance(f64::from_bits(*score))
                }
                (CompiledOrder::Score { .. }, RankValue::Score(score)) => {
                    OrderValue::Score(f64::from_bits(*score))
                }
                // The driver's key is the walk's own bookkeeping, not an
                // answer about the row: a cell number is not a distance and a
                // sequence is already `id`.
                (CompiledOrder::Driver(_), _) => OrderValue::Driver,
                _ => unreachable!("prepared order and rank key agree"),
            };
            // Every returned ID must still have an authoritative primary row.
            // This remains winner-only so native index scans do not pay a
            // primary point-get for every rejected candidate -- and it is
            // skipped entirely when the driver is already that authority and
            // no field is projected (`winner_needs_no_row`), because then the
            // fetch decodes nothing and only re-proves what the candidate
            // stream proved.
            let mut projected = Vec::with_capacity(self.projection.len());
            // The row the candidate walk already had, if it kept one.
            let carried = winner.row.take();
            if carried.is_none() && !winner_needs_no_row && !walk_reads_every_row && !ranked_rows_read
            {
                meter.charge(WorkResource::PrimaryReads, 1)?;
                if self.projection.is_empty() {
                    // Nothing is decoded from these bytes -- the read is here
                    // to refuse an orphan -- so do not copy them out of the
                    // leaf.
                    if !winner_rows.exists(winner.key.id)? {
                        return Err(corrupt_query("query winner is missing its entity"));
                    }
                } else {
                    let bytes = winner_rows
                        .read(winner.key.id)?
                        .ok_or_else(|| corrupt_query("query winner is missing its entity"))?;
                    let row = decode_row(self.db, bytes)?;
                    project_fields(
                        self.db,
                        winner.key.id,
                        &row,
                        &self.projection,
                        &mut scratch,
                        &mut projected,
                        meter,
                    )?;
                }
            } else if let Some(row) = carried {
                project_fields(
                    self.db,
                    winner.key.id,
                    &row,
                    &self.projection,
                    &mut scratch,
                    &mut projected,
                    meter,
                )?;
            }
            let row = QueryRow {
                id: winner.key.id,
                order,
                projected,
            };
            meter.charge(WorkResource::OutputBytes, checked_output_size(&row)?)?;
            rows.push(row);
        }
        Ok(rows)
    }
    pub fn next_page<C: FnMut() -> bool>(
        &mut self,
        page_size: usize,
        budget: QueryBudget,
        mut cancelled: C,
    ) -> QueryResult<QueryPage> {
        if page_size == 0 || page_size > MAX_PAGE_SIZE {
            return Err(invalid_query("query page size requires 1..8192 rows"));
        }
        let remaining = self
            .total_limit
            .map_or(usize::MAX, |limit| limit.saturating_sub(self.emitted));
        if remaining == 0 {
            return Ok(QueryPage {
                rows: Vec::new(),
                done: true,
                driver: self.driver.diagnostic(),
                work: QueryWork::default(),
                approximation: None,
            });
        }
        let wanted = page_size.min(remaining);
        let needs_extra = remaining > wanted;
        let capacity = wanted + usize::from(needs_extra);
        // How many ranked rows this page will hold on to. A page that can
        // resume holds exactly what it returns; one that cannot holds a whole
        // run, because the rows past this page are rows it is about to rank
        // anyway and dropping them is what makes the next page walk again.
        let held = if self.keeps_a_run() {
            capacity.max(RUN_ROWS.min(remaining))
        } else {
            capacity
        };
        let descending = matches!(
            self.order,
            CompiledOrder::Scalar {
                direction: SortDirection::Descending,
                ..
            } | CompiledOrder::Bm25(_)
                | CompiledOrder::Score {
                    direction: SortDirection::Descending,
                    ..
                }
        );
        let mut meter = WorkMeter::new(budget, &mut cancelled);
        meter.check_cancelled()?;
        self.ensure_membership_sets(&mut meter)?;
        // Rows an earlier page's walk already ranked and could not return.
        // They are in rank order, last first, so this takes the next ones off
        // the end -- and the whole walk is skipped, which is the point.
        if !self.run.is_empty() {
            let take = wanted.min(self.run.len());
            // Taken, not yet given up. `emit_rows` still charges this page's
            // output bytes and, where the page owes a winner probe, its
            // primary reads -- so it can still fail on a budget or a
            // cancellation, and a page that fails must leave the run exactly
            // as it found it. Popping first handed the retry the SLICE AFTER
            // the one that failed, which is the same defect as a committed
            // `after`. The run is stored last-first, so the tail is this
            // page's winners and reversing it puts them in rank order.
            let keep = self.run.len() - take;
            let mut winners = self.run.split_off(keep);
            winners.reverse();
            let has_more = keep > 0 || self.run_bounded;
            let rows = match self.emit_rows(&mut winners, &mut meter) {
                Ok(rows) => rows,
                Err(err) => {
                    winners.reverse();
                    self.run.append(&mut winners);
                    return Err(err);
                }
            };
            let work = meter.used;
            return self.finish_page(rows, &winners, has_more, None, work);
        }
        // One decoded `0x7B` norm block held for the page. Candidates that
        // arrive in ascending sequence -- the text and entity cursors -- reuse
        // it 255 times out of 256; one that does not simply re-decodes.
        let mut scratch = RowScratch::default();
        let graph = execute_graph_filters(self.db, &self.filters, &mut meter)?;
        let in_rank_order = self.driver_walks_in_rank_order();
        let needs = self.cursor_needs();
        let reverse = self.scalar_driver_descends();
        let resume = self
            .after
            .clone()
            .filter(|_| in_rank_order != RankWalk::No);
        // Resume state, exactly like `after` and `run`: the walk carries the
        // ring the previous page stopped in, and reading it ADVANCES it. A
        // page that fails must leave it where it found it, so the page walks
        // a CLONE and the clone is committed only once the page's rows are
        // final; on any error the original goes back untouched.
        //
        // Cost: one clone per Distance page -- the current ring's `held` and
        // `ready` hits (40 bytes each), the Hilbert cover ranges, and the
        // index descriptor. That is a copy of what the walk already holds,
        // taken by a page that is about to examine that same ring's postings,
        // and it adds no bound proportional to the database. The alternative,
        // dropping the walk on failure and re-walking from the centre next
        // page, costs re-examining every posting the earlier pages already
        // charged.
        let nearest_before = self.nearest.take();
        let mut nearest_walk = nearest_before.clone();
        let result = (|| {
        let fast_scan = match self.unfiltered_vector_scan(held, &mut meter)? {
            Some(scan) => Some(scan),
            None => self.filtered_vector_scan(held, &mut meter)?,
        };
        if let Some((fast_winners, fast_approx)) = fast_scan {
            let mut winners = fast_winners.into_vec();
            winners.sort_unstable_by(|left, right| {
                compare_rank(&left.key, &right.key, descending)
            });
            let has_more = winners.len() > wanted;
            // Split the hold out of the winners, but do not hand it to the
            // query yet: `emit_rows` can still fail -- on a cancellation or a
            // budget -- and a failed page must leave the query exactly as it
            // found it, the hold as much as `after`. A hold committed before
            // the page was built is a page's worth of ranked rows that the
            // retry then skips past, because the retry reads the hold instead
            // of walking, and the rows this page had truncated away are gone.
            let holds = winners.len() > wanted && self.keeps_a_run();
            let bounded = winners.len() == held;
            let mut hold = if holds {
                winners.split_off(wanted)
            } else {
                Vec::new()
            };
            winners.truncate(wanted);
            let rows = self.emit_rows(&mut winners, &mut meter)?;
            if holds {
                hold.reverse();
                self.run_bounded = bounded;
                self.run = hold;
            }
            let work = meter.used;
            return self.finish_page(rows, &winners, has_more, fast_approx, work);
        }
        let geometry_seen = if in_rank_order == RankWalk::Exact {
            self.geometry_seen.clone()
        } else {
            HashSet::new()
        };
        let mut driver = DriverCursor::new(
            self.db,
            self.collection,
            &self.driver,
            &graph,
            needs,
            resume.as_ref(),
            reverse,
            nearest_walk.as_mut(),
            geometry_seen,
        )?;
        // Whether a kept candidate should carry its row into the heap.
        let wants_rows = !self.projection.is_empty();
        let db = self.db;
        let mut rows = PrimaryRows::new(db, self.driver_walks_ids_ascending());
        let mut winners = Winners::new();
        let approximation =
            if let CompiledOrder::ApproximateVector {
                info,
                query,
                query_norm,
                metric,
                ef,
            } = &self.order
            {
                let mut examined = 0usize;
                let mut shortlist = BinaryHeap::with_capacity((*ef).min(1024));
                while let Some(mut candidate) = driver.next(&mut meter)? {
                    meter.charge(WorkResource::Candidates, 1)?;
                    if candidate.id.collection != self.collection {
                        return Err(corrupt_query("query driver crossed collection boundary"));
                    }
                    let mut encoded = candidate.row.take();
                    let mut row = None;
                    if !filters_match(
                        db,
                        &mut rows,
                        &self.filters,
                        &self.membership,
                        &candidate,
                        &mut row,
                        &mut encoded,
                        &graph,
                        &mut scratch,
                        &mut meter,
                    )? {
                        continue;
                    }
                    examined = examined
                        .checked_add(1)
                        .ok_or_else(|| invalid_query("approximate examined count overflow"))?;
                    let Some((distance, locator)) = approximate_vector_score(
                        self.db, &candidate, info, query, *metric, &mut meter,
                    )?
                    else {
                        continue;
                    };
                    shortlist.push(ApproxHeapEntry {
                        distance,
                        id: candidate.id,
                        locator,
                    });
                    if shortlist.len() > *ef {
                        shortlist.pop();
                    }
                }

                let reranked = shortlist.len();
                let mut ordered: Vec<ApproxHeapEntry> = shortlist.into_vec();
                ordered.sort_unstable_by(|left, right| {
                    left.id
                        .sequence
                        .cmp(&right.id.sequence)
                        .then_with(|| left.locator.cmp(&right.locator))
                        .then_with(|| left.id.cmp(&right.id))
                });
                for candidate in ordered {
                    let Some(distance) = rerank_quantized_vector(
                        self.db,
                        info,
                        &candidate,
                        query,
                        *query_norm,
                        *metric,
                        &mut meter,
                    )?
                    else {
                        continue;
                    };
                    let key = RankKey {
                        value: RankValue::Score(distance.to_bits()),
                        id: candidate.id,
                    };
                    if self
                        .after
                        .as_ref()
                        .is_some_and(|after| compare_rank(&key, after, false) != Ordering::Greater)
                    {
                        continue;
                    }
                    let entry = HeapEntry {
                        key,
                        descending: false,
                        row: None,
                    };
                    if winners.len() < capacity {
                        winners.push(capacity, entry);
                    } else if winners
                        .worst()
                        .is_some_and(|worst| entry.cmp(worst) == Ordering::Less)
                    {
                        winners.pop_worst();
                        winners.push(capacity, entry);
                    }
                }
                Some(ApproximationDiagnostics {
                    method: ApproxVectorMethod::SymmetricInt8ScanV1,
                    ef: *ef,
                    examined,
                    reranked,
                })
            } else {
                // Does this page gather its candidates before reading their
                // rows? THE BOUND RULE, stated once: a batch is taken only
                // where the walk has no stop condition at all, and it is never
                // larger than the page could return. So the rows a batch reads
                // are rows the row-by-row walk would have read too -- the same
                // set, in the primary tree's order instead of the driver's.
                let batched = self.batches_row_reads();
                let keep_batch_rows = wants_rows || self.order_needs_the_row();
                // A page whose driver ALREADY ascends gathers nothing -- one
                // forward cursor serves it in a single pass -- but it was
                // still copying each row out of the leaf to look at one field
                // of it and then dropping the copy: the last allocation per
                // candidate on `filter/and_half_indexed`. It can borrow
                // instead, on exactly the terms a batch can.
                let borrowed = !batched
                    && !keep_batch_rows
                    && self.a_filter_reads_the_row()
                    && self.filters_are_row_pure();
                // Only a filtered nearest walk has a use for how often its
                // hits are rejected, so only it pays the per-candidate call.
                let reports_acceptance =
                    matches!(self.driver, DriverPlan::Nearest { .. }) && !self.filters.is_empty();
                let batch_bound = capacity.min(ROW_BATCH);
                let mut batch: Vec<Candidate> = Vec::new();
                let mut batch_order: Vec<(u64, u32)> = Vec::new();
                'walk: loop {
                    let mut candidate = if batched {
                        if batch.is_empty() {
                            while batch.len() < batch_bound {
                                let Some(candidate) = driver.next(&mut meter)? else {
                                    break;
                                };
                                meter.charge(WorkResource::Candidates, 1)?;
                                if candidate.id.collection != self.collection {
                                    return Err(corrupt_query(
                                        "query driver crossed collection boundary",
                                    ));
                                }
                                batch.push(candidate);
                            }
                            if batch.is_empty() {
                                break 'walk;
                            }
                            read_batch_rows(
                                db,
                                &mut rows,
                                &self.filters,
                                &self.membership,
                                keep_batch_rows,
                                &mut batch,
                                &mut batch_order,
                                &mut scratch,
                                &mut meter,
                            )?;
                            // `pop` takes from the end, so reversing hands the
                            // candidates back in the driver's own order: the
                            // rows were read in another order, nothing else
                            // was.
                            batch.reverse();
                        }
                        batch.pop().expect("the batch was just filled")
                    } else {
                        let Some(candidate) = driver.next(&mut meter)? else {
                            break 'walk;
                        };
                        meter.charge(WorkResource::Candidates, 1)?;
                        if candidate.id.collection != self.collection {
                            return Err(corrupt_query("query driver crossed collection boundary"));
                        }
                        candidate
                    };
                    let mut encoded = candidate.row.take();
                    let mut row = None;
                    if borrowed && encoded.is_none() && candidate.row_filtered.is_none() {
                        meter.charge(WorkResource::PrimaryReads, 1)?;
                        let id = candidate.id;
                        let satisfied = candidate.satisfied_filter;
                        let filters = &self.filters;
                        let ranges = &self.membership;
                        let scratch = &mut scratch;
                        let meter = &mut meter;
                        candidate.row_filtered = rows.with_row(id, |bytes| match bytes {
                            Some(bytes) => batch_filters_match(
                                db, filters, ranges, satisfied, id, bytes, scratch, meter,
                            ),
                            None => Err(corrupt_query(
                                "query candidate points to a missing entity",
                            )),
                        })?;
                    }
                    let kept = match candidate.row_filtered {
                        // The batched pass read this candidate's row and ran
                        // every filter against it; there is nothing here to
                        // repeat.
                        Some(kept) => kept,
                        // `filters_match` over an empty slice can only say
                        // yes, and saying it costs a nine-argument call per
                        // row: 5.6% of a key-only enumeration that has no
                        // filters to evaluate at all.
                        None if self.filters.is_empty() => true,
                        None => filters_match(
                            db,
                            &mut rows,
                            &self.filters,
                            &self.membership,
                            &candidate,
                            &mut row,
                            &mut encoded,
                            &graph,
                            &mut scratch,
                            &mut meter,
                        )?,
                    };
                    // The nearest walk sizes its next ring from how many of
                    // the hits it offered survived; nothing else listens.
                    if reports_acceptance {
                        driver.note_kept(kept);
                    }
                    if !kept {
                        continue;
                    }
                    let Some(key) = rank_candidate(
                        db,
                        &mut rows,
                        &self.order,
                        &candidate,
                        &mut row,
                        &mut encoded,
                        &mut scratch,
                        &mut meter,
                    )?
                    else {
                        continue;
                    };
                    if self.after.as_ref().is_some_and(|after| {
                        compare_rank(&key, after, descending) != Ordering::Greater
                    }) {
                        continue;
                    }
                    // A walk that is only VALUE-monotone (a descending scalar
                    // order, whose reverse walk hands each tie group over id
                    // descending while the rank wants id ascending) cannot
                    // stop on a full heap: the rest of the boundary value's
                    // tie group still outranks what is held. It CAN stop the
                    // moment a candidate's value falls strictly past the worst
                    // held one, because the walk never comes back up.
                    if in_rank_order == RankWalk::ByValue
                        && winners.len() >= capacity
                        && winners.worst().is_some_and(|worst| {
                            compare_rank_value(&key.value, &worst.key.value, descending)
                                == Ordering::Greater
                        })
                    {
                        break;
                    }
                    let mut entry = HeapEntry {
                        key,
                        descending,
                        row: None,
                    };
                    // The bytes this candidate's row was read from are still
                    // in hand -- the entity cursor copied them out of the leaf
                    // it was standing on, or a filter fetched them. Hand them
                    // to the heap ONLY if the page will project something and
                    // ONLY if the entry is being kept, so a losing candidate
                    // costs nothing and a key-only page carries nothing.
                    let keep = |entry: &mut HeapEntry,
                                    row: &mut Option<RowData>,
                                    encoded: &mut Option<Vec<u8>>|
                     -> QueryResult<()> {
                        if wants_rows {
                            entry.row = match row.take() {
                                Some(row) => Some(Box::new(row)),
                                None => match encoded.take() {
                                    Some(bytes) => Some(Box::new(decode_row(self.db, bytes)?)),
                                    None => None,
                                },
                            };
                        }
                        Ok(())
                    };
                    // `held` is the page's own hold, which is the page size
                    // unless the walk cannot resume -- then it is a whole run,
                    // and the entries past this page are kept for the pages
                    // after it instead of being walked for again. `capacity`
                    // stays the RESERVE: a ten-row answer must not reserve a
                    // run's worth of entries to hold ten.
                    if winners.len() < held {
                        keep(&mut entry, &mut row, &mut encoded)?;
                        winners.push(capacity, entry);
                    } else if winners
                        .worst()
                        .is_some_and(|worst| entry.cmp(worst) == Ordering::Less)
                    {
                        winners.pop_worst();
                        keep(&mut entry, &mut row, &mut encoded)?;
                        winners.push(capacity, entry);
                    }
                    // The page is full and the walk is already in rank order,
                    // so every candidate still ahead ranks after everything
                    // held. Without this, `LIMIT 10` reads the whole
                    // collection to answer with ten rows, and a page of a scan
                    // reads to the end of the collection to fill 8,192 rows.
                    if in_rank_order == RankWalk::Exact && winners.len() >= capacity {
                        break;
                    }
                }
                None
            };

        let geometry_driven = in_rank_order == RankWalk::Exact
            && matches!(driver, DriverCursor::Geometry(_));
        // Held, not committed, for the reason `after` and `run` are held: a
        // page that fails inside `emit_rows` would otherwise leave the seen
        // set advanced, and the retry would skip the entities the failed page
        // had marked -- so the first page comes back as a later slice of the
        // cell walk. One clone either way; only the moment it lands moves.
        let mut geometry_seen = match (geometry_driven, &driver) {
            (true, DriverCursor::Geometry(cursor)) => Some(cursor.seen.clone()),
            _ => None,
        };

        // A page that never had to name its worst entry is still in the order
        // the walk handed it over, and an EXACT walk hands it over in rank
        // order. That page is already sorted and sorting it again is 8,192
        // comparisons over 459 KB for an answer that cannot change.
        let ordered = in_rank_order == RankWalk::Exact && !winners.heaped();
        let mut winners = winners.into_vec();
        if !ordered {
            // A rank key ends in the entity id, so no two entries compare
            // equal and a stable sort is ordering something that cannot be
            // observed -- while allocating a scratch buffer the size of the
            // page to do it.
            winners.sort_unstable_by(|left, right| compare_rank(&left.key, &right.key, descending));
        }
        debug_assert!(
            winners
                .windows(2)
                .all(|pair| compare_rank(&pair[0].key, &pair[1].key, descending)
                    != Ordering::Greater),
            "a page returns its winners in rank order"
        );
        let has_more = winners.len() > wanted;
        // A geometry-driven page kept one candidate past what it returns, to
        // learn whether more exist. The next page re-opens at the last
        // RETURNED posting and must be allowed to admit that extra entity
        // again, so it must not count as seen.
        if let Some(seen) = geometry_seen.as_mut() {
            for extra in winners.iter().skip(wanted) {
                seen.remove(&extra.key.id.sequence);
            }
        }
        // Everything this walk ranked past the page it is returning. It was
        // ranked; the pages after this one take it from here rather than
        // opening the whole candidate stream again. `run_bounded` records
        // whether the walk filled the hold -- if it did, emptying the run is
        // not the end of the answer and a later page walks once more, from the
        // last row handed out.
        // Committed only once `emit_rows` has built the page, for the same
        // reason `after` is: a page that fails hands nothing back, so it must
        // leave no resume state behind either.
        let holds = winners.len() > wanted && self.keeps_a_run();
        let bounded = winners.len() == held;
        let mut hold = if holds {
            winners.split_off(wanted)
        } else {
            Vec::new()
        };
        winners.truncate(wanted);
        let rows = self.emit_rows(&mut winners, &mut meter)?;
        if let Some(seen) = geometry_seen {
            self.geometry_seen = seen;
        }
        if holds {
            hold.reverse();
            self.run_bounded = bounded;
            self.run = hold;
        }
        let work = meter.used;
        self.finish_page(rows, &winners, has_more, approximation, work)
        })();
        match result {
            Ok(page) => {
                self.nearest = nearest_walk;
                Ok(page)
            }
            Err(err) => {
                self.nearest = nearest_before;
                Err(err)
            }
        }
    }
}
