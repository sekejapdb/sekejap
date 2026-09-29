//! The one walk over one adjacency range: every edge of one
//! `(node, context, type, direction)`, in key order.
//!
//! Every walk over adjacency goes through it: the graph index's neighbour
//! reads, the one BFS hop both `traverse_bfs` and the query engine's graph
//! filter run (`Hop`), and the GQL `Expand` and path searches. One copy
//! means one place that knows how an edge key is laid out, where its id
//! segment sits (`docs/core/GRAPH_CONTRACT.md` §2.3), and what a reverse
//! posting may hold.
//!
//! It is a PULL cursor: [`AdjacencyCursor::next_posting`] hands out one
//! posting per call, borrowed from the pinned leaf, and allocates nothing.
//! A caller interleaves its own work between calls -- a meter charge, a
//! cancel poll, a visited-set test -- which a callback walk cannot give it.
//!
//! Finding a posting and reading it are two steps, [`Posting::edge`] being
//! the second, so a caller can bound or cancel its walk BEFORE a malformed
//! posting is decoded -- the order both traversals have always checked in.
//!
//! An outgoing posting is the PRIMARY one and its value is the edge's
//! property bag, so the bag comes free. An incoming posting is the reverse
//! marker, empty by construction: its bag lives in the primary posting, one
//! point read away (`docs/core/GRAPH_CONTRACT.md` §4.2), and the cursor
//! leaves that read to the caller, who decides whether it is needed and
//! charges it. A caller that does read primary postings drops the cursor
//! first: it holds a leaf pinned while it lives.
//!
//! A caller that must read between two stretches of one walk -- the GQL
//! `Expand`, which hands each edge's row downstream before it takes the next
//! -- [`AdjacencyCursor::pause`]s it instead: a [`PausedAdjacency`] holds the
//! last key handed out and no page, and [`PausedAdjacency::resume`] opens the
//! walk again strictly after that key.
//!
//! [`primary_posting`] is the one read of an edge's primary posting by its
//! identity: what an incoming posting leaves to its caller.

use super::{
    adjacent_from_tail, decode_properties, edge_key_id, edge_prefix_into, Direction, EdgeKey,
    EdgeTypeId, GraphContextId, GraphHeader, MAX_EDGE_PREFIX, PRIMARY_EDGE, REVERSE_EDGE,
};
use crate::collections::{corrupt, invalid, Database, EntityId, Result};
use kernel::btree::RangeIter;
use serde_json::Value;

/// A walk over the edges of one node, in one direction, of one context and
/// (optionally) one edge type, in key order: for one type that is far-node
/// order, and parallel edges of one tuple are adjacent, in id order.
pub struct AdjacencyCursor<'db> {
    walk: RangeIter<'db>,
    range: AdjacencyRange,
    /// The walk is standing on the posting the last call handed out, and
    /// steps past it on the next.
    handed_out: bool,
    /// The walk has left the range.
    done: bool,
    /// It left by reading a key past the range, not by finding the
    /// keyspace's end.
    read_past: bool,
}

/// What every posting of one range shares, which a posting's read needs.
struct AdjacencyRange {
    prefix: [u8; MAX_EDGE_PREFIX],
    len: usize,
    near: EntityId,
    incoming: bool,
    context: GraphContextId,
    edge_type: Option<EdgeTypeId>,
    header: GraphHeader,
}

/// One posting of the range, not yet read.
pub struct Posting<'c> {
    key: &'c [u8],
    value: &'c [u8],
    range: &'c AdjacencyRange,
}

/// One edge the cursor walked.
#[derive(Clone, Copy, Debug)]
pub struct AdjacentEdge<'c> {
    /// The STORED orientation, source to destination, whichever direction
    /// the walk went.
    pub key: EdgeKey,
    /// The edge's id within its tuple: 0 for a tuple-keyed edge.
    pub id: u64,
    /// The endpoint that is not the node the walk started at (for a
    /// self-loop, that node again).
    pub far: EntityId,
    /// The encoded bag of an outgoing posting; `None` for an incoming one.
    pub(super) bag: Option<&'c [u8]>,
}

impl<'db> AdjacencyCursor<'db> {
    /// The edges of `near` in `direction` (`Outgoing` or `Incoming`; `Both`
    /// is two walks and is refused), in `context`, of `edge_type` or of
    /// every type. The context and the type must be ones this graph has
    /// handed out.
    pub fn open(
        db: &'db Database,
        near: EntityId,
        direction: Direction,
        context: GraphContextId,
        edge_type: Option<EdgeTypeId>,
    ) -> Result<Self> {
        let header = db.graph_header()?;
        db.validate_query_ids(header, context, edge_type)?;
        Self::at(db, header, near, direction, context, edge_type)
    }

    /// [`AdjacencyCursor::open`] for a traversal that has already read the
    /// graph header and checked the context and the type against it, once
    /// for the whole walk rather than once per node.
    pub(crate) fn at(
        db: &'db Database,
        header: GraphHeader,
        near: EntityId,
        direction: Direction,
        context: GraphContextId,
        edge_type: Option<EdgeTypeId>,
    ) -> Result<Self> {
        let incoming = match direction {
            Direction::Outgoing => false,
            Direction::Incoming => true,
            Direction::Both => return Err(invalid("an adjacency cursor walks one direction")),
        };
        let tag = if incoming { REVERSE_EDGE } else { PRIMARY_EDGE };
        // The prefix is built on the stack: a heap `Vec` here was one
        // allocation per node per direction of a traversal.
        let mut prefix = [0u8; MAX_EDGE_PREFIX];
        let len = edge_prefix_into(&mut prefix, tag, near, Some(context), edge_type);
        let walk = db.store()?.range(&prefix[..len])?;
        Ok(Self {
            walk,
            range: AdjacencyRange {
                prefix,
                len,
                near,
                incoming,
                context,
                edge_type,
                header,
            },
            handed_out: false,
            done: false,
            read_past: false,
        })
    }

    /// The next posting of the range, or `None` once the walk has left it.
    /// One call is one turn of the walk: the call that returns `None` has
    /// read the first key past the range, or found the keyspace's end.
    pub fn next_posting(&mut self) -> Result<Option<Posting<'_>>> {
        if self.done {
            return Ok(None);
        }
        if self.handed_out {
            self.walk.step();
            self.handed_out = false;
        }
        let Self {
            walk,
            range,
            handed_out,
            done,
            read_past,
        } = self;
        match walk.peek_ref()? {
            // The near entity, the context and (when named) the type are the
            // prefix itself: `starts_with` proves the key carries exactly the
            // bytes that were built, so only the tail is read back out.
            Some((key, value)) if key.starts_with(&range.prefix[..range.len]) => {
                *handed_out = true;
                Ok(Some(Posting { key, value, range }))
            }
            past => {
                *done = true;
                *read_past = past.is_some();
                Ok(None)
            }
        }
    }

    /// Whether the walk, once it has left the range, read a row to learn
    /// it -- the first key past the range -- rather than finding the
    /// keyspace's end. A caller that polls once per row read polls for it.
    pub(crate) fn read_past_range(&self) -> bool {
        self.read_past
    }
}

/// A walk set down between two postings. It pins nothing, so the caller
/// may read anything while it waits.
pub struct PausedAdjacency {
    range: AdjacencyRange,
    /// The key of the last posting handed out, or `None` when the walk had
    /// handed out none: it resumes at the start of the range.
    after: Option<Vec<u8>>,
}

impl<'db> AdjacencyCursor<'db> {
    /// Set the walk down after the posting the last call handed out. A walk
    /// that has left its range has nothing to continue: its caller drops it.
    pub fn pause(mut self) -> Result<PausedAdjacency> {
        if self.done {
            return Err(invalid("a finished adjacency walk cannot be paused"));
        }
        let after = if self.handed_out {
            let (key, _) = self
                .walk
                .peek_ref()?
                .ok_or_else(|| corrupt("adjacency walk lost the posting it stood on"))?;
            Some(key.to_vec())
        } else {
            None
        };
        Ok(PausedAdjacency {
            range: self.range,
            after,
        })
    }
}

impl PausedAdjacency {
    /// The walk again, over `db` -- the database or snapshot it was paused
    /// on -- at the first posting after the last one handed out: the smallest
    /// key above `after` is `after` followed by a zero byte.
    pub fn resume(self, db: &Database) -> Result<AdjacencyCursor<'_>> {
        let walk = match self.after {
            Some(mut after) => {
                after.push(0);
                db.store()?.range(&after)?
            }
            None => db.store()?.range(&self.range.prefix[..self.range.len])?,
        };
        Ok(AdjacencyCursor {
            walk,
            range: self.range,
            handed_out: false,
            done: false,
            read_past: false,
        })
    }
}

impl<'c> Posting<'c> {
    /// The edge this posting names: its tuple in stored orientation, its id
    /// and its far endpoint, with the bag when the posting carries one.
    ///
    /// A reverse posting that is not empty, and a key whose tail does not
    /// parse or names a type or context the graph never handed out, is
    /// corruption.
    pub fn edge(self) -> Result<AdjacentEdge<'c>> {
        let range = self.range;
        // Nothing is read across the pair for STRUCTURE: both directions are
        // written in one transaction, so a committed snapshot cannot hold
        // half a pair, and `verify_indexed_source` is the tool that checks
        // pair consistency.
        if range.incoming && !self.value.is_empty() {
            return Err(corrupt("nonempty reverse edge marker"));
        }
        let (edge_type, far, id) = adjacent_from_tail(
            self.key,
            range.len,
            range.edge_type,
            range.context,
            range.header,
        )?;
        let (source, destination) = if range.incoming {
            (far, range.near)
        } else {
            (range.near, far)
        };
        Ok(AdjacentEdge {
            key: EdgeKey {
                source,
                context: range.context,
                edge_type,
                destination,
            },
            id,
            far,
            bag: (!range.incoming).then_some(self.value),
        })
    }
}

impl AdjacentEdge<'_> {
    /// The decoded property bag of an outgoing edge. `None` for an incoming
    /// one: its bag is in the primary posting, which the caller reads when it
    /// needs it.
    pub fn bag(&self) -> Result<Option<Value>> {
        self.bag.map(decode_properties).transpose()
    }
}

/// The primary posting of edge `id` of tuple `key`: its encoded property
/// bag, the one read an incoming posting leaves to its caller. The edge was
/// walked, or its reference made, in this same snapshot, so a missing
/// posting is corruption -- as a missing row is to every query candidate.
/// The primary posting of edge `(key, id)`, or `None` when there is no such
/// edge: one lookup, for a caller that knows where the edge would be.
pub(crate) fn primary_posting_if_any(db: &Database, key: EdgeKey, id: u64) -> Result<Option<Vec<u8>>> {
    Ok(db.store()?.get(&edge_key_id(PRIMARY_EDGE, key, id))?)
}

pub(crate) fn primary_posting(db: &Database, key: EdgeKey, id: u64) -> Result<Vec<u8>> {
    db.store()?
        .get(&edge_key_id(PRIMARY_EDGE, key, id))?
        .ok_or_else(|| corrupt("an edge's primary posting is missing"))
}
