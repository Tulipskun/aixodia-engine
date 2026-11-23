//! The pane: one compositor, and where a finger's events go.
//!
//! Two doors lead to the same screen — the app's socket and the RFB server a VNC
//! viewer dials — and both must show the same picture and move the same pointer.
//! So both ask for the pane through here and get back the one compositor the
//! process has: two viewers of two compositors would be two machines wearing one
//! name.
//!
//! The compositor is ours. An X display somebody else owns was the other option
//! and is gone: it needed Xvfb, xterm, six shared libraries linked into the
//! engine, and a host that could install all of it before the pane would open.

use std::sync::Arc;

use crate::cursor;
use crate::frame::{Button, Frame, ScreenError};
use crate::wayland::{self, Pane};

/// What the pane is showing, and the events that move it.
pub struct Panes {
    pub source: Arc<Pane>,
    pub events: Events,
}

/// Events for the display `source` came from.
///
/// A wrapper rather than the `Pane` itself, because the picture and the input are
/// asked for by code that wants only one of them, and because the input handle
/// must be able to outlive a closed picture.
pub struct Events {
    pane: Arc<Pane>,
}

impl Events {
    /// Events for a pane somebody else opened, so the picture and the input can
    /// be held by different parts of the engine.
    pub fn new(pane: Arc<Pane>) -> Self {
        Self { pane }
    }

    pub fn display_size(&self) -> (u32, u32) {
        self.pane.display_size()
    }

    /// Where the machine's pointer is, in display pixels.
    pub fn pointer_position(&self) -> Option<(i32, i32)> {
        self.pane.pointer_position()
    }

    /// The shape the machine is drawing for the pointer, or nothing when the
    /// client under it has not set one.
    pub fn cursor_shape(&self) -> Option<cursor::Now> {
        self.pane.cursor_shape()
    }

    pub fn move_pointer(&self, x: i32, y: i32) {
        self.pane.move_pointer(x, y);
    }

    pub fn button(&self, button: Button, pressed: bool) {
        self.pane.button(button, pressed);
    }

    pub fn key(&self, name: &str, mods: &[String]) {
        self.pane.key(name, mods);
    }

    pub fn keysym(&self, keysym: u32, pressed: bool) {
        self.pane.keysym(keysym, pressed);
    }

    pub fn type_text(&mut self, text: &str) {
        self.pane.type_text(text);
    }
}

/// Opens the pane, or hands back the one already open.
///
/// A size of 0×0 means the default: a caller that only wants to touch the screen
/// it already has (the RFB server, before it has read a viewer's preferences)
/// must not be able to resize it.
pub fn open(width: u32, height: u32) -> Result<Panes, String> {
    let (width, height) = if width == 0 || height == 0 {
        (DEFAULT_WIDTH, DEFAULT_HEIGHT)
    } else {
        (width, height)
    };
    let pane = wayland::shared(width, height).map_err(|e| e.to_string())?;
    Ok(Panes {
        source: Arc::clone(&pane),
        events: Events { pane },
    })
}

/// Opens the events handle on the pane already open, without opening a picture.
pub fn open_events(width: u32, height: u32) -> Result<Events, String> {
    let (width, height) = if width == 0 || height == 0 {
        (DEFAULT_WIDTH, DEFAULT_HEIGHT)
    } else {
        (width, height)
    };
    let pane = wayland::shared(width, height).map_err(|e| e.to_string())?;
    Ok(Events { pane })
}

/// The size a pane gets when its caller did not ask for one.
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;

/// A frame, for callers that hold the pane and not the wrapper.
pub fn frame(pane: &Arc<Pane>) -> Result<Frame, ScreenError> {
    pane.frame()
}
