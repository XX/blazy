//! Text from the platform: the clipboard and the input method.
//!
//! Masonry's text widgets ask the host for the clipboard (by signal) and the input method
//! (from the focus pass). This is the host's part, as free functions over the root, so an
//! embedder or a test does the same.
//!
//! There is no paste signal: the host turns `Ctrl+V` (`Cmd+V` on macOS) into
//! `TextEvent::ClipboardPaste`, but only while a text field has focus
//! ([`text_input_focused`]), so that a keymap can bind the key elsewhere. In a field it is
//! converted even with an empty clipboard, or the field would type a `v`.

use masonry::app::{RenderRoot, RenderRootSignal};
use masonry::core::keyboard::{Code, Key, KeyState};
use masonry::core::{Ime, TextEvent};
use masonry::dpi::{LogicalPosition, LogicalSize};

use crate::window::ShellCtx;

/// Where copied text goes and pasted text comes from: the platform's ([`SystemClipboard`]),
/// or a [`MemoryClipboard`].
pub trait Clipboard {
    /// The text on the clipboard, or `None` when there is none — nothing copied yet, or
    /// something that is not text.
    fn read(&mut self) -> Option<String>;

    /// Puts `text` on the clipboard.
    fn write(&mut self, text: String);
}

/// A clipboard that lives in the process: what a test uses, and what the shell falls back
/// to when the platform's cannot be opened.
#[derive(Clone, Debug, Default)]
pub struct MemoryClipboard {
    text: Option<String>,
}

impl MemoryClipboard {
    /// A clipboard holding `text`, as if it had been copied.
    pub fn holding(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
        }
    }
}

impl Clipboard for MemoryClipboard {
    fn read(&mut self) -> Option<String> {
        self.text.clone()
    }

    fn write(&mut self, text: String) {
        self.text = Some(text);
    }
}

/// The platform's clipboard, text only, through `arboard` (on Wayland via XWayland).
pub struct SystemClipboard {
    inner: arboard::Clipboard,
}

impl SystemClipboard {
    /// Opens the platform's clipboard.
    ///
    /// # Errors
    ///
    /// When there is none to open — no display server, typically; the error is the
    /// platform's, as text.
    pub fn open() -> Result<Self, String> {
        arboard::Clipboard::new()
            .map(|inner| Self { inner })
            .map_err(|error| error.to_string())
    }
}

impl Clipboard for SystemClipboard {
    fn read(&mut self) -> Option<String> {
        match self.inner.get_text() {
            Ok(text) => Some(text),
            // Nothing copied, or not text.
            Err(arboard::Error::ContentNotAvailable) => None,
            Err(error) => {
                tracing::warn!(%error, "could not read the clipboard");
                None
            },
        }
    }

    fn write(&mut self, text: String) {
        if let Err(error) = self.inner.set_text(text) {
            tracing::warn!(%error, "could not write the clipboard");
        }
    }
}

/// What a window has to do for the input method; a trait so tests need no window.
pub trait ImeWindow {
    /// Lets the input method compose into this window, or stops it.
    fn set_ime_allowed(&self, allowed: bool);

    /// Tells the input method where the text being composed is, for its candidate window.
    fn set_ime_cursor_area(&self, position: LogicalPosition<f64>, size: LogicalSize<f64>);
}

impl ImeWindow for winit::window::Window {
    fn set_ime_allowed(&self, allowed: bool) {
        winit::window::Window::set_ime_allowed(self, allowed);
    }

    fn set_ime_cursor_area(&self, position: LogicalPosition<f64>, size: LogicalSize<f64>) {
        winit::window::Window::set_ime_cursor_area(self, position, size);
    }
}

/// What the host did with text, counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TextCounters {
    /// `Ctrl+V` turned into text for a focused text field.
    pub pastes: u64,
    /// `Ctrl+V` in a text field with nothing on the clipboard: swallowed rather than
    /// delivered as a key.
    pub empty_pastes: u64,
    /// Text a widget put on the clipboard.
    pub stores: u64,
    /// Times the input method was allowed, and stopped.
    pub ime_starts: u64,
    /// See `ime_starts`.
    pub ime_ends: u64,
    /// Times the composing area was moved.
    pub ime_moves: u64,
}

/// Whether the focused widget takes text input (a text field, not a node editor holding
/// the keys). Costs no rewrite passes.
pub fn text_input_focused(root: &RenderRoot) -> bool {
    root.focused_widget()
        .and_then(|id| root.get_widget(id))
        .is_some_and(|widget| widget.ctx().accepts_text_input())
}

/// Whether `event` is the paste key, pressed or released: `Ctrl+V`, or `Cmd+V` on macOS.
///
/// With `Shift` too, as upstream's runner and every text field take it.
pub fn is_paste_key(event: &TextEvent) -> bool {
    let TextEvent::Keyboard(key) = event else {
        return false;
    };
    let action = if cfg!(target_os = "macos") {
        key.modifiers.meta()
    } else {
        key.modifiers.ctrl()
    };
    // By its place when it types no Latin letter (`м` on a Russian layout), and by its
    // letter otherwise — the rule the keymap names keys by (`OpEvent::from_keyboard`).
    action
        && match &key.key {
            Key::Character(c) if c.chars().all(|c| c.is_ascii_graphic()) => c.eq_ignore_ascii_case("v"),
            _ => key.code == Code::KeyV,
        }
}

/// What the tree should get for `event`: the event, the clipboard's text in place of the
/// paste key in a text field, or nothing (the release, or an empty clipboard).
pub(crate) fn paste_or_key(cx: &mut ShellCtx, root: &RenderRoot, event: TextEvent) -> Option<TextEvent> {
    if !is_paste_key(&event) || !text_input_focused(root) {
        return Some(event);
    }
    let TextEvent::Keyboard(key) = &event else {
        return Some(event);
    };
    if key.state != KeyState::Down {
        return None;
    }
    let read = cx.clipboard().read();
    match read {
        Some(text) => {
            cx.text_counters_mut().pastes += 1;
            Some(TextEvent::ClipboardPaste(text))
        },
        None => {
            cx.text_counters_mut().empty_pastes += 1;
            None
        },
    }
}

/// Does what a text signal asks of the platform (the input method, copied text), or hands
/// the signal back if it is not one.
pub fn apply_text_signal(
    cx: &mut ShellCtx,
    window: &dyn ImeWindow,
    signal: RenderRootSignal,
) -> Option<RenderRootSignal> {
    match signal {
        RenderRootSignal::StartIme => {
            cx.text_counters_mut().ime_starts += 1;
            window.set_ime_allowed(true);
        },
        RenderRootSignal::EndIme => {
            cx.text_counters_mut().ime_ends += 1;
            window.set_ime_allowed(false);
        },
        RenderRootSignal::ImeMoved(position, size) => {
            cx.text_counters_mut().ime_moves += 1;
            window.set_ime_cursor_area(position, size);
        },
        RenderRootSignal::ClipboardStore(text) => {
            cx.text_counters_mut().stores += 1;
            cx.clipboard().write(text);
        },
        other => return Some(other),
    }
    None
}

/// winit's input method event as Masonry's: the same four cases, two crates.
pub fn ime_event(event: winit::event::Ime) -> TextEvent {
    TextEvent::Ime(match event {
        winit::event::Ime::Enabled => Ime::Enabled,
        winit::event::Ime::Preedit(text, cursor) => Ime::Preedit(text, cursor),
        winit::event::Ime::Commit(text) => Ime::Commit(text),
        winit::event::Ime::Disabled => Ime::Disabled,
    })
}
