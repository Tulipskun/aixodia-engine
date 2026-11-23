//! The screen pane, or nothing at all.
//!
//! The pane needs X: the capture reads the framebuffer of a display the engine
//! starts, and the window manager speaks X11. Those libraries are linked into
//! the crate, so a machine without them cannot load the engine at all —
//! measured on a host that had been recreated without the X libraries:
//! `libX11.so.6: cannot open shared object file`, and the whole engine died,
//! terminal and chat with it, over a feature that host could not have used
//! anyway.
//!
//! So the pane is a cargo feature. Without it the binary mentions no X and runs
//! anywhere; with it, the same source builds the engine with one door more.
#[cfg(feature = "screen")]
pub use aixodia_screen::{display, rfb, router, ScreenHub};

#[cfg(not(feature = "screen"))]
mod absent {
    use axum::Router;

    /// A hub that holds nothing, so the rest of the engine needs no `cfg`.
    #[derive(Clone, Default)]
    pub struct ScreenHub;

    impl ScreenHub {
        pub fn new(_display: String, _token: String) -> Self {
            Self
        }
    }

    /// No pane, so no route to one.
    pub fn router(_hub: ScreenHub) -> Router {
        Router::new()
    }

    pub mod display {
        /// There is no display to stop.
        pub fn shutdown() {}
    }
}

#[cfg(not(feature = "screen"))]
pub use absent::{display, router, ScreenHub};
