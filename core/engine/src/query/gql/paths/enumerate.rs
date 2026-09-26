//! `PathSearch::Enumerate`: every match of a path pattern, per input row
//! (`docs/lang/GQL_PROFILE_DESIGN.md` §4.2, §4.5, §4.7).
//!
//! # The search
//!
//! Depth first over product states `(node, automaton state, k)`, `k` being
//! the active repeat's completed iterations. The current path is a
//! shared-prefix [`PathRef`]: extending it allocates one step, and every
//! frame of the stack shares its parent's prefix. There is NO visited set,
//! on nodes or on product states: reaching D after one hop does not stop D
//! after two (brief §2, difference 2), and two sides of a diamond are two
//! matches (difference 1). What stops the search is the pattern's bounds
//! and, for an unbounded repeat, the path mode:
//!
//! * `Walk` -- no check (and no unbounded repeat: refused when the
//!   execution opens);
//! * `Trail` -- the new edge must not already be on THIS path;
//! * `Acyclic` -- the new node must not already be on THIS path.
//!
//! Each check walks the cons-list: O(length) per extension, O(L^2) per path
//! of length L. **Cost (named).** A per-path bitset is a later optimisation.
//!
//! On arrival at a state: the mode check (edge arrivals), then the tests
//! both searches make (`arrive.rs`). A group slot holds the current
//! iteration's element while the search runs, and the list of all of them
//! in the emitted row. The slots the frames on the stack wrote, and the
//! group elements they bound, are two stacks of the search, each frame
//! owning the entries above its marks: popping a frame truncates them back.
//!
//! An either-direction step walks outgoing edges, then incoming ones, and
//! skips a self-loop in the incoming walk: a self-loop is one crossing,
//! forward. Parallel edges are crossed one each.
//!
//! # Duplicates (design Q11)
//!
//! Two runs of the automaton that cross the SAME path with the SAME
//! bindings are one match; runs with different bindings are different
//! matches. A run is fixed by its path unless two repeats both have a
//! variable count ([`PathAutomaton::ambiguous`]), so only then does the
//! search keep the set of matches emitted for the current input row, held
//! as `sort_bytes` and dropped when the row's search ends.
//!
//! # Charges
//!
//! * `graph_edges`: 1 per turn of an adjacency walk, the end probe
//!   included, as `Expand` charges; predicates charge what they read.
//! * `path_states`: 1 per product state the search enters (a frame).
//! * `queue_entries`, held: 1 per frame on the stack, and 1 per edge walked
//!   into a frame's refill buffer (at most 256 each) until it is tried.
//! * `list_bytes`: each emitted group list, while the row is built.
//! * `sort_bytes`, held: the duplicate set, when one is kept.
//! * `binding_rows`: 1 per match.
//!
//! The stack and the duplicate set outlive a page, so they are charged
//! again to each page ([`Held`]). Between two refills a walk holds no page:
//! it is paused, because predicates and every operator downstream read
//! while the search waits.

use super::super::super::{QueryResult, WorkResource};
use super::super::ops::{node_in, refill, ExecCx, Held, Op, Operator, Range, REFILL_POSTINGS};
use super::super::value::{BindingRow, BindingValue, EdgeRef, NodeRef, PathRef, SlotId};
use super::arrive::{
    admit, complete_row, edge_step, labels_admit, link_ranges, moves, restore, Move, Undo,
};
use super::automaton::{PathAutomaton, PathMode};
use crate::collections::{EntityId, PausedAdjacency};
use crate::index::graph::Direction;
use std::collections::{HashSet, VecDeque};

pub(in super::super) struct Enumerate<'q> {
    input: Op<'q>,
    from: SlotId,
    automaton: &'q PathAutomaton,
    /// Per link, the adjacency ranges its edge step walks (none for `Same`).
    ranges: Box<[Box<[Range]>]>,
    /// Whether matches must be deduplicated (see the module docs).
    ambiguous: bool,
    /// The search from the current input row.
    run: Option<Run>,
    held: Held,
}

/// The search from one input row.
struct Run {
    /// The input row with the bindings of the frames on the stack written
    /// in: what every predicate is evaluated over.
    row: BindingRow,
    stack: Vec<Frame>,
    /// The slots the frames' arrivals wrote, with what they held before.
    undo: Undo,
    /// The group elements the frames' arrivals bound, in path order.
    events: Undo,
    /// The matches emitted for this input row, kept only when ambiguous.
    seen: HashSet<(PathRef, Box<[BindingValue]>)>,
    seen_bytes: u64,
}

/// One product state on the DFS stack.
struct Frame {
    node: EntityId,
    state: usize,
    k: u32,
    path: PathRef,
    /// Where this frame's entries start in `Run::undo` and `Run::events`.
    undo_from: usize,
    events_from: usize,
    todo: Todo,
    /// Queue entries charged for this frame: itself, plus its buffered
    /// edges not yet tried.
    held: u64,
}

enum Todo {
    /// An accepting state not yet emitted.
    Emit,
    /// The next move to try, an index into [`moves`] of the frame.
    Moves(usize),
    /// An edge move being walked.
    Walk(Walk),
}

struct Walk {
    link: usize,
    /// The move after this one.
    after: usize,
    /// The range being walked: an index into the link's ranges.
    range: usize,
    paused: Option<PausedAdjacency>,
    /// Admitted edges of the last refill not yet tried: the edge, the far
    /// node, and whether it is crossed source to destination.
    refill: VecDeque<(EdgeRef, EntityId, bool)>,
}

impl<'q> Enumerate<'q> {
    /// The search of `automaton`, which the caller has checked.
    pub(in super::super) fn new(input: Op<'q>, from: SlotId, automaton: &'q PathAutomaton) -> Self {
        Self {
            input,
            from,
            automaton,
            ranges: link_ranges(automaton),
            ambiguous: automaton.ambiguous(),
            run: None,
            held: Held::default(),
        }
    }

    /// Enter `state` at `node` from the top frame (or as the start, when the
    /// stack is empty), crossing `via` when given. Pushes a frame if every
    /// test holds.
    fn arrive(
        &mut self,
        cx: &mut ExecCx<'q, '_, '_>,
        state: usize,
        node: EntityId,
        via: Option<(usize, EdgeRef, bool)>,
        k: u32,
    ) -> QueryResult<()> {
        let a = self.automaton;
        let run = self.run.as_mut().expect("a search is running");
        let path = match run.stack.last() {
            Some(top) => top.path.clone(),
            None => PathRef::new(NodeRef(node)),
        };
        if let Some((_, edge, _)) = &via {
            let revisits = match a.mode {
                PathMode::Walk => false,
                PathMode::Trail => path.has_edge(edge),
                PathMode::Acyclic => path.has_node(NodeRef(node)),
            };
            if revisits {
                return Ok(());
            }
        }
        let (undo_from, events_from) = (run.undo.len(), run.events.len());
        let crossed = via.as_ref().map(|(link, edge, _)| (*link, edge));
        let admitted = admit(
            cx,
            a,
            &mut run.row,
            &mut run.undo,
            Some(&mut run.events),
            state,
            node,
            crossed,
        );
        if !admitted? {
            restore(&mut run.row, &mut run.undo, undo_from);
            run.events.truncate(events_from);
            return Ok(());
        }
        cx.meter.charge(WorkResource::PathStates, 1)?;
        self.held.charge(cx, WorkResource::QueueEntries, 1)?;
        run.stack.push(Frame {
            node,
            state,
            k,
            path: match via {
                Some((_, edge, forward)) => path.extend(edge, forward, NodeRef(node)),
                None => path,
            },
            undo_from,
            events_from,
            todo: if state + 1 == a.states.len() {
                Todo::Emit
            } else {
                Todo::Moves(0)
            },
            held: 1,
        });
        Ok(())
    }

    /// The match the top frame completes, or `None` for a duplicate.
    fn emit(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        let a = self.automaton;
        let run = self.run.as_mut().expect("a search is running");
        let path = run.stack.last().expect("an accepting frame").path.clone();
        let mut row = run.row.clone();
        let mut lists: Vec<Vec<BindingValue>> = vec![Vec::new(); a.groups.len()];
        for (slot, value) in &run.events {
            if let Some(at) = a.groups.iter().position(|(s, _)| usize::from(s.0) == *slot) {
                lists[at].push(value.clone());
            }
        }
        complete_row(cx, a, &mut row, lists, path.clone())?;
        if self.ambiguous {
            let bytes = BindingValue::Path(path.clone()).held_bytes() + row.held_bytes();
            let key = (path, row.slots.clone());
            if run.seen.contains(&key) {
                return Ok(None);
            }
            self.held.charge(cx, WorkResource::SortBytes, bytes)?;
            run.seen_bytes += bytes;
            run.seen.insert(key);
        }
        cx.meter.charge(WorkResource::BindingRows, 1)?;
        Ok(Some(row))
    }

    /// Walk the next refill of the top frame's current range into its
    /// buffer -- or, at the range's end, move to the next range.
    fn refill(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<()> {
        let a = self.automaton;
        let run = self.run.as_mut().expect("a search is running");
        let frame = run.stack.last_mut().expect("a walking frame");
        let Todo::Walk(walk) = &mut frame.todo else {
            unreachable!("a refill walks an edge move")
        };
        let step = edge_step(a, walk.link);
        let far_state = walk.link + 1;
        let range = self.ranges[walk.link][walk.range];
        let forward = range.0 == Direction::Outgoing;
        let (held, frame_held, buffer) = (&mut self.held, &mut frame.held, &mut walk.refill);
        let paused = refill(
            cx,
            frame.node,
            step.context,
            step.direction,
            range,
            step.bind.is_some(),
            walk.paused.take(),
            REFILL_POSTINGS,
            |far| labels_admit(a, far_state, far),
            |cx, edge, far| {
                held.charge(cx, WorkResource::QueueEntries, 1)?;
                *frame_held += 1;
                buffer.push_back((edge, far, forward));
                Ok(())
            },
        )?;
        if paused.is_none() {
            walk.range += 1;
        }
        walk.paused = paused;
        Ok(())
    }

    /// Drop the top frame, restoring the slots it wrote.
    fn pop(&mut self, cx: &mut ExecCx<'q, '_, '_>) {
        let run = self.run.as_mut().expect("a search is running");
        let frame = run.stack.pop().expect("a frame to pop");
        self.held
            .release(cx, WorkResource::QueueEntries, frame.held);
        restore(&mut run.row, &mut run.undo, frame.undo_from);
        run.events.truncate(frame.events_from);
    }

    /// The input row's search is over: drop its duplicate set.
    fn finish(&mut self, cx: &mut ExecCx<'q, '_, '_>) {
        if let Some(run) = self.run.take() {
            self.held
                .release(cx, WorkResource::SortBytes, run.seen_bytes);
        }
    }

    /// Start the search from `row`, or skip a row whose start is `Null`.
    fn start(&mut self, cx: &mut ExecCx<'q, '_, '_>, row: BindingRow) -> QueryResult<()> {
        let Some(start) = node_in(&row, self.from, "a path pattern's start")? else {
            return Ok(());
        };
        self.run = Some(Run {
            row,
            stack: Vec::new(),
            undo: Vec::new(),
            events: Vec::new(),
            seen: HashSet::new(),
            seen_bytes: 0,
        });
        self.arrive(cx, 0, start, None, 0)
    }
}

impl<'q> Operator<'q> for Enumerate<'q> {
    fn next(&mut self, cx: &mut ExecCx<'q, '_, '_>) -> QueryResult<Option<BindingRow>> {
        self.held.recharge(cx)?;
        loop {
            let Some(run) = self.run.as_mut() else {
                let Some(row) = self.input.next(cx)? else {
                    return Ok(None);
                };
                self.start(cx, row)?;
                continue;
            };
            let Some(top) = run.stack.last_mut() else {
                self.finish(cx);
                continue;
            };
            match &mut top.todo {
                Todo::Emit => {
                    top.todo = Todo::Moves(0);
                    if let Some(row) = self.emit(cx)? {
                        return Ok(Some(row));
                    }
                }
                Todo::Moves(next) => {
                    let (index, node, state, k) = (*next, top.node, top.state, top.k);
                    match moves(self.automaton, state, k).get(index).copied().flatten() {
                        None => self.pop(cx),
                        Some(Move::Same { to, k }) => {
                            top.todo = Todo::Moves(index + 1);
                            self.arrive(cx, to, node, None, k)?;
                        }
                        Some(Move::Edge { link }) => {
                            top.todo = Todo::Walk(Walk {
                                link,
                                after: index + 1,
                                range: 0,
                                paused: None,
                                refill: VecDeque::new(),
                            });
                        }
                    }
                }
                Todo::Walk(walk) => {
                    if let Some((edge, far, forward)) = walk.refill.pop_front() {
                        let (link, k) = (walk.link, top.k);
                        top.held -= 1;
                        self.held.release(cx, WorkResource::QueueEntries, 1);
                        self.arrive(cx, link + 1, far, Some((link, edge, forward)), k)?;
                    } else if walk.range < self.ranges[walk.link].len() {
                        self.refill(cx)?;
                    } else {
                        top.todo = Todo::Moves(walk.after);
                    }
                }
            }
        }
    }
}
