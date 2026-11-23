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

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

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
        y: i32,
    ) -> Option<Panel> {
        let screen = xlib::XDefaultScreen(display);
        let white = xlib::XWhitePixel(display, screen);
        let black = xlib::XBlackPixel(display, screen);
        let window = xlib::XCreateSimpleWindow(
            display,
            root,
            0,
            y,
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
            let label = shorten(title, 16);
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

/// A label no longer than the button or bar that carries it.
///
/// Counted in characters, not bytes: a title in Thai would otherwise be cut in
/// the middle of a character and drawn as a replacement mark.
pub fn shorten(title: &str, limit: usize) -> String {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return "window".to_string();
    }
    match trimmed.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
        None => trimmed.to_string(),
    }
}

/// Whether the process that wrote a Chrome profile lock is gone.
///
/// Chrome's lock is a symlink whose target is `<hostname>-<pid>`. A lock left by
/// a machine that no longer exists, or by a pid that is not running, is stale:
/// nothing is holding the profile and Chrome would still refuse to start.
fn lock_is_stale(target: &str, host: &str, alive: impl Fn(u32) -> bool) -> bool {
    let Some((holder, pid)) = target.rsplit_once('-') else {
        // A lock that is not a host and a pid is not one this can judge.
        return false;
    };
    if holder != host {
        return true;
    }
    match pid.parse::<u32>() {
        Ok(pid) => !alive(pid),
        Err(_) => false,
    }
}

/// Removes the profile locks a dead Chrome left behind.
///
/// The three links Chrome writes are removed together: a leftover socket or
/// cookie with no lock makes Chrome fail in a different way.
fn clear_stale_lock() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let profile = Path::new(&home).join(".config/google-chrome");
    let lock = profile.join("SingletonLock");
    let Ok(target) = std::fs::read_link(&lock) else {
        return;
    };
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_string())
        .unwrap_or_default();
    let stale = lock_is_stale(&target.to_string_lossy(), &host, |pid| {
        Path::new(&format!("/proc/{pid}")).exists()
    });
    if !stale {
        return;
    }
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(profile.join(name));
    }
    tracing::info!("browser: cleared a lock left by a Chrome that is gone ({target:?})");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_from_a_machine_that_is_gone_is_stale() {
        // Measured after a container was recreated: the old hostname, a pid that
        // cannot exist here, and Chrome refusing to start.
        assert!(lock_is_stale("7d167a919da1-361937", "0f5fe2a9b582", |_| {
            true
        }));
    }

    #[test]
    fn a_lock_from_a_dead_pid_on_this_machine_is_stale() {
        assert!(lock_is_stale("box-4242", "box", |pid| pid != 4242));
    }

    #[test]
    fn a_live_chrome_keeps_its_lock() {
        // Taking the lock from a running Chrome would let two of them corrupt the
        // profile, which is worse than not starting.
        assert!(!lock_is_stale("box-4242", "box", |pid| pid == 4242));
    }

    #[test]
    fn a_lock_that_is_not_a_host_and_a_pid_is_left_alone() {
        assert!(!lock_is_stale("nonsense", "box", |_| false));
    }

    #[test]
    fn a_short_title_is_kept_as_it_is() {
        assert_eq!(shorten("xterm", 16), "xterm");
        assert_eq!(shorten("  1234567890123456  ", 16), "1234567890123456");
    }

    #[test]
    fn a_long_title_is_cut_with_a_mark() {
        assert_eq!(shorten("12345678901234567", 16), "1234567890123456…");
    }

    #[test]
    fn a_title_in_a_wide_alphabet_is_cut_between_characters() {
        // Three characters, a limit of two: one character is dropped and the
        // result is still valid text, never half of one.
        assert_eq!(shorten("กขค", 2), "กข…");
    }

    #[test]
    fn a_window_without_a_title_says_so() {
        assert_eq!(shorten("", 16), "window");
        assert_eq!(shorten("   ", 16), "window");
    }

    #[test]
    fn the_clock_says_which_hour_it_is() {
        // The stamp ends in its zone, so a phone showing an hour that disagrees
        // with its own cannot be mistaken for a bug in the desktop.
        let stamp = clock().1;
        assert_eq!(stamp.len(), 6);
        assert!(stamp.ends_with('z'));
        assert_eq!(&stamp[2..3], ":");
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
        // A fontconfig family rather than a core font: `-fn fixed` needs an X
        // font path that a bare Xvfb does not have, and asking for it makes
        // xterm exit without a word — measured here, where `-fn fixed` produced
        // no window and no error at all. `-fa` goes through fontconfig, which
        // resolves the alias to whatever monospace the machine has.
        _ => (
            "xterm",
            vec![
                "-fa",
                "monospace",
                "-fs",
                "12",
                "-geometry",
                "100x28+24+24",
                "-bg",
                "#101820",
                "-fg",
                "#e8eef5",
            ],
        ),
    };
    if binary == "chromium" {
        // Chrome refuses to start while its profile says it is in use, and the
        // lock that says so outlives the Chrome that wrote it: measured after a
        // container restart, every launch from the strip died with "The profile
        // appears to be in use by another Google Chrome process (361937) on
        // another computer (7d167a919da1)" and the browser could not be opened
        // at all until the lock was removed by hand.
        clear_stale_lock();
    }
    let mut command = Command::new(binary);
    command.args(args).env("DISPLAY", display_name);
    if let Some(bus) = session_bus() {
        command.env("DBUS_SESSION_BUS_ADDRESS", bus);
    }
    let started = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match started {
        // A child that is never waited for stays a zombie until the engine
        // exits, one per launch. The wait is on its own thread so a launcher
        // never holds up the manager.
        Ok(mut child) => {
            std::thread::spawn(move || {
                if let Ok(status) = child.wait() {
                    if !status.success() {
                        tracing::warn!("{binary} exited with {status}");
                    }
                }
            });
        }
        Err(err) => {
            tracing::warn!("could not start {program} ({binary}) on {display_name}: {err}");
        }
    }
}

/// A session bus for the programs this pane starts, once per engine.
///
/// chromium asks the session bus about the desktop portal, the secret service and
/// power management many times a second and, with no bus at all, it retries every
/// one of them: measured in the browser's own log, hundreds of
/// "Failed to connect to the bus" lines in a few seconds of idle. A private bus
/// costs one process and turns that noise into one answer — "nothing is there".
fn session_bus() -> Option<String> {
    static BUS: OnceLock<Option<String>> = OnceLock::new();
    BUS.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("aixodia-bus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let socket = dir.join("bus");
        let address = format!("unix:path={}", socket.display());
        // Not `--fork`: a daemon that detaches leaves this engine unable to say
        // whether it is alive, and `--print-address` through a pipe from a
        // forked child was measured as an empty string with no socket behind it.
        Command::new("dbus-daemon")
            .args(["--session", "--address"])
            .arg(&address)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // The socket is the proof: the address is worth nothing until it exists.
        for _ in 0..50 {
            if socket.exists() {
                return Some(address);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        tracing::warn!("the pane's session bus did not come up; programs will retry one");
        None
    })
    .clone()
}

/// Starts one of the strip's programs inside our own compositor.
///
/// A Wayland client is told two things: where the session's sockets live and
/// which one of them is the display. `DISPLAY` is removed rather than left alone,
/// because a chromium that can see an X server takes it and draws a window
/// nothing composits.
///
/// A terminal is the one program this cannot start: `xterm` is an X client, and
/// the compositor mode exists for hosts where X is not installed at all. It says
/// so in the log instead of failing silently, which is the same rule the launcher
/// follows when a shared library is missing.
pub fn launch_in_compositor(program: &str, runtime_dir: &std::path::Path, socket: &str) {
    let (binary, args): (&str, Vec<&str>) = match program {
        "browser" => (
            "chromium",
            vec![
                "--ozone-platform=wayland",
                // There is no GPU here and the browser's own log says its GPU
                // channel fails to open; asking it not to try is honest, and it
                // keeps the software path the only path.
                "--disable-gpu",
                "--no-sandbox",
                // The first run's "Welcome" dialog and its "restore pages?"
                // bubble are windows on a desktop nobody is sitting at.
                "--no-first-run",
                "--hide-crash-restore-bubble",
                "--window-size=1200,640",
                // Without a page to open, chromium starts with no window at all:
                // the pane then shows the desktop and nothing else, which looks
                // exactly like a compositor that does not work.
                "about:blank",
            ],
        ),
        other => {
            tracing::warn!("{other}: a terminal needs an X server, and this pane has none");
            return;
        }
    };
    if binary == "chromium" {
        clear_stale_lock();
    }
    // What a client says when it dies belongs in a file. Measured: chromium was
    // killed by its own SIGSEGV while typing into an address bar, and a log of
    // nothing at all left nothing to read — the same silence as an xterm that
    // exits with no window and no error.
    let (out, err) = match std::fs::File::create(runtime_dir.join(format!("{program}.log"))) {
        Ok(log) => match log.try_clone() {
            Ok(clone) => (Stdio::from(clone), Stdio::from(log)),
            Err(_) => (Stdio::from(log), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    };
    let mut command = Command::new(binary);
    command
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("WAYLAND_DISPLAY", socket)
        .env_remove("DISPLAY");
    if let Some(bus) = session_bus() {
        command.env("DBUS_SESSION_BUS_ADDRESS", bus);
    }
    let started = command.stdin(Stdio::null()).stdout(out).stderr(err).spawn();
    match started {
        Ok(mut child) => {
            std::thread::spawn(move || {
                if let Ok(status) = child.wait() {
                    if !status.success() {
                        tracing::warn!("{binary} exited with {status}");
                    }
                }
            });
        }
        Err(err) => tracing::warn!("could not start {program} ({binary}) in the compositor: {err}"),
    }
}
