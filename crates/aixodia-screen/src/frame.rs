//! What every display has in common: a frame of pixels and a pointer button.
//!
//! The pane used to be an X display, so these lived beside the X capture code.
//! They are not X's: the compositor hands back the same `Frame` in the same
//! order, and both doors — the app's socket and the RFB server — speak these
//! names to whichever display is behind them.

/// What went wrong reading the display.
#[derive(Debug)]
pub enum ScreenError {
    /// The display could not be opened at all.
    Open(String),
    /// The display was there and is not any more.
    Gone,
}

impl std::fmt::Display for ScreenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(why) => write!(f, "cannot open display: {why}"),
            Self::Gone => write!(f, "display is gone"),
        }
    }
}

impl std::error::Error for ScreenError {}

/// One frame: BGRX pixels, `width * height` of them, top row first.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Four bytes per pixel in the order a little-endian machine hands them
    /// over — blue, green, red, pad — so no conversion happens on the hot path;
    /// the encoder reads it directly.
    pub pixels: Vec<u8>,
}

/// A pointer button, in the numbering X and RFB share: 1 is left, 3 is right, 4
/// and 5 are the vertical wheel, 6 and 7 the horizontal one. A wheel notch is a
/// press and a release of one of those, which is how both protocols report
/// scrolling — including sideways, which a two-finger drag needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}
