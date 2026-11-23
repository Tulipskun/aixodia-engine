//! Looking at, and touching, the display the engine runs in front of.
//!
//! The app's own transport is a socket on the engine it already signs in to,
//! carrying screen tiles one way and pointer and keyboard events the other, so a
//! phone can see the machine the agent works on and put a finger on it. Beside
//! it, [`rfb`] serves the published VNC protocol on loopback, for a viewer that
//! is already installed: two doors onto one display, and the app's is the faster
//! one.
//!
//! What travels is deliberately not a video stream. The screen is cut into
//! tiling squares, each is compared with the previous frame, and only the ones
//! that changed are encoded as JPEG — see [`encoder`]. A terminal cursor moving
//! is a few hundred bytes; a browser scrolling is the whole page. That is what
//! keeps it usable on a phone link without a codec negotiation.
//!
//! Everything is adjustable at run time ([`Settings`]): the picture's quality,
//! the scale it is captured at, the tile size and the frame rate. A phone on a
//! bad link turns the quality and rate down from the client, with no restart.

pub mod capture;
pub mod cursor;
pub mod display;
pub mod encoder;
pub mod pane;
pub mod panel;
pub mod rfb;
pub mod server;
pub mod wayland;
pub mod wm;

pub use capture::{Button, Frame, Input, Screen, ScreenError};
pub use encoder::{diff_and_encode, TileUpdate};
pub use server::{router, ScreenHub, Settings};
pub use wayland::Pane;
