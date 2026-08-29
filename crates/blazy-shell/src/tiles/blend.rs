//! The blend stack, as a map of the frame in tiles.
//!
//! The second of the two buffers [`super`] models, and the one that is per *tile*
//! rather than per path: a group charges every tile of its box, a rectangular clip only
//! the tiles along its outline, and what is charged is the deepest each tile ever gets
//! (§34.2). A map rather than a sum over layers because the same group is level five
//! over one tile and level one over another.

use masonry::kurbo::Rect;

use super::{BLEND_STACK_SPLIT, SPILL_PER_TILE, TILE};

/// The blend stack, one entry per tile of the frame.
///
/// A map rather than a sum over layers, because the quantity is per tile: the same
/// group is level 5 over one tile and level 1 over another, and only the deepest each
/// tile ever gets is charged. `depth` follows the walk, `deepest` remembers.
pub(super) struct Blend {
    cols: u32,
    rows: u32,
    depth: Vec<u16>,
    deepest: Vec<u16>,
    /// One entry per open layer: what it charged, and the region visible around it.
    stack: Vec<Open>,
    /// Tiles still reachable inside the clips currently open. A tile outside an
    /// enclosing clip takes vello's `clip_zero` branch: nothing nested inside it is
    /// included there, so nothing charges it.
    pub(super) visible: TileBox,
}

pub(super) struct Open {
    charged: Charge,
    visible: TileBox,
}

/// The tiles one layer is included in.
#[derive(Clone, Copy)]
pub(super) enum Charge {
    /// Every tile of the box: a group, or a clip whose outline is not a rectangle.
    Whole(TileBox),
    /// The far column and the far row of the box: `columns + rows - 1` tiles, the count
    /// a rectangular clip was measured to charge (§34.2, §34.3).
    Edges(TileBox),
}

/// A half-open box in tile coordinates, already clipped to the frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct TileBox {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl TileBox {
    pub(super) fn of(bounds: Rect) -> Self {
        if bounds.is_zero_area() {
            return Self {
                x0: 0,
                y0: 0,
                x1: 0,
                y1: 0,
            };
        }
        Self {
            x0: (bounds.x0 / TILE).floor().max(0.0) as u32,
            y0: (bounds.y0 / TILE).floor().max(0.0) as u32,
            x1: (bounds.x1 / TILE).ceil().max(0.0) as u32,
            y1: (bounds.y1 / TILE).ceil().max(0.0) as u32,
        }
    }

    pub(super) fn intersect(self, other: Self) -> Self {
        let (x0, y0) = (self.x0.max(other.x0), self.y0.max(other.y0));
        let (x1, y1) = (self.x1.min(other.x1), self.y1.min(other.y1));
        Self {
            x0,
            y0,
            x1: x1.max(x0),
            y1: y1.max(y0),
        }
    }
}

impl Blend {
    pub(super) fn new(frame: &Rect) -> Self {
        let cols = (frame.x1 / TILE).ceil().max(0.0) as u32;
        let rows = (frame.y1 / TILE).ceil().max(0.0) as u32;
        let cells = (cols as usize) * (rows as usize);
        Self {
            cols,
            rows,
            depth: vec![0; cells],
            deepest: vec![0; cells],
            stack: Vec::new(),
            visible: TileBox {
                x0: 0,
                y0: 0,
                x1: cols,
                y1: rows,
            },
        }
    }

    /// Opens a layer over `box_`, charging the tiles vello would include it in.
    pub(super) fn enter(&mut self, charged: Charge, visible: TileBox) {
        self.stack.push(Open {
            charged,
            visible: self.visible,
        });
        self.visible = visible;
        self.step(charged, 1);
    }

    pub(super) fn leave(&mut self) {
        if let Some(open) = self.stack.pop() {
            self.step(open.charged, -1);
            self.visible = open.visible;
        }
    }

    fn step(&mut self, charged: Charge, by: i32) {
        match charged {
            Charge::Whole(box_) => self.walk(box_, by, false),
            Charge::Edges(box_) => self.walk(box_, by, true),
        }
    }

    fn walk(&mut self, box_: TileBox, by: i32, edges_only: bool) {
        for y in box_.y0..box_.y1.min(self.rows) {
            let far_row = y + 1 == box_.y1;
            for x in box_.x0..box_.x1.min(self.cols) {
                if edges_only && !far_row && x + 1 != box_.x1 {
                    continue;
                }
                let ix = (y as usize) * (self.cols as usize) + x as usize;
                let depth = &mut self.depth[ix];
                *depth = depth.saturating_add_signed(by as i16);
                self.deepest[ix] = self.deepest[ix].max(*depth);
            }
        }
    }

    pub(super) fn words(self) -> u64 {
        self.deepest
            .iter()
            .map(|deepest| u64::from(u32::from(*deepest).saturating_sub(BLEND_STACK_SPLIT)) * SPILL_PER_TILE)
            .sum()
    }
}
