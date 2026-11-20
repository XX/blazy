//! A widget that shows something of the graph without being a canvas — a properties panel.

use std::any::TypeId;
use std::collections::BTreeSet;
use std::rc::Rc;

use blazy_areas::{CarriesScale, UiScale, push_ui_scale};
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PropertiesMut, PropertiesRef,
    RegisterCtx, Update, UpdateCtx, Widget, WidgetId, WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Size};
use masonry::layout::{LenReq, Length, SizeDef};

use crate::{NodeGraph, SharedGraph, WatchToken};

/// What builds a panel's content from the graph, the selection and the panel's own id. A
/// field in the content records its changes past that id, so typing does not rebuild it.
pub type BuildPanel<G> = dyn Fn(&SharedGraph<G>, &BTreeSet<usize>, WidgetId) -> NewWidget<dyn Widget>;

/// What a panel did, summed over its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PanelCounters {
    /// Times the content was built, the first build included.
    pub builds: u64,
}

/// Shows what the application builds from the graph and its selection, and rebuilds it
/// whole when something it can show changes (the selection, content, structure, a move of
/// a selected node), in any area or window.
///
/// A watcher of the graph's [`Views`](crate::Views): refreshed by the editor in the same
/// window, by the pull in another; many changes before a frame cost one build. Carries the
/// region's interface scale to its content ([`CarriesScale`]).
pub struct GraphPanel<G: NodeGraph> {
    graph: SharedGraph<G>,
    build: Rc<BuildPanel<G>>,
    child: Option<WidgetPod<dyn Widget>>,
    token: Option<WatchToken>,
    counters: PanelCounters,
}

impl<G: NodeGraph> GraphPanel<G> {
    /// A panel over `graph`, its content built by `build`.
    pub fn new(
        graph: &SharedGraph<G>,
        build: impl Fn(&SharedGraph<G>, &BTreeSet<usize>, WidgetId) -> NewWidget<dyn Widget> + 'static,
    ) -> Self {
        Self {
            graph: graph.clone(),
            build: Rc::new(build),
            child: None,
            token: None,
            counters: PanelCounters::default(),
        }
    }

    /// What this panel did.
    pub fn counters(&self) -> PanelCounters {
        self.counters
    }

    /// The content's id, once it is built.
    pub fn content_id(&self) -> Option<WidgetId> {
        self.child.as_ref().map(WidgetPod::id)
    }

    /// The content, built from the graph as it is now, at `scale`.
    fn build_content(&mut self, panel: WidgetId, scale: f64) -> WidgetPod<dyn Widget> {
        self.counters.builds += 1;
        let selection = self.graph.borrow().views().selection();
        (self.build)(&self.graph, &selection, panel)
            .with_props(UiScale(scale))
            .to_pod()
    }

    /// Builds the content again from the graph.
    pub fn refresh(this: &mut WidgetMut<'_, Self>) {
        let panel = this.ctx.widget_id();
        this.widget.graph.borrow().views().refreshed(panel);
        let scale = this.get_prop::<UiScale>().0;
        if let Some(old) = this.widget.child.take() {
            this.ctx.remove_child(old);
        }
        let content = this.widget.build_content(panel, scale);
        this.widget.child = Some(content);
        this.ctx.children_changed();
        this.ctx.request_layout();
    }

    /// [`refresh`](Self::refresh), for a `WidgetMut` whose type the registry does not know.
    fn refresh_dyn(widget: &mut WidgetMut<'_, dyn Widget>) {
        if let Some(mut panel) = widget.try_downcast::<Self>() {
            Self::refresh(&mut panel);
        }
    }
}

impl<G: NodeGraph> CarriesScale for GraphPanel<G> {
    fn carry_now(this: &mut WidgetMut<'_, Self>, scale: f64) {
        if let Some(child) = this.widget.child.as_mut() {
            let mut child = this.ctx.get_mut(child);
            push_ui_scale(&mut child, scale);
        }
        this.ctx.request_layout();
    }
}

impl<G: NodeGraph> Widget for GraphPanel<G> {
    type Action = NoAction;

    fn property_changed(&mut self, ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        UiScale::prop_changed(ctx, property_type);
    }

    /// Joins the graph's watchers once it knows its id, and builds its first content.
    fn update(&mut self, ctx: &mut UpdateCtx<'_>, _props: &mut PropertiesMut<'_>, event: &Update) {
        if matches!(event, Update::WidgetAdded) && self.token.is_none() {
            let id = ctx.widget_id();
            self.token = Some(self.graph.borrow().views().watch(id, Self::refresh_dyn));
            ctx.mutate_self_later(|mut this| Self::refresh(&mut this.downcast::<Self>()));
        }
    }

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        _axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        match len_req {
            LenReq::FitContent(space) => space,
            _ => Length::ZERO,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        if let Some(child) = self.child.as_mut() {
            let chosen = ctx.compute_size(child, SizeDef::fixed(size), size.into());
            ctx.run_layout(child, chosen);
            ctx.place_child(child, Point::ORIGIN);
        }
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        if let Some(child) = self.child.as_mut() {
            ctx.register_child(child);
        }
    }

    fn children_ids(&self) -> ChildrenIds {
        self.child.iter().map(WidgetPod::id).collect()
    }

    fn accessibility_role(&self) -> Role {
        Role::Pane
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}
