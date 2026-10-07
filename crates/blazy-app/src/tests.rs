//! The assembly against a graph of its own, in two windows, driven the way the shell
//! drives them.
//!
//! Not a harness: a window is a `RenderRoot`, and a harness does not hand one out (§39.5).
//! Each test does what the shell does — deliver a change, call `settled`, call `frame`
//! on the window about to draw — and reads the canvases back.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use blazy_areas::SplitTree;
use blazy_canvas::{CanvasLayer, Detail, Link, NodeSource};
use blazy_node_editor::ops::{CANVAS_SCOPE, default_keymap};
use blazy_node_editor::{NodeEditor, NodeGraph, SessionHandle, SharedGraph, ViewToken, Views};
use blazy_ops::event::{OpEvent, Trigger};
use blazy_ops::keymap::{Props, Scope};
use blazy_shell::window::{ShellCtx, ShellDriver};
use masonry::app::{RenderRoot, RenderRootOptions, WindowSizePolicy};
use masonry::core::keyboard::{Code, Key, KeyState, KeyboardEvent, Modifiers};
use masonry::core::{NewWidget, Widget, WidgetId, WidgetMut};
use masonry::dpi::PhysicalSize;
use masonry::kurbo::{Point, Rect, Size};
use masonry::theme::default_property_set;
use masonry::widgets::Label;

use crate::{EditorApp, EditorScreen, ScreenAction, ScreenKeys, detach_area};

const NODE: Size = Size::new(120.0, 60.0);

/// The smallest graph an application could write: rectangles, links, its views.
#[derive(Default)]
struct Grid {
    rects: Vec<Option<Rect>>,
    free: Vec<usize>,
    links: Vec<Link>,
    views: Views,
}

impl Grid {
    fn new(count: usize) -> SharedGraph<Self> {
        let rects = (0..count)
            .map(|i| {
                let pos = Point::new(20.0 + (i % 4) as f64 * 160.0, 20.0 + (i / 4) as f64 * 100.0);
                Some(Rect::from_origin_size(pos, NODE))
            })
            .collect();
        Rc::new(RefCell::new(Self {
            rects,
            ..Self::default()
        }))
    }
}

impl NodeGraph for Grid {
    fn node_count(&self) -> usize {
        self.rects.iter().flatten().count()
    }

    fn node_rect(&self, index: usize) -> Rect {
        self.rects.get(index).copied().flatten().unwrap_or_default()
    }

    fn set_node_pos(&mut self, index: usize, pos: Point) {
        if let Some(Some(rect)) = self.rects.get_mut(index) {
            *rect = Rect::from_origin_size(pos, rect.size());
        }
    }

    fn views(&self) -> &Views {
        &self.views
    }

    fn insert_node(&mut self, rect: Rect) -> usize {
        match self.free.pop() {
            Some(index) => {
                self.rects[index] = Some(rect);
                index
            },
            None => {
                self.rects.push(Some(rect));
                self.rects.len() - 1
            },
        }
    }

    fn restore_node(&mut self, index: usize, rect: Rect) {
        if self.rects.len() <= index {
            self.rects.resize(index + 1, None);
        }
        self.free.retain(|&free| free != index);
        self.rects[index] = Some(rect);
    }

    fn remove_node(&mut self, index: usize) -> Vec<Link> {
        self.rects[index] = None;
        self.free.push(index);
        let (gone, kept) = self
            .links
            .iter()
            .partition(|link| link.from as usize == index || link.to as usize == index);
        self.links = kept;
        gone
    }

    fn insert_link(&mut self, link: Link) -> bool {
        if self.links.contains(&link) {
            return false;
        }
        self.links.push(link);
        true
    }

    fn remove_link(&mut self, link: Link) {
        self.links.retain(|&l| l != link);
    }
}

/// Nodes as labels, and the canvas's place among the graph's views.
struct Source {
    graph: SharedGraph<Grid>,
    view: Option<ViewToken>,
}

impl NodeSource for Source {
    fn build(&mut self, index: usize, _detail: Detail) -> NewWidget<dyn Widget> {
        NewWidget::new(Label::new(format!("node {index}"))).erased()
    }

    fn attached(&mut self, canvas: WidgetId) {
        self.view = Some(self.graph.borrow().views().attach(canvas));
    }
}

/// What an application writes: the editor an area holds.
fn app(graph: &SharedGraph<Grid>) -> EditorApp<Grid> {
    let building = graph.clone();
    EditorApp::new(graph, move |_area, session: &SessionHandle<Grid>| {
        let names = building.borrow().rects.len();
        let geometry = {
            let graph = building.clone();
            move |i: usize| {
                graph
                    .borrow()
                    .rects
                    .get(i)
                    .copied()
                    .flatten()
                    .map(|r| (r.origin(), r.size()))
            }
        };
        let source = Source {
            graph: building.clone(),
            view: None,
        };
        let canvas = CanvasLayer::new(names, geometry, source).with_links(building.borrow().links.clone());
        NewWidget::new(NodeEditor::with_session(canvas, session.clone())).erased()
    })
}

/// A window, minus the window.
fn window(app: &EditorApp<Grid>, areas: usize) -> RenderRoot {
    let screen = app.screen(SplitTree::balanced(areas));
    let mut root = RenderRoot::new(NewWidget::new(screen).erased(), |_signal| {}, RenderRootOptions {
        default_properties: Arc::new(default_property_set()),
        use_system_fonts: false,
        size_policy: WindowSizePolicy::User,
        size: PhysicalSize::new(1200, 800),
        scale_factor: 1.0,
        test_font: None,
    });
    let _ = root.redraw();
    root
}

/// The editors of one window, in area order.
fn editors(root: &mut RenderRoot) -> Vec<WidgetId> {
    root.edit_base_layer(|mut widget| widget.downcast::<EditorScreen<Grid>>().widget.area_ids())
}

fn edit_editor<R>(root: &mut RenderRoot, area: usize, f: impl FnOnce(&mut WidgetMut<'_, NodeEditor<Grid>>) -> R) -> R {
    let id = editors(root)[area];
    root.edit_widget(id, |mut widget| f(&mut widget.downcast::<NodeEditor<Grid>>()))
}

fn has_link(root: &mut RenderRoot, area: usize, link: Link) -> bool {
    edit_editor(root, area, |editor| {
        NodeEditor::with_canvas(editor, |mut canvas| CanvasLayer::link_name(&mut canvas, link).is_some())
    })
}

/// A link made in one window is in the other once it draws, and so is its removal.
///
/// The defect this pins down: what a view was owed was the application's to record, and
/// the example's record knew nodes and not links — so `link.add` in one window was in
/// the model and in that window, and nowhere else, for good.
#[test]
fn a_link_made_in_one_window_reaches_the_other() {
    let graph = Grid::new(8);
    let mut app = app(&graph);
    let mut first = window(&app, 2);
    let mut second = window(&app, 1);
    let link = Link::new(0, 5);
    let add = Props::new().with_int("from", 0).with_int("to", 5);

    edit_editor(&mut second, 0, |editor| NodeEditor::exec(editor, "link.add", &add));
    let _ = second.redraw();
    assert!(has_link(&mut second, 0, link));
    assert!(
        !has_link(&mut first, 0, link),
        "before its frame the other window is behind"
    );

    app.pull(&mut first);
    let _ = first.redraw();
    assert!(has_link(&mut first, 0, link), "after its frame it has the link");
    assert!(has_link(&mut first, 1, link), "in every area");

    edit_editor(&mut second, 0, |editor| {
        NodeEditor::exec(editor, "link.delete", &add);
    });
    let _ = second.redraw();
    app.pull(&mut first);
    let _ = first.redraw();
    assert!(!has_link(&mut first, 0, link), "and the removal follows it");
}

/// A view that left the tree is owed nothing, and nobody is woken on its account.
///
/// The other defect: the example registered a canvas as a view and never forgot it, so
/// after the first join every change was owed to a canvas that no longer existed —
/// and since the windows are woken while anything is owed, every window drew after every
/// event, for good.
#[test]
fn a_view_that_left_the_tree_is_owed_nothing() {
    let graph = Grid::new(8);
    let mut app = app(&graph);
    let mut root = window(&app, 3);
    assert_eq!(graph.borrow().views().len(), 3, "three areas, three views");

    root.edit_base_layer(|mut widget| {
        let mut screen = widget.downcast::<EditorScreen<Grid>>();
        // Whichever area has a sibling that is an area: on three, one of them does not
        // (§41.1).
        let tree = screen.widget.tree().clone();
        let (keep, gone) = tree
            .areas()
            .find_map(|area| tree.joinable(area).map(|sibling| (area, sibling)))
            .expect("three areas hold a joinable pair");
        assert!(EditorScreen::<Grid>::join(&mut screen, keep, gone));
    });
    let _ = root.redraw();
    assert_eq!(graph.borrow().views().len(), 2, "the joined-away canvas is forgotten");

    edit_editor(&mut root, 0, |editor| {
        NodeEditor::exec(editor, "node.select", &Props::new().with_int("index", 1));
        NodeEditor::exec(editor, "node.move", &Props::new().with_float("dx", 30.0));
    });
    let _ = root.redraw();
    app.pull(&mut root);
    let _ = root.redraw();
    assert!(!graph.borrow().views().has_pending(), "everything owed was collected");
    assert!(app.windows_to_wake().is_empty(), "so nobody is woken");
    assert_eq!(graph.borrow().views().counters().detached, 1);
}

/// Detach moves the session and the view goes with the canvas, not with the session.
#[test]
fn a_detached_area_leaves_no_view_behind() {
    let graph = Grid::new(8);
    let app = app(&graph);
    let mut root = window(&app, 2);
    let session = root.edit_base_layer(|mut widget| {
        let mut screen = widget.downcast::<EditorScreen<Grid>>();
        detach_area(&mut screen, 1)
    });
    let session = session.expect("one of two areas detaches");
    let _ = root.redraw();
    assert_eq!(graph.borrow().views().len(), 1, "the canvas that left is forgotten");

    // Rebuilt in a window of its own, with the session it had.
    let screen = app.screen_carrying(SplitTree::balanced(1), Some(session.clone()));
    let mut other = RenderRoot::new(NewWidget::new(screen).erased(), |_signal| {}, RenderRootOptions {
        default_properties: Arc::new(default_property_set()),
        use_system_fonts: false,
        size_policy: WindowSizePolicy::User,
        size: PhysicalSize::new(800, 600),
        scale_factor: 1.0,
        test_font: None,
    });
    let _ = other.redraw();
    assert_eq!(graph.borrow().views().len(), 2, "and the new canvas is a view");
    let carried = other.edit_base_layer(|mut widget| {
        widget
            .downcast::<EditorScreen<Grid>>()
            .widget
            .area_ids()
            .first()
            .copied()
    });
    let carried = carried.expect("the detached window has an area");
    let same = other.edit_widget(carried, |mut widget| {
        let editor = widget.downcast::<NodeEditor<Grid>>();
        editor.widget.session().is_some_and(|own| Rc::ptr_eq(own, &session))
    });
    assert!(same, "the area in the new window shows the session it had");
}

/// A change that touched nothing wakes nobody.
#[test]
fn an_idle_graph_wakes_no_window() {
    let graph = Grid::new(8);
    let mut app = app(&graph);
    let mut root = window(&app, 2);
    let mut cx = ShellCtx::new();
    let key = cx.name_window();
    app.started(&mut cx, key, &mut root);
    let _ = cx.drain();
    app.settled(&mut cx, key, &mut root);
    assert!(cx.drain().is_empty());
    assert_eq!(app.counters().wakes, 0);
}

/// No default screen binding shadows a binding of the editor's.
///
/// The screen's keys are offered every event before the tree is, so a collision is not a
/// conflict to resolve but an editor binding that can never fire — which is what plain
/// `X` for a split did to `node.delete`.
#[test]
fn the_default_screen_keys_shadow_no_editor_binding() {
    let keymap = default_keymap();
    for (pattern, action) in ScreenKeys::default().bindings {
        let Trigger::Key(key) = pattern.trigger.clone() else {
            panic!("{action:?} is bound to something other than a key");
        };
        let event = OpEvent::Key {
            key,
            mods: pattern.mods,
            down: true,
        };
        let shadowed: Vec<_> = keymap.matches(Scope(&CANVAS_SCOPE), &event).collect();
        assert!(shadowed.is_empty(), "{action:?} shadows {shadowed:?}");
    }
}

/// A key reaches the action it is bound to, with its modifiers exactly.
#[test]
fn a_bound_key_names_its_action_and_an_unbound_one_nothing() {
    let keys = ScreenKeys::default();
    let event = |name: &str, mods: Modifiers| {
        masonry::core::TextEvent::Keyboard(KeyboardEvent {
            state: KeyState::Down,
            key: Key::Character(name.into()),
            code: Code::Unidentified,
            modifiers: mods,
            ..KeyboardEvent::default()
        })
    };
    assert_eq!(
        keys.action_for(&event("x", Modifiers::ALT)),
        Some(ScreenAction::SplitHorizontal)
    );
    assert_eq!(
        keys.action_for(&event("x", Modifiers::empty())),
        None,
        "plain X is the editor's"
    );
}

/// Keys go to the editor of the area under the pointer.
///
/// Masonry hands a key to the focus fallback and nobody else; a window that names none
/// has editors that never hear a key — which is what the window this crate replaced was,
/// and nothing failed, because nothing that was not sent can fail.
#[test]
fn keys_go_to_the_area_under_the_pointer() {
    use masonry::core::{PointerEvent, PointerInfo, PointerState, PointerType, PointerUpdate, TextEvent};
    use masonry::dpi::PhysicalPosition;

    let graph = Grid::new(8);
    let mut app = app(&graph);
    let mut root = window(&app, 2);
    let mut cx = ShellCtx::new();
    let key = cx.name_window();
    app.started(&mut cx, key, &mut root);

    let zoom = |root: &mut RenderRoot, area: usize| {
        edit_editor(root, area, |editor| {
            NodeEditor::with_canvas(editor, |canvas| canvas.widget.zoom())
        })
    };
    let before = (zoom(&mut root, 0), zoom(&mut root, 1));

    // Over the right-hand area, the way the shell delivers a move — then `settled`.
    let _ = root.handle_pointer_event(PointerEvent::Move(PointerUpdate {
        pointer: PointerInfo {
            pointer_id: None,
            persistent_device_id: None,
            pointer_type: PointerType::Mouse,
        },
        current: PointerState {
            position: PhysicalPosition::new(900.0, 400.0),
            ..Default::default()
        },
        coalesced: vec![],
        predicted: vec![],
    }));
    app.settled(&mut cx, key, &mut root);
    let _ = root.handle_text_event(TextEvent::Keyboard(KeyboardEvent {
        state: KeyState::Down,
        key: Key::Character("-".into()),
        code: Code::Unidentified,
        modifiers: Modifiers::empty(),
        ..KeyboardEvent::default()
    }));
    let _ = root.redraw();

    assert_eq!(
        zoom(&mut root, 0),
        before.0,
        "the area the pointer is not over heard nothing"
    );
    assert!(zoom(&mut root, 1) < before.1, "the one it is over zoomed out");
    assert!(app.counters().key_targets >= 1);
}

/// One file rebinds an editor operator and a screen action, and both take.
#[test]
fn one_file_overrides_the_editor_and_the_screen() {
    let overrides = "blazy-keymap 1\n\
        context screen\n\
        unbind alt+key:x screen.split_horizontal\n\
        bind ctrl+alt+key:h screen.split_horizontal\n\
        context canvas\n\
        unbind key:x node.delete\n\
        bind key:d node.delete\n";
    let path = std::env::temp_dir().join(format!("blazy-app-overrides-{}.keymap", std::process::id()));
    std::fs::write(&path, overrides).unwrap();

    let graph = Grid::new(8);
    let app = app(&graph).with_keymap_overrides(&path).expect("the overrides read");
    let _ = std::fs::remove_file(&path);

    let key = |name: &str, mods: Modifiers| {
        masonry::core::TextEvent::Keyboard(KeyboardEvent {
            state: KeyState::Down,
            key: Key::Character(name.into()),
            code: Code::Unidentified,
            modifiers: mods,
            ..KeyboardEvent::default()
        })
    };
    assert_eq!(
        app.keys.action_for(&key("h", Modifiers::CONTROL | Modifiers::ALT)),
        Some(ScreenAction::SplitHorizontal)
    );
    assert_eq!(
        app.keys.action_for(&key("x", Modifiers::ALT)),
        None,
        "the old binding is gone"
    );

    // And the editors of this application run with the same file in force.
    let screen = app.screen(SplitTree::balanced(1));
    let session = screen.payload(0).expect("an area carries a session").clone();
    let keymap = session.borrow().runtime.keymap().clone();
    let event = OpEvent::Key {
        key: Key::Character("d".into()),
        mods: Modifiers::empty(),
        down: true,
    };
    let ops: Vec<String> = keymap
        .matches(Scope(&CANVAS_SCOPE), &event)
        .map(|binding| binding.op.to_string())
        .collect();
    assert_eq!(ops, ["node.delete"]);
}

/// A bad overrides file leaves the application as it was and says which line.
#[test]
fn a_bad_overrides_file_names_its_line() {
    let path = std::env::temp_dir().join(format!("blazy-app-bad-{}.keymap", std::process::id()));
    std::fs::write(&path, "blazy-keymap 1\ncontext screen\nbind alt+key:q screen.quit\n").unwrap();
    let graph = Grid::new(8);
    let error = app(&graph).with_keymap_overrides(&path).err();
    let _ = std::fs::remove_file(&path);
    assert!(
        matches!(error, Some(crate::KeymapLoadError::Screen(ref unknown)) if unknown.0 == "screen.quit"),
        "{error:?}"
    );
}

/// The application's whole default keymap — editor and screen — survives its own file.
#[test]
fn the_default_keymap_reads_back_from_its_file() {
    let keymap = crate::default_keymap();
    assert_eq!(blazy_ops::keymap::Keymap::parse(&keymap.write()), Ok(keymap));
}
