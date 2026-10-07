//! The keymap: data, not code.
//!
//! A binding is `(context, event pattern) -> (operator, properties)`, which is §11's
//! shape and Blender's. Nothing here holds a closure or a type: an operator is named
//! by a string and its arguments are values, so a keymap can be written to a file,
//! edited by a user and read back. That is the whole reason the indirection exists —
//! menus, macros and user overrides all become possible later without any of them
//! being built now.
//!
//! **Contexts nest, and the innermost wins.** A [`Scope`] is the chain of contexts an
//! event happened in, innermost first — the canvas inside an area inside the screen
//! inside the window. Lookup walks it outwards and stops at the first binding whose
//! operator's poll agrees, so a canvas binding shadows a window one, and a window
//! binding still catches what no canvas claimed.

use std::borrow::Cow;

use crate::event::{Device, OpEvent, Pattern};
pub use crate::keyfile::KeymapError;

/// A name in a keymap: an operator's, a context's or a property's.
///
/// `Cow` rather than `&'static str`, because a keymap is data (see the module docs). One
/// written in code borrows its literals and costs what it cost before; one read from a
/// file owns what it read. With `&'static str` the second kind could only be built by
/// leaking every string it contained, and user overrides — the reason the keymap is
/// data at all — would have had nothing to be made of.
///
/// An operator's own [`name`](crate::Operator::name) stays `&'static str`: operators are
/// code, registered from code, and it is only the keymap that has to survive a file.
pub type Name = Cow<'static, str>;

/// One property value.
///
/// Three cases, because they are the three an operator has needed so far and a value
/// type is easier to widen than to narrow. Strings are deliberately absent: the first
/// one will want an enum, and an enum written as a string is a bug waiting for a typo.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    /// A flag, e.g. `extend`.
    Bool(bool),
    /// A whole number, e.g. `index`.
    Int(i64),
    /// A distance or a fraction, e.g. `dx`.
    Float(f64),
}

/// An operator's arguments, as data.
///
/// A short association list rather than a map: a binding carries one or two of these
/// and is looked up on an event, so a hash would cost more than the scan it replaces.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Props(Vec<(Name, Value)>);

impl Props {
    /// No properties at all.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// The same properties, with a flag set.
    #[must_use]
    pub fn with_bool(mut self, name: impl Into<Name>, value: bool) -> Self {
        self.0.push((name.into(), Value::Bool(value)));
        self
    }

    /// The same properties, with a whole number set.
    #[must_use]
    pub fn with_int(mut self, name: impl Into<Name>, value: i64) -> Self {
        self.0.push((name.into(), Value::Int(value)));
        self
    }

    /// The same properties, with a float set.
    #[must_use]
    pub fn with_float(mut self, name: impl Into<Name>, value: f64) -> Self {
        self.0.push((name.into(), Value::Float(value)));
        self
    }

    /// The value of `name`, or `default` if the binding did not set it.
    pub fn bool(&self, name: &str, default: bool) -> bool {
        match self.get(name) {
            Some(Value::Bool(value)) => value,
            _ => default,
        }
    }

    /// As [`bool`](Self::bool), for an integer property.
    pub fn int(&self, name: &str, default: i64) -> i64 {
        match self.get(name) {
            Some(Value::Int(value)) => value,
            _ => default,
        }
    }

    /// As [`bool`](Self::bool), for a float property.
    ///
    /// An integer is accepted where a float is asked for, because `{"dx": 30}` is what
    /// a hand-written keymap will say and refusing it teaches nothing.
    pub fn float(&self, name: &str, default: f64) -> f64 {
        match self.get(name) {
            Some(Value::Float(value)) => value,
            Some(Value::Int(value)) => value as f64,
            _ => default,
        }
    }

    /// Sets `name` to `value`, replacing what it was.
    ///
    /// The in-place twin of the `with_*` builders, for a reader that does not know the
    /// value's kind until it has read it. A name set twice keeps the last value, as a
    /// file that says it twice means.
    pub fn set(&mut self, name: Name, value: Value) {
        match self.0.iter_mut().find(|(key, _)| *key == name) {
            Some((_, slot)) => *slot = value,
            None => self.0.push((name, value)),
        }
    }

    /// Every property, in the order it was set.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Value)> {
        self.0.iter().map(|(name, value)| (name.as_ref(), *value))
    }

    /// Whether any property is set.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn get(&self, name: &str) -> Option<Value> {
        self.0.iter().find(|(key, _)| key == name).map(|(_, value)| *value)
    }
}

/// One entry of a keymap.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    /// The event this binding fires on.
    pub pattern: Pattern,
    /// The operator's name, as [`Operator::name`](crate::Operator::name) gives it.
    pub op: Name,
    /// The arguments the operator is run with.
    pub props: Props,
}

impl Binding {
    /// A binding with no properties.
    pub fn new(pattern: Pattern, op: impl Into<Name>) -> Self {
        Self {
            pattern,
            op: op.into(),
            props: Props::new(),
        }
    }

    /// The same binding, with the operator's arguments.
    #[must_use]
    pub fn with_props(mut self, props: Props) -> Self {
        self.props = props;
        self
    }
}

/// The bindings that apply in one context.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    /// The context this section belongs to, e.g. `"canvas"`.
    pub context: Name,
    /// Bindings, in the order they are tried.
    ///
    /// Order is meaning: two bindings may share a pattern and be told apart by their
    /// operators' polls — press-on-a-node selects, press-on-nothing starts a box —
    /// and that is the mechanism, not a workaround for one.
    pub bindings: Vec<Binding>,
}

/// How far and how fast a gesture has to be to count as one.
///
/// **On the keymap rather than on a binding**, and that is not filing convenience: two
/// bindings on the same button that disagreed about what a click is would resolve the
/// same gesture two ways, and which one the user got would depend on binding order.
/// Blender puts these in preferences for the same reason, and computes the answer in the
/// window manager before the keymap ever sees the event (§39.2).
///
/// The distances are in **screen pixels**, taken from [`Sample::screen`](crate::event::Sample);
/// a threshold measured in the driver's own units would mean something different at
/// every zoom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    /// Travel past which a mouse press is a drag. Blender's default is 3 px.
    pub drag_mouse: f64,
    /// The same for a pen or stylus, which shakes more. Blender's tablet default is 10 px.
    pub drag_pen: f64,
    /// The same for a finger, which shakes more again and covers more of the screen.
    pub drag_touch: f64,
    /// How long after a click a second one still counts as a double click.
    ///
    /// Counted here rather than taken from the platform's own `PointerState::count`:
    /// `ui-events-winit` fills that in, other backends may not, and a double click that
    /// works on one backend is worse than none (§39.2).
    pub double_click_ms: u64,
    /// How far apart two clicks may be and still be a double click.
    pub double_click_slop: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            drag_mouse: 3.0,
            drag_pen: 10.0,
            drag_touch: 15.0,
            double_click_ms: 250,
            double_click_slop: 6.0,
        }
    }
}

impl Thresholds {
    /// The drag threshold for the device that produced the press.
    pub fn drag_for(&self, device: Device) -> f64 {
        match device {
            Device::Mouse | Device::Other => self.drag_mouse,
            Device::Pen => self.drag_pen,
            Device::Touch => self.drag_touch,
        }
    }
}

/// A whole keymap.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Keymap {
    sections: Vec<Section>,
    thresholds: Thresholds,
}

/// The chain of contexts an event happened in, innermost first.
///
/// §11's nesting — Window → Screen → Area → Region → the active modal operator — with
/// the modal end handled by the runtime's stack rather than by a name here. The names
/// are the application's: this crate never invents one.
#[derive(Clone, Copy, Debug)]
pub struct Scope<'a>(
    /// The context names, innermost first.
    pub &'a [&'static str],
);

impl Keymap {
    /// A keymap with no bindings and the default [`Thresholds`].
    pub fn new() -> Self {
        Self::default()
    }

    /// The thresholds a gesture is measured against.
    pub fn thresholds(&self) -> Thresholds {
        self.thresholds
    }

    /// Replaces them. What a preferences panel, or a test with a fat finger, does.
    #[must_use]
    pub fn with_thresholds(mut self, thresholds: Thresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Adds a section, or appends to the one that is already there.
    #[must_use]
    pub fn with(mut self, context: impl Into<Name>, bindings: Vec<Binding>) -> Self {
        let context = context.into();
        match self.sections.iter_mut().find(|section| section.context == context) {
            Some(section) => section.bindings.extend(bindings),
            None => self.sections.push(Section { context, bindings }),
        }
        self
    }

    /// The bindings matching `event`, innermost context first.
    ///
    /// Returns every match rather than the first, because whether a binding runs is
    /// decided by its operator's poll and the runtime is the one holding the
    /// operators. A keymap that answered "the binding" would have to know them.
    pub fn matches<'a>(&'a self, scope: Scope<'a>, event: &'a OpEvent) -> impl Iterator<Item = &'a Binding> + 'a {
        self.sections_for(scope)
            .filter(move |binding| binding.pattern.matches(event))
    }

    /// Every binding in scope, innermost context first, whatever the event.
    ///
    /// [`matches`](Self::matches) answers "what fires on this event"; this answers "what
    /// could fire on this button at all", which is what the runtime needs *before* it
    /// knows whether a press will become a click or a drag. The one is the other with a
    /// filter, and they are written that way so the *order* they walk the scope in
    /// cannot come apart.
    ///
    /// The scope is borrowed for as long as the iterator lives rather than copied into
    /// one: a lookup happens on every input event, and this is called twice on a press.
    pub fn sections_for<'a>(&'a self, scope: Scope<'a>) -> impl Iterator<Item = &'a Binding> + 'a {
        scope.0.iter().flat_map(move |&context| {
            self.sections
                .iter()
                .filter(move |section| section.context == context)
                .flat_map(|section| section.bindings.iter())
        })
    }

    /// Every context's bindings, in the order the contexts were first added.
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// Takes out the first binding of `op` on `pattern` in `context`, and says whether
    /// there was one.
    pub fn remove(&mut self, context: &str, pattern: &Pattern, op: &str) -> bool {
        let Some(section) = self.sections.iter_mut().find(|section| section.context == context) else {
            return false;
        };
        match section
            .bindings
            .iter()
            .position(|binding| binding.pattern == *pattern && binding.op == op)
        {
            Some(at) => {
                section.bindings.remove(at);
                true
            },
            None => false,
        }
    }

    /// Puts `bindings` ahead of what `context` already holds, in their own order.
    ///
    /// What an override is: tried before the binding it overrides.
    pub fn prepend(&mut self, context: impl Into<Name>, bindings: Vec<Binding>) {
        let context = context.into();
        match self.sections.iter_mut().find(|section| section.context == context) {
            Some(section) => {
                section.bindings.splice(0..0, bindings);
            },
            None => self.sections.push(Section { context, bindings }),
        }
    }

    /// The bindings in a context, for a menu or a "what is bound to this" report.
    pub fn section(&self, context: &str) -> Option<&Section> {
        self.sections.iter().find(|section| section.context == context)
    }

    /// Total bindings, over every context.
    pub fn len(&self) -> usize {
        self.sections.iter().map(|section| section.bindings.len()).sum()
    }

    /// Whether the keymap binds anything at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use masonry::core::keyboard::Modifiers;
    use masonry::kurbo::Point;
    use masonry::ui_events::pointer::PointerButton;

    use super::*;

    fn press(mods: Modifiers) -> OpEvent {
        OpEvent::Press {
            button: PointerButton::Primary,
            pos: Point::ORIGIN,
            mods,
        }
    }

    fn keymap() -> Keymap {
        Keymap::new()
            .with("canvas", vec![
                Binding::new(Pattern::press(PointerButton::Primary), "node.select"),
                Binding::new(Pattern::press(PointerButton::Primary), "node.box_select"),
            ])
            .with("window", vec![Binding::new(
                Pattern::press(PointerButton::Primary),
                "window.click",
            )])
    }

    /// The nesting of §11, as a test: the inner context is offered first and the outer
    /// one still gets its turn.
    #[test]
    fn the_innermost_context_is_tried_first() {
        let keymap = keymap();
        let names: Vec<_> = keymap
            .matches(Scope(&["canvas", "window"]), &press(Modifiers::empty()))
            .map(|binding| binding.op.to_string())
            .collect();
        assert_eq!(names, ["node.select", "node.box_select", "window.click"]);
    }

    /// A scope that does not contain a context does not see its bindings — which is
    /// what makes "this key does something else in the other editor" expressible.
    #[test]
    fn a_context_outside_the_scope_is_not_matched() {
        let keymap = keymap();
        let names: Vec<_> = keymap
            .matches(Scope(&["window"]), &press(Modifiers::empty()))
            .map(|binding| binding.op.to_string())
            .collect();
        assert_eq!(names, ["window.click"]);
    }

    #[test]
    fn properties_fall_back_to_the_default() {
        let props = Props::new().with_bool("extend", true).with_int("count", 3);
        assert!(props.bool("extend", false));
        assert!(!props.bool("missing", false));
        assert_eq!(props.int("count", 0), 3);
        // An integer where a float was asked for: what a hand-written keymap says.
        assert_eq!(props.float("count", 0.0), 3.0);
        assert_eq!(props.float("dx", 1.5), 1.5);
    }
}
