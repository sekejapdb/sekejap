//! The path automaton compiler: a path pattern with quantifiers, subpaths,
//! a path mode, a selector or a path variable, as the engine's
//! [`PathAutomaton`] (`docs/lang/GQL_PROFILE_DESIGN.md` §4.1, §4.3-§4.5).
//!
//! # The line
//!
//! The engine's automaton is a LINE of node positions (its states) joined
//! by links: an edge, or `Same` -- two positions that test ONE node, which
//! is how adjacent node patterns concatenate. A quantified run of the line
//! is a [`Repeat`]. [`layout`] lays a pattern out:
//!
//! ```text
//! (s)-[e]->{1,3}(t)            states  s  .  .  t      links  Same e Same
//!                              repeat  [1..=2] {1,3}
//! (s)((a)-[:r]->(b)){2,}(t)    states  s  a  b  t      links  Same r Same
//!                              repeat  [1..=2] {2,}
//! ((a)-[e]->(b))*              states  .  a  b  .      links  Same e Same
//! ```
//!
//! * A quantified edge is a body of two IMPLIED positions (`.` above) and
//!   the edge between them, so the labels and predicates written on its
//!   endpoints test the endpoints only, never an intermediate node
//!   (positional labels).
//! * The engine wants a position of its own before and after every repeat
//!   (one active counter, Q10): an implied position is added where the
//!   pattern writes none -- at either end, and between two repeats that
//!   touch. Zero iterations join the positions either side of the body on
//!   one node, so both tests apply to it (zero-length paths).
//! * A variable written inside a repeat is a GROUP variable: its slot is
//!   typed as a LIST of its elements (§4.5). The inline predicates and the
//!   `COST` written inside that repeat see one element, the current
//!   iteration's; everywhere else naming it is refused
//!   ([`super::expr::Lowering`]) -- including an inline predicate AFTER the
//!   repeat (owner decision); a horizontal aggregate folds it instead
//!   (`horizontal.rs`).
//!
//! # Admission (compile time)
//!
//! * an unbounded quantifier under `WALK` with no selector is refused: it
//!   enumerates walks without end (§4.2); `TRAIL`, `ACYCLIC` and every
//!   selector terminate on a finite graph;
//! * a selector and a path mode together, and a quantifier inside a
//!   quantified subpath, are refused by the parser (P1);
//! * `COST` is read only by `ANY CHEAPEST`, which needs one; and because
//!   the engine's cheapest search takes ONE cost (M4-C), its pattern
//!   crosses exactly one edge step (one edge, quantified or not) and the
//!   cost reads only that edge, constants and parameters (brief §8.2).
//!
//! # Placement (plan time)
//!
//! [`BoundPath::compile`] places each inline conjunct at the FIRST position
//! of the line where every variable it names is bound -- its own position,
//! or a later one it names -- as that position's node test or edge
//! predicate, so a predicate prunes during the search, before a selector
//! chooses. It never leaves the repeat it is written in.

use super::ast::{Expr, PathElement, PathPattern, PathPrefix, Quantifier};
use super::bind::{
    BoundPattern, Chain, EdgeOcc, NodeOcc, TypeRef, edge_occurrence, implied_node, node_occurrence,
};
use super::expr::{Conjunct, Ex, Lowering, conjuncts};
use super::plan::Planner;
use super::schema::{BindingSchema, PatternId, Provenance, SlotInfo};
use crate::{SqlError, SqlResult2};
use sekejap_core::collections::gql::{
    EdgeStep, NodeTest, PathAutomaton, PathLink, PathMode, PathSearch, Repeat, SlotId, ValueType,
};
use sekejap_core::collections::{Database, GraphContextId};

/// A path pattern laid out as a line, its slots allocated and its
/// expressions lowered: what the planner compiles.
pub(crate) struct BoundPath {
    /// The node positions; `nodes[0]` is where the search starts.
    pub(crate) nodes: Vec<NodeOcc>,
    /// `links[i]` joins `nodes[i]` and `nodes[i + 1]`.
    links: Vec<Link>,
    repeats: Vec<Repeat>,
    mode: PathMode,
    selector: Option<Selector>,
    /// The `COST` and the link whose edge it weighs.
    cost: Option<(usize, Ex)>,
    /// The path variable's slot.
    path: Option<SlotId>,
}

enum Link {
    Same,
    Edge(EdgeOcc),
}

impl BoundPath {
    /// Every expression the path holds before planning -- node and edge
    /// inline predicates and the `COST` -- for the planner's `$n` typing.
    pub(crate) fn exprs(&self) -> impl Iterator<Item = &Ex> {
        let nodes = self.nodes.iter().flat_map(|node| node.inline.iter().map(|c| &c.ex));
        let edges = self.links.iter().flat_map(|link| match link {
            Link::Edge(edge) => edge.inline.as_slice(),
            Link::Same => &[],
        });
        nodes
            .chain(edges.map(|c| &c.ex))
            .chain(self.cost.iter().map(|(_, ex)| ex))
    }

    /// Does the path cross an edge, or only stay on its start node?
    pub(crate) fn has_edges(&self) -> bool {
        self.links.iter().any(|link| matches!(link, Link::Edge(_)))
    }
}

#[derive(Clone, Copy)]
enum Selector {
    Any,
    Shortest,
    Cheapest,
}

/// A position of the line an inline expression was written at.
#[derive(Clone, Copy)]
enum Place {
    Node(usize),
    Link(usize),
}

impl Place {
    /// One order over states and links: state `i` is `2i`, link `i` is
    /// `2i + 1`, so a repeat's body is the positions `2 first ..= 2 last`.
    fn at(self) -> usize {
        match self {
            Self::Node(i) => 2 * i,
            Self::Link(i) => 2 * i + 1,
        }
    }
}

/// A laid-out path between the binder's passes: the inline predicates and
/// the `COST` are still the text's, lowered once every slot of the stage
/// exists.
pub(crate) struct Layout<'a> {
    path: BoundPath,
    /// Nodes and single edges only, with no path variable or prefix: the
    /// hop planner's [`Chain`].
    chain: bool,
    predicates: Vec<(Place, &'a Expr)>,
    cost: Option<(usize, &'a Expr)>,
}

/// Lay `pattern` out as a line (pass 1 of the binder): allocate its slots,
/// type its group variables, and refuse what the automaton cannot mean.
pub(crate) fn layout<'a>(
    db: &Database,
    schema: &mut BindingSchema,
    pattern: &'a PathPattern,
    id: PatternId,
    notices: &mut Vec<String>,
) -> SqlResult2<Layout<'a>> {
    let (mode, selector) = match pattern.prefix {
        None | Some(PathPrefix::Walk) => (PathMode::Walk, None),
        Some(PathPrefix::Trail) => (PathMode::Trail, None),
        Some(PathPrefix::Acyclic) => (PathMode::Acyclic, None),
        // A selector implies WALK (§4.3).
        Some(PathPrefix::Any) => (PathMode::Walk, Some(Selector::Any)),
        Some(PathPrefix::AnyShortest) => (PathMode::Walk, Some(Selector::Shortest)),
        Some(PathPrefix::AnyCheapest) => (PathMode::Walk, Some(Selector::Cheapest)),
    };
    let path = match &pattern.name {
        None => None,
        Some(name) => Some(schema.add(SlotInfo {
            name: Some(name.clone()),
            ty: ValueType::Path,
            provenance: Provenance::Element {
                pattern: id,
                position: 0,
            },
            nullable: false,
        })?),
    };
    let mut line = Line {
        db,
        schema,
        notices,
        id,
        written: 0,
        nodes: Vec::new(),
        links: Vec::new(),
        repeats: Vec::new(),
        first: 0,
        predicates: Vec::new(),
        costs: Vec::new(),
    };
    for element in &pattern.elements {
        match element {
            PathElement::Node(node) => line.node(node, false)?,
            PathElement::Edge(edge) => match edge.quantifier {
                None => line.edge(edge, false)?,
                Some(quantifier) => {
                    line.open()?;
                    line.implied()?;
                    line.edge(edge, true)?;
                    line.implied()?;
                    line.close(quantifier)?;
                }
            },
            PathElement::Group {
                elements,
                quantifier,
            } => {
                line.open()?;
                for inner in elements {
                    match inner {
                        PathElement::Node(node) => line.node(node, true)?,
                        PathElement::Edge(edge) => line.edge(edge, true)?,
                        PathElement::Group { .. } => {
                            unreachable!("the parser refuses a nested quantifier")
                        }
                    }
                }
                line.close(*quantifier)?;
            }
        }
    }
    line.finish()?;
    let Line {
        nodes,
        links,
        repeats,
        predicates,
        costs,
        ..
    } = line;

    if selector.is_none()
        && mode == PathMode::Walk
        && repeats.iter().any(|repeat| repeat.max.is_none())
    {
        return Err(SqlError::unsupported(
            "an unbounded quantifier (`*`, `+`, `{m,}`) under WALK, with no selector, enumerates walks without end: give it an upper bound, or write TRAIL or ACYCLIC (which end on a finite graph), or a selector (ANY, ANY SHORTEST, ANY CHEAPEST) (GQL profile §4.2)",
        ));
    }
    let edges = links
        .iter()
        .filter(|link| matches!(link, Link::Edge(_)))
        .count();
    let cost = match selector {
        Some(Selector::Cheapest) => {
            if edges != 1 {
                return Err(SqlError::unsupported(format!(
                    "ANY CHEAPEST here crosses {edges} edge steps: the profile's cheapest search takes ONE COST, so its pattern crosses exactly one edge step -- one edge, quantified or not -- which carries it (a COST per step is not built, GQL profile M4-C)"
                )));
            }
            match costs.as_slice() {
                [cost] => Some(*cost),
                _ => {
                    return Err(SqlError::unsupported(
                        "ANY CHEAPEST minimises a COST, and its edge has none: write it on the edge, `-[e IS t COST e.weight]->{1,8}` (brief §8.2)",
                    ));
                }
            }
        }
        _ => {
            if !costs.is_empty() {
                return Err(SqlError::unsupported(
                    "an edge COST is the weight ANY CHEAPEST minimises, and this path pattern has no ANY CHEAPEST selector: remove the COST, or write `ANY CHEAPEST` before the pattern",
                ));
            }
            None
        }
    };
    Ok(Layout {
        path: BoundPath {
            nodes,
            links,
            repeats,
            mode,
            selector,
            cost: None,
            path,
        },
        chain: pattern.is_chain(),
        predicates,
        cost,
    })
}

/// The line being laid out.
struct Line<'d, 'a> {
    db: &'d Database,
    schema: &'d mut BindingSchema,
    notices: &'d mut Vec<String>,
    id: PatternId,
    /// Elements written so far, for provenance: nodes and edges in written
    /// order, a subpath's inside flattened.
    written: u16,
    nodes: Vec<NodeOcc>,
    links: Vec<Link>,
    repeats: Vec<Repeat>,
    /// The first state of the repeat being laid out.
    first: usize,
    predicates: Vec<(Place, &'a Expr)>,
    costs: Vec<(usize, &'a Expr)>,
}

impl<'a> Line<'_, 'a> {
    fn provenance(&self) -> Provenance {
        Provenance::Element {
            pattern: self.id,
            position: self.written,
        }
    }

    /// Does the line end on a state (rather than being empty or ending on
    /// a link)?
    fn on_state(&self) -> bool {
        !self.nodes.is_empty() && self.nodes.len() == self.links.len() + 1
    }

    /// Does the line end on the last state of a repeat?
    fn on_repeat_end(&self) -> bool {
        self.on_state()
            && self
                .repeats
                .last()
                .is_some_and(|r| usize::from(r.last) + 1 == self.nodes.len())
    }

    /// A state; after a state, joined to it by `Same` (concatenation).
    fn push_state(&mut self, occ: NodeOcc) {
        if self.on_state() {
            self.links.push(Link::Same);
        }
        self.nodes.push(occ);
    }

    fn implied(&mut self) -> SqlResult2<()> {
        let occ = implied_node(self.schema, self.provenance())?;
        self.push_state(occ);
        Ok(())
    }

    fn node(&mut self, node: &'a super::ast::NodePattern, group: bool) -> SqlResult2<()> {
        let occ = node_occurrence(self.db, self.schema, node, self.provenance(), group)?;
        self.written += 1;
        self.push_state(occ);
        if let Some(predicate) = &node.where_ {
            self.predicates
                .push((Place::Node(self.nodes.len() - 1), predicate));
        }
        Ok(())
    }

    fn edge(&mut self, edge: &'a super::ast::EdgePattern, group: bool) -> SqlResult2<()> {
        // An edge leaves a position of its own: never the end of a repeat,
        // whose way out is `Same`.
        if !self.on_state() || (!group && self.on_repeat_end()) {
            self.implied()?;
        }
        let occ = edge_occurrence(
            self.db,
            self.schema,
            edge,
            self.provenance(),
            group,
            self.notices,
        )?;
        self.written += 1;
        let link = self.links.len();
        if let Some(predicate) = &edge.where_ {
            self.predicates.push((Place::Link(link), predicate));
        }
        if let Some(cost) = &edge.cost {
            self.costs.push((link, cost));
        }
        self.links.push(Link::Edge(occ));
        Ok(())
    }

    /// Start a repeat's body: after a state of its own, which is not the
    /// end of another repeat (one counter, Q10).
    fn open(&mut self) -> SqlResult2<()> {
        if !self.on_state() || self.on_repeat_end() {
            self.implied()?;
        }
        self.first = self.nodes.len();
        Ok(())
    }

    fn close(&mut self, quantifier: Quantifier) -> SqlResult2<()> {
        let last = self.nodes.len() - 1;
        if !self.links[self.first..last]
            .iter()
            .any(|link| matches!(link, Link::Edge(_)))
        {
            return Err(SqlError::unsupported(
                "a parenthesized path pattern repeats a subpath that crosses no edge: write at least one edge inside it",
            ));
        }
        let position = |at: usize| {
            u16::try_from(at).map_err(|_| {
                SqlError::unsupported("a path pattern of more than 65,535 node positions")
            })
        };
        self.repeats.push(Repeat {
            first: position(self.first)?,
            last: position(last)?,
            min: quantifier.lo,
            max: quantifier.hi,
        });
        Ok(())
    }

    /// The line ends on a state of its own.
    fn finish(&mut self) -> SqlResult2<()> {
        if !self.on_state() || self.on_repeat_end() {
            self.implied()?;
        }
        Ok(())
    }
}

impl Layout<'_> {
    /// Pass 2 of the binder: lower the inline predicates and the `COST`,
    /// each seeing the group variables of the repeat it is written in as
    /// one element.
    pub(crate) fn lower(self, lowering: &mut Lowering) -> SqlResult2<BoundPattern> {
        let Layout {
            mut path,
            chain,
            predicates,
            cost,
        } = self;
        for (place, predicate) in predicates {
            lowering.admitted = path.group_slots(place);
            let ex = lowering.predicate(predicate);
            lowering.admitted.clear();
            let inline = match place {
                Place::Node(i) => &mut path.nodes[i].inline,
                Place::Link(i) => &mut path.edge_mut(i).inline,
            };
            conjuncts(ex?, inline);
        }
        if let Some((link, cost)) = cost {
            lowering.admitted = path.group_slots(Place::Link(link));
            let ex = lowering.operand(cost);
            lowering.admitted.clear();
            let ex = ex?;
            let own = path.edge_mut(link).var;
            if let Some(other) = ex.refs().into_iter().find(|slot| Some(*slot) != own) {
                let name = lowering.schema.slot(other).described();
                return Err(SqlError::unsupported(format!(
                    "a COST reads only its own edge, constants and parameters (brief §8.2), and this one names {name}"
                )));
            }
            path.cost = Some((link, ex));
        }
        Ok(if chain {
            BoundPattern::Chain(path.into_chain())
        } else {
            BoundPattern::Path(path)
        })
    }
}

/// What the planner needs from [`BoundPath::compile`].
pub(crate) struct Compiled {
    pub(crate) automaton: PathAutomaton,
    /// Each link's edge types as written (`None` for a `Same` link or any
    /// type), looked up when an execution's tree is built.
    pub(crate) types: Vec<Option<Vec<TypeRef>>>,
    /// Each link's edge label as written, for `EXPLAIN`.
    pub(crate) labels: Vec<Option<String>>,
    pub(crate) search: PathSearch,
    /// Inline conjuncts that name only slots bound before the search: a
    /// filter right after the start's seed.
    pub(crate) start: Vec<Ex>,
    /// Inline conjuncts naming a variable a LATER pattern binds: evaluated
    /// after the patterns (only without a selector, outside a repeat).
    pub(crate) deferred: Vec<Conjunct>,
}

impl BoundPath {
    /// The repeat whose body holds line position `at` ([`Place::at`]).
    fn repeat_at(&self, at: usize) -> Option<usize> {
        self.repeats
            .iter()
            .position(|r| 2 * usize::from(r.first) <= at && at <= 2 * usize::from(r.last))
    }

    /// The slots written inside the repeat that holds `place`, if any.
    fn group_slots(&self, place: Place) -> Vec<SlotId> {
        let Some(r) = self.repeat_at(place.at()) else {
            return Vec::new();
        };
        let (first, last) = (
            usize::from(self.repeats[r].first),
            usize::from(self.repeats[r].last),
        );
        let nodes = self.nodes[first..=last].iter().map(|node| node.slot);
        let edges = self.links[first..last]
            .iter()
            .filter_map(|link| match link {
                Link::Edge(edge) => edge.var,
                Link::Same => None,
            });
        nodes.chain(edges).collect()
    }

    /// A chain's line as the hop planner takes it: a chain's nodes are
    /// joined by edges, never by `Same`.
    fn into_chain(self) -> Chain {
        let edges = self
            .links
            .into_iter()
            .map(|link| match link {
                Link::Edge(edge) => edge,
                Link::Same => unreachable!("a chain's nodes are joined by edges"),
            })
            .collect();
        Chain {
            nodes: self.nodes,
            edges,
        }
    }

    fn edge_mut(&mut self, link: usize) -> &mut EdgeOcc {
        match &mut self.links[link] {
            Link::Edge(edge) => edge,
            Link::Same => unreachable!("an expression is written on an edge link"),
        }
    }

    /// The engine's automaton, its expressions and hidden slots added to
    /// `planner`'s. The slots `planner` has bound are those bound before the
    /// search -- upstream, and by the seeds of `nodes[0]` (and of the last
    /// node, when `end_seeded`: a point-to-point search). The start's own
    /// labels are its seed's, so state 0 tests nothing: the conjuncts that
    /// land there are returned as [`Compiled::start`].
    pub(crate) fn compile(mut self, planner: &mut Planner, end_seeded: bool) -> SqlResult2<Compiled> {
        let bound = planner.bound.clone();
        let states = self.nodes.len();
        // The first position binding each slot the search binds.
        let mut first_at: Vec<(SlotId, usize)> = Vec::new();
        let mut note = |slot: SlotId, at: usize| {
            if !bound.contains(&slot) && !first_at.iter().any(|(s, _)| *s == slot) {
                first_at.push((slot, at));
            }
        };
        for i in 0..states {
            note(self.nodes[i].slot, Place::Node(i).at());
            if let Some(Link::Edge(EdgeOcc {
                var: Some(slot), ..
            })) = self.links.get(i)
            {
                note(*slot, Place::Link(i).at());
            }
        }
        // Each conjunct to the first position where all it names is bound.
        let mut written = Vec::new();
        for (i, node) in self.nodes.iter_mut().enumerate() {
            for conjunct in std::mem::take(&mut node.inline) {
                written.push((Place::Node(i).at(), conjunct));
            }
        }
        for (i, link) in self.links.iter_mut().enumerate() {
            if let Link::Edge(edge) = link {
                for conjunct in std::mem::take(&mut edge.inline) {
                    written.push((Place::Link(i).at(), conjunct));
                }
            }
        }
        let mut placed: Vec<Vec<Ex>> = (0..2 * states - 1).map(|_| Vec::new()).collect();
        let mut deferred = Vec::new();
        for (at, conjunct) in written {
            let mut target = at;
            let mut later = None;
            for slot in &conjunct.refs {
                if bound.contains(slot) {
                    continue;
                }
                match first_at.iter().find(|(s, _)| s == slot) {
                    Some((_, position)) => target = target.max(*position),
                    None => later = Some(*slot),
                }
            }
            let name = |slot: SlotId| planner.schema.slot(slot).described();
            if let Some(slot) = later {
                if self.selector.is_some() || self.repeat_at(at).is_some() {
                    return Err(SqlError::unsupported(format!(
                        "an inline predicate of this path pattern names {}, which a later pattern binds: a {} applies its inline predicates during the search, before that variable exists; write the condition in the MATCH's WHERE",
                        name(slot),
                        if self.selector.is_some() {
                            "selective pattern"
                        } else {
                            "quantifier"
                        },
                    )));
                }
                deferred.push(conjunct);
                continue;
            }
            if self.repeat_at(target) != self.repeat_at(at) {
                let (slot, _) = first_at
                    .iter()
                    .find(|(_, position)| *position == target)
                    .expect("the target is a slot's position");
                return Err(SqlError::unsupported(format!(
                    "an inline predicate written inside a quantifier names {}, which is bound after that quantifier: a condition inside a quantifier is tested at every iteration, so it can name only what is bound by then",
                    name(*slot)
                )));
            }
            placed[target].push(conjunct.ex);
        }
        let mut placed = placed.into_iter();
        let start = placed.next().expect("state 0");

        let mut tests = Vec::with_capacity(states);
        let mut links = Vec::with_capacity(states - 1);
        let mut types = Vec::with_capacity(states - 1);
        let mut labels = Vec::with_capacity(states - 1);
        let mut groups = Vec::new();
        let mut cost_ex = None;
        let element = |ty: &ValueType| match ty {
            ValueType::List(element) => (**element).clone(),
            ty => ty.clone(),
        };
        let occs = std::mem::take(&mut self.nodes);
        let mut link_occs = std::mem::take(&mut self.links).into_iter();
        for (i, node) in occs.into_iter().enumerate() {
            let test = if i == 0 {
                NodeTest::default()
            } else {
                let filter = planner.conjunction(placed.next().expect("a position per state"));
                let named = planner.schema.slot(node.slot).name.is_some();
                let pinned = end_seeded && i == states - 1;
                NodeTest {
                    labels: node.labels.map(|labels| labels.ids),
                    filter,
                    bind: (named || filter.is_some() || pinned).then_some(node.slot),
                }
            };
            if let Some(slot) = test.bind {
                if self.repeat_at(Place::Node(i).at()).is_some() {
                    groups.push((slot, element(&planner.schema.slot(slot).ty)));
                }
            }
            tests.push(test);
            let Some(link) = link_occs.next() else { break };
            let filter = planner.conjunction(placed.next().expect("a position per link"));
            let (link, written, label) = match link {
                Link::Same => (PathLink::Same, None, None),
                Link::Edge(edge) => {
                    let group = self.repeat_at(Place::Link(i).at()).is_some();
                    let costed = self.cost.as_ref().is_some_and(|(at, _)| *at == i);
                    let bind = match edge.var {
                        Some(slot) => Some(slot),
                        None if filter.is_some() || costed => {
                            let ty = ValueType::Edge(Box::new([]));
                            let ty = if group { ValueType::List(Box::new(ty)) } else { ty };
                            Some(planner.schema.add(SlotInfo::hidden(ty, edge.provenance.clone()))?)
                        }
                        None => None,
                    };
                    if let (Some(slot), true) = (bind, group) {
                        groups.push((slot, element(&planner.schema.slot(slot).ty)));
                    }
                    if costed {
                        cost_ex = self.cost.take().map(|(_, ex)| ex);
                    }
                    let step = EdgeStep {
                        // Both are set when the execution's tree is built:
                        // the names are looked up under its snapshot.
                        context: GraphContextId::BASE,
                        types: None,
                        direction: edge.direction.engine(false),
                        filter,
                        bind,
                    };
                    (PathLink::Edge(step), edge.types, edge.label)
                }
            };
            links.push(link);
            types.push(written);
            labels.push(label);
        }
        let search = match self.selector {
            None => PathSearch::Enumerate,
            Some(Selector::Any) => PathSearch::Any,
            Some(Selector::Shortest) => PathSearch::Shortest,
            Some(Selector::Cheapest) => {
                let cost = cost_ex.expect("the binder gives ANY CHEAPEST its one COST");
                PathSearch::Cheapest {
                    cost: planner.expr(cost),
                }
            }
        };
        Ok(Compiled {
            automaton: PathAutomaton {
                states: tests.into(),
                links: links.into(),
                repeats: self.repeats.into(),
                mode: self.mode,
                path: self.path,
                groups: groups.into(),
            },
            types,
            labels,
            search,
            start,
            deferred,
        })
    }
}

/// The slots `automaton` binds: what the operators after it can read.
pub(crate) fn binds(automaton: &PathAutomaton) -> Vec<SlotId> {
    let states = automaton.states.iter().filter_map(|test| test.bind);
    let links = automaton.links.iter().filter_map(|link| match link {
        PathLink::Edge(step) => step.bind,
        PathLink::Same => None,
    });
    states.chain(links).chain(automaton.path).collect()
}

#[cfg(test)]
mod tests {
    //! The exact automaton each construct compiles to, read from the plan's
    //! operator tree (`lang/tests/gql_automaton.rs` pins the parse and the
    //! refusals, `lang/tests/gql_paths.rs` the answers).

    use super::super::plan::GqlPlan;
    use crate::{ast, parser};
    use kernel::io::IoMode;
    use kernel::store::{Config, SyncMode};
    use sekejap_core::Kind;
    use sekejap_core::collections::gql::{
        OpSpec, PathAutomaton, PathLink, PathMode, PathSearch, Repeat, ValueType,
    };
    use sekejap_core::collections::{CollectionId, Database, Direction, EdgeTypeId};
    use tempfile::TempDir;

    struct Fixture {
        db: Database,
        site: CollectionId,
        r: EdgeTypeId,
        s: EdgeTypeId,
        _dir: TempDir,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let config = Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        };
        let mut db = Database::create(dir.path().join("g.sekejap"), config).unwrap();
        let site = db
            .create_collection(
                "site",
                vec![("w".to_owned(), Kind::Int)],
                Default::default(),
            )
            .unwrap();
        db.enable_graph().unwrap();
        let r = db.create_edge_type("r").unwrap();
        let s = db.create_edge_type("s").unwrap();
        db.commit().unwrap();
        Fixture {
            db,
            site,
            r,
            s,
            _dir: dir,
        }
    }

    /// Every path search of `body`'s plan, first to last, with the ops that
    /// run before each.
    fn searches(f: &Fixture, body: &str) -> Vec<(PathAutomaton, PathSearch, Vec<&'static str>)> {
        let text = format!("SELECT * FROM GRAPH_TABLE (base {body})");
        let ast::Stmt::Select(select) = parser::parse(&text).unwrap() else {
            panic!("a SELECT")
        };
        let ast::Source::Gql(graph) = select.source else {
            panic!("a GQL body")
        };
        let plan = GqlPlan::compile(&f.db, &graph, &mut Vec::new())
            .unwrap_or_else(|error| panic!("`{body}`: {error}"));
        let mut out = Vec::new();
        let mut op = plan.root().expect("every name resolved");
        let mut chain = Vec::new();
        loop {
            let (name, input) = match op {
                OpSpec::Unit { .. } => break,
                OpSpec::Seed { input, .. } => ("seed", input),
                OpSpec::Expand { input, .. } => ("expand", input),
                OpSpec::Filter { input, .. } => ("filter", input),
                OpSpec::Project { input, .. } => ("project", input),
                OpSpec::PathSearch {
                    input,
                    automaton,
                    search,
                    ..
                } => {
                    out.push((automaton.clone(), *search));
                    ("path", input)
                }
                other => panic!("an M2/M4 plan holds no {other:?}"),
            };
            chain.push(name);
            op = input;
        }
        chain.reverse();
        out.reverse();
        out.into_iter()
            .map(|(a, s)| (a, s, chain.clone()))
            .collect()
    }

    fn one(f: &Fixture, body: &str) -> (PathAutomaton, PathSearch, Vec<&'static str>) {
        let mut all = searches(f, body);
        assert_eq!(all.len(), 1, "`{body}`");
        all.pop().unwrap()
    }

    fn edge(link: &PathLink) -> &sekejap_core::collections::gql::EdgeStep {
        match link {
            PathLink::Edge(step) => step,
            PathLink::Same => panic!("a Same link"),
        }
    }

    fn shape(a: &PathAutomaton) -> String {
        a.links
            .iter()
            .map(|link| match link {
                PathLink::Same => "=",
                PathLink::Edge(_) => "e",
            })
            .collect()
    }

    #[test]
    fn a_quantified_edge_is_a_body_of_two_implied_positions() {
        let f = fixture();
        let (a, search, ops) = one(
            &f,
            "MATCH (s IS site WHERE s._key = 'n0')-[e IS r]->{1,3}(t IS site) RETURN t._key AS k",
        );
        assert_eq!(ops, ["seed", "path", "project"]);
        assert_eq!(search, PathSearch::Enumerate);
        assert_eq!(a.mode, PathMode::Walk);
        assert_eq!(shape(&a), "=e=");
        assert_eq!(
            *a.repeats,
            [Repeat {
                first: 1,
                last: 2,
                min: 1,
                max: Some(3)
            }]
        );
        // The start's labels are its seed's; the intermediates test nothing;
        // the end's label is tested at the end only (positional labels).
        assert!(a.states[0].labels.is_none() && a.states[0].bind.is_none());
        for implied in &a.states[1..3] {
            assert!(implied.labels.is_none() && implied.filter.is_none() && implied.bind.is_none());
        }
        assert_eq!(a.states[3].labels.as_deref(), Some(&[f.site][..]));
        assert!(a.states[3].bind.is_some());
        let step = edge(&a.links[1]);
        assert_eq!(step.types.as_deref(), Some(&[f.r][..]));
        assert_eq!(step.direction, Direction::Outgoing);
        // `e` is a group variable: a list of `r` edges outside the repeat.
        let e = step.bind.expect("e binds");
        assert_eq!(*a.groups, [(e, ValueType::Edge(Box::new([f.r])))]);
        assert_eq!(a.path, None);
    }

    #[test]
    fn a_subpath_repeats_as_a_whole_and_its_variables_are_groups() {
        let f = fixture();
        let (a, _, _) = one(
            &f,
            "MATCH TRAIL (s WHERE s._key = 'n0')((x IS site)-[:r]->(y)-[g:s]->(z)){2,}(t) RETURN t._key AS k",
        );
        assert_eq!(a.mode, PathMode::Trail);
        assert_eq!(shape(&a), "=ee=");
        assert_eq!(
            *a.repeats,
            [Repeat {
                first: 1,
                last: 3,
                min: 2,
                max: None
            }]
        );
        assert_eq!(a.states[1].labels.as_deref(), Some(&[f.site][..]));
        let (x, y, z) = (
            a.states[1].bind.unwrap(),
            a.states[2].bind.unwrap(),
            a.states[3].bind.unwrap(),
        );
        assert_eq!(
            edge(&a.links[1]).bind,
            None,
            "an anonymous edge binds nothing"
        );
        let g = edge(&a.links[2]).bind.unwrap();
        assert_eq!(edge(&a.links[2]).types.as_deref(), Some(&[f.s][..]));
        // In line order.
        let slots: Vec<_> = a.groups.iter().map(|(slot, _)| *slot).collect();
        assert_eq!(slots, [x, y, g, z]);
        assert_eq!(a.groups[0].1, ValueType::Node(Box::new([f.site])));
    }

    #[test]
    fn implied_positions_open_close_and_separate_repeats() {
        let f = fixture();
        // A subpath at both ends: an implied start and an implied end.
        let (a, _, ops) = one(&f, "MATCH ACYCLIC ((x)-[e]->(y))* RETURN 1 AS one");
        assert_eq!(ops, ["seed", "path", "project"]);
        assert_eq!(shape(&a), "=e=");
        assert_eq!(
            *a.repeats,
            [Repeat {
                first: 1,
                last: 2,
                min: 0,
                max: None
            }]
        );
        // Two repeats that touch get a position between them (one counter).
        let (a, _, _) = one(
            &f,
            "MATCH (s WHERE s._key = 'n0')-[:r]->{1,2}((x)-[:s]->(y)){0,1}-[:r]->(t) RETURN t._key AS k",
        );
        // s . . | . x y | . t: the implied position 3 separates the repeats,
        // and 6 lets the plain edge leave the second from a position of its
        // own.
        assert_eq!(shape(&a), "=e==e=e");
        assert_eq!(
            a.repeats
                .iter()
                .map(|r| (r.first, r.last))
                .collect::<Vec<_>>(),
            [(1, 2), (4, 5)]
        );
        assert_eq!(a.states.len(), 8);
        assert!(matches!(a.links[5], PathLink::Same) && matches!(a.links[6], PathLink::Edge(_)));
    }

    #[test]
    fn zero_length_and_every_quantifier_spelling() {
        let f = fixture();
        for (q, min, max) in [
            ("{0,0}", 0, Some(0)),
            ("{0}", 0, Some(0)),
            ("?", 0, Some(1)),
            ("{2,5}", 2, Some(5)),
            ("{4}", 4, Some(4)),
            ("*", 0, None),
            ("+", 1, None),
            ("{3,}", 3, None),
        ] {
            let (a, _, _) = one(
                &f,
                &format!("MATCH TRAIL (s WHERE s._key = 'n0')-[:r]->{q}(t) RETURN t._key AS k"),
            );
            assert_eq!((a.repeats[0].min, a.repeats[0].max), (min, max), "`{q}`");
        }
    }

    #[test]
    fn directions_and_the_path_variable() {
        let f = fixture();
        let (a, _, _) = one(
            &f,
            "MATCH p = (s WHERE s._key = 'n0')<-[:r]-(m)-[:s]-(t) RETURN t._key AS k",
        );
        assert_eq!(shape(&a), "ee");
        assert!(a.repeats.is_empty());
        assert_eq!(edge(&a.links[0]).direction, Direction::Incoming);
        assert_eq!(edge(&a.links[1]).direction, Direction::Both);
        assert!(a.path.is_some());
        // Without a prefix, a variable or a quantifier, a pattern is a chain
        // of hops, not a path search.
        assert!(
            searches(
                &f,
                "MATCH (s WHERE s._key = 'n0')-[:r]->(t) RETURN t._key AS k"
            )
            .is_empty()
        );
    }

    #[test]
    fn selectors_choose_the_search_and_a_far_key_runs_point_to_point() {
        let f = fixture();
        let body = |prefix: &str, cost: &str| {
            format!(
                "MATCH p = {prefix} (s WHERE s._key = 'n0')-[e IS r{cost}]->{{0,32}}(t WHERE t._key = 'n2') RETURN t._key AS k"
            )
        };
        let (a, search, ops) = one(&f, &body("ANY SHORTEST", ""));
        assert_eq!(search, PathSearch::Shortest);
        assert_eq!(a.mode, PathMode::Walk);
        // Both ends are key seeds, the far one first; the end state binds
        // its seeded slot, which makes the search point to point.
        assert_eq!(ops, ["seed", "seed", "path", "project"]);
        let end = a.states.last().unwrap();
        assert!(end.bind.is_some() && end.filter.is_none());
        assert_eq!(one(&f, &body("ANY", "")).1, PathSearch::Any);
        let (a, search, _) = one(&f, &body("ANY CHEAPEST", " COST e.w"));
        assert!(matches!(search, PathSearch::Cheapest { .. }));
        assert!(edge(&a.links[1]).bind.is_some());
        // An anonymous edge still binds, so the COST can be evaluated.
        let (a, _, _) = one(
            &f,
            "MATCH ANY CHEAPEST (s WHERE s._key = 'n0')-[IS r COST 2]->{1,4}(t) RETURN t._key AS k",
        );
        let step = edge(&a.links[1]);
        assert!(step.bind.is_some());
        assert_eq!(a.groups.len(), 1, "the hidden edge slot is a group slot");
    }

    #[test]
    fn inline_predicates_are_placed_where_what_they_name_is_bound() {
        let f = fixture();
        // Line: s . . m t -- links = e = f.
        let (a, _, ops) = one(
            &f,
            "MATCH (s WHERE s._key = 'n0' AND s.w > 0)-[e WHERE e.w > 0]->{1,2}(m WHERE m.w < t.w)-[f WHERE f.w > m.w]->(t) RETURN t._key AS k",
        );
        assert_eq!(shape(&a), "=e=e");
        // The start's own conjunct is a filter after its seed.
        assert_eq!(ops, ["seed", "filter", "path", "project"]);
        assert!(edge(&a.links[1]).filter.is_some(), "per iteration");
        assert!(
            a.states[3].filter.is_none(),
            "m's predicate names t, bound later"
        );
        assert!(a.states[4].filter.is_some(), "... so it runs at t");
        assert!(edge(&a.links[3]).filter.is_some());
        // A predicate on an anonymous inner node binds its hidden slot, a
        // group slot.
        let (a, _, _) = one(
            &f,
            "MATCH TRAIL (s WHERE s._key = 'n0')((x)-[:r]->(WHERE $1 > 0)){1,2}(t) RETURN t._key AS k",
        );
        assert!(a.states[2].bind.is_some() && a.states[2].filter.is_some());
        assert_eq!(a.groups.len(), 2);
    }
}
