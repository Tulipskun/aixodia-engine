//! The bar along the bottom of the engine's display, and the buttons on it.
//!
//! A display with a window manager but no desktop is still an empty room: a
//! black root window, no way to start anything, and no way to find a window that
//! went behind another. That is what the screen showed, and "black" is not
//! distinguishable from "broken" on a phone.
//!
//! So the manager owns one window of its own: a strip with two launchers, a
//! button for every open window, and the time. It is an override-redirect
//! window, which means the manager's own frame code never sees it — a panel that
//! adopted itself into a frame with a title bar would be a comedy of errors — and
//! it lives as long as the manager does.

use std::process::{Command, Stdio};

use x11::xlib;

/// Height of the strip, in pixels.
pub const HEIGHT: i32 = 26;

/// What a press on the strip asked for.
#[derive(Clone, Copy)]
pub enum Action {
    /// Start one of the two programs a phone can actually use.
    Launch(&'static str),
    /// Bring a window to the front and give it the keyboard.
    Raise(u64),
}

/// One button on the painted strip, in strip coordinates.
struct Hit {
    x: i32,
    width: i32,
    action: Action,
}

/// The strip itself, with what it drew where.
pub struct Panel {
    window: xlib::Window,
    gc: xlib::GC,
    font: Option<*mut xlib::XFontStruct>,
    width: i32,
    hits: Vec<Hit>,
    /// The minute the clock was last drawn at, so the strip is repainted once a
    /// minute rather than on every pass of the manager's loop.
    minute: u64,
}

impl Panel {
    /// Creates the strip across the bottom of `root`.
    ///
    /// Returning `None` is survivable: a display that refuses one still streams,
    /// it just has no launcher. Nothing here is worth failing a screen over.
    pub unsafe fn create(
        display: *mut xlib::Display,
        root: xlib::Window,
        width: i32,
    ) -> Option<Panel> {
        let screen = xlib::XDefaultScreen(display);
        let white = xlib::XWhitePixel(display, screen);
        let black = xlib::XBlackPixel(display, screen);
        let window = xlib::XCreateSimpleWindow(
            display,
            root,
            0,
            0,
            width as u32,
            HEIGHT as u32,
            0,
            white,
            black,
        );
        if window == 0 {
            return None;
        }
        // Override-redirect before it is mapped: the manager has substructure
        // redirect on the root, so an ordinary window would arrive as a
        // MapRequest and be adopted into a frame with a title bar.
        let mut attrs: xlib::XSetWindowAttributes = std::mem::zeroed();
        attrs.override_redirect = 1;
        attrs.background_pixel = black;
        xlib::XChangeWindowAttributes(
            display,
            window,
            xlib::CWOverrideRedirect | xlib::CWBackPixel,
            &mut attrs,
        );
        xlib::XSelectInput(display, window, xlib::ExposureMask | xlib::ButtonPressMask);
        // The strip sits under everything else on the display, so a window
        // dragged over it is not hidden behind it.
        xlib::XMapWindow(display, window);
        xlib::XLowerWindow(display, window);
        let gc = xlib::XCreateGC(display, window, 0, std::ptr::null_mut());
        let font = xlib::XLoadQueryFont(display, c"fixed".as_ptr());
        let font = if font.is_null() { None } else { Some(font) };
        if let Some(font) = font {
            xlib::XSetFont(display, gc, (*font).fid);
        }
        Some(Panel {
            window,
            gc,
            font,
            width,
            hits: Vec::new(),
            minute: u64::MAX,
        })
    }

    /// The strip's own window, so the manager can tell its events from a frame's.
    pub fn window(&self) -> xlib::Window {
        self.window
    }

    /// Draws the strip: launchers, one button per window, and the time.
    ///
    /// `tasks` are the windows currently open, in the order they were adopted,
    /// as `(client, title)`.
    pub unsafe fn paint(&mut self, display: *mut xlib::Display, tasks: &[(u64, String)]) {
        let screen = xlib::XDefaultScreen(display);
        let white = xlib::XWhitePixel(display, screen);
        let black = xlib::XBlackPixel(display, screen);
        self.hits.clear();

        xlib::XSetForeground(display, self.gc, black);
        xlib::XFillRectangle(
            display,
            self.window,
            self.gc,
            0,
            0,
            self.width as u32,
            HEIGHT as u32,
        );
        // A hairline above the strip, so it reads as a panel rather than as a
        // window someone left at the bottom of the screen.
        xlib::XSetForeground(display, self.gc, white);
        xlib::XDrawLine(display, self.window, self.gc, 0, 0, self.width, 0);

        let mut x = 8;
        for (label, program) in [("terminal", "terminal"), ("browser", "browser")] {
            let width = self.text_width(label) + 16;
            self.button(display, x, width, label);
            self.hits.push(Hit {
                x,
                width,
                action: Action::Launch(program),
            });
            x += width + 6;
        }
        // The window list, as buttons named after their titles. A title too long
        // for its button is cut rather than allowed to run into the next one.
        for (client, title) in tasks {
            if x > self.width - 90 {
                break;
            }
            let label = truncation(title, 16);
            let width = (self.text_width(&label) + 20).min(180);
            self.button(display, x, width, &label);
            self.hits.push(Hit {
                x,
                width,
                action: Action::Raise(*client),
            });
            x += width + 6;
        }

        let now = clock();
        self.minute = now.0;
        let stamp = now.1;
        let width = self.text_width(&stamp);
        self.draw_text(display, self.width - width - 10, 18, &stamp);
        xlib::XSync(display, 0);
    }

    /// Which action a press at `x` on the strip asks for.
    pub fn click(&self, x: i32) -> Option<Action> {
        self.hits
            .iter()
            .find(|hit| x >= hit.x && x < hit.x + hit.width)
            .map(|hit| hit.action)
    }

    /// Whether the clock has moved on since the strip was drawn.
    pub fn clock_moved(&self) -> bool {
        clock().0 != self.minute
    }

    pub unsafe fn destroy(&mut self, display: *mut xlib::Display) {
        if let Some(font) = self.font.take() {
            xlib::XFreeFont(display, font);
        }
        xlib::XFreeGC(display, self.gc);
        xlib::XDestroyWindow(display, self.window);
    }

    unsafe fn button(&self, display: *mut xlib::Display, x: i32, width: i32, label: &str) {
        let screen = xlib::XDefaultScreen(display);
        let white = xlib::XWhitePixel(display, screen);
        xlib::XSetForeground(display, self.gc, white);
        xlib::XDrawRectangle(
            display,
            self.window,
            self.gc,
            x,
            6,
            width as u32,
            (HEIGHT - 12) as u32,
        );
        self.draw_text(display, x + 8, 18, label);
    }

    unsafe fn draw_text(&self, display: *mut xlib::Display, x: i32, y: i32, text: &str) {
        let screen = xlib::XDefaultScreen(display);
        let white = xlib::XWhitePixel(display, screen);
        xlib::XSetForeground(display, self.gc, white);
        let Ok(drawn) = std::ffi::CString::new(text) else {
            return;
        };
        let bytes = drawn.as_bytes().len();
        xlib::XDrawString(
            display,
            self.window,
            self.gc,
            x,
            y,
            drawn.as_ptr(),
            bytes as i32,
        );
    }

    /// How wide a label is, with the strip's own font.
    fn text_width(&self, text: &str) -> i32 {
        match self.font {
            Some(font) => {
                let Ok(drawn) = std::ffi::CString::new(text) else {
                    return 0;
                };
                // The font's widest character is a safe estimate when the glyph
                // table is unavailable; the exact width is only for packing.
                let width = unsafe {
                    xlib::XTextWidth(font, drawn.as_ptr(), drawn.as_bytes().len() as i32)
                };
                if width > 0 {
                    width
                } else {
                    6 * text.len() as i32
                }
            }
            None => 6 * text.len() as i32,
        }
    }
}

/// A label no longer than the button that carries it.
pub fn truncation(title: &str, limit: usize) -> String {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return "window".to_string();
    }
    match trimmed.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
        None => trimmed.to_string(),
    }
}

/// The minute since the epoch, and the time to write on the strip.
///
/// UTC, and labelled as such: the engine has no locale to ask and a clock that
/// silently shows the wrong hour is worse than one that says which hour it is.
fn clock() -> (u64, String) {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let minute = seconds / 60;
    let hour = (seconds / 3600) % 24;
    let rest = (seconds / 60) % 60;
    (minute, format!("{hour:02}:{rest:02}z"))
}

/// Starts one of the programs the strip offers, on the display it belongs to.
///
/// Both are looked for on `PATH` and started detached: the manager must not wait
/// on a program, and a program that fails to start must not take the panel down
/// with it. `chromium` runs without its sandbox because the engine usually runs as
/// root in a container, where the sandbox refuses to start at all.
pub fn launch(program: &str, display_name: &str) {
    let (binary, args): (&str, Vec<&str>) = match program {
        "browser" => (
            "chromium",
            vec![
                "--no-sandbox",
                "--window-size=1200,640",
                "--window-position=40,40",
            ],
        ),
        // A core font and an explicit geometry: `-fa` needs fontconfig, and
        // without a usable font xterm opens at a handful of pixels, which is a
        // window nobody can use on a phone.
        _ => (
            "xterm",
            vec![
                "-fn",
                "fixed",
                "-geometry",
                "100x28+24+24",
                "-bg",
                "#101820",
                "-fg",
                "#e8eef5",
            ],
        ),
    };
    let started = Command::new(binary)
        .args(args)
        .env("DISPLAY", display_name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(err) = started {
        tracing::warn!("could not start {program} ({binary}) on {display_name}: {err}");
    }
}
