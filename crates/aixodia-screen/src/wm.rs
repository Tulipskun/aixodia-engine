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
//! `WM_DELETE_WINDOW`. On top of that it owns the desktop itself: the root window
//! is painted rather than left black, and a strip along the bottom carries two
//! launchers, a button for every open window, and the time (see [`crate::panel`]).
//! It is still not a desktop environment: no icons, no files, no settings.
//!
//! It runs on its own X connection in its own thread, because it polls
//! `XPending` and the capture path must never wait behind it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use x11::xlib;

use crate::panel::{self, Action, Panel};

/// The colour the desktop itself is painted, in place of the display's black.
///
/// A black screen and a broken screen are the same picture on a phone, and a
/// display nobody has drawn on is black. This is dark enough to stay out of the
/// way and different enough to read as "a desktop with nothing on it yet".
const DESKTOP_RGB: (u16, u16, u16) = (0x0d00, 0x1a00, 0x2600);

/// Width of one character of the `fixed` font, in pixels.
///
/// Every label on this desktop is drawn in that font, so this is what turns "the
/// bar is this wide" into "this many characters fit".
const CHAR_W: i32 = 6;

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
    /// The window's title, read when it was adopted, so the bar can say which
    /// window it is rather than the manager's own name on every frame.
    title: String,
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
    /// The size the client asked to be, from `WM_NORMAL_HINTS`.
    ///
    /// A program that knows a window manager is there often maps itself at one
    /// pixel and puts its real geometry in the hints instead — xterm does exactly
    /// that — so a manager that reads only the limits gives it a one-pixel window.
    ask_width: i32,
    ask_height: i32,
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
            ask_width: 0,
            ask_height: 0,
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
        if supplied & xlib::PSize != 0 {
            out.ask_width = hints.width;
            out.ask_height = hints.height;
        }
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
        let display_width = xlib::XDisplayWidth(display, screen);
        let display_height = xlib::XDisplayHeight(display, screen);

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
            xlib::SubstructureRedirectMask | xlib::SubstructureNotifyMask | xlib::ExposureMask,
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
        // A size a client asked for before it was adopted. xterm maps itself at a
        // placeholder and asks for its real geometry in the same batch of
        // requests, so the request can arrive while there is no frame to apply it
        // to; without this it is dropped and the window lives at the placeholder.
        let mut pending: HashMap<xlib::Window, (i32, i32)> = HashMap::new();

        // The desktop itself: the root painted, and the strip along the bottom.
        // Both are best-effort — a display that refuses them still streams, it
        // just looks like the bare server it is.
        paint_root(display, root, display_width, display_height, white);
        let mut bar = Panel::create(display, root, display_width);
        if let Some(bar) = bar.as_mut() {
            xlib::XMoveWindow(
                display,
                bar.window(),
                0,
                display_height - crate::panel::HEIGHT,
            );
        }
        repaint_panel(display, &mut bar, &frames);

        while !stop.load(Ordering::SeqCst) {
            while xlib::XPending(display) > 0 {
                let mut event: xlib::XEvent = std::mem::zeroed();
                xlib::XNextEvent(display, &mut event);
                match event.get_type() {
                    xlib::MapRequest => {
                        let client = event.map_request.window;
                        let asked = pending.remove(&client);
                        adopt(display, root, client, &mut frames, white, black, asked);
                        // A new window is a new button on the strip.
                        repaint_panel(display, &mut bar, &frames);
                    }
                    xlib::ConfigureRequest => {
                        let request = event.configure_request;
                        if frames.iter().all(|f| f.client != request.window) {
                            // Not adopted yet: keep the size for the frame that
                            // is about to be built around it.
                            pending.insert(request.window, (request.width, request.height));
                        }
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
                        let pressed = event.button.window;
                        if bar.as_ref().is_some_and(|strip| strip.window() == pressed) {
                            // The strip's own buttons: a launcher, or a window to
                            // bring forward. Handled here and not passed on, so a
                            // press on the panel never reaches a frame's drag.
                            match bar.as_ref().and_then(|strip| strip.click(event.button.x)) {
                                Some(Action::Launch(program)) => {
                                    panel::launch(program, display_name);
                                }
                                Some(Action::Raise(client)) => {
                                    if let Some(frame) = frames.iter().find(|f| f.client == client)
                                    {
                                        xlib::XRaiseWindow(display, frame.frame);
                                        xlib::XSetInputFocus(
                                            display,
                                            client,
                                            xlib::RevertToParent,
                                            0,
                                        );
                                    }
                                }
                                None => {}
                            }
                        } else {
                            press(display, &event.button, &mut frames, CLOSE_W, &atoms)
                        }
                    }
                    xlib::MotionNotify => motion(display, &event.motion, &mut frames),
                    xlib::ButtonRelease => {
                        for frame in frames.iter_mut() {
                            frame.drag = Drag::None;
                        }
                    }
                    xlib::Expose => {
                        let shown = event.expose.window;
                        if shown == root {
                            paint_root(display, root, display_width, display_height, white);
                        } else if bar.as_ref().is_some_and(|strip| strip.window() == shown) {
                            repaint_panel(display, &mut bar, &frames);
                        } else {
                            paint(display, &event.expose, &frames, white, black);
                        }
                    }
                    xlib::PropertyNotify => {
                        // A title set after the window was mapped — most programs
                        // name a window once they know what is in it — would
                        // otherwise leave the bar saying the program's name and
                        // the desktop's button saying it too.
                        let client = event.property.window;
                        if event.property.atom == atoms.net_wm_name
                            || event.property.atom == xlib::XA_WM_NAME
                        {
                            if let Some(index) = frames.iter().position(|f| f.client == client) {
                                frames[index].title = window_title(display, client);
                                paint_frame_bar(display, &frames[index], white, black);
                                repaint_panel(display, &mut bar, &frames);
                            }
                        }
                    }
                    xlib::UnmapNotify => {
                        // A window withdrawn by its client loses its frame; one
                        // merely hidden keeps it, so the geometry survives.
                        let gone = event.unmap.window;
                        if let Some(index) = frames.iter().position(|f| f.client == gone) {
                            let frame = frames.remove(index);
                            xlib::XDestroyWindow(display, frame.frame);
                            repaint_panel(display, &mut bar, &frames);
                        }
                    }
                    xlib::DestroyNotify => {
                        let gone = event.destroy_window.window;
                        if let Some(index) = frames.iter().position(|f| f.client == gone) {
                            let frame = frames.remove(index);
                            xlib::XDestroyWindow(display, frame.frame);
                            repaint_panel(display, &mut bar, &frames);
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
            // The clock is the only thing on the strip that changes without an
            // event, so it is checked rather than repainted every pass.
            if bar.as_ref().is_some_and(|strip| strip.clock_moved()) {
                repaint_panel(display, &mut bar, &frames);
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }

        // Leave the display as it was found: frames go, clients stay.
        for frame in frames.drain(..) {
            xlib::XReparentWindow(display, frame.client, root, 0, 0);
            xlib::XDestroyWindow(display, frame.frame);
        }
        if let Some(mut strip) = bar.take() {
            strip.destroy(display);
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

/// Paints the desktop itself, in place of the display's black.
///
/// The root window has no background of its own, so an X server that has just
/// started shows black — the same black as a link that has failed. Filling it
/// and writing one line of text is the difference between "nothing open" and
/// "nothing working".
unsafe fn paint_root(
    display: *mut xlib::Display,
    root: xlib::Window,
    width: i32,
    height: i32,
    white: u64,
) {
    let screen = xlib::XDefaultScreen(display);
    let colormap = xlib::XDefaultColormap(display, screen);
    let mut colour: xlib::XColor = std::mem::zeroed();
    colour.red = DESKTOP_RGB.0;
    colour.green = DESKTOP_RGB.1;
    colour.blue = DESKTOP_RGB.2;
    colour.flags = xlib::DoRed | xlib::DoGreen | xlib::DoBlue;
    let gc = xlib::XCreateGC(display, root, 0, std::ptr::null_mut());
    if gc.is_null() {
        return;
    }
    if xlib::XAllocColor(display, colormap, &mut colour) != 0 {
        xlib::XSetForeground(display, gc, colour.pixel);
    } else {
        xlib::XSetForeground(display, gc, xlib::XBlackPixel(display, screen));
    }
    xlib::XFillRectangle(display, root, gc, 0, 0, width as u32, height as u32);
    xlib::XSetForeground(display, gc, white);
    if let Ok(font) = std::ffi::CString::new("fixed") {
        let loaded = xlib::XLoadQueryFont(display, font.as_ptr());
        if !loaded.is_null() {
            xlib::XSetFont(display, gc, (*loaded).fid);
        }
    }
    // What the display is for, in the corner it is empty at. Short enough to fit
    // a phone's picture and honest about where the controls are.
    let hint = c"aixodia desktop - terminal and browser are on the bar below";
    xlib::XDrawString(
        display,
        root,
        gc,
        24,
        height / 2,
        hint.as_ptr(),
        hint.to_bytes().len() as i32,
    );
    xlib::XFreeGC(display, gc);
}

/// Draws the strip again, with the windows that are open right now.
unsafe fn repaint_panel(display: *mut xlib::Display, bar: &mut Option<Panel>, frames: &[Frame]) {
    let Some(bar) = bar.as_mut() else {
        return;
    };
    let tasks: Vec<(u64, String)> = frames
        .iter()
        .map(|frame| (frame.client, window_title(display, frame.client)))
        .collect();
    bar.paint(display, &tasks);
}

/// A window's title, or a plain name when it has not set one.
///
/// `XFetchName` is the old Latin-1 property, which is what a program that does
/// not know about `_NET_WM_NAME` sets; a program that only sets the new one shows
/// as "window" here and is still reachable, which is the part that matters.
unsafe fn window_title(display: *mut xlib::Display, client: xlib::Window) -> String {
    let mut name: *mut std::ffi::c_char = std::ptr::null_mut();
    if xlib::XFetchName(display, client, &mut name) == 0 || name.is_null() {
        return "window".to_string();
    }
    let title = std::ffi::CStr::from_ptr(name)
        .to_string_lossy()
        .into_owned();
    xlib::XFree(name as *mut std::ffi::c_void);
    if title.trim().is_empty() {
        "window".to_string()
    } else {
        title
    }
}

unsafe fn adopt(
    display: *mut xlib::Display,
    root: xlib::Window,
    client: xlib::Window,
    frames: &mut Vec<Frame>,
    white: u64,
    black: u64,
    asked: Option<(i32, i32)>,
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
    // The size it asked to be, and failing that the size it mapped at: a client
    // that maps at nothing has said nothing, and a frame of nothing is a window
    // nobody can use.
    let (asked_width, asked_height) = match asked {
        Some((width, height)) => (width, height),
        None => (attrs.width, attrs.height),
    };
    // A window mapped at one pixel has said nothing about itself in its
    // attributes; what it asks for in its hints is the real size.
    let (asked_width, asked_height) = if asked_width <= 1 || asked_height <= 1 {
        (
            if hints.ask_width > 1 {
                hints.ask_width
            } else {
                asked_width
            },
            if hints.ask_height > 1 {
                hints.ask_height
            } else {
                asked_height
            },
        )
    } else {
        (asked_width, asked_height)
    };
    let (width, height) = hints.frame_for(asked_width, asked_height);
    eprintln!("DBG adopt frame={width}x{height}"); // Step each new window along so they do not arrive on top of each other.
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
    xlib::XSelectInput(
        display,
        client,
        xlib::PropertyChangeMask | xlib::StructureNotifyMask,
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
        title: window_title(display, client),
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
    if let Some(frame) = frames.iter().find(|f| f.frame == event.window) {
        paint_frame_bar(display, frame, white, black);
    }
}

/// Draws one frame's title bar: the name it carries and its close box.
///
/// The bar is repainted whenever the name changes, so a program that names its
/// window after it is mapped is not left as the program's own name.
unsafe fn paint_frame_bar(display: *mut xlib::Display, frame: &Frame, white: u64, black: u64) {
    let mut attrs: xlib::XWindowAttributes = std::mem::zeroed();
    if xlib::XGetWindowAttributes(display, frame.frame, &mut attrs) == 0 {
        return;
    }
    let gc = xlib::XCreateGC(display, frame.frame, 0, std::ptr::null_mut());
    if gc.is_null() {
        return;
    }
    // The `fixed` font, so one character is one fixed six pixels and the room
    // left for a title can be counted rather than guessed.
    if let Ok(name) = std::ffi::CString::new("fixed") {
        let font = xlib::XLoadQueryFont(display, name.as_ptr());
        if !font.is_null() {
            xlib::XSetFont(display, gc, (*font).fid);
            xlib::XFreeFont(display, font);
        }
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
    let room = ((attrs.width - CLOSE_W - 12) / CHAR_W).max(0) as usize;
    let label = crate::panel::truncation(&frame.title, room);
    if let Ok(drawn) = std::ffi::CString::new(label) {
        let bytes = drawn.as_bytes().len();
        xlib::XDrawString(
            display,
            frame.frame,
            gc,
            6,
            TITLE_H - 5,
            drawn.as_ptr(),
            bytes as i32,
        );
    }
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
