//! The vector scans: the unfiltered and filtered compact scans, and the work
//! meter progress callbacks that bound them.
use super::*;

/// How many vector records a page-order scan may read, plus one.
///
/// The plus one is the whole point. The scan charges what it has read before
/// it reports its own ceiling, so stopping ONE record past the budget is what
/// turns "I have read as much as I am allowed" into a `BudgetExceeded` that
/// names the resource and its limit. A scan that stopped exactly at the
/// budget would charge exactly the budget and report a bare resource limit.
fn scan_ceiling<C: FnMut() -> bool>(
    meter: &mut WorkMeter<'_, C>,
    lanes_per_record: u64,
    per_record: WorkResource,
) -> usize {
    let mut cap = meter
        .remaining(WorkResource::Candidates)
        .min(meter.remaining(per_record));
    if lanes_per_record > 0 {
        cap = cap.min(meter.remaining(WorkResource::VectorLanes) / lanes_per_record);
    }
    usize::try_from(cap).unwrap_or(usize::MAX).saturating_add(1)
}

/// The charge hook a page-order vector scan calls every `SCAN_STEP` records.
///
/// `per_record` is the second resource one scored record costs beyond a
/// candidate: a sidecar for the exact scan, a locator for the quantized one,
/// whose compact entry carries the locator inline.
///
/// A budget refusal is stashed WHOLE rather than returned: it names its
/// resource, its limit and what was attempted, and the scan's error type has
/// nowhere to put any of that. The scan is stopped with `Cancelled` and the
/// caller reads the stash back before it looks at the scan's own result.
fn vector_scan_progress<'a, 'm, C: FnMut() -> bool>(
    meter: &'a mut WorkMeter<'m, C>,
    budget: &'a mut Option<QueryError>,
    lanes_per_record: u64,
    per_record: WorkResource,
) -> impl FnMut(crate::index::vector::exact::ScanStep) -> crate::collections::Result<()> + use<'a, 'm, C> {
    move |step| {
        let charged = match step {
            crate::index::vector::exact::ScanStep::Locators(records) => {
                meter.charge(WorkResource::VectorLocators, records)
            }
            crate::index::vector::exact::ScanStep::Scored(records) => meter
                .charge(WorkResource::Candidates, records)
                .and_then(|()| meter.charge(per_record, records))
                .and_then(|()| {
                    meter.charge(
                        WorkResource::VectorLanes,
                        records.saturating_mul(lanes_per_record),
                    )
                }),
        };
        match charged {
            Ok(()) => Ok(()),
            Err(QueryError::Cancelled) => Err(Error::Cancelled),
            Err(other) => {
                *budget = Some(other);
                Err(Error::Cancelled)
            }
        }
    }
}

/// The charge hook the FILTERED page-order quantized scan calls, where the
/// records READ and the records SCORED are different counts.
///
/// Every entry the walk steps over is a candidate it considered and a compact
/// entry it read, whether the filters admitted it or not, so `Locators` --
/// what the scan reports for entries read -- is charged as both. Only the
/// admitted ones have their int8 lanes decoded, so `Scored` is charged as
/// lanes alone. A refused entry therefore costs exactly what it really cost:
/// one candidate and one compact read, and no lane work.
fn filtered_vector_scan_progress<'a, 'm, C: FnMut() -> bool>(
    meter: &'a mut WorkMeter<'m, C>,
    budget: &'a mut Option<QueryError>,
    lanes_per_record: u64,
) -> impl FnMut(crate::index::vector::exact::ScanStep) -> crate::collections::Result<()> + use<'a, 'm, C> {
    move |step| {
        let charged = match step {
            crate::index::vector::exact::ScanStep::Locators(records) => meter
                .charge(WorkResource::Candidates, records)
                .and_then(|()| meter.charge(WorkResource::VectorLocators, records)),
            crate::index::vector::exact::ScanStep::Scored(records) => meter.charge(
                WorkResource::VectorLanes,
                records.saturating_mul(lanes_per_record),
            ),
        };
        match charged {
            Ok(()) => Ok(()),
            Err(QueryError::Cancelled) => Err(Error::Cancelled),
            Err(other) => {
                *budget = Some(other);
                Err(Error::Cancelled)
            }
        }
    }
}

impl PreparedQuery<'_> {
    /// This page's vector cursor: the `(distance, entity)` the previous page
    /// ended on, in the form the scans filter candidates by.
    ///
    /// A vector page ranks by score, so its cursor can only be a score key.
    /// Anything else is a prepared query whose order and cursor disagree.
    fn vector_after(&self) -> QueryResult<Option<crate::index::vector::exact::VectorAfter>> {
        match self.after.as_ref() {
            None => Ok(None),
            Some(RankKey {
                value: RankValue::Score(bits),
                id,
            }) => Ok(Some(crate::index::vector::exact::VectorAfter {
                distance: f64::from_bits(*bits),
                id: *id,
            })),
            Some(_) => Err(corrupt_query("vector page cursor is not a score")),
        }
    }
    /// Unfiltered exact/ANN order over its own driver: one page-order scan of
    /// sidecar or compact leaves instead of a per-entity locator lookup plus a
    /// per-entity sidecar get. Filters and a non-vector driver keep the
    /// candidate loop.
    ///
    /// Two things the scan does that the candidate loop used to do for it.
    /// The page CURSOR goes in, so the bounded heap is taken over the rows
    /// after it and page two is the next `held` rows rather than page one
    /// with its returned rows removed. And the BUDGET goes in, as a record
    /// ceiling and as a charge every `SCAN_STEP` records, so a candidate
    /// budget of 100 stops the walk at 100 instead of reading the collection
    /// and reporting the overrun afterwards.
    pub(super) fn unfiltered_vector_scan<C: FnMut() -> bool>(
        &self,
        held: usize,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<(Winners, Option<ApproximationDiagnostics>)>> {
        if !self.filters.is_empty() {
            return Ok(None);
        }
        match (&self.order, &self.driver) {
            (
                CompiledOrder::ExactVector {
                    info,
                    query,
                    query_norm,
                    metric,
                },
                DriverPlan::ExactVector { .. },
            ) => {
                let dim =
                    u64::try_from(crate::index::vector::exact::dimension(info)?).map_err(invalid_query)?;
                let after = self.vector_after()?;
                // One past what the budget allows, so the scan's own ceiling
                // trips only after the charge that names the exhausted
                // resource has been made.
                let ceiling = scan_ceiling(meter, dim, WorkResource::VectorSidecars);
                let mut budget = None;
                let scan = {
                    let mut progress =
                        vector_scan_progress(meter, &mut budget, dim, WorkResource::VectorSidecars);
                    crate::index::vector::exact::scan_exact_all(
                        self.db,
                        info,
                        query,
                        *query_norm,
                        *metric,
                        held,
                        after,
                        ceiling,
                        &mut progress,
                    )
                };
                if let Some(error) = budget {
                    return Err(error);
                }
                let hits = scan?;
                // The scan's own heap was bounded by `held`, so what it hands
                // back IS the page's winner set: reserving `held` here would
                // allocate a whole run's worth of rank keys to hold ten.
                let bound = hits.len().max(1);
                let mut winners = Winners::new();
                for hit in hits {
                    let entry = HeapEntry {
                        key: RankKey {
                            value: RankValue::Score(hit.distance.to_bits()),
                            id: hit.id,
                        },
                        descending: false,
                        row: None,
                        edge: None,
                    };
                    winners.push(bound, entry);
                }
                Ok(Some((winners, None)))
            }
            (
                CompiledOrder::ApproximateVector {
                    info,
                    query,
                    query_norm,
                    metric,
                    ef,
                },
                DriverPlan::QuantizedVector { .. },
            ) => {
                let dim = u64::try_from(crate::index::vector::quantized::dimension(info)?)
                    .map_err(invalid_query)?;
                let after = self.vector_after()?;
                let k = (*ef).min(held.max(1));
                let ceiling = scan_ceiling(meter, dim, WorkResource::VectorLocators);
                let mut budget = None;
                let result = {
                    let mut progress =
                        vector_scan_progress(meter, &mut budget, dim, WorkResource::VectorLocators);
                    self.db.scan_quantized(
                        info.id,
                        query,
                        *metric,
                        k,
                        *ef,
                        crate::index::vector::quantized::QuantizedVectorCandidates::All,
                        after,
                        ceiling,
                        usize::MAX,
                        &mut progress,
                    )
                };
                if let Some(error) = budget {
                    return Err(error);
                }
                let result = result?;
                // The shortlist was charged as it was scanned; the rerank's
                // sidecar gets are `reranked` and are charged here, once,
                // because `ef` already bounds them.
                let reranked = u64::try_from(result.reranked).map_err(invalid_query)?;
                meter.charge(WorkResource::VectorSidecars, reranked)?;
                meter.charge(WorkResource::VectorLanes, reranked.saturating_mul(dim))?;
                let bound = result.hits.len().max(1);
                let mut winners = Winners::new();
                for hit in result.hits {
                    let entry = HeapEntry {
                        key: RankKey {
                            value: RankValue::Score(hit.distance.to_bits()),
                            id: hit.id,
                        },
                        descending: false,
                        row: None,
                        edge: None,
                    };
                    winners.push(bound, entry);
                }
                let _ = query_norm;
                Ok(Some((
                    winners,
                    Some(ApproximationDiagnostics {
                        method: result.method,
                        ef: result.ef,
                        examined: result.examined,
                        reranked: result.reranked,
                    }),
                )))
            }
            _ => Ok(None),
        }
    }
    /// True when every filter of this page can be decided from an index
    /// alone, with no primary row and no posting probe per candidate:
    /// each one either has a built [`MembershipSet`] (a scalar equality or
    /// range walked out of its own postings, a point filter walked out of its
    /// cover) or is a [`CompiledFilter::Folded`] position, whose predicate a
    /// surviving position already carries.
    ///
    /// `Overflow` is not index-side: the set was abandoned, so the filter is
    /// back on the row-read path and the whole page must be too. Text,
    /// geometry, JSON and graph filters are never index-side here -- a text
    /// posting establishes candidacy but a phrase is settled against the row,
    /// a geometry posting's box is only a candidate test, and a JSON
    /// predicate has no index at all.
    fn vector_filters_are_index_side(&self) -> bool {
        self.filters
            .iter()
            .enumerate()
            .all(|(position, filter)| match filter {
                CompiledFilter::Folded { .. } => true,
                CompiledFilter::Scalar { .. } | CompiledFilter::Point { .. } => matches!(
                    self.membership[position],
                    MembershipSet::Ids(_) | MembershipSet::Bitmap(_)
                ),
                _ => false,
            })
    }
    /// FILTERED approximate order over its own driver: the same page-order
    /// compact scan [`unfiltered_vector_scan`] runs, with the filters tested
    /// index-side, per compact entry, BEFORE the entry is scored.
    ///
    /// What this replaces is the per-candidate path, where the quantized
    /// cursor hands over every entry of the collection one at a time -- a key
    /// and a value copied out of the leaf per entry -- and a non-driving
    /// equality answers each one with its own root-to-leaf posting descent.
    /// Here each filter's postings are walked ONCE into a `MembershipSet` (by
    /// `ensure_membership_sets`, before any page) and an entry the sets refuse
    /// is stepped over without decoding its int8 codes at all.
    ///
    /// `ef` semantics are unchanged: the shortlist is the best `ef` among the
    /// entries that PASSED the filters, and the rerank over it is the same one
    /// the unfiltered scan does. So this is the same answer the candidate loop
    /// produced, and at a selectivity where `ef` covers the matching rows it
    /// is the exact filtered top-k.
    ///
    /// [`unfiltered_vector_scan`]: Self::unfiltered_vector_scan
    pub(super) fn filtered_vector_scan<C: FnMut() -> bool>(
        &self,
        held: usize,
        meter: &mut WorkMeter<'_, C>,
    ) -> QueryResult<Option<(Winners, Option<ApproximationDiagnostics>)>> {
        if self.filters.is_empty() {
            return Ok(None);
        }
        let (info, query, metric, ef) = match (&self.order, &self.driver) {
            (
                CompiledOrder::ApproximateVector {
                    info,
                    query,
                    metric,
                    ef,
                    ..
                },
                DriverPlan::QuantizedVector { .. },
            ) => (info, query, *metric, *ef),
            _ => return Ok(None),
        };
        if !self.vector_filters_are_index_side() {
            return Ok(None);
        }
        // The sets this page's admission test consults, gathered once rather
        // than per entry. A folded position contributes nothing: its predicate
        // is already part of the surviving position's.
        let sets = self
            .filters
            .iter()
            .enumerate()
            .filter(|(_, filter)| !matches!(filter, CompiledFilter::Folded { .. }))
            .map(|(position, _)| &self.membership[position])
            .collect::<Vec<_>>();
        let admit = |sequence: u64| -> bool {
            sets.iter().all(|set| match set {
                MembershipSet::Ids(ids) => ids.binary_search(&sequence).is_ok(),
                MembershipSet::Bitmap(bits) => membership_bitmap_contains(bits, sequence),
                // `vector_filters_are_index_side` admitted this page, so every
                // set here is one of the two above. Refusing is the safe
                // answer for a set that is not, not admitting.
                _ => false,
            })
        };
        let dim = u64::try_from(crate::index::vector::quantized::dimension(info)?)
            .map_err(invalid_query)?;
        let after = self.vector_after()?;
        let k = ef.min(held.max(1));
        // Two ceilings, in the two currencies the filtered walk spends. The
        // READ ceiling is what a refused entry still costs: one candidate and
        // one compact read. The SCORED ceiling comes from the lane budget: a
        // refused entry decodes no lanes, so the lanes bound the entries the
        // filters admit, one past the budget so the charge that names the
        // exhausted resource is made before the scan's own limit trips.
        let ceiling = scan_ceiling(meter, 0, WorkResource::VectorLocators);
        let scored_ceiling = if dim > 0 {
            usize::try_from(meter.remaining(WorkResource::VectorLanes) / dim)
                .unwrap_or(usize::MAX)
                .saturating_add(1)
        } else {
            usize::MAX
        };
        let mut budget = None;
        let result = {
            let mut progress = filtered_vector_scan_progress(meter, &mut budget, dim);
            self.db.scan_quantized(
                info.id,
                query,
                metric,
                k,
                ef,
                crate::index::vector::quantized::QuantizedVectorCandidates::Admitted(&admit),
                after,
                ceiling,
                scored_ceiling,
                &mut progress,
            )
        };
        if let Some(error) = budget {
            return Err(error);
        }
        let result = result?;
        // The shortlist was charged as it was scanned; the rerank's sidecar
        // gets are `reranked` and are charged here, once, because `ef` already
        // bounds them.
        let reranked = u64::try_from(result.reranked).map_err(invalid_query)?;
        meter.charge(WorkResource::VectorSidecars, reranked)?;
        meter.charge(WorkResource::VectorLanes, reranked.saturating_mul(dim))?;
        let bound = result.hits.len().max(1);
        let mut winners = Winners::new();
        for hit in result.hits {
            let entry = HeapEntry {
                key: RankKey {
                    value: RankValue::Score(hit.distance.to_bits()),
                    id: hit.id,
                },
                descending: false,
                row: None,
                edge: None,
            };
            winners.push(bound, entry);
        }
        Ok(Some((
            winners,
            Some(ApproximationDiagnostics {
                method: result.method,
                ef: result.ef,
                examined: result.examined,
                reranked: result.reranked,
            }),
        )))
    }
}
