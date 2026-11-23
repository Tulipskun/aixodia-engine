//! The screen pane: the compositor, the frame encoder, and the two doors onto it.
//!
//! There used to be a second path, an X display somebody else owned, behind a
//! cargo feature because it linked six X libraries and a machine without them
//! could not load the engine at all. The pane is our own compositor now: it asks
//! the host for nothing, so there is nothing left to switch off.

pub use aixodia_screen::{rfb, router, ScreenHub};
