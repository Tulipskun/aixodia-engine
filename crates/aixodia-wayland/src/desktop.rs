//! The desktop itself: the colour behind every window, and the one thing drawn
//! on it that is not a client — a tile that opens an app.
//!
//! A pane with no window in it is a navy rectangle, and measured on the real
//! machine that is indistinguishable from a compositor that has stopped working.
//! The browser is started with the pane, but a browser that is closed, or killed
//! by its own crash, leaves exactly that rectangle behind; so the desktop carries
//! a launcher of its own, drawn in pixels we own, and clicking it starts the
//! browser again.
//!
//! There is no font here and no text: a glyph is drawn from the same arithmetic
//! as the tile around it, which keeps this file testable and keeps the desktop
//! independent of any toolkit.

/// The colour behind every window.
pub const BACKGROUND: u32 = 0xff10_1a2b;

/// The launcher tile: a square at the bottom left, clear of a window's corner.
pub const TILE: i32 = 56;
pub const MARGIN: i32 = 16;
pub const RADIUS: i32 = 12;

/// The tile's face and the glyph on it.
const FACE: u32 = 0xff2a_3654;
const EDGE: u32 = 0xff3d_4d72;
const GLYPH: u32 = 0xffc9_d3e8;

/// Where the tile's top left corner sits, for a desktop of this size.
pub fn tile_at(_width: u32, height: u32) -> (i32, i32) {
    (MARGIN, height as i32 - MARGIN - TILE)
}

/// Whether a point on the desktop is on the launcher.
pub fn contains(x: f64, y: f64, width: u32, height: u32) -> bool {
    let (left, top) = tile_at(width, height);
    x >= f64::from(left)
        && x < f64::from(left + TILE)
        && y >= f64::from(top)
        && y < f64::from(top + TILE)
}

/// Draws the desktop: the background, and the launcher on it.
pub fn draw(frame: &mut [u32], width: u32, height: u32) {
    frame.fill(BACKGROUND);
    let (left, top) = tile_at(width, height);
    rounded_rect(frame, width, left, top, TILE, TILE, RADIUS, EDGE);
    rounded_rect(
        frame,
        width,
        left + 1,
        top + 1,
        TILE - 2,
        TILE - 2,
        RADIUS - 1,
        FACE,
    );
    glyph(frame, width, left, top);
}

/// An app window: a frame with a bar across its top, three dots on the bar.
fn glyph(frame: &mut [u32], width: u32, left: i32, top: i32) {
    let x = left + 14;
    let y = top + 16;
    let w = TILE - 28;
    let h = TILE - 32;
    let dot = 2;
    // The window's frame, two pixels of it.
    for t in 0..2 {
        rect(frame, width, x + t, y + t, w - 2 * t, h - 2 * t, GLYPH);
    }
    // The bar's lower edge, so the frame reads as a window and not a rectangle.
    rect(frame, width, x + 2, y + 10, w - 4, 2, GLYPH);
    // Three dots on the bar.
    for i in 0..3 {
        rect(frame, width, x + 4 + i * 6, y + 4, dot, dot, GLYPH);
    }
}

fn rect(frame: &mut [u32], width: u32, x: i32, y: i32, w: i32, h: i32, colour: u32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let (width, height) = (width as i32, frame.len() as i32 / width.max(1) as i32);
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(width);
    let y1 = (y + h).min(height);
    for row in y0..y1 {
        let start = (row * width + x0) as usize;
        let end = (row * width + x1) as usize;
        if let Some(slice) = frame.get_mut(start..end) {
            slice.fill(colour);
        }
    }
}

/// A filled rectangle with its corners taken off, one row at a time.
fn rounded_rect(
    frame: &mut [u32],
    width: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    radius: i32,
    colour: u32,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let radius = radius.min(w / 2).min(h / 2).max(0);
    for row in 0..h {
        // How far in this row starts, from the corner's circle.
        let inset = if row < radius {
            radius - circle_inset(radius, row)
        } else if row >= h - radius {
            radius - circle_inset(radius, h - 1 - row)
        } else {
            0
        };
        rect(frame, width, x + inset, y + row, w - 2 * inset, 1, colour);
    }
}

/// Half-width of a circle of `radius` at `row` from its top edge.
fn circle_inset(radius: i32, row: i32) -> i32 {
    let dy = f64::from(radius - row - 1);
    let r = f64::from(radius);
    let dx = (r * r - dy * dy).max(0.0).sqrt();
    // Rounded up, so a corner never loses a pixel it should keep.
    (r - dx).ceil() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_is_a_corner_tile_and_not_the_whole_screen() {
        let (width, height) = (1280, 720);
        let (left, top) = tile_at(width, height);
        assert_eq!((left, top), (MARGIN, 720 - MARGIN - TILE));
        assert!(contains(40.0, 690.0, width, height), "inside the tile");
        assert!(!contains(40.0, 400.0, width, height), "above the tile");
        assert!(!contains(400.0, 690.0, width, height), "right of the tile");
        assert!(
            !contains(8.0, 690.0, width, height),
            "the margin is not the tile"
        );
    }

    #[test]
    fn the_desktop_is_painted_even_when_nothing_is_open() {
        let (width, height) = (1280, 720);
        let mut frame = vec![0u32; (width * height) as usize];
        draw(&mut frame, width, height);
        assert!(frame
            .iter()
            .all(|pixel| *pixel == BACKGROUND || *pixel != 0));
        let (left, top) = tile_at(width, height);
        // Inside the tile and clear of the glyph in its middle.
        let face = ((top + TILE / 2) * width as i32 + left + 5) as usize;
        assert_eq!(frame[face], FACE, "the tile's face is where it should be");
        assert_eq!(frame[0], BACKGROUND, "the top left corner is the desktop");
    }

    #[test]
    fn a_tile_drawn_on_a_small_desktop_stays_on_it() {
        // A launcher that is off the screen is a launcher nobody can click.
        let (width, height) = (320, 200);
        let mut frame = vec![0u32; (width * height) as usize];
        draw(&mut frame, width, height);
        let (left, top) = tile_at(width, height);
        assert!(left >= 0 && top >= 0);
        assert!(left + TILE <= width as i32 && top + TILE <= height as i32);
    }

    #[test]
    fn a_corner_is_rounded_rather_than_square() {
        let radius = 12;
        let top = circle_inset(radius, 0);
        assert!(
            top > 1 && top < radius,
            "the top row is inset, not cut off: {top}"
        );
        assert!(
            top > circle_inset(radius, radius / 2),
            "and the middle of the corner is less inset"
        );
        assert_eq!(
            circle_inset(radius, radius - 1),
            0,
            "the row where the corner meets the side is not inset at all"
        );
    }
}
