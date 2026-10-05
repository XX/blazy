//! Which canvases show a graph, and what each of them has not been told yet.

use std::cell::RefCell;
use std::rc::Rc;

use blazy_canvas::CanvasLayer;
use masonry::core::{WidgetId, WidgetMut};
use masonry::kurbo::Point;

use crate::Edit;

/// A change of the graph, as a view that missed it has to hear about it.
///
/// Everything a canvas copies out of the model: its geometry, which the canvas keeps a
/// copy of (§30), its structure (§43), and — for a node that is already on screen — what
/// the node holds, because a node widget reads the model once, when it is built (§44.9).
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum Change {
    /// A node moved to `pos`.
    Moved {
        /// The node's name.
        index: usize,
        /// Where its top-left corner is now, in canvas coordinates.
        pos: Point,
    },
    /// The shape of the graph changed.
    Structure(Edit),
    /// What node `index` holds changed — a value, a flag — and a widget built before the
    /// change shows the old one.
    Contents {
        /// The node's name.
        index: usize,
    },
}

/// What a graph's views have been through, summed over the registry's life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ViewCounters {
    /// Canvases registered as views.
    pub attached: u64,
    /// Views that left the widget tree and were forgotten.
    ///
    /// The number that was missing (`issues/application assembly.md`): a view nobody
    /// forgets is owed every change made after it left, for ever, and a registry that
    /// still owes something wakes every window after every event (§36).
    pub detached: u64,
    /// Changes recorded, one per change per view that was owed it.
    pub noted: u64,
    /// Changes a view collected through [`sync_canvas`].
    pub delivered: u64,
}

/// The canvases showing one graph, and what each of them still owes.
///
/// **Two deliveries, one list.** Inside a window a change reaches the graph's other
/// views as a push, in the same frame (§30); that push names a widget in one
/// `RenderRoot`'s arena and is dropped without a word for any other (§44.3). So every
/// change is also recorded here, against every view but the one that made it, and a
/// window collects its share when it next draws ([`sync_canvas`]). Applying a change a
/// second time writes the same truth a second time, so the two paths need not know about
/// each other.
///
/// The editor records what its operators change — the same list it pushes, so the pull
/// cannot miss what the push carries. What an application changes **past the
/// operators** — a slider inside a node, a script writing the model — it records itself
/// with [`note`](Self::note).
///
/// A handle: cloning it shares the registry. The graph keeps one, and every canvas over
/// the graph holds a [`ViewToken`] into it.
#[derive(Clone, Default)]
pub struct Views(Rc<RefCell<Ledger>>);

#[derive(Default)]
struct Ledger {
    /// Each view, and the changes it has not collected.
    views: Vec<(WidgetId, Vec<Change>)>,
    counters: ViewCounters,
}

impl std::fmt::Debug for Views {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ledger = self.0.borrow();
        f.debug_struct("Views")
            .field("views", &ledger.views.len())
            .field("owed", &ledger.views.iter().map(|(_, owed)| owed.len()).sum::<usize>())
            .finish()
    }
}

impl Views {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `canvas` as a view, for as long as the token lives.
    ///
    /// Called from [`NodeSource::attached`](blazy_canvas::NodeSource::attached), which
    /// is the one moment a canvas knows its own id. Keep the token in the source: the
    /// canvas owns its source and drops it when it leaves the tree, and the token's
    /// `Drop` is then the only notice anyone gets. Masonry has no removal event for a
    /// widget (`remove_child` carries a TODO where one would go), and without this a
    /// view joined away or detached stays owed every change made after it left.
    #[must_use = "the view is forgotten as soon as the token is dropped"]
    pub fn attach(&self, canvas: WidgetId) -> ViewToken {
        let mut ledger = self.0.borrow_mut();
        if !ledger.views.iter().any(|(id, _)| *id == canvas) {
            ledger.views.push((canvas, Vec::new()));
            ledger.counters.attached += 1;
        }
        ViewToken {
            ledger: self.0.clone(),
            canvas,
        }
    }

    /// Records a change for every view.
    ///
    /// For a change no view has applied yet — one an application made past the
    /// operators. A change one canvas has already shown goes through
    /// [`note_except`](Self::note_except).
    pub fn note(&self, change: Change) {
        self.0.borrow_mut().note(None, change);
    }

    /// Records a change for every view but `origin`, which has shown it already.
    pub fn note_except(&self, origin: WidgetId, change: Change) {
        self.0.borrow_mut().note(Some(origin), change);
    }

    /// Pushes into `out` every view but `this`.
    ///
    /// The other canvases a change has to reach in the same frame (§30).
    pub fn others(&self, this: WidgetId, out: &mut Vec<WidgetId>) {
        out.extend(self.0.borrow().views.iter().map(|(id, _)| *id).filter(|&id| id != this));
    }

    /// Every view, in the order they were attached.
    pub fn ids(&self) -> Vec<WidgetId> {
        self.0.borrow().views.iter().map(|(id, _)| *id).collect()
    }

    /// How many views the graph has.
    pub fn len(&self) -> usize {
        self.0.borrow().views.len()
    }

    /// Whether no canvas shows the graph.
    pub fn is_empty(&self) -> bool {
        self.0.borrow().views.is_empty()
    }

    /// Whether any view is behind.
    ///
    /// What a driver asks before waking other windows: an idle window is idle on
    /// purpose (§36), and waking it after every event would undo that.
    pub fn has_pending(&self) -> bool {
        self.0.borrow().views.iter().any(|(_, owed)| !owed.is_empty())
    }

    /// How many changes the views owe, summed.
    pub fn owed(&self) -> usize {
        self.0.borrow().views.iter().map(|(_, owed)| owed.len()).sum()
    }

    /// Takes what `view` has not caught up with.
    pub fn take(&self, view: WidgetId) -> Vec<Change> {
        let mut ledger = self.0.borrow_mut();
        let changes = ledger
            .views
            .iter_mut()
            .find(|(id, _)| *id == view)
            .map(|(_, owed)| std::mem::take(owed))
            .unwrap_or_default();
        ledger.counters.delivered += changes.len() as u64;
        changes
    }

    /// What the registry has been through.
    pub fn counters(&self) -> ViewCounters {
        self.0.borrow().counters
    }
}

impl Ledger {
    fn note(&mut self, origin: Option<WidgetId>, change: Change) {
        for (id, owed) in &mut self.views {
            if Some(*id) != origin {
                owed.push(change);
                self.counters.noted += 1;
            }
        }
    }
}

/// A canvas's place in a graph's [`Views`], given up when the token is dropped.
///
/// See [`Views::attach`] for why the drop is the mechanism and not a convenience.
pub struct ViewToken {
    ledger: Rc<RefCell<Ledger>>,
    canvas: WidgetId,
}

impl ViewToken {
    /// The canvas this token registers.
    pub fn canvas(&self) -> WidgetId {
        self.canvas
    }
}

impl std::fmt::Debug for ViewToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ViewToken").field(&self.canvas).finish()
    }
}

impl Drop for ViewToken {
    fn drop(&mut self) {
        // A drop happens inside whatever removed the canvas — a join, a detach, a
        // workspace load — and nothing in those paths holds the registry. If something
        // ever does, forgetting the view later is better than a panic in a destructor;
        // the debug assertion is what says the assumption broke.
        let Ok(mut ledger) = self.ledger.try_borrow_mut() else {
            debug_assert!(false, "the view registry was borrowed while a view left the tree");
            return;
        };
        let before = ledger.views.len();
        ledger.views.retain(|(id, _)| *id != self.canvas);
        if ledger.views.len() != before {
            ledger.counters.detached += 1;
        }
    }
}

/// Brings one canvas up to date with what `views` says it owes.
///
/// The half of §30 that crosses a window boundary, and it does not care what the canvas
/// sits inside: a view *is* a canvas, named by the id it was attached under, so a window
/// that finds the id in its own tree applies the changes to it — whatever area, editor or
/// region wraps it. The pull used to look for the wrapper, and a wrapper it did not know
/// was a window that collected nothing, silently (§44.6).
///
/// Every change is applied so that applying it twice is harmless, because inside a
/// window the push has usually delivered it already.
///
/// Returns how many changes were applied.
pub fn sync_canvas(canvas: &mut WidgetMut<'_, CanvasLayer>, views: &Views) -> usize {
    let changes = views.take(canvas.ctx.widget_id());
    for &change in &changes {
        apply(canvas, change);
    }
    changes.len()
}

/// Brings every view in one window's tree up to date, and says how many changes that took.
///
/// What a driver calls before a window draws. A view that is in another window is not in
/// this tree and is left for that window; a view in no window cannot exist, because a
/// canvas that leaves the tree drops its [`ViewToken`].
pub fn sync_root(root: &mut masonry::app::RenderRoot, views: &Views) -> usize {
    let mut applied = 0;
    for id in views.ids() {
        if root.get_widget(id).is_none() {
            continue;
        }
        root.edit_widget(id, |mut widget| {
            match widget.try_downcast::<CanvasLayer>() {
                Some(mut canvas) => applied += sync_canvas(&mut canvas, views),
                None => {
                    // A view is registered under its canvas's own id, so this is a
                    // registry that was handed some other widget's id. Loud, because a
                    // view that cannot be reached goes on showing the wrong graph.
                    debug_assert!(false, "a view registered under an id that is not a canvas");
                    tracing::warn!(view = ?id, "a view registered under an id that is not a canvas");
                },
            }
        });
    }
    applied
}

/// One change, against the canvas that shows it.
fn apply(canvas: &mut WidgetMut<'_, CanvasLayer>, change: Change) {
    match change {
        Change::Moved { index, pos } => CanvasLayer::move_child(canvas, index, pos),
        Change::Structure(edit) => crate::editor::apply_edit(canvas, edit),
        Change::Contents { index } => {
            CanvasLayer::refresh_node(canvas, index);
        },
    }
}
