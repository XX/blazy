//! What a canvas reports: the answer to a pick, its state, and its work counters.

use masonry::kurbo::Point;

use crate::detail::Detail;
use crate::links::{Link, PortSide};

/// What the canvas found under a point. Nodes win over links, as they are painted over them.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum CanvasHit {
    /// A node, with the canvas-space position of its top-left corner.
    Node {
        /// Index into the node array the canvas was built over.
        index: usize,
        /// Canvas-space position of the node's top-left corner.
        pos: Point,
    },
    /// A port of a node, which is where a link is dragged out of or into.
    Port {
        /// The node the port is on.
        index: usize,
        /// Which side of the node.
        side: PortSide,
        /// Its number on that side.
        port: u16,
        /// Where it is, in canvas coordinates.
        pos: Point,
    },
    /// A link, by its index in the edge list.
    Link {
        /// Index into the edge list given to [`CanvasLayer::with_links`](crate::CanvasLayer::with_links).
        edge: usize,
        /// The edge itself, so the caller need not index the list again.
        link: Link,
    },
}

impl CanvasHit {
    /// The node index, if this is a node or one of its ports (a click on a port selects
    /// its node).
    pub fn node(self) -> Option<usize> {
        match self {
            Self::Node { index, .. } | Self::Port { index, .. } => Some(index),
            Self::Link { .. } => None,
        }
    }

    /// The edge index, if this is a link.
    pub fn link(self) -> Option<usize> {
        match self {
            Self::Link { edge, .. } => Some(edge),
            Self::Node { .. } | Self::Port { .. } => None,
        }
    }

    /// The port, if this is one: its node, side and number.
    pub fn port(self) -> Option<(usize, PortSide, u16)> {
        match self {
            Self::Port { index, side, port, .. } => Some((index, side, port)),
            Self::Node { .. } | Self::Link { .. } => None,
        }
    }
}

/// What the canvas is doing right now, plus work counters; captured during layout, so
/// [`CanvasCounters::far_repaints`] (bumped in paint) is a frame behind.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct CanvasStats {
    /// Total number of nodes.
    pub total: usize,
    /// Nodes that currently have a widget in the tree; zero in far-field mode.
    pub materialised: usize,
    /// Nodes inside the visible rect, whether or not they have a widget: what the budget is
    /// decided on.
    pub visible: usize,
    /// Detail level applied at the last layout: the stricter of
    /// [`DetailThresholds`](crate::DetailThresholds) and [`DetailBudget`](crate::DetailBudget).
    pub detail: Option<Detail>,
    /// Current zoom factor.
    pub zoom: f64,
    /// Nodes in the far-field recording, and therefore drawn on every repaint of it.
    pub recorded_far: usize,
    /// Link curves currently recorded, and therefore drawn on every repaint.
    pub recorded_links: usize,
    /// Link curves the last selection dropped as too short to see
    /// ([`LinkStyle::min_screen_length`](crate::LinkStyle::min_screen_length)).
    pub hidden_links: usize,
    /// What the pointer was last found to be over, as of the last pointer move.
    pub hovered: Option<CanvasHit>,
    /// Cumulative work counters.
    pub counters: CanvasCounters,
}

/// Cumulative counters, for spotting work that should not be happening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CanvasCounters {
    /// Layout passes run on the content widget.
    pub content_layouts: u64,
    /// Nodes that actually needed layout, summed over all passes; flat while panning.
    pub child_layouts: u64,
    /// Compose passes run on the content widget.
    pub composes: u64,
    /// Widgets built, one per node entering the materialised region.
    pub builds: u64,
    /// Classes put on or taken off nodes (`CanvasLayer::set_node_class`).
    pub class_changes: u64,
    /// Nodes raised to the top of the stacking order (`CanvasLayer::raise_node`).
    pub raises: u64,
    /// Nodes inserted into or removed from the canvas.
    pub node_edits: u64,
    /// Links inserted into or removed from the canvas.
    pub link_edits: u64,
    /// Times the packed adjacency was rebuilt.
    pub link_compactions: u64,
    /// Link names walked by structural edits.
    pub edit_edge_scans: u64,
    /// Node geometries examined while deciding what is on screen, summed over all passes.
    pub slot_visits: u64,
    /// Times the link curves have been re-emitted into the scene (on any repaint).
    pub link_repaints: u64,
    /// Times the canvas has re-chosen which links are recorded: only leaving the region
    /// should, not a pan inside it or a node drag.
    pub link_reselects: u64,
    /// Times the effective detail level actually changed (what the hysteresis is judged on).
    pub level_switches: u64,
    /// Times the far-field scene has been re-emitted (on any repaint).
    pub far_repaints: u64,
    /// Times the canvas has re-chosen which nodes the far field records: only leaving the
    /// region should.
    pub far_records: u64,
    /// Picks performed: pointer moves, button presses, explicit hit tests.
    pub hit_queries: u64,
    /// Node geometries examined while answering picks, summed: bounded by density, not size.
    pub hit_node_tests: u64,
    /// Ports measured against the pointer, summed over every pick.
    pub hit_port_tests: u64,
    /// Link curves a pick rebuilt and measured, summed; kept small by the stored boxes.
    pub hit_curve_tests: u64,
    /// Recorded curves a pick walked past, summed (box tests): the candidate count, which
    /// grows as the view pulls back.
    pub hit_curve_scans: u64,
}
