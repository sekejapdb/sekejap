//! One walk per candidate driver: `DriverCursor` dispatch and the per-cursor
//! `next` implementations (entities, scalar, keys, text, spatial, nearest,
//! geometry, vector, quantized vector).
use super::*;

impl<'a> DriverCursor<'a> {
    /// Open the candidate stream for one page.
    ///
    /// `resume` is the previous page's last rank key, and is passed ONLY when
    /// the caller has established that this driver walks in the query's rank
    /// order (`PreparedQuery::driver_walks_in_rank_order`). Then the cursor
    /// opens at that key instead of at the start of the collection or posting
    /// range, so page k+1 costs what it emits rather than everything emitted
    /// so far. The resumed row is yielded once more and dropped by the
    /// `after` comparison in `next_page`: one candidate per page, where
    /// re-opening from the start costs one whole pass per page.
    pub(super) fn new(
        db: &'a Database,
        collection: CollectionId,
        plan: &DriverPlan,
        graph: &[Option<GraphAnswer>],
        needs: CursorNeeds,
        resume: Option<&RankKey>,
        descending: bool,
        nearest: Option<&'a mut crate::index::spatial::point::NearestWalk>,
        geometry_seen: HashSet<u64>,
        membership: &[MembershipSet],
    ) -> QueryResult<Self> {
        match plan {
            DriverPlan::Membership { position } => {
                let set = membership
                    .get(*position)
                    .ok_or_else(|| corrupt_query("boolean driver names no filter position"))?;
                if matches!(
                    set,
                    MembershipSet::Ineligible | MembershipSet::Unbuilt | MembershipSet::Overflow
                ) {
                    return Err(corrupt_query(
                        "a boolean driver opened before its membership set was built",
                    ));
                }
                // A resumed page re-yields the row the last one stopped on
                // and lets `next_page`'s own `after` comparison drop it --
                // the same shape the entity walk resumes with, and for the
                // same reason: a set is unique per sequence, so there is no
                // tie group whose still-owed members lie on the far side.
                let from = resume.map_or(0, |after| after.id.sequence);
                let at = match set {
                    MembershipSet::Ids(ids) => {
                        ids.partition_point(|sequence| *sequence < from) as u64
                    }
                    _ => from.max(1),
                };
                Ok(Self::Membership(MembershipCursor {
                    collection,
                    set: set.clone(),
                    certifies: *position,
                    at,
                    done: false,
                }))
            }
            DriverPlan::Entities => {
                let prefix = prefix(0x40, collection);
                let start = match resume {
                    Some(after) => row_key(after.id),
                    None => prefix.clone(),
                };
                let inner = db
                    .store()?
                    .range(&start)
                    .map_err(Error::from)
                    .map_err(QueryError::from)?;
                Ok(Self::Entities(EntityCursor {
                    inner,
                    prefix,
                    collection,
                    wants_row: needs.row,
                    done: false,
                }))
            }
            DriverPlan::Scalar {
                info,
                predicate,
                position,
            } => {
                let prefix = scalar_prefix(info.id);
                let walk = if descending {
                    match scalar_reverse_start(info, &prefix, predicate) {
                        None => ScalarWalk::Nothing,
                        Some(mut start) => {
                            if let Some(key) =
                                resume.and_then(|after| resume_scalar_reverse_key(info, after))
                            {
                                start = key;
                            }
                            match db.index_range_reverse(info, &start).map_err(QueryError::from)? {
                                Some(iter) => ScalarWalk::Reverse(iter),
                                None => ScalarWalk::Nothing,
                            }
                        }
                    }
                } else if matches!(predicate, EncodedScalarFilter::Empty) {
                    ScalarWalk::Nothing
                } else {
                    let mut start = prefix.clone();
                    if let Some(lower) = scalar_lower(predicate) {
                        start.extend_from_slice(lower);
                    }
                    if let Some(key) = resume.and_then(|after| resume_scalar_key(info, predicate, after))
                    {
                        start = key;
                    }
                    match db.index_range(info, &start).map_err(QueryError::from)? {
                        Some(iter) => ScalarWalk::Forward(iter),
                        None => ScalarWalk::Nothing,
                    }
                };
                // A posting key IS the predicate's proof: the walk yields an
                // entry only when `scalar_key_position` puts its value inside
                // the predicate, so re-reading the row to ask the same
                // question again costs a primary point-get per matched row and
                // can only give the same answer. The one value that does not
                // prove itself is the nullish sentinel, which a NULL and a
                // MISSING field share -- `scalar_filter_matches` tells those
                // apart and the index cannot -- so that key is left uncertified
                // per candidate below.
                let certifies = match (position, predicate) {
                    (
                        Some(position),
                        EncodedScalarFilter::Eq(_) | EncodedScalarFilter::Range { .. },
                    ) => Some(*position),
                    _ => None,
                };
                Ok(Self::Scalar(ScalarCursor {
                    walk,
                    prefix,
                    info: info.clone(),
                    predicate: predicate.clone(),
                    certifies,
                    wants_scalar: needs.scalar_key,
                    done: false,
                }))
            }
            DriverPlan::Graph { position } => {
                let answer = graph
                    .get(*position)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| corrupt_query("missing prepared graph result"))?;
                let ids = answer
                    .ids
                    .iter()
                    .copied()
                    .enumerate()
                    .filter(|(_, id)| id.collection == collection)
                    // A traversal hands its result over sorted, so under
                    // driver order a resumed page opens past the row the last
                    // one ended on: the same walk with its head cut off.
                    .filter(|(_, id)| match resume {
                        Some(after) => *id > after.id,
                        None => true,
                    })
                    // The reaching edge travels with the candidate when this
                    // query reads it (`docs/core/GRAPH_CONTRACT.md` §4.2): one
                    // `Arc` clone, shared with the traversal's own answer.
                    .map(|(at, id)| {
                        (
                            id,
                            needs.edge.then(|| answer.via.get(at).cloned().flatten()).flatten(),
                        )
                    })
                    .collect::<Vec<_>>();
                Ok(Self::Ids(ids.into_iter()))
            }
            DriverPlan::Spatial {
                info,
                predicate,
                position,
                ranges,
                ..
            } => {
                // A spatial posting is `prefix || cell || sequence` and the
                // merged Hilbert ranges are walked low cell to high, so the
                // walk ascends by `(cell, sequence)` from end to end. Under
                // driver order that IS the rank key, so the previous page's
                // last key is a posting key this cursor can open on: the
                // ranges wholly below it are finished and skipped, and the
                // one holding it opens at the posting itself. That posting is
                // yielded once more and dropped by the `after` comparison in
                // `next_page` -- one candidate per page, where re-opening at
                // the envelope's first cell costs a whole pass, and a whole
                // geodesic refine of it, per page.
                let prefix = crate::index::spatial::point::posting_prefix(info.id);
                let mut range = 0usize;
                let mut start = None;
                if let Some(RankKey {
                    value: RankValue::Cell(cell),
                    id,
                }) = resume
                {
                    while ranges
                        .get(range)
                        .is_some_and(|(_, hi)| *hi < u64::from(*cell))
                    {
                        range += 1;
                    }
                    // A cell that falls in the GAP between two ranges leaves
                    // the walk at the next range's own start; only a cell
                    // inside a range names a posting to open on.
                    if ranges
                        .get(range)
                        .is_some_and(|(lo, _)| *lo <= u64::from(*cell))
                    {
                        start = Some(crate::index::spatial::point::posting_key_at(
                            info.id,
                            *cell,
                            id.sequence,
                        ));
                    }
                }
                Ok(Self::Spatial(SpatialCursor {
                    db,
                    info: info.clone(),
                    predicate: *predicate,
                    position: *position,
                    prefix,
                    ranges: ranges.clone(),
                    range,
                    start,
                    inner: None,
                    done: false,
                }))
            }
            DriverPlan::Nearest {
                info, certifies, ..
            } => {
                let walk = nearest.ok_or_else(|| {
                    corrupt_query("nearest walk missing from prepared query")
                })?;
                // The page before this one kept one hit past what it returned;
                // hand it over again so the `after` comparison can drop it.
                if let Some(RankKey {
                    value: RankValue::Score(bits),
                    id,
                }) = resume
                {
                    walk.rewind_past(f64::from_bits(*bits), *id);
                }
                Ok(Self::Nearest(NearestCursor {
                    db,
                    index: info.id,
                    certifies: *certifies,
                    walk,
                }))
            }
            DriverPlan::Geometry {
                info,
                position: _,
                ranges,
                query_bbox,
                ..
            } => {
                let prefix = crate::index::spatial::geometry_index::posting_prefix(info.id);
                let mut range = 0usize;
                let mut start = None;
                if let Some(RankKey {
                    value: RankValue::GeomCell { level, cell },
                    id,
                }) = resume
                {
                    while ranges.get(range).is_some_and(|r| {
                        r.level < *level || (r.level == *level && r.hi < u64::from(*cell))
                    }) {
                        range += 1;
                    }
                    if ranges.get(range).is_some_and(|r| {
                        r.level == *level && r.lo <= u64::from(*cell)
                    }) {
                        start = Some(crate::index::spatial::geometry_index::posting_key_at(
                            info.id,
                            *level,
                            *cell,
                            id.sequence,
                        ));
                    }
                }
                Ok(Self::Geometry(GeometryCursor {
                    db,
                    info: info.clone(),
                    prefix,
                    ranges: ranges.clone(),
                    range,
                    start,
                    inner: None,
                    query_bbox: *query_bbox,
                    seen: geometry_seen,
                    done: false,
                }))
            }
            DriverPlan::Text { prepared, position } => {
                // The merge hands documents over in strictly ascending
                // sequence -- it says so and refuses a round that does not
                // advance -- so under an id ranking the previous page's last
                // key is a document number every term stream can be opened
                // at. Each stream seeks to it; nothing walks the postings the
                // earlier pages already emitted.
                let from = resume.map(|after| after.id.sequence);
                let mut streams = Vec::with_capacity(prepared.terms.len());
                for (term, expected) in prepared.terms.iter().zip(&prepared.dfs) {
                    streams.push(TextPostingCursor {
                        inner: match from {
                            Some(from) => crate::index::text::TermPostings::open_from(
                                db,
                                prepared.info.id,
                                term,
                                *expected,
                                from,
                            )?,
                            None => crate::index::text::TermPostings::open(
                                db,
                                prepared.info.id,
                                term,
                                *expected,
                            )?,
                        },
                        head: None,
                        done: false,
                    });
                }
                let carries = streams.len() <= INLINE_TEXT_TERMS;
                Ok(Self::Text(TextCursor {
                    streams,
                    collection: prepared.info.collection,
                    matching: prepared.matching,
                    position: *position,
                    source: match position {
                        Some(position) => TextSource::Filter(*position),
                        None => TextSource::Order,
                    },
                    carries,
                    previous: None,
                    initialized: false,
                    done: false,
                }))
            }
            DriverPlan::ExactVector { info } => {
                let prefix = crate::index::vector::exact::locator_prefix(info.id);
                let inner = db.store()?.range(&prefix).map_err(Error::from)?;
                Ok(Self::Vector(VectorCursor {
                    inner,
                    prefix,
                    info: info.clone(),
                    done: false,
                }))
            }
            DriverPlan::QuantizedVector { info } => {
                let prefix = crate::index::vector::quantized::entry_prefix(info.id);
                let inner = db.store()?.range(&prefix).map_err(Error::from)?;
                Ok(Self::QuantizedVector(QuantizedVectorCursor {
                    inner,
                    prefix,
                    info: info.clone(),
                    done: false,
                }))
            }
            DriverPlan::Keys { predicate, position } => {
                let prefix = prefix(0x20, collection);
                let mut start = prefix.clone();
                if let Some(lower) = scalar_lower(predicate) {
                    start.extend_from_slice(lower);
                }
                if let Some(key) = resume.and_then(|after| resume_key_walk(&prefix, after)) {
                    start = key;
                }
                let inner = db
                    .store()?
                    .range(&start)
                    .map_err(Error::from)
                    .map_err(QueryError::from)?;
                // A posting-membership-style certification: every entry this
                // cursor yields already passed `scalar_key_position` against
                // the predicate, so there is no candidate left for
                // `filters_match` to re-check -- see `CompiledFilter::Key`.
                let certifies = match (position, predicate) {
                    (Some(position), EncodedScalarFilter::Range { .. }) => Some(*position),
                    _ => None,
                };
                Ok(Self::Keys(KeysCursor {
                    inner,
                    prefix,
                    collection,
                    predicate: predicate.clone(),
                    certifies,
                    wants_key: needs.key,
                    done: matches!(predicate, EncodedScalarFilter::Empty),
                }))
            }
        }
    }

    pub(super) fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        match self {
            Self::Membership(cursor) => cursor.next(meter),
            Self::Entities(cursor) => cursor.next(meter),
            Self::Scalar(cursor) => cursor.next(meter),
            Self::Spatial(cursor) => cursor.next(meter),
            Self::Nearest(cursor) => cursor.next(meter),
            Self::Geometry(cursor) => cursor.next(meter),
            Self::Text(cursor) => cursor.next(meter),
            Self::Vector(cursor) => cursor.next(meter),
            Self::QuantizedVector(cursor) => cursor.next(meter),
            Self::Keys(cursor) => cursor.next(meter),
            Self::Ids(ids) => Ok(ids.next().map(|(id, edge)| Candidate {
                edge,
                ..Candidate::bare(id)
            })),
        }
    }

    /// Report back to the driver whether the candidate it just handed over
    /// survived the page's filters.
    ///
    /// Only the nearest walk has anything to do with the answer: it sizes its
    /// next ring from it (`NearestWalk::note`). Every other cursor's walk is
    /// fixed by its predicate and hears nothing.
    pub(super) fn note_kept(&mut self, kept: bool) {
        if let Self::Nearest(cursor) = self {
            cursor.walk.note(kept);
        }
    }
}

/// How many bits of a membership bitmap one cancellation poll and one
/// `Candidates` charge cover while the walk is scanning past ZERO bits: 4
/// KiB of bitmap. The walk's cost is a scan whether or not it finds members,
/// so this is the unit the scan is charged and interrupted in, the same way a
/// posting walk is charged per posting.
const MEMBERSHIP_SCAN_POLL_BITS: u64 = (4 << 10) * 8;

impl MembershipCursor {
    /// The next member, ascending.
    ///
    /// Nothing is read: the set is in memory and the postings that built it
    /// were charged when it was built. What the walk still owes is the
    /// cancellation check every other cursor makes per candidate, so a
    /// complement over a million-row collection is as interruptible as a
    /// posting range is.
    pub(super) fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        meter.check_cancelled()?;
        let sequence = match &self.set {
            MembershipSet::Ids(ids) => match ids.get(self.at as usize) {
                Some(sequence) => {
                    self.at += 1;
                    *sequence
                }
                None => {
                    self.done = true;
                    return Ok(None);
                }
            },
            MembershipSet::Bitmap(bits) => {
                let span = (bits.len() as u64).saturating_mul(8);
                let mut found = None;
                // Where the current run of zero bits started, so the poll
                // below is one per MEMBERSHIP_SCAN_POLL_BITS of scanning and
                // not one per member. A sparse complement over a 67-million
                // sequence collection is ~8 MiB of zero bits between two
                // members; without this the whole run happened inside one
                // uninterruptible, uncharged `next`.
                let mut polled_at = self.at;
                while self.at <= span {
                    let sequence = self.at;
                    if sequence.saturating_sub(polled_at) >= MEMBERSHIP_SCAN_POLL_BITS {
                        meter.check_cancelled()?;
                        meter.charge(WorkResource::Candidates, 1)?;
                        polled_at = sequence;
                    }
                    self.at += 1;
                    if membership_bitmap_contains(bits, sequence) {
                        found = Some(sequence);
                        break;
                    }
                }
                match found {
                    Some(sequence) => sequence,
                    None => {
                        self.done = true;
                        return Ok(None);
                    }
                }
            }
            _ => {
                self.done = true;
                return Err(corrupt_query(
                    "a boolean driver walked a membership set that was never built",
                ));
            }
        };
        Ok(Some(Candidate {
            satisfied_filter: Some(self.certifies),
            ..Candidate::bare(EntityId {
                collection: self.collection,
                sequence,
            })
        }))
    }
}

impl EntityCursor<'_> {
    pub(super) fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        meter.charge(WorkResource::PrimaryReads, 1)?;
        // Borrow the record out of the pinned leaf rather than draining a key
        // `Vec` and a value `Vec` per row out of it. The pinned-leaf borrow
        // ends with this block so the cursor can step.
        let (id, row) = {
            let Some((key, value)) = self
                .inner
                .peek_ref()
                .map_err(Error::from)
                .map_err(QueryError::from)?
            else {
                self.done = true;
                return Ok(None);
            };
            if !crate::collections::has_prefix(key, &self.prefix) {
                self.done = true;
                return Ok(None);
            }
            (
                crate::collections::row_id_after_prefix(key, self.prefix.len(), self.collection)?,
                if self.wants_row {
                    Some(value.to_vec())
                } else {
                    None
                },
            )
        };
        self.inner.step();
        Ok(Some(Candidate { row, ..Candidate::bare(id) }))
    }
}

impl ScalarCursor<'_> {
    pub(super) fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        // Which side of the predicate the walk has NOT reached yet, and which
        // side means it is finished. An ascending walk approaches from below;
        // a descending one approaches from above.
        let past = if self.walk.descending() {
            Ordering::Less
        } else {
            Ordering::Greater
        };
        loop {
            meter.charge(WorkResource::ScalarPostings, 1)?;
            // Same pull-cursor shape as the entity walk: peek into the pinned
            // leaf, decide, then step. The key and the (always empty) value of
            // a posting were a `Vec` each before.
            let decoded = {
                let Some((key, value)) = self
                    .walk
                    .peek()
                    .map_err(Error::from)
                    .map_err(QueryError::from)?
                else {
                    self.done = true;
                    return Ok(None);
                };
                if !key.starts_with(&self.prefix) {
                    self.done = true;
                    return Ok(None);
                }
                let suffix = &key[self.prefix.len()..];
                let value_len = scalar_key::width(&self.info.kind, suffix)?;
                let encoded = suffix
                    .get(..value_len)
                    .ok_or_else(|| corrupt_query("truncated scalar value key"))?;
                let position = scalar_key_position(&self.predicate, encoded);
                if position == past {
                    self.done = true;
                    return Ok(None);
                }
                match position {
                    Ordering::Equal => {
                        let mut at = self.prefix.len() + value_len;
                        let sequence = read_ordered(key, &mut at)?;
                        if at != key.len() || sequence == 0 || !value.is_empty() {
                            return Err(corrupt_query("scalar index entry"));
                        }
                        Some((
                            sequence,
                            if self.wants_scalar {
                                Some(encoded.to_vec())
                            } else {
                                None
                            },
                            encoded != NULLISH_SCALAR_KEY,
                        ))
                    }
                    // Not yet inside the predicate: keep walking.
                    _ => None,
                }
            };
            self.walk.step();
            let Some((sequence, encoded, proves_predicate)) = decoded else {
                continue;
            };
            return Ok(Some(Candidate {
                carried: encoded.map(|key| CarriedKey::Scalar(self.info.id, key)),
                satisfied_filter: self.certifies.filter(|_| proves_predicate),
                ..Candidate::bare(EntityId {
                    collection: self.info.collection,
                    sequence,
                })
            }));
        }
    }
}

impl KeysCursor<'_> {
    pub(super) fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        loop {
            meter.charge(WorkResource::KeyPostings, 1)?;
            let decoded = {
                let Some((key, value)) = self
                    .inner
                    .peek_ref()
                    .map_err(Error::from)
                    .map_err(QueryError::from)?
                else {
                    self.done = true;
                    return Ok(None);
                };
                if !crate::collections::has_prefix(key, &self.prefix) {
                    self.done = true;
                    return Ok(None);
                }
                let suffix = &key[self.prefix.len()..];
                let position = scalar_key_position(&self.predicate, suffix);
                if position == Ordering::Greater {
                    self.done = true;
                    return Ok(None);
                }
                match position {
                    Ordering::Equal => {
                        let mut at = 0;
                        let sequence = read_ordered(value, &mut at)?;
                        if at != value.len() || sequence == 0 {
                            return Err(corrupt_query("external-key mapping entry"));
                        }
                        Some((
                            sequence,
                            if self.wants_key {
                                Some(suffix.to_vec())
                            } else {
                                None
                            },
                        ))
                    }
                    // Below the predicate's lower bound: keep walking.
                    Ordering::Less => None,
                    Ordering::Greater => unreachable!("handled above"),
                }
            };
            self.inner.step();
            let Some((sequence, key_bytes)) = decoded else {
                continue;
            };
            return Ok(Some(Candidate {
                carried: key_bytes.map(CarriedKey::Key),
                satisfied_filter: self.certifies,
                ..Candidate::bare(EntityId {
                    collection: self.collection,
                    sequence,
                })
            }));
        }
    }
}

impl TextPostingCursor<'_> {
    fn advance<C: FnMut() -> bool>(&mut self, meter: &mut WorkMeter<'_, C>) -> QueryResult<()> {
        if self.done {
            return Ok(());
        }
        // One stream over both tiers: the packed segments a late build wrote
        // and the head rows written since, with head rows overriding.
        self.head = self
            .inner
            .next(&mut || meter.charge(WorkResource::TextPostings, 1))?;
        if self.head.is_none() {
            self.done = true;
        }
        Ok(())
    }
}

impl TextCursor<'_> {
    fn drain<C: FnMut() -> bool>(&mut self, meter: &mut WorkMeter<'_, C>) -> QueryResult<()> {
        for stream in &mut self.streams {
            while !stream.done {
                stream.advance(meter)?;
            }
        }
        Ok(())
    }

    pub(super) fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        if !self.initialized {
            for stream in &mut self.streams {
                stream.advance(meter)?;
            }
            self.initialized = true;
        }
        if self.streams.is_empty() {
            self.done = true;
            return Ok(None);
        }
        // The frequencies this document's streams are standing on, harvested
        // before the streams are advanced off them.
        let mut slots = [0u32; INLINE_TEXT_TERMS];
        let sequence = match self.matching {
            TextMatch::Any => {
                let Some(sequence) = self
                    .streams
                    .iter()
                    .filter_map(|stream| stream.head.map(|(sequence, _)| sequence))
                    .min()
                else {
                    self.done = true;
                    return Ok(None);
                };
                for (position, stream) in self.streams.iter_mut().enumerate() {
                    if stream.head.is_some_and(|(at, _)| at == sequence) {
                        if let Some(slot) = slots.get_mut(position) {
                            *slot = stream.head.unwrap().1;
                        }
                        stream.advance(meter)?;
                    }
                }
                sequence
            }
            TextMatch::All | TextMatch::Phrase => loop {
                if self.streams.iter().any(|stream| stream.done) {
                    self.drain(meter)?;
                    self.done = true;
                    return Ok(None);
                }
                let target = self
                    .streams
                    .iter()
                    .filter_map(|stream| stream.head.map(|(sequence, _)| sequence))
                    .max()
                    .ok_or_else(|| corrupt_query("initialized text stream has no head"))?;
                for stream in &mut self.streams {
                    while stream.head.is_some_and(|(sequence, _)| sequence < target) {
                        stream.advance(meter)?;
                    }
                }
                if self.streams.iter().any(|stream| stream.done) {
                    continue;
                }
                if self
                    .streams
                    .iter()
                    .all(|stream| stream.head.is_some_and(|(at, _)| at == target))
                {
                    for (position, stream) in self.streams.iter_mut().enumerate() {
                        if let Some(slot) = slots.get_mut(position) {
                            *slot = stream.head.unwrap().1;
                        }
                        stream.advance(meter)?;
                    }
                    break target;
                }
            },
        };
        // The merge emits documents in strictly ascending sequence -- every
        // stream is ascending (`TermPostings` refuses a posting that does not
        // advance) and each round takes the smallest or the common head and
        // then steps past it. The scorer's segment and norm windows amortize
        // against exactly this; if it ever stopped holding, they would go on
        // answering correctly but at the old per-document cost, so it is
        // cheaper to state it here than to discover it in a profile.
        if self.previous.is_some_and(|previous| previous >= sequence) {
            return Err(corrupt_query("text merge did not advance"));
        }
        self.previous = Some(sequence);
        Ok(Some(Candidate {
            // Phrase postings establish only distinct all-term candidacy. The
            // filter remains pending until authoritative primary refinement.
            satisfied_filter: if self.matching == TextMatch::Phrase {
                None
            } else {
                self.position
            },
            text: self.carries.then(|| TextFrequencies {
                source: self.source,
                len: self.streams.len() as u8,
                slots,
            }),
            ..Candidate::bare(EntityId {
                collection: self.collection,
                sequence,
            })
        }))
    }
}

impl SpatialCursor<'_> {
    pub(super) fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        loop {
            if self.range == self.ranges.len() {
                self.done = true;
                return Ok(None);
            }
            let (lo, hi) = self.ranges[self.range];
            if self.inner.is_none() {
                // `start` is the resumed posting, and only the first range
                // this cursor opens can have one.
                let start = match self.start.take() {
                    Some(key) => key,
                    None => {
                        let mut start = self.prefix.clone();
                        start.extend((lo as u32).to_be_bytes());
                        start
                    }
                };
                // An index whose tree is still empty has no postings at all,
                // in any range: finish rather than walk the remaining ranges.
                let Some(iter) = self.db.index_range(&self.info, &start)? else {
                    self.done = true;
                    return Ok(None);
                };
                self.inner = Some(iter);
            }
            meter.charge(WorkResource::SpatialPostings, 1)?;
            let Some(row) = self.inner.as_mut().unwrap().next() else {
                self.inner = None;
                self.range += 1;
                continue;
            };
            let (key, value) = row.map_err(Error::from)?;
            if !key.starts_with(&self.prefix) {
                self.inner = None;
                self.range += 1;
                continue;
            }
            let cell = key
                .get(self.prefix.len()..self.prefix.len() + 4)
                .ok_or_else(|| corrupt_query("spatial point posting key"))?;
            let cell = u32::from_be_bytes(cell.try_into().unwrap());
            let hilbert = u64::from(cell);
            if hilbert > hi {
                self.inner = None;
                self.range += 1;
                continue;
            }
            let (_, sequence, point) =
                crate::index::spatial::point::decode_posting(&self.prefix, &key, &value)?;
            let matches = match self.predicate {
                PointFilter::Bbox(bounds) => bounds.contains(point),
                PointFilter::Radius {
                    center,
                    radius_metres,
                } => within_radius(center, point, radius_metres).map_err(corrupt_query)?,
            };
            if !matches {
                continue;
            }
            return Ok(Some(Candidate {
                satisfied_filter: Some(self.position),
                // The cell this posting is filed under. A page ranked in the
                // driver's own order ranks by it; handing it over costs
                // nothing, because the key it came out of is decoded already.
                carried: Some(CarriedKey::Cell(self.info.id, cell, point)),
                ..Candidate::bare(EntityId {
                    collection: self.info.collection,
                    sequence,
                })
            }));
        }
    }
}

pub(super) struct NearestCursor<'a> {
    db: &'a Database,
    index: IndexId,
    certifies: Option<usize>,
    walk: &'a mut crate::index::spatial::point::NearestWalk,
}

impl NearestCursor<'_> {
    pub(super) fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        let mut budget_err = None;
        let hit = {
            let mut extra = || match meter.charge(WorkResource::SpatialPostings, 1) {
                Ok(()) => Ok(()),
                Err(QueryError::Database(err)) => Err(err),
                Err(QueryError::Cancelled) => Err(Error::Cancelled),
                Err(err) => {
                    budget_err = Some(err);
                    Err(invalid("query budget"))
                }
            };
            self.walk
                .next(self.db, &mut || false, &mut extra)
                .map_err(QueryError::from)
        };
        if let Some(err) = budget_err {
            return Err(err);
        }
        let Some(hit) = hit? else {
            return Ok(None);
        };
        Ok(Some(Candidate {
            satisfied_filter: self.certifies,
            carried: Some(CarriedKey::Distance(
                self.index,
                hit.point,
                hit.distance_metres,
            )),
            ..Candidate::bare(hit.id)
        }))
    }
}

impl GeometryCursor<'_> {
    pub(super) fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        loop {
            if self.range == self.ranges.len() {
                self.done = true;
                return Ok(None);
            }
            let GeomRange { level, lo, hi } = self.ranges[self.range];
            if self.inner.is_none() {
                let start = match self.start.take() {
                    Some(key) => key,
                    None => {
                        let mut start = self.prefix.clone();
                        start.push(level);
                        start.extend((lo as u32).to_be_bytes());
                        start
                    }
                };
                let Some(iter) = self.db.index_range(&self.info, &start)? else {
                    self.done = true;
                    return Ok(None);
                };
                self.inner = Some(iter);
            }
            meter.charge(WorkResource::SpatialPostings, 1)?;
            let Some(row) = self.inner.as_mut().unwrap().next() else {
                self.inner = None;
                self.range += 1;
                continue;
            };
            let (key, value) = row.map_err(Error::from)?;
            if !key.starts_with(&self.prefix) {
                self.inner = None;
                self.range += 1;
                continue;
            }
            let (post_level, cell, sequence, bbox) =
                crate::index::spatial::geometry_index::decode_posting(&self.prefix, &key, &value)?;
            if post_level != level {
                self.inner = None;
                self.range += 1;
                continue;
            }
            let hilbert = u64::from(cell);
            if hilbert > hi {
                self.inner = None;
                self.range += 1;
                continue;
            }
            if !bbox.intersects(&self.query_bbox) {
                continue;
            }
            if !self.seen.insert(sequence) {
                continue;
            }
            return Ok(Some(Candidate {
                // BoxF overlap is a candidate test, not a proof: the filter
                // is refined against the row. Do not certify.
                satisfied_filter: None,
                carried: Some(CarriedKey::GeomCell(self.info.id, level, cell)),
                ..Candidate::bare(EntityId {
                    collection: self.info.collection,
                    sequence,
                })
            }));
        }
    }
}

impl VectorCursor<'_> {
    pub(super) fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        meter.charge(WorkResource::VectorLocators, 1)?;
        let Some(row) = self.inner.next() else {
            self.done = true;
            return Ok(None);
        };
        let (key, value) = row.map_err(Error::from)?;
        if !key.starts_with(&self.prefix) {
            self.done = true;
            return Ok(None);
        }
        let mut at = self.prefix.len();
        let sequence = read_ordered(&key, &mut at)?;
        if at != key.len() || sequence == 0 {
            return Err(corrupt_query("exact vector locator key"));
        }
        Ok(Some(Candidate {
            carried: Some(CarriedKey::Vector(self.info.id, value)),
            ..Candidate::bare(EntityId {
                collection: self.info.collection,
                sequence,
            })
        }))
    }
}

impl QuantizedVectorCursor<'_> {
    pub(super) fn next<C: FnMut() -> bool>(
        &mut self,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<Candidate>> {
        if self.done {
            return Ok(None);
        }
        // One charge covers the compact entry probe and its referenced layout
        // validation. Terminal and prefix-boundary probes are charged too.
        meter.charge(WorkResource::VectorLocators, 1)?;
        let Some(row) = self.inner.next() else {
            self.done = true;
            return Ok(None);
        };
        let (key, value) = row.map_err(Error::from)?;
        if !key.starts_with(&self.prefix) {
            self.done = true;
            return Ok(None);
        }
        let mut at = self.prefix.len();
        let sequence = read_ordered(&key, &mut at)?;
        if at != key.len() || sequence == 0 {
            return Err(corrupt_query("quantized vector entry key"));
        }
        Ok(Some(Candidate {
            carried: Some(CarriedKey::Quantized(self.info.id, value)),
            ..Candidate::bare(EntityId {
                collection: self.info.collection,
                sequence,
            })
        }))
    }
}
