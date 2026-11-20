//! Which canvases show a graph, and what each of them has not been told yet.

use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use blazy_canvas::{CanvasLayer, Link};
use masonry::core::{WidgetId, WidgetMut};
use masonry::kurbo::Point;

use crate::Edit;

/// A change of the graph, as a view that missed it has to hear about it.
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
    /// What node `index` holds changed; its widget is rebuilt.
    Contents {
        /// The node's name.
        index: usize,
    },
    /// Node `index` was raised to stacking stamp `stamp`; the stacking order is the graph's.
    Raised {
        /// The node's name.
        index: usize,
        /// Its stamp from [`Views::raise`]; nodes stack by `(stamp, name)`.
        stamp: u64,
    },
    /// The graph's selection changed ([`Views::selection`]); the editor over the canvas reads
    /// it anew.
    Selection,
    /// The graph was replaced as a whole ([`Views::graph_replaced`]): the view starts over.
    Reset {
        /// How many names the new graph uses.
        count: usize,
    },
}

/// What a graph's views have been through, summed over the registry's life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ViewCounters {
    /// Canvases registered as views.
    pub attached: u64,
    /// Views that left the widget tree and were forgotten.
    pub detached: u64,
    /// Changes recorded, one per change per view that was owed it.
    pub noted: u64,
    /// Changes a view collected through [`sync_canvas`].
    pub delivered: u64,
}

/// The views of one graph and what each is owed, plus what is the graph's rather than any
/// view's: selection, stacking order, the copy, the history.
///
/// Inside a window a change reaches the other views as a push in the same frame; it is
/// also recorded here for every view but its origin, and a window collects its share when
/// it draws ([`sync_canvas`]). Applying a change twice is harmless. The editor records what
/// its operators change; record changes made past them with [`note`](Self::note).
///
/// Cloning the handle shares the registry.
#[derive(Clone, Default)]
pub struct Views(Rc<RefCell<Ledger>>);

#[derive(Default)]
struct Ledger {
    /// Each view, and the changes it has not collected.
    views: Vec<(WidgetId, Vec<Change>)>,
    counters: ViewCounters,
    /// Each node's stacking stamp, by name; zero, or past the end, for never raised.
    stacking: Vec<u64>,
    /// The last stamp handed out.
    raise_seq: u64,
    /// The selected nodes, by name.
    selection: BTreeSet<usize>,
    /// What `node.copy` copied last: a `Clip` of the graph's content type, behind `Any`
    /// because the registry is not generic.
    clip: Option<Rc<dyn Any>>,
    /// The copy as text, to recognise it when it comes back from the system clipboard.
    clip_text: Option<String>,
    /// The copy's text, not yet handed to the system clipboard.
    outbound: Option<String>,
    /// Text from the system clipboard that is not this process's copy, for `node.paste`.
    offered: Option<String>,
    /// Widgets that show the graph without being a canvas (panels), whether each is owed a
    /// refresh, and what refreshes it.
    watchers: Vec<(WidgetId, bool, Refresh)>,
    /// How many times the graph has been replaced as a whole.
    generation: u64,
    /// The links of the graph as last replaced, for the views that start over.
    replacement: Option<Rc<[Link]>>,
    /// The graph's one history, shared by every session: a `SharedHistory` behind `Any`.
    history: Option<Rc<dyn Any>>,
}

/// What brings a watcher up to date, given its `WidgetMut`.
pub type Refresh = fn(&mut WidgetMut<'_, dyn masonry::core::Widget>);

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
    /// Call it from [`NodeSource::attached`](blazy_canvas::NodeSource::attached) and keep
    /// the token in the source: dropped with the canvas, it is the only notice that the view
    /// is gone (Masonry has no removal event).
    #[must_use = "the view is forgotten as soon as the token is dropped"]
    pub fn attach(&self, canvas: WidgetId) -> ViewToken {
        let mut ledger = self.0.borrow_mut();
        if !ledger.views.iter().any(|(id, _)| *id == canvas) {
            // A new canvas stacks in name order; nodes raised before it are owed to it.
            let owed: Vec<Change> = ledger
                .stacking
                .iter()
                .enumerate()
                .filter(|&(_, &stamp)| stamp > 0)
                .map(|(index, &stamp)| Change::Raised { index, stamp })
                .collect();
            ledger.counters.noted += owed.len() as u64;
            ledger.views.push((canvas, owed));
            ledger.counters.attached += 1;
        }
        ViewToken {
            ledger: self.0.clone(),
            canvas,
        }
    }

    /// Registers a widget that shows the graph but is not a canvas (a panel), for as long as
    /// the token lives.
    ///
    /// It is owed a refresh after a change it can show, unless it made it
    /// ([`note_except`](Self::note_except)); `refresh` runs in its own window's push and in
    /// [`sync_root`] elsewhere, and must call [`refreshed`](Self::refreshed).
    #[must_use = "the watcher is forgotten as soon as the token is dropped"]
    pub fn watch(&self, widget: WidgetId, refresh: Refresh) -> WatchToken {
        let mut ledger = self.0.borrow_mut();
        if !ledger.watchers.iter().any(|(id, ..)| *id == widget) {
            ledger.watchers.push((widget, false, refresh));
        }
        WatchToken {
            ledger: self.0.clone(),
            widget,
        }
    }

    /// The watchers owed a refresh, with what refreshes each.
    pub fn owed_watchers(&self) -> Vec<(WidgetId, Refresh)> {
        let ledger = self.0.borrow();
        ledger
            .watchers
            .iter()
            .filter(|(_, owed, _)| *owed)
            .map(|&(id, _, refresh)| (id, refresh))
            .collect()
    }

    /// Says watcher `widget` is up to date.
    pub fn refreshed(&self, widget: WidgetId) {
        if let Some((_, owed, _)) = self.0.borrow_mut().watchers.iter_mut().find(|(id, ..)| *id == widget) {
            *owed = false;
        }
    }

    /// Records a change for every view.
    pub fn note(&self, change: Change) {
        self.0.borrow_mut().note(None, change);
    }

    /// Records a change for every view but `origin`, which has shown it already.
    pub fn note_except(&self, origin: WidgetId, change: Change) {
        self.0.borrow_mut().note(Some(origin), change);
    }

    /// Brings node `index` to the front and returns its new stamp, or `None` when it is in
    /// front already; the caller stacks its canvas and records [`Change::Raised`].
    pub fn raise(&self, index: usize) -> Option<u64> {
        self.0.borrow_mut().raise(index)
    }

    /// The selected nodes, by name: one selection for every view of the graph.
    pub fn selection(&self) -> BTreeSet<usize> {
        self.0.borrow().selection.clone()
    }

    /// Whether node `index` is selected.
    pub fn is_selected(&self, index: usize) -> bool {
        self.0.borrow().selection.contains(&index)
    }

    /// Replaces the selection and returns the old one, or `None` if nothing changed.
    /// Announces nothing: see [`announce_selection`](Self::announce_selection).
    pub(crate) fn replace_selection(&self, nodes: BTreeSet<usize>) -> Option<BTreeSet<usize>> {
        let mut ledger = self.0.borrow_mut();
        if ledger.selection == nodes {
            return None;
        }
        Some(std::mem::replace(&mut ledger.selection, nodes))
    }

    /// Tells every view that the selection changed and raises the nodes that `came` into it;
    /// returns what it recorded, for the push.
    ///
    /// Called after the gesture's structure edits, so a node added and selected exists
    /// before it is raised.
    pub(crate) fn announce_selection(&self, came: &[usize]) -> Vec<Change> {
        let mut ledger = self.0.borrow_mut();
        let mut announced = Vec::new();
        for &index in came {
            if !ledger.selection.contains(&index) {
                continue;
            }
            if let Some(stamp) = ledger.raise(index) {
                announced.push(Change::Raised { index, stamp });
            }
        }
        announced.push(Change::Selection);
        for &change in &announced {
            ledger.note(None, change);
        }
        announced
    }

    /// Puts a copy on the graph's clipboard, in place of whatever was there.
    pub(crate) fn set_clip(&self, clip: Rc<dyn Any>, text: Option<String>) {
        let mut ledger = self.0.borrow_mut();
        ledger.clip = Some(clip);
        ledger.outbound.clone_from(&text);
        ledger.clip_text = text;
        ledger.offered = None;
    }

    /// The text of the last copy, once, for a driver that writes the system clipboard
    /// itself. `None` if nothing was copied since, or the content has no text form.
    pub fn take_outbound_clip(&self) -> Option<String> {
        self.0.borrow_mut().outbound.take()
    }

    /// Offers what the system clipboard holds, before a paste. This process's own copy is
    /// recognised; anything else is kept for `node.paste`.
    pub fn offer_clip_text(&self, text: String) {
        let mut ledger = self.0.borrow_mut();
        ledger.offered = (ledger.clip_text.as_ref() != Some(&text)).then_some(text);
    }

    /// Says that the graph was replaced as a whole and every view has to start over.
    ///
    /// Drops the selection, the stacking, the copy and whatever the views were owed; each
    /// view is owed [`Change::Reset`], each watcher a refresh, and each editor clears its
    /// history on the new [`generation`](Self::generation). Call it with the graph already
    /// replaced and the gestures over it cancelled.
    pub fn graph_replaced<G: crate::NodeGraph>(&self, graph: &G) {
        let count = graph.names();
        let links: Vec<Link> = (0..count)
            .flat_map(|index| {
                graph
                    .links_of(index)
                    .into_iter()
                    .filter(move |l| l.from as usize == index)
            })
            .collect();
        let mut ledger = self.0.borrow_mut();
        ledger.selection.clear();
        ledger.stacking.clear();
        ledger.clip = None;
        ledger.clip_text = None;
        ledger.outbound = None;
        ledger.offered = None;
        ledger.generation += 1;
        ledger.replacement = Some(links.into());
        for (_, owed) in &mut ledger.views {
            owed.clear();
            owed.push(Change::Reset { count });
        }
        for (_, owed, _) in &mut ledger.watchers {
            *owed = true;
        }
    }

    /// The graph's history, if a session over it has made one.
    pub(crate) fn history<W: 'static>(&self) -> Option<blazy_ops::runtime::SharedHistory<W>> {
        let history = self.0.borrow().history.clone()?;
        history
            .downcast::<std::cell::RefCell<blazy_ops::undo::UndoStack<W>>>()
            .ok()
    }

    /// Makes `history` the graph's.
    pub(crate) fn set_history<W: 'static>(&self, history: blazy_ops::runtime::SharedHistory<W>) {
        self.0.borrow_mut().history = Some(history);
    }

    /// How many times the graph has been replaced as a whole.
    pub fn generation(&self) -> u64 {
        self.0.borrow().generation
    }

    /// The links a view that starts over is given.
    fn replacement(&self) -> Vec<Link> {
        self.0
            .borrow()
            .replacement
            .as_deref()
            .map(<[Link]>::to_vec)
            .unwrap_or_default()
    }

    /// The offered text, taken: what `node.paste` reads once.
    pub(crate) fn take_offered(&self) -> Option<String> {
        self.0.borrow_mut().offered.take()
    }

    /// What is on the graph's clipboard.
    pub(crate) fn clip(&self) -> Option<Rc<dyn Any>> {
        self.0.borrow().clip.clone()
    }

    /// Node `index`'s stacking stamp; zero for a node never raised.
    pub fn stacking(&self, index: usize) -> u64 {
        self.0.borrow().stacking.get(index).copied().unwrap_or(0)
    }

    /// Pushes into `out` every view but `this`.
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

    /// Whether any view is behind (whether to wake other windows).
    pub fn has_pending(&self) -> bool {
        let ledger = self.0.borrow();
        ledger.views.iter().any(|(_, owed)| !owed.is_empty()) || ledger.watchers.iter().any(|(_, owed, _)| *owed)
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
    fn raise(&mut self, index: usize) -> Option<u64> {
        if self.raise_seq > 0 && self.stacking.get(index) == Some(&self.raise_seq) {
            return None;
        }
        if self.stacking.len() <= index {
            self.stacking.resize(index + 1, 0);
        }
        self.raise_seq += 1;
        self.stacking[index] = self.raise_seq;
        Some(self.raise_seq)
    }

    fn note(&mut self, origin: Option<WidgetId>, change: Change) {
        // A reused name is a new node: it starts at the bottom.
        if let Change::Structure(Edit::NodeAdded { index, .. } | Edit::NodeRemoved { index }) = change
            && let Some(stamp) = self.stacking.get_mut(index)
        {
            *stamp = 0;
        }
        // A removed node is not selected (an insertion is noted after `node.add` selected it).
        if let Change::Structure(Edit::NodeRemoved { index }) = change {
            self.selection.remove(&index);
        }
        for (id, owed) in &mut self.views {
            if Some(*id) == origin {
                continue;
            }
            // A move replaces the owed move of the same node, so a window that is not
            // drawing owes one per node, not one per frame. Never across a structure edit,
            // which may have given the name out again.
            if let Change::Moved { index, .. } = change
                && let Some(slot) = owed
                    .iter_mut()
                    .rev()
                    .take_while(|owed| !matches!(owed, Change::Structure(_) | Change::Reset { .. }))
                    .find(|owed| matches!(owed, Change::Moved { index: i, .. } if *i == index))
            {
                *slot = change;
                continue;
            }
            owed.push(change);
            self.counters.noted += 1;
        }
        // A watcher shows the selection: a move of an unselected node is not its concern.
        let concerns_the_shown = match change {
            Change::Moved { index, .. } | Change::Structure(Edit::NodeResized { index, .. }) => {
                self.selection.contains(&index)
            },
            Change::Raised { .. } => false,
            _ => true,
        };
        if concerns_the_shown {
            for (id, owed, _) in &mut self.watchers {
                if Some(*id) != origin {
                    *owed = true;
                }
            }
        }
    }
}

/// A canvas's place in a graph's [`Views`], given up when the token is dropped.
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
        // Nothing that removes a canvas holds the registry; if something does, skip rather
        // than panic in a destructor.
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

/// A watcher's place in a graph's [`Views`], given up when the token is dropped — the
/// [`ViewToken`] of a widget that is not a canvas.
pub struct WatchToken {
    ledger: Rc<RefCell<Ledger>>,
    widget: WidgetId,
}

impl std::fmt::Debug for WatchToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("WatchToken").field(&self.widget).finish()
    }
}

impl Drop for WatchToken {
    fn drop(&mut self) {
        let Ok(mut ledger) = self.ledger.try_borrow_mut() else {
            debug_assert!(false, "the view registry was borrowed while a watcher left the tree");
            return;
        };
        ledger.watchers.retain(|(id, ..)| *id != self.widget);
    }
}

/// Brings one canvas up to date with what `views` says it owes, and returns how many changes
/// it applied. Applying a change twice is harmless.
pub fn sync_canvas(canvas: &mut WidgetMut<'_, CanvasLayer>, views: &Views) -> usize {
    let changes = views.take(canvas.ctx.widget_id());
    for &change in &changes {
        match change {
            // Only the pull carries a reset: it has the registry, with the links.
            Change::Reset { count } => CanvasLayer::reset(canvas, count, views.replacement()),
            change => apply(canvas, change),
        }
    }
    changes.len()
}

/// Brings every view and watcher in one window's tree up to date, before it draws, and
/// returns how many changes that took.
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
                    // A registry given some other widget's id: that view shows the wrong graph.
                    debug_assert!(false, "a view registered under an id that is not a canvas");
                    tracing::warn!(view = ?id, "a view registered under an id that is not a canvas");
                },
            }
        });
    }
    // And the watchers in this window.
    for (id, refresh) in views.owed_watchers() {
        if root.get_widget(id).is_some() {
            root.edit_widget(id, |mut widget| refresh(&mut widget));
            applied += 1;
        }
    }
    applied
}

/// One change, against the canvas that shows it.
pub(crate) fn apply(canvas: &mut WidgetMut<'_, CanvasLayer>, change: Change) {
    match change {
        Change::Moved { index, pos } => CanvasLayer::move_child(canvas, index, pos),
        Change::Structure(edit) => crate::editor::apply_edit(canvas, edit),
        Change::Contents { index } => {
            CanvasLayer::refresh_node(canvas, index);
        },
        Change::Raised { index, stamp } => {
            CanvasLayer::stack_node(canvas, index, stamp);
        },
        // The editor above re-reads the selection after its layout.
        Change::Selection => canvas.ctx.request_layout(),
        Change::Reset { .. } => {
            debug_assert!(false, "a reset is delivered by `sync_canvas`, which has the links");
        },
    }
}
