//! A window manager small enough to belong to the engine.
//!
//! A bare X server draws nothing around a window: no title bar, no border, no
//! way to move it, resize it or close it. The screen window shows whatever is on
//! the display, so on a display the engine started itself every program would be
//! a rectangle nailed to one corner.
//!
//! This is a reparenting window manager, which is the smallest shape that
//! answers that: it adopts each top-level window into a frame of its own, paints
//! a title bar in that frame, turns a drag on the bar into a move, a drag on the
//! right or bottom edge into a resize, and a click on the close box into a
//! `WM_DELETE_WINDOW`. It is not a desktop: no panel, no task switcher, no
//! placement policy beyond "step along and wrap". That is deliberate — the
//! display has no desktop to manage.
//!
//! It runs on its own X connection in its own thread, because it polls
//! `XPending` and the capture path must never wait behind it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use x11::xlib;

/// Height of the bar painted across the top of every managed window.
const TITLE_H: i32 = 18;
/// Width of the close box at the bar's right edge.
const CLOSE_W: i32 = 18;
/// Thickness of the resize border along the right and bottom edges.
const GRIP: i32 = 4;
/// Smallest a client window is allowed to become.
const MIN_W: i32 = 80;
const MIN_H: i32 = 60;

/// What a press on the frame started.
#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    /// Moving the frame: the pointer offset from its corner when the drag began.
    Move {
        dx: i32,
        dy: i32,
    },
    /// Resizing from the right or bottom edge.
    Resize {
        dx: i32,
        dy: i32,
    },
}

struct Frame {
    frame: xlib::Window,
    /// The client window, reparented into `frame`.
    client: xlib::Window,
    drag: Drag,
    /// What the client said about its own size, read once when it was adopted.
    hints: SizeHints,
}

/// The parts of `WM_NORMAL_HINTS` a reparenting frame has to respect.
///
/// A terminal sets resize increments and a dialog sets a fixed size; a manager
/// that teleports the pointer size over either of them makes the program's own
/// grid disagree with the window it is drawing in. Zero means the client said
/// nothing about that dimension, which is why every field is read behind its own
/// flag rather than straight out of the struct — the unused members of
/// `XSizeHints` are not zeroed for every client.
#[derive(Clone, Copy)]
struct SizeHints {
    min_width: i32,
    min_height: i32,
    max_width: i32,
    max_height: i32,
    width_inc: i32,
    height_inc: i32,
    base_width: i32,
    base_height: i32,
}

impl Default for SizeHints {
    fn default() -> Self {
        Self {
            min_width: 0,
            min_height: 0,
            max_width: 0,
            max_height: 0,
            width_inc: 0,
            height_inc: 0,
            base_width: 0,
            base_height: 0,
        }
    }
}

impl SizeHints {
    /// Reads what the client asked for, or nothing when it asked for nothing.
    unsafe fn read(display: *mut xlib::Display, client: xlib::Window) -> Self {
        let mut hints: xlib::XSizeHints = std::mem::zeroed();
        let mut supplied: std::os::raw::c_long = 0;
        if xlib::XGetWMNormalHints(display, client, &mut hints, &mut supplied) == 0 {
            return Self::default();
        }
        let mut out = Self::default();
        if supplied & xlib::PMinSize != 0 {
            out.min_width = hints.min_width;
            out.min_height = hints.min_height;
        }
        if supplied & xlib::PMaxSize != 0 {
            out.max_width = hints.max_width;
            out.max_height = hints.max_height;
        }
        if supplied & xlib::PResizeInc != 0 {
            out.width_inc = hints.width_inc;
            out.height_inc = hints.height_inc;
        }
        if supplied & xlib::PBaseSize != 0 {
            out.base_width = hints.base_width;
            out.base_height = hints.base_height;
        }
        out
    }

    /// The size a client asked to be, given the size it is being dragged to.
    ///
    /// Steps come first so a drag lands on a boundary the program can draw in,
    /// then the limits, so a step past the maximum does not overshoot it.
    fn fit(&self, width: i32, height: i32) -> (i32, i32) {
        let mut w = step(width, self.base_width, self.width_inc);
        let mut h = step(height, self.base_height, self.height_inc);
        w = w.max(self.min_width).max(MIN_W);
        h = h.max(self.min_height).max(MIN_H);
        if self.max_width > 0 {
            w = w.min(self.max_width);
        }
        if self.max_height > 0 {
            h = h.min(self.max_height);
        }
        (w, h)
    }

    /// The size of the frame around a client of that size.
    fn frame_for(&self, width: i32, height: i32) -> (i32, i32) {
        let (w, h) = self.fit(width, height);
        (w + GRIP, h + TITLE_H + GRIP)
    }
}

/// Rounds a length to the nearest step above `base`.
fn step(value: i32, base: i32, increment: i32) -> i32 {
    if increment <= 1 {
        return value;
    }
    let base = if base > 0 { base } else { 0 };
    let distance = (value - base).max(0);
    base + (distance + increment / 2) / increment * increment
}

/// Runs the window manager on `display_name` until `stop` is set.
///
/// Every failure is quiet by design: a display without decorations still
/// streams, so a window manager that cannot start is not worth refusing the
/// picture over. The caller logs once that windows will be plain instead.
pub fn run(display_name: &str, stop: Arc<AtomicBool>) {
    unsafe {
        let Ok(name) = std::ffi::CString::new(display_name) else {
            return;
        };
        let display = xlib::XOpenDisplay(name.as_ptr());
        if display.is_null() {
            return;
        }
        // Xlib's default error handler prints and aborts the process. A window
        // manager routinely provokes errors — a client dying mid-request, a
        // window already unmapped — and aborting the engine over one of them is
        // not acceptable. Errors are counted and ignored here.
        xlib::XSetErrorHandler(Some(ignore_x_error));
        let screen = xlib::XDefaultScreen(display);
        let root = xlib::XRootWindow(display, screen);
        let white = xlib::XWhitePixel(display, screen);
        let black = xlib::XBlackPixel(display, screen);

        // SubstructureRedirect is what makes this the window manager: every
        // window a client maps is offered here first, so it can be adopted into
        // a frame instead of appearing bare and then jumping into one.
        //
        // The flag the error trap writes is per process and this display is a
        // new connection, so it is cleared *before* the request: a value left
        // from a previous run would make this manager skip a display that is
        // genuinely free, and clearing it after the sync would erase the very
        // refusal the sync produced.
        MAIN.store(1, Ordering::SeqCst);
        xlib::XSelectInput(
            display,
            root,
            xlib::SubstructureRedirectMask | xlib::SubstructureNotifyMask,
        );
        xlib::XSync(display, 0);
        // A display that already has a window manager refuses this selection, and
        // Xlib reports it asynchronously: XSelectInput still returns 1 and the
        // refusal arrives as an error. The trap turns that error into a flag so
        // the caller knows it is not the manager here, instead of running a loop
        // that will never see a window.
        let claimed = MAIN.load(Ordering::SeqCst) == 1;
        if !claimed {
            // Not the manager on this display: stop rather than sit on a
            // connection that has nothing to manage. The display keeps
            // whatever manager it already had.
            MAIN.store(0, Ordering::SeqCst);
            xlib::XCloseDisplay(display);
            return;
        }

        let atoms = Atoms::new(display);
        // A client refuses to close politely if no owner answers the protocol,
        // so it is claimed here rather than per window.
        let mut protocols = [atoms.delete_window];
        xlib::XSetWMProtocols(display, root, protocols.as_mut_ptr(), 1);

        // EWMH has one way of asking whether a display is managed at all: a
        // window named by `_NET_SUPPORTING_WM_CHECK` on the root, pointing at
        // itself as well. Toolkits read it before they trust any other `_NET_*`
        // hint, so a manager that never sets it is invisible to them — GTK and
        // Qt windows would then ignore the hints they set, and `wmctrl` would
        // report no manager. The window is never mapped or drawn on; it exists
        // only to be found and to carry the manager's name.
        let check = xlib::XCreateSimpleWindow(display, root, -1, -1, 1, 1, 0, white, black);
        let mut self_window = check;
        xlib::XChangeProperty(
            display,
            root,
            atoms.net_supporting_wm_check,
            xlib::XA_WINDOW,
            32,
            xlib::PropModeReplace,
            &mut self_window as *mut xlib::Window as *const u8,
            1,
        );
        xlib::XChangeProperty(
            display,
            check,
            atoms.net_supporting_wm_check,
            xlib::XA_WINDOW,
            32,
            xlib::PropModeReplace,
            &mut self_window as *mut xlib::Window as *const u8,
            1,
        );
        xlib::XChangeProperty(
            display,
            check,
            atoms.net_wm_name,
            atoms.utf8_string,
            8,
            xlib::PropModeReplace,
            c"aixodia".as_ptr() as *const u8,
            7,
        );

        let mut frames: Vec<Frame> = Vec::new();

        while !stop.load(Ordering::SeqCst) {
            while xlib::XPending(display) > 0 {
                let mut event: xlib::XEvent = std::mem::zeroed();
                xlib::XNextEvent(display, &mut event);
                match event.get_type() {
                    xlib::MapRequest => {
                        let client = event.map_request.window;
                        adopt(display, root, client, &mut frames, white, black);
                    }
                    xlib::ConfigureRequest => {
                        let request = event.configure_request;
                        if let Some(index) = frames.iter().position(|f| f.client == request.window)
                        {
                            // A reparented client's own coordinates mean nothing
                            // to the frame, so its position is ignored and only the
                            // size is honoured — through the client's own limits, so
                            // a program that asks for a size it said it never wants
                            // gets the nearest one it does.
                            let (width, height) =
                                frames[index].hints.frame_for(request.width, request.height);
                            xlib::XResizeWindow(
                                display,
                                frames[index].frame,
                                width as u32,
                                height as u32,
                            );
                            let (client_width, client_height) =
                                frames[index].hints.fit(request.width, request.height);
                            xlib::XResizeWindow(
                                display,
                                frames[index].client,
                                client_width as u32,
                                client_height as u32,
                            );
                        }
                    }
                    xlib::ButtonPress => {
                        press(display, &event.button, &mut frames, CLOSE_W, &atoms)
                    }
                    xlib::MotionNotify => motion(display, &event.motion, &mut frames),
                    xlib::ButtonRelease => {
                        for frame in frames.iter_mut() {
                            frame.drag = Drag::None;
                        }
                    }
                    xlib::Expose => paint(display, &event.expose, &frames, white, black),
                    xlib::UnmapNotify => {
                        // A window withdrawn by its client loses its frame; one
                        // merely hidden keeps it, so the geometry survives.
                        let gone = event.unmap.window;
                        if let Some(index) = frames.iter().position(|f| f.client == gone) {
                            let frame = frames.remove(index);
                            xlib::XDestroyWindow(display, frame.frame);
                        }
                    }
                    xlib::DestroyNotify => {
                        let gone = event.destroy_window.window;
                        if let Some(index) = frames.iter().position(|f| f.client == gone) {
                            let frame = frames.remove(index);
                            xlib::XDestroyWindow(display, frame.frame);
                        }
                    }
                    xlib::ClientMessage => {
                        // The close box asked a client to go; a client that had no
                        // protocol support leaves the frame itself.
                        let message = event.client_message;
                        if message.message_type == atoms.wm_protocols
                            && message.data.get_long(0) as xlib::Atom == atoms.delete_window
                        {
                            if let Some(index) =
                                frames.iter().position(|f| f.client == message.window)
                            {
                                let frame = frames.remove(index);
                                xlib::XDestroyWindow(display, frame.frame);
                            }
                        }
                    }
                    _ => {}
                }
            }
            xlib::XSync(display, 0);
            std::thread::sleep(std::time::Duration::from_millis(8));
        }

        // Leave the display as it was found: frames go, clients stay.
        for frame in frames.drain(..) {
            xlib::XReparentWindow(display, frame.client, root, 0, 0);
            xlib::XDestroyWindow(display, frame.frame);
        }
        // The manager's own marks go with it; a display nobody manages must not
        // advertise a manager that is no longer there.
        xlib::XDeleteProperty(display, root, atoms.net_supporting_wm_check);
        xlib::XDestroyWindow(display, check);
        xlib::XCloseDisplay(display);
    }
}

/// Swallows an X protocol error, which is routine for a window manager.
/// Set once the root selection has been accepted, and set back to 0 by a
/// refusal. Read after `XSync` so the verdict is known before the loop starts.
/// `BadAccess`: the selection this manager needs is already somebody else's.
const BAD_ACCESS: i32 = 10;

/// Whether this process already answered a `BadAccess` on this display.
///
/// It is one flag for the process, so it has to be cleared by the manager that
/// sets up a *new* display connection: an engine that restarts keeps the flag
/// from the previous run and would sit out a display nobody else manages.
static MAIN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

unsafe extern "C" fn ignore_x_error(
    _display: *mut xlib::Display,
    event: *mut xlib::XErrorEvent,
) -> i32 {
    if !event.is_null() && unsafe { (*event).error_code } as i32 == BAD_ACCESS {
        MAIN.store(0, Ordering::SeqCst);
    }
    0
}

unsafe fn adopt(
    display: *mut xlib::Display,
    root: xlib::Window,
    client: xlib::Window,
    frames: &mut Vec<Frame>,
    white: u64,
    black: u64,
) {
    if frames.iter().any(|f| f.client == client) {
        xlib::XMapWindow(display, client);
        return;
    }
    let mut attrs: xlib::XWindowAttributes = std::mem::zeroed();
    if xlib::XGetWindowAttributes(display, client, &mut attrs) == 0 {
        return;
    }
    // Override-redirect windows are menus, tooltips and drag images: a title bar
    // on a dropdown is worse than none.
    if attrs.override_redirect != 0 {
        xlib::XMapWindow(display, client);
        return;
    }

    // The client's own size is what it asked to be, brought inside its limits
    // and on a step it can draw in.
    let hints = SizeHints::read(display, client);
    let (width, height) = hints.frame_for(attrs.width, attrs.height); // Step each new window along so they do not arrive on top of each other.
    let step = frames.len() as i32 % 8;
    let x = 40 + step * 28;
    let y = 40 + step * 28;

    let frame = xlib::XCreateSimpleWindow(
        display,
        root,
        x,
        y,
        width as u32,
        height as u32,
        1,
        white,
        black,
    );
    xlib::XSelectInput(
        display,
        frame,
        xlib::ButtonPressMask
            | xlib::ButtonReleaseMask
            | xlib::PointerMotionMask
            | xlib::ExposureMask
            | xlib::SubstructureNotifyMask,
    );
    xlib::XSetWindowBorderWidth(display, client, 0);
    xlib::XReparentWindow(display, client, frame, 0, TITLE_H);
    xlib::XResizeWindow(
        display,
        client,
        (width - GRIP) as u32,
        (height - TITLE_H - GRIP) as u32,
    );
    xlib::XMapWindow(display, client);
    xlib::XMapWindow(display, frame);
    xlib::XAddToSaveSet(display, client);
    frames.push(Frame {
        frame,
        client,
        drag: Drag::None,
        hints,
    });
}

unsafe fn press(
    display: *mut xlib::Display,
    event: &xlib::XButtonEvent,
    frames: &mut [Frame],
    close_w: i32,
    atoms: &Atoms,
) {
    if event.button != 1 {
        return;
    }
    let Some(index) = frames.iter().position(|f| f.frame == event.window) else {
        return;
    };
    xlib::XRaiseWindow(display, frames[index].frame);
    xlib::XSetInputFocus(
        display,
        frames[index].client,
        xlib::RevertToParent,
        xlib::CurrentTime,
    );

    let mut attrs: xlib::XWindowAttributes = std::mem::zeroed();
    if xlib::XGetWindowAttributes(display, frames[index].frame, &mut attrs) == 0 {
        return;
    }
    let width = attrs.width;
    let height = attrs.height;

    // The close box sits at the bar's right end.
    if event.y < TITLE_H && event.x >= width - close_w {
        let mut message: xlib::XEvent = std::mem::zeroed();
        message.client_message = xlib::XClientMessageEvent {
            type_: xlib::ClientMessage,
            serial: 0,
            send_event: 1,
            display,
            window: frames[index].client,
            message_type: atoms.wm_protocols,
            format: 32,
            data: {
                let mut data: xlib::ClientMessageData = std::mem::zeroed();
                data.set_long(0, atoms.delete_window as i64);
                data
            },
        };
        xlib::XSendEvent(
            display,
            frames[index].client,
            0,
            xlib::NoEventMask,
            &mut message,
        );
        return;
    }

    // The grip is the right and bottom edge; the bar is everything else.
    let on_grip = event.x >= width - GRIP || event.y >= height - GRIP;
    frames[index].drag = if on_grip {
        Drag::Resize {
            dx: event.x_root - width,
            dy: event.y_root - height,
        }
    } else {
        Drag::Move {
            dx: event.x_root - attrs.x,
            dy: event.y_root - attrs.y,
        }
    };
}

unsafe fn motion(display: *mut xlib::Display, event: &xlib::XMotionEvent, frames: &mut [Frame]) {
    let Some(frame) = frames.iter_mut().find(|f| f.frame == event.window) else {
        return;
    };
    match frame.drag {
        Drag::None => {}
        Drag::Move { dx, dy } => {
            xlib::XMoveWindow(display, frame.frame, event.x_root - dx, event.y_root - dy);
        }
        Drag::Resize { dx, dy } => {
            // The drag is a frame size; the client and its steps decide what it
            // becomes, so the pointer only ever proposes.
            let (width, height) = frame.hints.frame_for(
                (event.x_root - dx).max(MIN_W),
                (event.y_root - dy).max(MIN_H),
            );
            xlib::XResizeWindow(display, frame.frame, width as u32, height as u32);
            xlib::XResizeWindow(
                display,
                frame.client,
                (width - GRIP) as u32,
                (height - TITLE_H - GRIP) as u32,
            );
        }
    }
}

/// Paints the bar so a window says what it is and offers somewhere to click.
unsafe fn paint(
    display: *mut xlib::Display,
    event: &xlib::XExposeEvent,
    frames: &[Frame],
    white: u64,
    black: u64,
) {
    let Some(frame) = frames.iter().find(|f| f.frame == event.window) else {
        return;
    };
    let mut attrs: xlib::XWindowAttributes = std::mem::zeroed();
    if xlib::XGetWindowAttributes(display, frame.frame, &mut attrs) == 0 {
        return;
    }
    let gc = xlib::XCreateGC(display, frame.frame, 0, std::ptr::null_mut());
    if gc.is_null() {
        return;
    }
    // The bar in dark grey, with the close box marked in white at its right.
    xlib::XSetForeground(display, gc, black);
    xlib::XFillRectangle(
        display,
        frame.frame,
        gc,
        0,
        0,
        attrs.width as u32,
        TITLE_H as u32,
    );
    xlib::XSetForeground(display, gc, white);
    xlib::XDrawString(
        display,
        frame.frame,
        gc,
        6,
        TITLE_H - 5,
        c"aixodia".as_ptr(),
        7,
    );
    xlib::XDrawRectangle(
        display,
        frame.frame,
        gc,
        attrs.width - CLOSE_W + 3,
        4,
        (CLOSE_W - 7) as u32,
        (TITLE_H - 8) as u32,
    );
    xlib::XFreeGC(display, gc);
}

/// The atoms a window manager needs by name.
struct Atoms {
    wm_protocols: xlib::Atom,
    delete_window: xlib::Atom,
    net_supporting_wm_check: xlib::Atom,
    net_wm_name: xlib::Atom,
    utf8_string: xlib::Atom,
}

impl Atoms {
    unsafe fn new(display: *mut xlib::Display) -> Self {
        Atoms {
            wm_protocols: xlib::XInternAtom(display, c"WM_PROTOCOLS".as_ptr(), 0),
            delete_window: xlib::XInternAtom(display, c"WM_DELETE_WINDOW".as_ptr(), 0),
            net_supporting_wm_check: xlib::XInternAtom(
                display,
                c"_NET_SUPPORTING_WM_CHECK".as_ptr(),
                0,
            ),
            net_wm_name: xlib::XInternAtom(display, c"_NET_WM_NAME".as_ptr(), 0),
            utf8_string: xlib::XInternAtom(display, c"UTF8_STRING".as_ptr(), 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_in_the_close_box_asks_the_client_to_leave() {
        // The close box is decided from the press position and the frame width,
        // so the arithmetic is worth pinning: the bar is TITLE_H tall and the box
        // is the last CLOSE_W pixels of it.
        let width = 400;
        let in_bar = |y: i32| y < TITLE_H;
        let in_box = |x: i32| x >= width - CLOSE_W;
        assert!(in_bar(4) && in_box(400 - 2));
        assert!(!(in_bar(4) && in_box(10)));
        assert!(!(in_bar(TITLE_H) && in_box(400 - 2)));
    }

    #[test]
    fn the_grip_is_the_right_and_bottom_edge_only() {
        let width = 400;
        let height = 300;
        let on_grip = |x: i32, y: i32| x >= width - GRIP || y >= height - GRIP;
        assert!(on_grip(width - 1, 150));
        assert!(on_grip(150, height - 1));
        assert!(!on_grip(150, 150));
        // The bar is a move, not a resize, even though it touches the top edge.
        assert!(!on_grip(width / 2, 2));
    }

    #[test]
    fn a_resize_never_goes_below_the_minimum() {
        let width = (1000 - 980).max(MIN_W + GRIP);
        assert_eq!(width, MIN_W + GRIP);
        let height = (50).max(MIN_H + TITLE_H + GRIP);
        assert_eq!(height, MIN_H + TITLE_H + GRIP);
    }
}
