//! The smallest application on blazy, and the check that one can be written.
//!
//! Two areas side by side with a small node canvas in each, in a window of its own —
//! against the facade and nothing else. Every other example reaches the library the
//! same way, but each of them is an experiment first; this one exists only so that
//! "an application needs nothing but `blazy`" is a program that compiles rather than a
//! sentence in `crates/blazy` (§15.1).
//!
//! ```text
//! cargo run -p hello
//! ```

// On Windows, don't open a console for the GUI mode.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use blazy::areas::{AreaId, AreaScreen, SplitTree};
use blazy::canvas::{CanvasLayer, Detail, Link};
use blazy::masonry::core::{NewWidget, Widget};
use blazy::masonry::kurbo::{Point, Size};
use blazy::masonry::theme::default_property_set;
use blazy::masonry::widgets::Button;
use blazy::shell::window::{Error, WindowConfig, run};

/// Nodes in each canvas.
const NODES: usize = 12;
/// Nodes per row of the grid they are laid out on.
const COLS: usize = 4;

/// A node canvas for one area.
fn canvas(area: AreaId) -> NewWidget<dyn Widget> {
    let geometry = |index: usize| {
        let (col, row) = (index % COLS, index / COLS);
        let pos = Point::new(40.0 + col as f64 * 180.0, 40.0 + row as f64 * 110.0);
        // `Some` for every name: this graph has no holes, because nothing deletes from
        // it. A graph that has been edited answers `None` for the names a removal freed.
        Some((pos, Size::new(140.0, 60.0)))
    };
    // A node's widget is built when the node scrolls into view and dropped when it
    // leaves, so it can hold no state of its own (§20.3): what it shows comes from the
    // index, which is to say from the model.
    let source = move |index: usize, _detail: Detail| {
        NewWidget::new(Button::with_text(format!("area {area}, node {index}"))).erased()
    };
    let links = (0..NODES)
        .flat_map(|i| {
            let right = (i % COLS != COLS - 1 && i + 1 < NODES).then(|| Link::new(i, i + 1));
            let below = (i + COLS < NODES).then(|| Link::new(i, i + COLS));
            right.into_iter().chain(below)
        })
        .collect();
    NewWidget::new(CanvasLayer::new(NODES, geometry, source).with_links(links)).erased()
}

/// The whole window: a screen of two areas, each with a canvas of its own.
fn screen() -> AreaScreen {
    AreaScreen::new(SplitTree::balanced(2), canvas)
}

fn main() -> Result<(), Error> {
    let config = WindowConfig::default()
        .with_title("blazy - hello")
        .with_size(1000.0, 600.0);
    run(config, NewWidget::new(screen()).erased(), default_property_set(), ())
}

#[cfg(test)]
mod tests {
    use blazy::masonry::dpi::PhysicalSize;
    use blazy::masonry::testing::TestHarness;

    use super::*;

    /// The window's content builds, lays out and virtualises, headless. A window cannot
    /// open on CI; the tree it would show can, and through the same facade.
    #[test]
    fn the_screen_builds_and_its_canvases_materialise_what_they_show() {
        let mut harness = TestHarness::create_with_size(
            default_property_set(),
            NewWidget::new(screen()),
            PhysicalSize::new(1000, 600),
        );
        let _ = harness.redraw();

        let areas = harness.root_widget().area_ids();
        assert_eq!(areas.len(), 2);
        for id in areas {
            let stats = harness
                .get_widget_with_id(id)
                .downcast::<CanvasLayer>()
                .expect("every area is a canvas")
                .stats();
            assert_eq!(stats.total, NODES);
            assert!(stats.materialised > 0, "a canvas showing nodes built widgets for them");
        }
    }
}
