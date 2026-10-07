//! The keymap as a file: written, read back, and laid over another as a user's overrides.
//!
//! The reason the keymap is data (§11, §38): a user rebinds a key without the application
//! being rebuilt. The format is line-based, like the workspace file, and for the same
//! reasons (§41.6) — no `serde`, a version on the first line, and a file is read as input:
//!
//! ```text
//! blazy-keymap 1
//! thresholds drag_mouse=3 drag_pen=10 drag_touch=15 double_click_ms=250 double_click_slop=6
//! context canvas
//! bind drag:primary node.move
//! bind shift+click:primary node.select extend=true
//! bind key:x node.delete
//! bind ctrl+scroll view.zoom
//! unbind key:f link.add
//! ```
//!
//! A pattern is `[modifiers+]trigger[:argument]`. The modifiers are `ctrl`, `shift`, `alt`
//! and `meta`; the triggers `press`, `release`, `click`, `drag` and `double-click` take a
//! button (`primary`, `secondary`, `middle`, `x1`, `x2`, `eraser`, `b7`…`b32`), `key` and
//! `key-up` take a key as the W3C names it (`g`, `Escape`, `ArrowLeft`; a whitespace
//! character as `U+0020`), and `scroll` takes nothing. Properties are `name=value`, where a
//! value is `true`, `false`, a whole number or a number with a point in it.
//!
//! # One format, two uses
//!
//! A file **lays itself over** a keymap: its `unbind` lines take bindings out, and its
//! `bind` lines go in **ahead** of what a context already holds, in the order the file
//! gives them. Read over an empty keymap that is the whole keymap, in file order — which
//! is what [`Keymap::parse`] does. Read over the application's defaults it is a user's
//! overrides — [`Keymap::patched`] — and a user's binding on a key the defaults also use
//! is tried first, which is what an override means.
//!
//! An `unbind` that finds nothing to take out is an **error**, not a no-op. It is the
//! normal way an override file goes stale — the defaults changed under it — and a miss
//! that looks like success is the shape every silent defect in this project has had
//! (§45).

use std::fmt::{self, Write as _};

use masonry::core::keyboard::{Key, Modifiers};
use masonry::ui_events::pointer::PointerButton;

use crate::event::{Pattern, Trigger};
use crate::keymap::{Binding, Keymap, Name, Props, Thresholds, Value};

/// The magic word and the version this build reads and writes.
const MAGIC: &str = "blazy-keymap";
const VERSION: u32 = 1;

/// Why a keymap file could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeymapError {
    /// The first line is not this format's, or names a version this build cannot read.
    NotAKeymap,
    /// A line could not be read. Lines are numbered from one.
    Malformed {
        /// The line the reader gave up on.
        line: usize,
        /// What was wrong with it.
        reason: &'static str,
    },
    /// An `unbind` line names a binding the keymap does not hold.
    NothingToUnbind {
        /// The line that names it.
        line: usize,
    },
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAKeymap => write!(f, "not a {MAGIC} file this build can read"),
            Self::Malformed { line, reason } => write!(f, "line {line}: {reason}"),
            Self::NothingToUnbind { line } => write!(f, "line {line}: there is no such binding to remove"),
        }
    }
}

impl std::error::Error for KeymapError {}

impl Keymap {
    /// Reads a whole keymap from its file.
    ///
    /// # Errors
    ///
    /// When the file is not a keymap this build can read, or a line of it cannot be read.
    pub fn parse(text: &str) -> Result<Self, KeymapError> {
        Self::new().patched(text)
    }

    /// Lays a file over this keymap: its `unbind` lines take bindings out, its `bind`
    /// lines go in ahead of the context's own, and a `thresholds` line replaces the
    /// thresholds it names.
    ///
    /// All or nothing: a file with a bad line changes nothing, so a typo in a user's
    /// overrides leaves the defaults whole rather than half-replaced.
    ///
    /// # Errors
    ///
    /// When the file is not a keymap, a line cannot be read, or an `unbind` names a
    /// binding that is not there.
    pub fn patched(self, text: &str) -> Result<Self, KeymapError> {
        let mut lines = text.lines().enumerate().map(|(index, line)| (index + 1, line.trim()));
        match lines.next() {
            Some((_, header)) if header == format!("{MAGIC} {VERSION}") => {},
            _ => return Err(KeymapError::NotAKeymap),
        }

        let mut keymap = self;
        let mut thresholds = keymap.thresholds();
        // What the file binds, by context, in file order: they go in ahead of the
        // context's own bindings once every `unbind` has had its turn.
        let mut added: Vec<(Name, Vec<Binding>)> = Vec::new();
        let mut context: Option<Name> = None;

        for (line, text) in lines {
            if text.is_empty() || text.starts_with('#') {
                continue;
            }
            let malformed = |reason| KeymapError::Malformed { line, reason };
            let mut words = text.split_whitespace();
            match words.next() {
                Some("thresholds") => {
                    for word in words {
                        let (name, value) = word.split_once('=').ok_or(malformed("a threshold is name=value"))?;
                        let value: f64 = value.parse().map_err(|_| malformed("a threshold is a number"))?;
                        set_threshold(&mut thresholds, name, value).ok_or(malformed("no such threshold"))?;
                    }
                },
                Some("context") => {
                    let name = words.next().ok_or(malformed("a context needs a name"))?;
                    if words.next().is_some() {
                        return Err(malformed("a context name is one word"));
                    }
                    context = Some(Name::Owned(name.to_owned()));
                },
                Some(verb @ ("bind" | "unbind")) => {
                    let context = context.clone().ok_or(malformed("a binding before any context"))?;
                    let pattern = parse_pattern(words.next().ok_or(malformed("a binding needs a pattern"))?)
                        .map_err(malformed)?;
                    let op = words.next().ok_or(malformed("a binding needs an operator"))?;
                    let mut props = Props::new();
                    for word in words {
                        if verb == "unbind" {
                            return Err(malformed("an unbind names a pattern and an operator, nothing more"));
                        }
                        let (name, value) = word.split_once('=').ok_or(malformed("a property is name=value"))?;
                        props.set(Name::Owned(name.to_owned()), parse_value(value).map_err(malformed)?);
                    }
                    if verb == "bind" {
                        let binding = Binding::new(pattern, Name::Owned(op.to_owned())).with_props(props);
                        match added.iter_mut().find(|(name, _)| *name == context) {
                            Some((_, bindings)) => bindings.push(binding),
                            None => added.push((context, vec![binding])),
                        }
                    } else if !keymap.remove(&context, &pattern, op) {
                        return Err(KeymapError::NothingToUnbind { line });
                    }
                },
                _ => return Err(malformed("not a line of a keymap")),
            }
        }

        for (context, bindings) in added {
            keymap.prepend(context, bindings);
        }
        Ok(keymap.with_thresholds(thresholds))
    }

    /// Writes this keymap as a file [`parse`](Self::parse) reads back to the same keymap.
    pub fn write(&self) -> String {
        let mut out = format!("{MAGIC} {VERSION}\n");
        let t = self.thresholds();
        let _ = writeln!(
            out,
            "thresholds drag_mouse={} drag_pen={} drag_touch={} double_click_ms={} double_click_slop={}",
            t.drag_mouse, t.drag_pen, t.drag_touch, t.double_click_ms, t.double_click_slop
        );
        for section in self.sections() {
            let _ = writeln!(out, "context {}", section.context);
            for binding in &section.bindings {
                let _ = write!(out, "bind {} {}", write_pattern(&binding.pattern), binding.op);
                for (name, value) in binding.props.iter() {
                    let _ = write!(out, " {name}={}", write_value(value));
                }
                out.push('\n');
            }
        }
        out
    }
}

fn set_threshold(thresholds: &mut Thresholds, name: &str, value: f64) -> Option<()> {
    match name {
        "drag_mouse" => thresholds.drag_mouse = value,
        "drag_pen" => thresholds.drag_pen = value,
        "drag_touch" => thresholds.drag_touch = value,
        // A duration in whole milliseconds; a fraction or a negative is a typo.
        "double_click_ms" if value >= 0.0 && value.fract() == 0.0 => thresholds.double_click_ms = value as u64,
        "double_click_slop" => thresholds.double_click_slop = value,
        _ => return None,
    }
    Some(())
}

/// The modifiers a file can name, in the order they are written.
const MODIFIERS: [(&str, Modifiers); 4] = [
    ("ctrl", Modifiers::CONTROL),
    ("shift", Modifiers::SHIFT),
    ("alt", Modifiers::ALT),
    ("meta", Modifiers::META),
];

/// Every button a pattern can name.
///
/// The first six by what they are; the rest by number, as the device reports them.
const BUTTONS: [(&str, PointerButton); 32] = [
    ("primary", PointerButton::Primary),
    ("secondary", PointerButton::Secondary),
    ("middle", PointerButton::Auxiliary),
    ("x1", PointerButton::X1),
    ("x2", PointerButton::X2),
    ("eraser", PointerButton::PenEraser),
    ("b7", PointerButton::B7),
    ("b8", PointerButton::B8),
    ("b9", PointerButton::B9),
    ("b10", PointerButton::B10),
    ("b11", PointerButton::B11),
    ("b12", PointerButton::B12),
    ("b13", PointerButton::B13),
    ("b14", PointerButton::B14),
    ("b15", PointerButton::B15),
    ("b16", PointerButton::B16),
    ("b17", PointerButton::B17),
    ("b18", PointerButton::B18),
    ("b19", PointerButton::B19),
    ("b20", PointerButton::B20),
    ("b21", PointerButton::B21),
    ("b22", PointerButton::B22),
    ("b23", PointerButton::B23),
    ("b24", PointerButton::B24),
    ("b25", PointerButton::B25),
    ("b26", PointerButton::B26),
    ("b27", PointerButton::B27),
    ("b28", PointerButton::B28),
    ("b29", PointerButton::B29),
    ("b30", PointerButton::B30),
    ("b31", PointerButton::B31),
    ("b32", PointerButton::B32),
];

fn parse_pattern(word: &str) -> Result<Pattern, &'static str> {
    // The argument first: a key may itself be `+` or contain one, and it comes after
    // the colon, where splitting the modifiers off cannot reach it.
    let (head, arg) = match word.split_once(':') {
        Some((head, arg)) => (head, Some(arg)),
        None => (word, None),
    };
    let mut parts: Vec<&str> = head.split('+').collect();
    let trigger = parts.pop().ok_or("a pattern needs a trigger")?;
    let mut mods = Modifiers::empty();
    for part in parts {
        let (_, modifier) = MODIFIERS
            .iter()
            .find(|(name, _)| *name == part)
            .ok_or("no such modifier")?;
        mods |= *modifier;
    }
    let button = || -> Result<PointerButton, &'static str> {
        let arg = arg.ok_or("this trigger needs a button")?;
        BUTTONS
            .iter()
            .find(|(name, _)| *name == arg)
            .map(|(_, button)| *button)
            .ok_or("no such button")
    };
    let key = || -> Result<Key, &'static str> { parse_key(arg.ok_or("this trigger needs a key")?) };
    let trigger = match trigger {
        "press" => Trigger::Press(button()?),
        "release" => Trigger::Release(button()?),
        "click" => Trigger::Click(button()?),
        "drag" => Trigger::Drag(button()?),
        "double-click" => Trigger::DoubleClick(button()?),
        "key" => Trigger::Key(key()?),
        "key-up" => Trigger::KeyUp(key()?),
        "scroll" if arg.is_none() => Trigger::Scroll,
        "scroll" => return Err("scroll takes no argument"),
        _ => return Err("no such trigger"),
    };
    Ok(Pattern::new(trigger).with_mods(mods))
}

fn write_pattern(pattern: &Pattern) -> String {
    let mut out = String::new();
    for (name, modifier) in MODIFIERS {
        if pattern.mods.contains(modifier) {
            out.push_str(name);
            out.push('+');
        }
    }
    let button = |button: PointerButton| {
        BUTTONS
            .iter()
            .find(|(_, b)| *b == button)
            .map_or("primary", |(name, _)| *name)
    };
    match &pattern.trigger {
        Trigger::Press(b) => write!(out, "press:{}", button(*b)),
        Trigger::Release(b) => write!(out, "release:{}", button(*b)),
        Trigger::Click(b) => write!(out, "click:{}", button(*b)),
        Trigger::Drag(b) => write!(out, "drag:{}", button(*b)),
        Trigger::DoubleClick(b) => write!(out, "double-click:{}", button(*b)),
        Trigger::Key(key) => write!(out, "key:{}", write_key(key)),
        Trigger::KeyUp(key) => write!(out, "key-up:{}", write_key(key)),
        Trigger::Scroll => write!(out, "scroll"),
    }
    .ok();
    out
}

impl Pattern {
    /// The pattern as a person reads it: `Shift+A`, `Ctrl+Z`, `Drag Middle`, `Wheel`.
    ///
    /// For a menu entry's shortcut and a "what is bound" report — not for a file, which is
    /// what [`Keymap::write`] is for: this one loses the difference between a key going
    /// down and coming up, and does not read back.
    pub fn label(&self) -> String {
        let mut out = String::new();
        for (name, modifier) in [
            ("Ctrl", Modifiers::CONTROL),
            ("Shift", Modifiers::SHIFT),
            ("Alt", Modifiers::ALT),
            ("Meta", Modifiers::META),
        ] {
            if self.mods.contains(modifier) {
                out.push_str(name);
                out.push('+');
            }
        }
        let button = |button: PointerButton| {
            let name = BUTTONS
                .iter()
                .find(|(_, b)| *b == button)
                .map_or("?", |(name, _)| *name);
            let mut chars = name.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        };
        let key = |key: &Key| match key {
            Key::Character(text) if text == " " => "Space".to_owned(),
            Key::Character(text) => text.to_uppercase(),
            Key::Named(named) => named.to_string(),
        };
        match &self.trigger {
            Trigger::Press(b) => write!(out, "Press {}", button(*b)),
            Trigger::Release(b) => write!(out, "Release {}", button(*b)),
            Trigger::Click(b) => write!(out, "Click {}", button(*b)),
            Trigger::Drag(b) => write!(out, "Drag {}", button(*b)),
            Trigger::DoubleClick(b) => write!(out, "Double-click {}", button(*b)),
            Trigger::Key(k) => write!(out, "{}", key(k)),
            Trigger::KeyUp(k) => write!(out, "{} (up)", key(k)),
            Trigger::Scroll => write!(out, "Wheel"),
        }
        .ok();
        out
    }
}

/// A key as the W3C names it, with whitespace written as a code point so a line can still
/// be split on it.
fn parse_key(word: &str) -> Result<Key, &'static str> {
    if let Some(hex) = word.strip_prefix("U+") {
        let code = u32::from_str_radix(hex, 16).map_err(|_| "a code point is U+ and hex digits")?;
        let character = char::from_u32(code).ok_or("not a character")?;
        return Ok(Key::Character(character.to_string()));
    }
    word.parse().map_err(|_| "no such key")
}

fn write_key(key: &Key) -> String {
    match key {
        Key::Character(text) if text.chars().any(char::is_whitespace) || text.starts_with("U+") => {
            text.chars().map(|c| format!("U+{:04X}", c as u32)).collect()
        },
        _ => key.to_string(),
    }
}

fn parse_value(word: &str) -> Result<Value, &'static str> {
    match word {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        _ if word.contains(['.', 'e', 'E']) => {
            let value: f64 = word.parse().map_err(|_| "a value is true, false or a number")?;
            if value.is_finite() {
                Ok(Value::Float(value))
            } else {
                Err("a number has to be finite")
            }
        },
        _ => word
            .parse()
            .map(Value::Int)
            .map_err(|_| "a value is true, false or a number"),
    }
}

fn write_value(value: Value) -> String {
    match value {
        Value::Bool(flag) => flag.to_string(),
        Value::Int(number) => number.to_string(),
        // `{:?}` keeps the point on a whole number — `3.0`, not `3` — which is what tells
        // the reader this is a float.
        Value::Float(number) => format!("{number:?}"),
    }
}

#[cfg(test)]
mod tests {
    use masonry::core::keyboard::NamedKey;

    use super::*;

    fn sample() -> Keymap {
        Keymap::new()
            .with("canvas", vec![
                Binding::new(Pattern::drag(PointerButton::Primary), "node.move"),
                Binding::new(
                    Pattern::click(PointerButton::Primary).with_mods(Modifiers::SHIFT),
                    "node.select",
                )
                .with_props(Props::new().with_bool("extend", true).with_float("weight", 3.0)),
                Binding::new(Pattern::key("x"), "node.delete").with_props(Props::new().with_int("count", -2)),
                Binding::new(Pattern::named(NamedKey::Escape), "op.cancel"),
                Binding::new(Pattern::new(Trigger::KeyUp(Key::Character(" ".into()))), "view.release"),
                Binding::new(Pattern::scroll().with_mods(Modifiers::CONTROL), "view.zoom"),
                Binding::new(
                    Pattern::key("+").with_mods(Modifiers::CONTROL | Modifiers::ALT),
                    "view.zoom",
                ),
            ])
            .with("window", vec![Binding::new(
                Pattern::key("z").with_mods(Modifiers::CONTROL),
                "ed.undo",
            )])
    }

    /// What is written is what is read back, every trigger, modifier and value kind.
    #[test]
    fn a_keymap_reads_back_as_itself() {
        let keymap = sample().with_thresholds(Thresholds {
            drag_mouse: 5.5,
            ..Thresholds::default()
        });
        let text = keymap.write();
        assert_eq!(Keymap::parse(&text), Ok(keymap), "{text}");
    }

    /// An override goes in ahead of the defaults, and an unbind takes one out.
    #[test]
    fn overrides_go_first_and_unbind_takes_out() {
        let overrides = "blazy-keymap 1\ncontext canvas\nunbind key:x node.delete\nbind key:d node.delete\nbind drag:primary view.pan\n";
        let patched = sample().patched(overrides).expect("the overrides read");
        let ops: Vec<&str> = patched
            .section("canvas")
            .unwrap()
            .bindings
            .iter()
            .map(|b| b.op.as_ref())
            .collect();
        assert_eq!(
            &ops[..3],
            ["node.delete", "view.pan", "node.move"],
            "the file's own bindings first, in file order"
        );
        assert!(!ops[3..].contains(&"node.delete"), "the unbound one is gone");
    }

    /// A stale override is an error, and changes nothing.
    #[test]
    fn unbinding_what_is_not_there_is_an_error() {
        let text = "blazy-keymap 1\ncontext canvas\nbind key:q quit\nunbind key:y node.delete\n";
        assert_eq!(sample().patched(text), Err(KeymapError::NothingToUnbind { line: 4 }));
    }

    /// A file is input: every way a line can be wrong is a numbered error, never a panic
    /// and never a line quietly skipped.
    #[test]
    fn a_bad_line_is_named_by_its_number() {
        let header = "blazy-keymap 1\ncontext canvas\n";
        for bad in [
            "bind",
            "bind drag:primary",
            "bind drag node.move",
            "bind drag:nosuch node.move",
            "bind hyper+key:x node.delete",
            "bind key:NoSuchKey node.delete",
            "bind scroll:up view.zoom",
            "bind wiggle:primary node.move",
            "bind key:x node.delete count=many",
            "bind key:x node.delete count",
            "bind key:x node.delete dx=1e999",
            "unbind key:x node.delete extra=1",
            "thresholds drag_mouse=fast",
            "thresholds nosuch=1",
            "thresholds double_click_ms=2.5",
            "context",
            "context two words",
            "frobnicate",
            "bind key:U+zz node.delete",
        ] {
            let text = format!("{header}{bad}\n");
            assert!(
                matches!(Keymap::parse(&text), Err(KeymapError::Malformed { line: 3, .. })),
                "{bad:?} was read"
            );
        }
        assert_eq!(Keymap::parse("bind key:x a\n"), Err(KeymapError::NotAKeymap));
        assert_eq!(Keymap::parse("blazy-keymap 2\n"), Err(KeymapError::NotAKeymap));
        assert!(matches!(
            Keymap::parse("blazy-keymap 1\nbind key:x a\n"),
            Err(KeymapError::Malformed { line: 2, .. })
        ));
    }

    /// Comments and blank lines are for people.
    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let text = "blazy-keymap 1\n\n# mine\ncontext canvas\n  bind key:g node.move  \n";
        assert_eq!(Keymap::parse(text).map(|k| k.len()), Ok(1));
    }
}

#[cfg(test)]
mod label_tests {
    use masonry::core::keyboard::Modifiers;
    use masonry::ui_events::pointer::PointerButton;

    use crate::event::Pattern;

    #[test]
    fn a_pattern_reads_the_way_a_menu_shows_it() {
        assert_eq!(Pattern::key("a").with_mods(Modifiers::SHIFT).label(), "Shift+A");
        assert_eq!(
            Pattern::key("z")
                .with_mods(Modifiers::CONTROL | Modifiers::SHIFT)
                .label(),
            "Ctrl+Shift+Z"
        );
        assert_eq!(Pattern::key(" ").with_mods(Modifiers::CONTROL).label(), "Ctrl+Space");
        assert_eq!(Pattern::drag(PointerButton::Auxiliary).label(), "Drag Middle");
        assert_eq!(Pattern::scroll().label(), "Wheel");
    }
}
