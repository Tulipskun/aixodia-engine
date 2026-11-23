//! Which display the pane is showing, and where a finger's events go.
//!
//! Two doors lead to the same screen — the app's socket and the RFB server a
//! VNC viewer dials — and both must show the same picture and move the same
//! pointer. So both ask for the pane through here, and in compositor mode what
//! they get back is one compositor shared by the whole process: two viewers of
//! two compositors would be two machines wearing one name.
//!
//! X mode keeps what it had: a display somebody else owns ([`crate::display`]
//! brings one up if the machine has none), captured through `XShm`, and input
//! sent through `XTest` on a connection of its own so an event never waits for a
//! grab.

use std::sync::Arc;

use crate::capture::{Button, Frame, Input, Screen, ScreenError};
use crate::cursor;
use crate::wayland;

/// Whether this engine shows an X display or its own compositor.
///
/// The compositor is the default now: the pane is a framebuffer this engine owns,
/// and the machine needs no X server, no Xvfb and none of the pane's packages to
/// draw it. `AIXODIA_PANE=x` asks for the old display instead — kept as a door
/// that still works, not as the path the product takes.
///
/// It stays opt-in because chromium still dies with a SIGSEGV the first time a
/// key is pressed while a text field has the keyboard, and the crash is inside
/// chromium rather than on the wire: libwayland validates every message this
/// compositor sends (it has rejected two of them with its own words — see
/// `text_input_commit`), nothing protocol-illegal is ever reported, and the last
/// request chromium makes before the fault is a surface of its own. What works on
/// this path, measured through a VNC viewer: the desktop, the browser's window,
/// clicking, moving the pointer, scrolling, typing into a page, and every key
/// that does not go into a text field.
pub fn compositor_mode() -> bool {
    match std::env::var("AIXODIA_PANE") {
        Ok(value) => !(value.eq_ignore_ascii_case("x") || value.eq_ignore_ascii_case("x11")),
        Err(_) => true,
    }
}

/// What the pane is showing: a display somebody else owns, or one of ours.
pub enum ScreenSource {
    X(Box<Screen>),
    Wayland(Arc<wayland::Pane>),
}

impl ScreenSource {
    pub fn frame(&mut self) -> Result<Frame, ScreenError> {
        match self {
            Self::X(screen) => screen.frame(),
            Self::Wayland(pane) => pane.frame(),
        }
    }

    pub fn width(&self) -> u32 {
        match self {
            Self::X(screen) => screen.width(),
            Self::Wayland(pane) => pane.width(),
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::X(screen) => screen.height(),
            Self::Wayland(pane) => pane.height(),
        }
    }

    pub fn using_shm(&self) -> bool {
        match self {
            Self::X(screen) => screen.using_shm(),
            Self::Wayland(pane) => pane.using_shm(),
        }
    }
}

/// Where a finger's events go.
pub enum Events {
    X(Box<Input>),
    Wayland(Arc<wayland::Pane>),
}

impl Events {
    pub fn display_size(&self) -> (u32, u32) {
        match self {
            Self::X(input) => input.display_size(),
            Self::Wayland(pane) => pane.display_size(),
        }
    }

    pub fn pointer_position(&self) -> Option<(i32, i32)> {
        match self {
            Self::X(input) => input.pointer_position(),
            Self::Wayland(pane) => pane.pointer_position(),
        }
    }

    /// No cursor of the machine's own: a compositor has no pointer art to read,
    /// and the phone already draws the arrow the user is holding.
    pub fn cursor_shape(&self) -> Option<cursor::Now> {
        match self {
            Self::X(input) => input.cursor_shape(),
            Self::Wayland(_) => None,
        }
    }

    pub fn move_pointer(&self, x: i32, y: i32) {
        match self {
            Self::X(input) => input.move_pointer(x, y),
            Self::Wayland(pane) => pane.move_pointer(x, y),
        }
    }

    pub fn button(&self, button: Button, pressed: bool) {
        match self {
            Self::X(input) => input.button(button, pressed),
            Self::Wayland(pane) => pane.button(button, pressed),
        }
    }

    pub fn key(&self, keysym: &str, mods: &[String]) {
        match self {
            Self::X(input) => input.key(keysym, mods),
            Self::Wayland(pane) => pane.key(keysym, mods),
        }
    }

    /// A key as a viewer that speaks RFC 6143 sends it: an X keysym *number*,
    /// which is what a VNC client has been sending since 1998. The compositor
    /// needs a keycode, so the number is read back as the name it stands for.
    pub fn keysym(&self, keysym: u32, pressed: bool) {
        match self {
            Self::X(input) => input.keysym(keysym, pressed),
            Self::Wayland(pane) => pane.keysym(keysym, pressed),
        }
    }

    pub fn type_text(&mut self, text: &str) -> Result<(), ScreenError> {
        match self {
            Self::X(input) => input.type_text(text),
            Self::Wayland(pane) => {
                pane.type_text(text);
                Ok(())
            }
        }
    }
}

/// Both halves of an open pane.
pub struct Panes {
    pub source: ScreenSource,
    pub events: Events,
}

/// Opens the pane at `width` x `height`, or at the display's own size when both
/// are zero — which is what the RFB door wants, because its handshake has to
/// carry the size the desktop really has.
pub fn open(width: u32, height: u32) -> Result<Panes, String> {
    if compositor_mode() {
        let (width, height) = if width == 0 || height == 0 {
            (1280, 720)
        } else {
            (width, height)
        };
        let pane = open_wayland(width, height)?;
        return Ok(Panes {
            source: ScreenSource::Wayland(Arc::clone(&pane)),
            events: Events::Wayland(pane),
        });
    }
    let display = crate::display::ensure().map_err(|e| e.0)?;
    let input = Input::open(&display).map_err(|e| e.to_string())?;
    let (width, height) = input.display_size();
    if width == 0 || height == 0 {
        return Err("the display has no size".into());
    }
    Ok(Panes {
        source: ScreenSource::X(Box::new(
            Screen::open(&display, width, height).map_err(|e| e.to_string())?,
        )),
        events: Events::X(Box::new(input)),
    })
}

/// The shared compositor, for a caller that keeps the two halves apart.
pub fn open_wayland(width: u32, height: u32) -> Result<Arc<wayland::Pane>, String> {
    wayland::shared(width, height).map_err(|e| e.to_string())
}

/// A source only, for a caller that already holds the input handle — the app's
/// socket opens the two separately so an event never waits for a grab.
pub fn open_source(width: u32, height: u32) -> Result<ScreenSource, String> {
    if compositor_mode() {
        let (width, height) = if width == 0 || height == 0 {
            (1280, 720)
        } else {
            (width, height)
        };
        return Ok(ScreenSource::Wayland(open_wayland(width, height)?));
    }
    let display = crate::display::ensure().map_err(|e| e.0)?;
    let (real_width, real_height) = if width == 0 || height == 0 {
        let input = Input::open(&display).map_err(|e| e.to_string())?;
        input.display_size()
    } else {
        (width, height)
    };
    Ok(ScreenSource::X(Box::new(
        Screen::open(&display, real_width, real_height).map_err(|e| e.to_string())?,
    )))
}

/// An input handle only, for the same reason.
pub fn open_events(width: u32, height: u32) -> Result<Events, String> {
    if compositor_mode() {
        let (width, height) = if width == 0 || height == 0 {
            (1280, 720)
        } else {
            (width, height)
        };
        return Ok(Events::Wayland(open_wayland(width, height)?));
    }
    let display = crate::display::ensure().map_err(|e| e.0)?;
    Input::open(&display)
        .map(|input| Events::X(Box::new(input)))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_is_a_word_and_not_a_shape_of_the_environment() {
        // Nothing here reads a display: the setting is a decision, and a typo in
        // it must leave the working path alone. The compositor is the working
        // path now, so an unset or misspelt value means the compositor and only
        // the word "x" asks for a display somebody else owns.
        std::env::remove_var("AIXODIA_PANE");
        assert!(compositor_mode());
        std::env::set_var("AIXODIA_PANE", "wayland");
        assert!(compositor_mode());
        std::env::set_var("AIXODIA_PANE", "Wayland");
        assert!(compositor_mode());
        std::env::set_var("AIXODIA_PANE", "waland");
        assert!(compositor_mode());
        std::env::set_var("AIXODIA_PANE", "x11");
        assert!(!compositor_mode());
        std::env::set_var("AIXODIA_PANE", "X");
        assert!(!compositor_mode());
        std::env::remove_var("AIXODIA_PANE");
    }
}
