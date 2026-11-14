//! Looking at, and touching, the display the engine runs in front of.
//!
//! This is not VNC and does not pretend to be: there is no RFB handshake, no
//! second password, and no separate port to open. It is a socket on the engine
//! the app already signs in to, carrying screen tiles one way and pointer and
//! keyboard events the other, so a phone can see the machine the agent works on
//! and put a finger on it.
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
pub mod encoder;
pub mod server;

pub use capture::{Button, Frame, Input, Screen, ScreenError};
pub use encoder::{diff_and_encode, TileUpdate};
pub use server::{router, ScreenHub, Settings};
