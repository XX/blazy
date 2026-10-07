//! The link layer: edges between nodes, drawn as curves rather than built as widgets.
//!
//! `rnd/architecture.md` §10.4 settles the shape: not widgets, but a separate layer of
//! cubic béziers batched into one stroke stream and recorded in a scene of its own,
//! re-recorded only when the topology or an endpoint moves.
//!
//! The reason it cannot be widgets is the reason Phase 0 exists. Virtualisation
//! bounded the cost of nodes by removing the off-screen ones from the widget tree
//! (§20.3). Links cannot leave the tree the same way: there are as many of them as
//! there are nodes, and one with a single endpoint on screen still has to be drawn.
//! A link that is a widget is a link that puts the linear cost back.
//!
//! # Which links get drawn
//!
//! Selected through the adjacency list rather than through a spatial index of their
//! own: a link is recorded when **either endpoint** lies in the recorded region. That
//! is exact for a graph whose edges are shorter than the region margin — a quarter of
//! a viewport — which is what a node editor is, and it makes a drag cheap, because the
//! links a moved node disturbs are exactly its own.
//!
//! It is not exact in general. A link whose two endpoints both lie outside the region
//! while the curve between them crosses the screen is not drawn. Indexing link
//! bounding boxes would fix it, and would cost a second index whose cells degenerate
//! as soon as one edge is long. The limitation is pinned by a test rather than left
//! to be discovered.
//!
//! A link too short to see is dropped from the set as well — see
//! [`retain_recorded`](LinkLayer::retain_recorded). It happens at selection time, so
//! the same set answers "what is drawn" and "what can be picked" and no new
//! invalidation is introduced.
//!
//! # How they are drawn, and what that costs the order
//!
//! All the recorded curves go into **one path per style** and are stroked with one
//! command; a `move_to` per link is what keeps them apart, and each subpath is capped
//! on its own (`kurbo::stroke` finishes the previous one on every `MoveTo`). A command
//! is charged in every frame it sits in the scene and costs fifteen times what the same
//! curve costs inside a shared one (§31.1), and the paint pass re-appends the whole
//! scene every frame whether or not anything changed — so N commands is the one thing a
//! link layer must not be.
//!
//! The price is that **the drawing order between links is not defined**. Curves are
//! grouped by style, not by index, so two overlapping links stack by group. Picking
//! is written to match: it takes any of the links under the pointer rather than
//! claiming the topmost one.

use std::collections::HashMap;

use masonry::kurbo::{BezPath, CubicBez, Point, Rect};
use masonry::peniko::Color;

/// A connection from an output of one node to an input of another.
///
/// Nodes by index into the canvas's node array, ports by their number on that side of the
/// node. [`Link::new`] connects port 0 to port 0, which is what a graph that knows nothing
/// about ports means — and with the default [`PortLayout`] it is drawn exactly as a link
/// was before ports existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    /// The node the curve leaves, by index.
    pub from: u32,
    /// The node the curve arrives at, by index.
    pub to: u32,
    /// The output of `from` the curve leaves.
    pub from_port: u16,
    /// The input of `to` the curve arrives at.
    pub to_port: u16,
}

impl Link {
    /// An edge between two nodes, from output 0 to input 0.
    pub fn new(from: usize, to: usize) -> Self {
        Self::between(from, 0, to, 0)
    }

    /// An edge from output `from_port` of `from` to input `to_port` of `to`.
    pub fn between(from: usize, from_port: u16, to: usize, to_port: u16) -> Self {
        Self {
            from: from as u32,
            to: to as u32,
            from_port,
            to_port,
        }
    }

    /// The same edge, written the other way round.
    ///
    /// A canvas names an edge whichever way it is written (`link_name`), because a graph
    /// that does not care about direction writes it either way; the ports go with their
    /// nodes.
    #[must_use]
    pub fn reversed(self) -> Self {
        Self {
            from: self.to,
            to: self.from,
            from_port: self.to_port,
            to_port: self.from_port,
        }
    }
}

/// Which side of a node a port is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[expect(
    clippy::exhaustive_enums,
    reason = "a port is an input or an output, and that is all"
)]
pub enum PortSide {
    /// On the left edge, where links arrive.
    Input,
    /// On the right edge, where links leave.
    Output,
}

/// How many ports a node has on each side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(clippy::exhaustive_structs, reason = "configuration, constructed by the application")]
pub struct Ports {
    /// Ports on the left edge.
    pub inputs: u16,
    /// Ports on the right edge.
    pub outputs: u16,
}

/// Where a node's ports are, as a rule rather than as a question to the application.
///
/// A rule because of where ports are needed: every link's curve ends at two of them, and
/// the canvas builds curves in bulk — thousands for the recorded set, in the far field
/// with no widget anywhere (§24, §31). A callback per endpoint would put the application
/// in the middle of that. What the application does say is *how many* ports a node has
/// (`NodeSource::ports`), and only when the pointer is near one.
#[derive(Clone, Copy, Debug, PartialEq)]
#[expect(
    clippy::exhaustive_structs,
    reason = "configuration, constructed with ..Default::default()"
)]
pub struct PortLayout {
    /// How far below the node's top edge the first port is, in canvas units; `None` puts
    /// every port at the middle of the edge, which is where a link ended before ports.
    pub first: Option<f64>,
    /// The distance between one port and the next, in canvas units.
    pub step: f64,
    /// The radius of the dot drawn at each port of a node that has a widget, in canvas
    /// units; zero draws none. Nodes without widgets — the far field — have no dots.
    pub dot_radius: f64,
    /// The colour of the dots.
    pub dot_color: Color,
}

impl Default for PortLayout {
    fn default() -> Self {
        Self {
            first: None,
            step: 0.0,
            dot_radius: 0.0,
            dot_color: Color::from_rgb8(0x9a, 0x9a, 0xa8),
        }
    }
}

impl PortLayout {
    /// Where port `port` on `side` of a node occupying `rect` is, in canvas coordinates.
    pub fn position(&self, rect: Rect, side: PortSide, port: u16) -> Point {
        let x = match side {
            PortSide::Input => rect.x0,
            PortSide::Output => rect.x1,
        };
        let y = match self.first {
            None => rect.center().y,
            Some(first) => rect.y0 + first + f64::from(port) * self.step,
        };
        Point::new(x, y)
    }

    /// Where `link` leaves `from` and arrives at `to`.
    pub(crate) fn ends(&self, link: Link, from: Rect, to: Rect) -> (Point, Point) {
        (
            self.position(from, PortSide::Output, link.from_port),
            self.position(to, PortSide::Input, link.to_port),
        )
    }
}

/// Topology, plus which of it is currently recorded.
#[derive(Default)]
pub(crate) struct LinkLayer {
    /// Edges by name. A removed edge leaves a hole rather than moving its neighbours,
    /// because its name is what `CanvasHit::Link` and the recorded set are written in
    /// — the rule §41.2 made for areas, applied to links.
    edges: Vec<Option<Link>>,
    /// Names a removal freed, for the next insertion to hand out again.
    free_edges: Vec<u32>,
    /// Live edges, so "is there anything to draw" costs nothing.
    live: usize,
    /// Edge names incident to each node, packed: `incident[offsets[n]..offsets[n+1]]`
    /// are node `n`'s edges **as of the last packing**.
    ///
    /// One array rather than a `Vec` per node: a vector per node is a vector per node,
    /// and on a million-node graph that is a million allocations to build and 24 MB of
    /// headers to hold, for lists that average two entries.
    ///
    /// The packing is a snapshot, not the truth: it used to be both, because the
    /// topology could not change after [`new`](Self::new). Edges added since live in
    /// [`extra`](Self::extra) and edges removed since are holes in `edges`, so reading a
    /// node's list means walking the packed slice, skipping holes, and then the extra
    /// list. Re-packing on every edit would make one edit cost the whole graph; this
    /// makes it cost what it touched, and the packing is redone when the extra lists
    /// have grown to a fraction of the graph (§43).
    offsets: Vec<u32>,
    incident: Vec<u32>,
    /// Edges added since the last packing, by node.
    extra: HashMap<u32, Vec<u32>>,
    /// Entries in `extra`, so the compaction threshold is a comparison rather than a walk.
    extra_len: usize,
    /// Times the packing has been redone.
    compactions: u64,
    /// Edge names walked by structural edits, so what an edit costs is a counter rather
    /// than an argument (§20.9).
    edit_scans: u64,
    /// The region the recorded set was chosen for, in canvas coordinates.
    region: Option<Rect>,
    /// Edges in the recorded scene, ascending and without duplicates.
    recorded: Vec<u32>,
    /// The canvas-space bounding box of each recorded curve, in step with `recorded`.
    ///
    /// Kept because it is computed anyway when the set is chosen, and because without it
    /// a pick rebuilds every recorded curve to find the one under the pointer — a set
    /// bounded by the *region*, which is what grows when the view pulls back (§28). It
    /// is a conservative filter: a box that is missing or stale can only cost a curve
    /// that would have been tested anyway.
    bounds: Vec<Rect>,
    /// Set when the recorded scene no longer matches the curves and must be redrawn.
    ///
    /// Distinct from [`reselect`](Self::reselect) on purpose: a node being dragged
    /// moves a curve without changing *which* curves are on screen, and conflating
    /// the two would re-choose the whole set on every frame of a drag.
    repaint: bool,
    /// Set when the recorded *set* is no longer the right one.
    reselect: bool,
    /// Links dropped by the last [`retain_recorded`](Self::retain_recorded).
    ///
    /// Published so the rule that drops them can be measured: a threshold nobody can
    /// see the effect of is a threshold nobody can check for vacuity (§20.9).
    hidden: usize,
    /// Times the set has been re-chosen.
    refreshes: u64,
    /// How much larger than the viewport the recorded region may be before the set is
    /// re-chosen. Follows the margin the canvas records with (`crate::region_slack`).
    slack: f64,
}

impl LinkLayer {
    pub(crate) fn new(edges: Vec<Link>, node_count: usize) -> Self {
        let live = edges.len();
        let edges: Vec<Option<Link>> = edges.into_iter().map(Some).collect();
        // A counting sort into one array: count each node's edges, prefix-sum the
        // counts into offsets, then fill. An edge naming a node outside the graph is
        // dropped here rather than rejected — the graph is the application's to
        // validate, and it is skipped when drawn for the same reason.
        let (offsets, incident) = pack(&edges, node_count);
        Self {
            edges,
            free_edges: Vec::new(),
            live,
            offsets,
            incident,
            extra: HashMap::new(),
            extra_len: 0,
            compactions: 0,
            edit_scans: 0,
            region: None,
            recorded: Vec::new(),
            bounds: Vec::new(),
            repaint: false,
            reselect: false,
            hidden: 0,
            refreshes: 0,
            slack: crate::region_slack(crate::FAR_OVERSCAN),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// The live edges incident to `node`, packed ones first.
    ///
    /// Holes are skipped here rather than compacted away, which is what keeps a removal
    /// from touching the graph: the name stays, the edge does not.
    fn incident(&self, node: usize) -> impl Iterator<Item = u32> + '_ {
        let packed = match (self.offsets.get(node), self.offsets.get(node + 1)) {
            (Some(&from), Some(&to)) => &self.incident[from as usize..to as usize],
            _ => &[][..],
        };
        packed
            .iter()
            .copied()
            .filter(|&edge| self.edges[edge as usize].is_some())
            .chain(self.extra.get(&(node as u32)).into_iter().flatten().copied())
    }

    /// The edges incident to `node`, live or not, as they are named.
    ///
    /// For a removal, which has to visit every one of them; the count is what the
    /// "a removal costs its own links" criterion is decided on.
    fn incident_names(&self, node: usize) -> Vec<u32> {
        self.incident(node).collect()
    }

    /// Times the packed adjacency has been rebuilt, and edge names walked by edits.
    pub(crate) fn edit_counters(&self) -> (u64, u64) {
        (self.compactions, self.edit_scans)
    }

    /// Adds an edge, and hands back the name it was filed under.
    ///
    /// The name comes from a removal if there is one to reuse, so the edge array grows
    /// with the number of links rather than with the number of edits a session has made.
    pub(crate) fn insert(&mut self, link: Link, node_count: usize) -> u32 {
        let name = match self.free_edges.pop() {
            Some(name) => {
                self.edges[name as usize] = Some(link);
                name
            },
            None => {
                self.edges.push(Some(link));
                (self.edges.len() - 1) as u32
            },
        };
        self.live += 1;
        for end in [link.from, link.to] {
            self.extra.entry(end).or_default().push(name);
            self.extra_len += 1;
        }
        self.edit_scans += 2;
        self.compact_if_due(node_count);
        // The set is chosen for a region, and a new edge may belong to it.
        self.reselect = true;
        self.repaint = true;
        name
    }

    /// Removes the edge named `name`, if it is live.
    pub(crate) fn remove(&mut self, name: u32) -> Option<Link> {
        let link = self.edges.get_mut(name as usize)?.take()?;
        self.live -= 1;
        self.free_edges.push(name);
        for end in [link.from, link.to] {
            if let Some(list) = self.extra.get_mut(&end) {
                self.edit_scans += list.len() as u64;
                if let Some(at) = list.iter().position(|&edge| edge == name) {
                    list.swap_remove(at);
                    self.extra_len -= 1;
                }
            }
        }
        if let Ok(at) = self.recorded.binary_search(&name) {
            self.recorded.remove(at);
            if at < self.bounds.len() {
                self.bounds.remove(at);
            }
        }
        self.repaint = true;
        Some(link)
    }

    /// The name of a live edge between `from` and `to`, in either direction.
    pub(crate) fn name_of(&self, link: Link) -> Option<u32> {
        self.incident(link.from as usize).find(|&name| {
            let edge = self.edges[name as usize].expect("incident lists only name live edges");
            edge == link || edge == link.reversed()
        })
    }

    /// Removes every edge incident to `node` and hands them back, named.
    ///
    /// What undo needs: a node comes back with the links it had, under the names it had
    /// (§41.2 again — a name nothing renumbers is what a view, a selection and a history
    /// are keyed by).
    pub(crate) fn remove_node(&mut self, node: usize) -> Vec<(u32, Link)> {
        let names = self.incident_names(node);
        self.edit_scans += names.len() as u64;
        names
            .into_iter()
            .filter_map(|name| self.remove(name).map(|link| (name, link)))
            .collect()
    }

    /// Puts an edge back under the name it had, for undo.
    pub(crate) fn restore(&mut self, name: u32, link: Link, node_count: usize) {
        if self.edges.len() <= name as usize {
            self.edges.resize(name as usize + 1, None);
        }
        if let Some(at) = self.free_edges.iter().position(|&free| free == name) {
            self.free_edges.swap_remove(at);
        }
        if self.edges[name as usize].is_none() {
            self.live += 1;
        }
        self.edges[name as usize] = Some(link);
        for end in [link.from, link.to] {
            self.extra.entry(end).or_default().push(name);
            self.extra_len += 1;
        }
        self.edit_scans += 2;
        self.compact_if_due(node_count);
        self.reselect = true;
        self.repaint = true;
    }

    /// Re-packs the adjacency once the extra lists have grown to a fraction of it.
    ///
    /// A fraction rather than a fixed count, so the amortised cost of an edit does not
    /// follow the graph: at a quarter, packing `E` edges is paid for by `E/4` edits.
    fn compact_if_due(&mut self, node_count: usize) {
        if self.extra_len * 4 < self.live.max(MIN_EXTRA_BEFORE_COMPACTION) {
            return;
        }
        let (offsets, incident) = pack(&self.edges, node_count.max(self.offsets.len().saturating_sub(1)));
        self.offsets = offsets;
        self.incident = incident;
        self.extra.clear();
        self.extra_len = 0;
        self.compactions += 1;
    }

    pub(crate) fn recorded(&self) -> &[u32] {
        &self.recorded
    }

    /// The bounding box of the recorded curve at `at`, if it has been measured.
    ///
    /// `None` between a selection and the measurement that follows it, which is the
    /// only moment the two lists can be out of step. A caller that gets `None` has to
    /// fall back to testing the curve.
    pub(crate) fn recorded_bounds(&self, at: usize) -> Option<Rect> {
        self.bounds.get(at).copied()
    }

    /// The edge named `index`, or `None` if a removal freed that name.
    pub(crate) fn edge(&self, index: u32) -> Option<Link> {
        self.edges.get(index as usize).copied().flatten()
    }

    pub(crate) fn refreshes(&self) -> u64 {
        self.refreshes
    }

    /// Links the last selection dropped as too short to see.
    pub(crate) fn hidden(&self) -> usize {
        self.hidden
    }

    /// Measures every recorded link, dropping the ones `measure` answers `None` for.
    ///
    /// Meant to be called straight after [`refresh`](Self::refresh), so a rule that
    /// depends on the zoom runs once per *selection* rather than once per frame. That
    /// is the whole trick: the set is then slightly stale between selections — a link
    /// that shrank below the threshold after the last one keeps being drawn for a
    /// while — and that is the affordable direction of the error. It also keeps
    /// drawing and picking honest for free, because both read this one set.
    ///
    /// The box that comes back is kept: the caller computes it to apply its rule, and a
    /// pick needs the same box to reject a curve without rebuilding it. Order is
    /// preserved, so `recorded` stays ascending and the boxes stay in step with it.
    pub(crate) fn measure_recorded(&mut self, mut measure: impl FnMut(Link) -> Option<Rect>) {
        let before = self.recorded.len();
        let edges = &self.edges;
        let bounds = &mut self.bounds;
        bounds.clear();
        self.recorded
            .retain(|&edge| match edges[edge as usize].and_then(&mut measure) {
                Some(rect) => {
                    bounds.push(rect);
                    true
                },
                None => false,
            });
        self.hidden = before - self.recorded.len();
    }

    /// Takes the "the scene must be redrawn" flag.
    pub(crate) fn take_repaint(&mut self) -> bool {
        std::mem::take(&mut self.repaint)
    }

    /// Marks the layer for re-recording because a node moved.
    ///
    /// Only if the node has links at all, and only if some of them are on screen: a
    /// drag in an empty corner of the graph should cost nothing.
    /// `bounds_of` re-measures a curve whose node has just moved: the stored boxes are
    /// what a pick rejects against, so a curve that moved without them would be
    /// unpickable until the next selection.
    pub(crate) fn node_moved(&mut self, node: usize, mut bounds_of: impl FnMut(Link) -> Rect) {
        if self.is_empty() {
            return;
        }
        for edge in self.incident_names(node) {
            let Ok(at) = self.recorded.binary_search(&edge) else {
                continue;
            };
            if let Some(box_of_a_curve) = self.bounds.get_mut(at)
                && let Some(link) = self.edges[edge as usize]
            {
                *box_of_a_curve = bounds_of(link);
            }
            self.repaint = true;
        }
    }

    /// Whether the recorded set has to be re-chosen for this viewport.
    pub(crate) fn needs_reselect(&self, live_rect: Rect) -> bool {
        !self.is_empty()
            && (self.reselect
                || !self
                    .region
                    .is_some_and(|region| crate::region_covers(region, live_rect, self.slack)))
    }

    /// Sets how much larger than the viewport the recorded region may be, which follows
    /// the margin it is recorded with (`crate::region_slack`).
    pub(crate) fn set_slack(&mut self, slack: f64) {
        self.slack = slack;
    }

    /// Re-chooses the recorded set if [`needs_reselect`](Self::needs_reselect) says so.
    /// `nodes` are the node indices inside `region`.
    ///
    /// Returns whether the set changed. The caller checks `needs_reselect` first to
    /// avoid the index query it would need to produce `nodes` at all; this re-checks
    /// rather than trusting it, because the two are far apart in the source and the
    /// cost of asking again is a rectangle comparison.
    pub(crate) fn refresh(&mut self, region: Rect, live_rect: Rect, nodes: &[usize]) -> bool {
        if !self.needs_reselect(live_rect) {
            return false;
        }
        self.reselect = false;

        // The recorded set is taken out so the incident lists can be read while it is
        // filled; both live in `self`.
        let mut recorded = std::mem::take(&mut self.recorded);
        recorded.clear();
        for &node in nodes {
            recorded.extend(self.incident(node));
        }
        self.recorded = recorded;
        // A link with both endpoints in the region is reached from each of them.
        self.recorded.sort_unstable();
        self.recorded.dedup();

        // The boxes belong to the set that has just been replaced; the caller's
        // `measure_recorded` fills them for the new one.
        self.bounds.clear();
        self.region = Some(region);
        self.hidden = 0;
        self.refreshes += 1;
        self.repaint = true;
        true
    }

    /// Drops the recorded set, so the next refresh rebuilds it.
    pub(crate) fn invalidate(&mut self) {
        self.region = None;
        self.reselect = true;
    }
}

/// Extra entries a graph must have accumulated before re-packing is worth it at all.
///
/// Without a floor, a graph of four links would re-pack on its first edit and every
/// second one after. The number is small enough to be free and big enough that building
/// a graph one link at a time does not pack on the way.
const MIN_EXTRA_BEFORE_COMPACTION: usize = 64;

/// Packs the live edges into offsets and incident lists, by node.
///
/// A counting sort into one array: count each node's edges, prefix-sum the counts into
/// offsets, then fill. An edge naming a node outside the graph is dropped here rather
/// than rejected — the graph is the application's to validate, and it is skipped when
/// drawn for the same reason.
fn pack(edges: &[Option<Link>], node_count: usize) -> (Vec<u32>, Vec<u32>) {
    let mut offsets = vec![0_u32; node_count + 1];
    let ends = |link: &Link| [link.from, link.to];
    for link in edges.iter().flatten() {
        for end in ends(link) {
            if (end as usize) < node_count {
                offsets[end as usize + 1] += 1;
            }
        }
    }
    for node in 0..node_count {
        offsets[node + 1] += offsets[node];
    }
    let mut incident = vec![0_u32; offsets[node_count] as usize];
    let mut cursor = offsets.clone();
    for (i, link) in edges.iter().enumerate() {
        let Some(link) = link else { continue };
        for end in ends(link) {
            if (end as usize) < node_count {
                let at = &mut cursor[end as usize];
                incident[*at as usize] = i as u32;
                *at += 1;
            }
        }
    }
    (offsets, incident)
}

/// The curve of a link from `start` to `end`.
///
/// A cubic with horizontal handles, which is what every node editor draws and what makes
/// two links between the same pair of columns distinguishable. From two points rather
/// than two nodes, so that the curve a link is drawn with and the one an editor previews
/// while a link is being dragged out of a port are the same curve.
///
/// The curve, not the path, is what both of the canvas's callers actually want: painting
/// strokes it and hit testing measures the distance to it. One function so that the two
/// can never disagree about where a link is — a pointer that picks a curve the eye does
/// not see there is worse than one that misses.
pub fn link_curve(start: Point, end: Point) -> CubicBez {
    // Handles scale with the gap so a short link does not loop and a long one does
    // not go slack.
    let reach = ((end.x - start.x).abs() * 0.5).max(24.0);

    CubicBez::new(
        start,
        Point::new(start.x + reach, start.y),
        Point::new(end.x - reach, end.y),
        end,
    )
}

/// Appends one link to a path being built for the whole batch.
///
/// A `move_to` rather than a `line_to`, which is what makes this a **new subpath**
/// instead of a continuation of the previous link: stroking finishes the subpath on
/// every `MoveTo` and caps it, so the two links never grow a segment joining them.
/// That is the only reason one command can carry thousands of unrelated curves.
pub(crate) fn push_link(path: &mut BezPath, start: Point, end: Point) {
    let curve = link_curve(start, end);
    path.move_to(curve.p0);
    path.curve_to(curve.p1, curve.p2, curve.p3);
}

/// Appends a link as a thin filled ribbon along its curve, `half` canvas units to either
/// side, flattened into `pieces` straight pieces.
///
/// Along the curve rather than its chord, and that was measured (§53): a link to a node
/// below loops out to the right and back, and its chord runs diagonally *under* the two
/// nodes it joins, so a chord ribbon showed 66 blocks of the picture where the curve shows
/// 888. One piece is the chord, which is what a link a pixel or two long needs.
///
/// Wound the same way whatever the link's direction — out along the left offset, back
/// along the right — so a batch filled with the non-zero rule never cancels where two
/// ribbons cross.
pub(crate) fn push_link_ribbon(path: &mut BezPath, start: Point, end: Point, half: f64, pieces: usize) {
    use masonry::kurbo::{ParamCurve, ParamCurveDeriv};
    let curve = link_curve(start, end);
    let tangent = curve.deriv();
    let pieces = pieces.max(1);
    let offset = |t: f64| {
        let d = tangent.eval(t).to_vec2();
        let length = d.hypot();
        if length <= f64::EPSILON {
            let chord = end - start;
            let l = chord.hypot().max(f64::EPSILON);
            return masonry::kurbo::Vec2::new(-chord.y, chord.x) * (half / l);
        }
        masonry::kurbo::Vec2::new(-d.y, d.x) * (half / length)
    };
    let at = |i: usize| i as f64 / pieces as f64;
    path.move_to(curve.eval(0.0) + offset(0.0));
    for i in 1..=pieces {
        let t = at(i);
        path.line_to(curve.eval(t) + offset(t));
    }
    for i in (0..=pieces).rev() {
        let t = at(i);
        path.line_to(curve.eval(t) - offset(t));
    }
    path.close_path();
}

/// How the canvas strokes its links.
///
/// Style rather than mechanism, like [`DetailThresholds`](crate::DetailThresholds): how a link should look
/// depends on the application, and baking it into the crate would mean editing this
/// file to retune a demo.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkStyle {
    /// Colour of an ordinary link.
    pub color: Color,
    /// Colour of the link under the pointer.
    pub hover_color: Color,
    /// Stroke width in canvas units, so links thicken with the zoom like everything
    /// else the canvas draws.
    ///
    /// Canvas units and not a minimum in screen pixels, which is a decision rather
    /// than an oversight (§31.3): a constant on-screen width means the *recorded*
    /// width depends on the zoom, and the scene is recorded in canvas coordinates
    /// precisely so that panning and zooming reuse it untouched. `imaging` has no
    /// non-scaling stroke — the transform is prepended to the whole draw — so buying
    /// a constant hairline means giving the scene a second axis of invalidation.
    pub width: f64,
    /// Below this on-screen length, a link is not drawn at all, in **logical pixels**.
    ///
    /// A curve two pixels long carries no information and still costs a subpath in
    /// every frame it is recorded for. Measured on the curve's bounding box, when the
    /// recorded set is chosen — see `CanvasContent::drop_short_links` for why there.
    ///
    /// The rule needs no "only when zoomed out" clause: an on-screen length grows
    /// with the zoom, so it stops firing on its own. Set to zero to switch it off.
    pub min_screen_length: f64,
    /// How far the pointer may miss a link and still pick it, in **screen pixels**.
    ///
    /// Screen pixels rather than canvas units, because the tolerance is about the
    /// pointer and not about the drawing: four canvas units are 0.08 px at the bottom
    /// of the zoom range and 32 px at the top, which would make a link unpickable
    /// exactly where it is thinnest (`rnd/architecture.md` §25.2).
    pub slop: f64,
    /// In the far field, draw each link as a thin filled ribbon along its curve rather
    /// than as a stroked curve.
    ///
    /// On the CPU rasteriser a stroked segment costs about seven times a filled one
    /// (§35.2), and a visible stroke far more: a one-pixel stroke made an overview of
    /// 5000 nodes cost 81 ms against 7 for the ribbon (§53). On the GPU path the ribbon
    /// costs 10–50% more than the stroke, which is the price of the default; an
    /// application that only ever draws on the GPU can turn this off.
    pub far_fill: bool,
    /// In the far field, the narrowest a link is drawn on screen, in pixels; zero draws
    /// it at [`width`](Self::width) canvas units whatever that comes to.
    ///
    /// Two canvas units at an overview zoom of 0.02 are 0.04 px: a line at four percent
    /// coverage, which no block of the frame shows (§53). A floor in pixels is what makes
    /// the graph's structure visible where the overview exists to show it.
    pub far_min_width_px: f64,
}

impl Default for LinkStyle {
    fn default() -> Self {
        Self {
            color: Color::from_rgb8(0x8a, 0x8a, 0x96),
            hover_color: Color::from_rgb8(0xd8, 0xd8, 0xe4),
            width: 2.0,
            min_screen_length: 2.0,
            slop: blazy_shape::DEFAULT_SLOP,
            // On by default, both: measured, an overview drawn the old way spends most of a
            // CPU frame on links nobody can see (§53).
            far_fill: true,
            far_min_width_px: 0.5,
        }
    }
}

#[cfg(test)]
mod tests {
    use masonry::kurbo::Size;

    use super::*;

    /// A link between two node rectangles, ending where the default layout puts ports.
    fn push_between(path: &mut BezPath, from: Rect, to: Rect) {
        let (start, end) = PortLayout::default().ends(Link::new(0, 1), from, to);
        push_link(path, start, end);
    }

    fn rect(x: f64, y: f64) -> Rect {
        Rect::from_origin_size(Point::new(x, y), Size::new(100.0, 50.0))
    }

    /// A chain of five nodes, each linked to the next.
    fn chain() -> LinkLayer {
        LinkLayer::new((0..4).map(|i| Link::new(i, i + 1)).collect(), 5)
    }

    #[test]
    fn a_link_is_recorded_from_either_end() {
        let mut layer = chain();
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);

        // Only node 2 is in the region; both of its links come along.
        layer.refresh(region, region, &[2]);
        assert_eq!(layer.recorded(), &[1, 2]);
    }

    #[test]
    fn a_link_reachable_from_both_ends_is_recorded_once() {
        let mut layer = chain();
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);
        layer.refresh(region, region, &[1, 2]);
        assert_eq!(layer.recorded(), &[0, 1, 2]);
    }

    /// The documented limitation, pinned so it is a decision rather than a surprise.
    #[test]
    fn a_link_with_both_ends_outside_the_region_is_not_recorded() {
        let mut layer = chain();
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);
        layer.refresh(region, region, &[]);
        assert!(layer.recorded().is_empty(), "nothing in the region, nothing drawn");
    }

    /// Dragging a node must redraw the curves without re-choosing which curves are on
    /// screen: at 60 frames a second, the difference is between touching one node's
    /// links and re-walking every node in the region.
    #[test]
    fn a_drag_repaints_without_reselecting() {
        let mut layer = chain();
        // The proportions a viewport really produces: the region is the visible rect
        // plus a quarter of it on each side (§35.2).
        let visible = Rect::new(0.0, 0.0, 100.0, 100.0);
        let region = visible.inflate(25.0, 25.0);
        layer.refresh(region, visible, &[0, 1, 2]);
        layer.take_repaint();
        let refreshes = layer.refreshes();

        for _ in 0..10 {
            layer.node_moved(1, |_| Rect::ZERO);
            assert!(layer.take_repaint());
            assert!(!layer.refresh(region, visible, &[0, 1, 2]));
        }
        assert_eq!(layer.refreshes(), refreshes, "a drag re-chose the recorded set");
    }

    /// Panning inside the region must not re-choose the set — the same trick the far
    /// field uses, and the reason a link scene can be reused across frames.
    #[test]
    fn staying_inside_the_region_does_not_refresh() {
        let mut layer = chain();
        let visible = Rect::new(0.0, 0.0, 100.0, 100.0);
        let region = visible.inflate(50.0, 50.0);
        layer.refresh(region, visible, &[0, 1]);
        layer.take_repaint();
        let after_first = layer.refreshes();

        assert!(!layer.refresh(region, Rect::new(10.0, 10.0, 110.0, 110.0), &[0, 1]));
        assert_eq!(layer.refreshes(), after_first);
    }

    /// A region chosen while zoomed out must not outlive the zoom that produced it.
    ///
    /// It contains every viewport that follows, so containment alone would keep it
    /// forever — and with it every edge it recorded. This is §28, and it is what made
    /// a canvas stay slow after one look at the whole graph.
    #[test]
    fn zooming_back_in_re_chooses_the_set() {
        let mut layer = chain();
        let wide = Rect::new(-5000.0, -5000.0, 5000.0, 5000.0);
        layer.refresh(wide, wide, &[0, 1, 2]);
        layer.take_repaint();
        assert_eq!(layer.recorded().len(), 3, "the whole graph is recorded");

        let close = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(
            layer.needs_reselect(close),
            "a region ten thousand times the viewport does not serve it"
        );
        assert!(layer.refresh(close.inflate(50.0, 50.0), close, &[0]));
        assert_eq!(layer.recorded().len(), 1, "and the set shrank with the viewport");
    }

    #[test]
    fn leaving_the_region_refreshes() {
        let mut layer = chain();
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);
        layer.refresh(region, region, &[0]);
        assert!(layer.refresh(region, Rect::new(500.0, 500.0, 600.0, 600.0), &[3]));
        assert_eq!(layer.recorded(), &[2, 3]);
    }

    /// Dragging a node with links on screen has to redraw them, and dragging one
    /// without has to cost nothing.
    #[test]
    fn only_a_node_with_recorded_links_dirties_the_layer() {
        let mut layer = chain();
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);
        layer.refresh(region, region, &[0]);
        layer.take_repaint();

        layer.node_moved(4, |_| Rect::ZERO);
        assert!(!layer.take_repaint(), "node 4's link is not recorded");

        layer.node_moved(0, |_| Rect::ZERO);
        assert!(layer.take_repaint(), "node 0's link is");
    }

    /// The promise the crate makes about a graph it did not validate: an edge naming a
    /// node that is not there is skipped, not rejected, and takes nothing with it.
    ///
    /// Worth a test of its own now that the adjacency is packed: with a vector per node
    /// the out-of-range end simply found no list, and with offsets it is an index that
    /// has to be checked before it is counted.
    #[test]
    fn an_edge_naming_a_missing_node_is_skipped() {
        let mut layer = LinkLayer::new(vec![Link::new(0, 1), Link::new(1, 9), Link::new(2, 2)], 3);
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);

        layer.refresh(region, region, &[0, 1, 2]);
        // Edge 1 is reachable from node 1 only, and edge 2 is a self-link recorded once.
        assert_eq!(layer.recorded(), &[0, 1, 2]);

        let mut layer = LinkLayer::new(vec![Link::new(0, 1), Link::new(1, 9)], 3);
        layer.refresh(region, region, &[2]);
        assert!(layer.recorded().is_empty(), "node 2 has no edges");
    }

    #[test]
    fn a_graph_without_links_never_refreshes() {
        let mut layer = LinkLayer::new(Vec::new(), 4);
        assert!(layer.is_empty());
        assert!(!layer.refresh(Rect::ZERO, Rect::ZERO, &[0, 1, 2, 3]));
        layer.node_moved(0, |_| Rect::ZERO);
        assert!(!layer.take_repaint());
    }

    #[test]
    fn the_curve_starts_and_ends_on_the_facing_edges() {
        let mut path = BezPath::new();
        push_between(&mut path, rect(0.0, 0.0), rect(300.0, 100.0));
        let start = path.elements()[0];
        assert!(
            matches!(start, masonry::kurbo::PathEl::MoveTo(p) if p == Point::new(100.0, 25.0)),
            "{start:?}"
        );
    }

    /// The property the batch stands on: every link starts a subpath of its own, so
    /// nothing joins the end of one to the start of the next.
    #[test]
    fn a_batch_starts_a_new_subpath_per_link() {
        let mut path = BezPath::new();
        push_between(&mut path, rect(0.0, 0.0), rect(300.0, 0.0));
        push_between(&mut path, rect(0.0, 900.0), rect(300.0, 900.0));

        let moves = path
            .elements()
            .iter()
            .filter(|el| matches!(el, masonry::kurbo::PathEl::MoveTo(_)))
            .count();
        assert_eq!(moves, 2, "two links, two subpaths");
        assert_eq!(path.subpaths().count(), 2);
    }

    #[test]
    fn a_short_link_can_be_dropped_from_the_recorded_set() {
        let mut layer = chain();
        let region = Rect::new(0.0, 0.0, 1000.0, 1000.0);
        layer.refresh(region, region, &[0, 1, 2, 3, 4]);
        assert_eq!(layer.recorded(), &[0, 1, 2, 3]);

        layer.measure_recorded(|link| (link.from % 2 == 0).then_some(Rect::ZERO));
        assert_eq!(layer.recorded(), &[0, 2], "ascending order survives the filter");
        assert_eq!(layer.hidden(), 2);

        // A fresh selection starts from nothing hidden, or the count would accumulate
        // across zooms and stop meaning "hidden right now".
        layer.invalidate();
        layer.refresh(region, region, &[0, 1, 2, 3, 4]);
        assert_eq!(layer.hidden(), 0);
    }
}
