//! Reading the pixels of an X display, and writing mouse and keyboard events
//! back into it.
//!
//! The whole point of this module is to be fast enough that a phone over a
//! phone network feels like it is looking at the machine directly. That rules
//! out spawning `scrot` for every frame: a process launch costs more than the
//! frame itself. Instead the display is opened once, the pixels come back
//! through XShm (shared memory, so no copy through the X socket), and the
//! pointer moves through XTest (the extension that feeds the same queue a real
//! mouse would).
//!
//! Every call here is blocking. The engine drives it from a blocking task and
//! never from the async runtime.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use x11::xlib;
use x11::xlib::SelectionNotify as SEL_EVENT;

/// The X event code for a selection reply, which the binding does not export by name.
const SEL_NOTIFY: i32 = SEL_EVENT as i32;
use x11::xshm;
use x11::xtest;

/// What went wrong opening or reading the display.
#[derive(Debug)]
pub enum ScreenError {
    /// The display could not be opened at all — a wrong DISPLAY, or a missing
    /// or unreadable X authority file.
    Open(String),
    /// The display opened but the shared-memory path was refused.
    Shm(String),
    /// A frame was requested on a display that is no longer there.
    Gone,
}

impl std::fmt::Display for ScreenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(why) => write!(f, "cannot open display: {why}"),
            Self::Shm(why) => write!(f, "shared-memory capture refused: {why}"),
            Self::Gone => write!(f, "display is gone"),
        }
    }
}

impl std::error::Error for ScreenError {}

/// One captured frame: BGRX pixels, `width * height` of them, top row first.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Four bytes per pixel in the order the X server hands them over on a
    /// little-endian machine — blue, green, red, pad. Kept in that order so no
    /// conversion happens on the hot path; the encoder reads it directly.
    pub pixels: Vec<u8>,
}

/// A pointer button, in X's numbering: 1 is left, 3 is right, 4 and 5 are the
/// wheel. A wheel notch is a press and a release of one of those, which is how
/// the X server has always reported scrolling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

impl Button {
    fn code(self) -> u32 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
            Self::WheelUp => 4,
            Self::WheelDown => 5,
        }
    }
}

/// An open X display, with one shared-memory image attached to it.
///
/// A frame is read by asking the server to copy the root window into that
/// image, which happens inside the server and never crosses the socket.
pub struct Screen {
    display: *mut xlib::Display,
    window: xlib::Window,
    width: u32,
    height: u32,
    depth: u32,
    shm: *mut xshm::XShmSegmentInfo,
    image: *mut xlib::XImage,
    /// True when the shared-memory attach worked. When it did not, frames are
    /// read the slow way rather than not at all — the feature still works, it
    /// is only slower, and a display that refuses XShm is usually one behind a
    /// remote transport anyway.
    using_shm: bool,
    /// The thread that holds the clipboard, when this display has typed text.
    clipboard: Option<ClipboardServer>,
}

// The display pointer is only ever touched from the thread that opened it; the
// engine wraps this in a mutex and calls it from one blocking task.
unsafe impl Send for Screen {}

/// The X server reports a failure to the process rather than to the call that
/// caused it, and the default handler exits. That is fine for a program whose
/// only job is that display and fatal for an engine that happens to be looking
/// at one: a refused `XShmAttach` — which is what a container sees, because the
/// server and the process are in different IPC namespaces — took the whole
/// engine down and every terminal with it.
///
/// So the handler is replaced with one that records the failure and returns.
/// The caller checks [`last_error`] after the call it cares about and falls back
/// to a path that works.
static X_FAILED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn note_error(_display: *mut xlib::Display, event: *mut xlib::XErrorEvent) -> i32 {
    X_FAILED.store(true, Ordering::SeqCst);
    let code = (*event).error_code;
    let minor = (*event).minor_code;
    eprintln!("screen: X request failed (error {code}, minor {minor}); continuing without it");
    0
}

/// True when an X request failed since the last call, and clears the mark.
fn take_error() -> bool {
    X_FAILED.swap(false, Ordering::SeqCst)
}

impl Screen {
    /// Opens `display_name` (empty means `$DISPLAY`) and prepares to read it.
    pub fn open(display_name: &str, width: u32, height: u32) -> Result<Self, ScreenError> {
        // Before the first request: without this a refused request ends the
        // process instead of the capture.
        unsafe {
            xlib::XSetErrorHandler(Some(note_error));
        }
        take_error();
        let name = if display_name.is_empty() {
            ptr::null()
        } else {
            let owned = std::ffi::CString::new(display_name)
                .map_err(|e| ScreenError::Open(e.to_string()))?;
            // XOpenDisplay keeps the string only for the duration of the call.
            let display = unsafe { xlib::XOpenDisplay(owned.as_ptr()) };
            if display.is_null() {
                return Err(ScreenError::Open(format!("{display_name} did not accept the connection")));
            }
            return Self::from_display(display, width, height);
        };
        let display = unsafe { xlib::XOpenDisplay(name) };
        if display.is_null() {
            return Err(ScreenError::Open(
                "no display; DISPLAY is unset or the X server refused the cookie".to_string(),
            ));
        }
        Self::from_display(display, width, height)
    }

    fn from_display(display: *mut xlib::Display, width: u32, height: u32) -> Result<Self, ScreenError> {
        let screen = unsafe { xlib::XDefaultScreen(display) };
        let root = unsafe { xlib::XRootWindow(display, screen) };
        let depth = unsafe { xlib::XDefaultDepth(display, screen) } as u32;

        // A root window smaller than the size asked for is not a failure: the
        // frame is simply smaller, and the client scales it up.
        let (screen_width, screen_height) = Self::root_size(display, root);
        let width = if width == 0 { screen_width } else { width.min(screen_width) };
        let height = if height == 0 { screen_height } else { height.min(screen_height) };

        let (shm, image, using_shm) = Self::attach_shm(display, depth, width, height)
            .or_else(|_| Self::attach_plain(display, depth, width, height).map(|i| (ptr::null_mut(), i, false)))?;

        Ok(Self {
            display,
            window: root,
            width,
            height,
            depth,
            shm,
            image,
            using_shm,
            clipboard: None,
        })
    }

    fn root_size(display: *mut xlib::Display, root: xlib::Window) -> (u32, u32) {
        let mut attributes: xlib::XWindowAttributes = unsafe { std::mem::zeroed() };
        let ok = unsafe { xlib::XGetWindowAttributes(display, root, &mut attributes) };
        if ok == 0 {
            return (1920, 1080);
        }
        (attributes.width as u32, attributes.height as u32)
    }

    /// Attaches a shared-memory image the server can draw into directly.
    fn attach_shm(
        display: *mut xlib::Display,
        depth: u32,
        width: u32,
        height: u32,
    ) -> Result<(*mut xshm::XShmSegmentInfo, *mut xlib::XImage, bool), ScreenError> {
        let visual = unsafe { xlib::XDefaultVisual(display, xlib::XDefaultScreen(display)) };
        let size = (width as usize) * (height as usize) * 4;
        let shm = unsafe { libc_shmat(size) }?;

        let image = unsafe {
            xshm::XShmCreateImage(
                display,
                visual,
                depth,
                xlib::ZPixmap,
                shm.data as *mut _,
                shm.info,
                width,
                height,
            )
        };
        if image.is_null() {
            unsafe { libc_shmdt(shm.data) };
            return Err(ScreenError::Shm("XShmCreateImage returned nothing".to_string()));
        }

        unsafe {
            (*shm.info).shmid = shm.id;
            (*shm.info).shmaddr = shm.data as *mut _;
            (*shm.info).readOnly = 0;
        }
        let attached = unsafe { xshm::XShmAttach(display, shm.info) };
        // XShmAttach only queues the request; the refusal arrives with the next
        // round trip. Syncing makes the error land here, where it can be seen,
        // rather than during the first frame.
        unsafe { xlib::XSync(display, 0) };
        if attached == 0 || take_error() {
            unsafe {
                let _ = xlib::XDestroyImage(image);
                libc_shmdt(shm.data);
                libc_free(shm.info as *mut c_void);
            }
            return Err(ScreenError::Shm("XShmAttach was refused".to_string()));
        }

        Ok((shm.info, image, true))
    }

    /// The fallback: a plain image in process memory, filled by the server over
    /// the socket. Slower, but it works where shared memory does not.
    fn attach_plain(
        display: *mut xlib::Display,
        depth: u32,
        width: u32,
        height: u32,
    ) -> Result<*mut xlib::XImage, ScreenError> {
        let visual = unsafe { xlib::XDefaultVisual(display, xlib::XDefaultScreen(display)) };
        let image = unsafe { xlib::XCreateImage(display, visual, depth, xlib::ZPixmap, 0, ptr::null_mut(), width, height, 32, 0) };
        if image.is_null() {
            return Err(ScreenError::Shm("XCreateImage returned nothing".to_string()));
        }
        let bytes = (height as usize) * unsafe { (*image).bytes_per_line } as usize;
        let buffer = unsafe { libc_malloc(bytes) };
        if buffer.is_null() {
            unsafe { xlib::XDestroyImage(image); }
            return Err(ScreenError::Shm("no memory for a frame buffer".to_string()));
        }
        unsafe { (*image).data = buffer as *mut _ };
        Ok(image)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The size of the display itself, before any scaling for a client.
    ///
    /// The pointer is reported in these pixels, so anything that has to line a
    /// pointer up with a scaled frame needs both sizes.
    pub fn display_size(&self) -> (u32, u32) {
        if self.display.is_null() {
            return (self.width, self.height);
        }
        unsafe {
            let screen = xlib::XDefaultScreen(self.display);
            (
                xlib::XDisplayWidth(self.display, screen) as u32,
                xlib::XDisplayHeight(self.display, screen) as u32,
            )
        }
    }

    pub fn using_shm(&self) -> bool {
        self.using_shm
    }

    /// Reads the whole window into the attached image and returns a copy of the
    /// pixels.
    pub fn frame(&mut self) -> Result<Frame, ScreenError> {
        if self.display.is_null() {
            return Err(ScreenError::Gone);
        }
        let ok = if self.using_shm {
            unsafe { xshm::XShmGetImage(self.display, self.window, self.image, 0, 0, xlib::XAllPlanes() as u32) }
        } else {
            unsafe {
                let image = xlib::XGetImage(
                    self.display,
                    self.window,
                    0,
                    0,
                    self.width,
                    self.height,
                    xlib::XAllPlanes(),
                    xlib::ZPixmap,
                );
                if image.is_null() {
                    return Err(ScreenError::Gone);
                }
                // The slow path hands over a fresh image each time; copy its
                // pixels out and let it go.
                let bytes = (self.height as usize) * (*image).bytes_per_line as usize;
                let data = std::slice::from_raw_parts((*image).data as *const u8, bytes);
                let target = std::slice::from_raw_parts_mut((*self.image).data as *mut u8, bytes);
                target.copy_from_slice(data);
                let _ = xlib::XDestroyImage(image);
                1
            }
        };
        if ok == 0 {
            return Err(ScreenError::Gone);
        }

        let bytes = (self.height as usize) * unsafe { (*self.image).bytes_per_line } as usize;
        let data = unsafe { std::slice::from_raw_parts((*self.image).data as *const u8, bytes) };
        let mut pixels = Vec::with_capacity((self.width as usize) * (self.height as usize) * 4);
        for row in 0..self.height as usize {
            let start = row * unsafe { (*self.image).bytes_per_line } as usize;
            pixels.extend_from_slice(&data[start..start + (self.width as usize) * 4]);
        }
        let _ = self.depth;

        Ok(Frame {
            width: self.width,
            height: self.height,
            pixels,
        })
    }

    /// Moves the pointer to an absolute position on the root window.
    pub fn move_pointer(&self, x: i32, y: i32) {
        if self.display.is_null() {
            return;
        }
        unsafe {
            xtest::XTestFakeMotionEvent(self.display, xlib::XDefaultScreen(self.display), x, y, 0);
            xlib::XFlush(self.display);
        }
    }

    /// Where the pointer is, in screen pixels.
    ///
    /// The client draws its own pointer, so it needs to know where the real one
    /// is — both to show it after a move it did not make, and to keep it in the
    /// right place while the picture is zoomed and panned.
    pub fn pointer_position(&self) -> Option<(i32, i32)> {
        if self.display.is_null() {
            return None;
        }
        let mut root = 0u64;
        let mut child = 0u64;
        let mut root_x = 0;
        let mut root_y = 0;
        let mut win_x = 0;
        let mut win_y = 0;
        let mut mask = 0u32;
        let found = unsafe {
            xlib::XQueryPointer(
                self.display,
                xlib::XDefaultRootWindow(self.display),
                &mut root,
                &mut child,
                &mut root_x,
                &mut root_y,
                &mut win_x,
                &mut win_y,
                &mut mask,
            )
        };
        (found != 0).then_some((root_x, root_y))
    }

    /// Types a string, in whatever language it is written in.
    ///
    /// Pressing keys by name only works for characters the machine's keyboard
    /// already has. X will happily accept a remapped keycode, but the other
    /// clients read the keyboard once and do not notice the change, so the
    /// character never arrives — measured, not assumed.
    ///
    /// The clipboard has no such problem: it carries text, not keystrokes. This
    /// takes the CLIPBOARD selection, puts the string on it, and presses the
    /// paste chord. Plane keys — Enter, Tab — are pressed directly, because
    /// pasting them would insert a character rather than press a key.
    pub fn type_text(&mut self, text: &str) -> Result<(), ScreenError> {
        if self.display.is_null() {
            return Err(ScreenError::Gone);
        }
        let mut plain = String::new();
        for character in text.chars() {
            match character {
                '\n' | '\r' => {
                    // Whatever is before the newline goes on the clipboard
                    // first, or the paste and the Enter land out of order.
                    self.paste(&std::mem::take(&mut plain));
                    self.key("Return", &[]);
                }
                '\t' => {
                    self.paste(&std::mem::take(&mut plain));
                    self.key("Tab", &[]);
                }
                other => plain.push(other),
            }
        }
        self.paste(&plain);
        Ok(())
    }

    /// Puts `text` on the clipboard and pastes it.
    fn paste(&mut self, text: &str) {
        if text.is_empty() || self.display.is_null() {
            return;
        }
        self.own_clipboard(text);
        self.key("v", &[String::from("ctrl")]);
    }

    /// Becomes the CLIPBOARD owner and answers requests for it.
    ///
    /// An X client asks for the clipboard with an event and waits for a reply,
    /// so the owner has to be present to answer. The engine has no event loop of
    /// its own to lend, so this opens one connection of its own, takes the
    /// selection on it, and answers on it — leaving the capture connection free
    /// for pixels. The connection is kept alive as long as it owns the selection:
    /// an owner that disconnects takes the clipboard with it, and a paste that
    /// arrives a moment later would find nothing.
    fn own_clipboard(&mut self, text: &str) {
        let display = self.display;
        unsafe {
            let _ = xlib::XFlush(display);
        }
        // The serving connection is made once and reused; the text it holds is
        // replaced each time.
        if self.clipboard.is_none() {
            self.clipboard = ClipboardServer::start(text);
        } else if let Some(server) = self.clipboard.as_mut() {
            server.set(text);
        }
    }

    /// Presses or releases a button at the current pointer position.
    pub fn button(&self, button: Button, pressed: bool) {
        if self.display.is_null() {
            return;
        }
        let down = if pressed { xlib::True } else { xlib::False };
        unsafe {
            xtest::XTestFakeButtonEvent(self.display, button.code(), down, 0);
            xlib::XFlush(self.display);
        }
    }

    /// Sends one key press and release, by keysym name (`Return`, `a`, `F5`).
    ///
    /// A keysym that is not on the current keyboard layout is dropped rather
    /// than remapped: remapping would change the user's own layout on the
    /// machine, which is a bigger surprise than a missing character.
    pub fn key(&self, keysym_name: &str, mods: &[String]) {
        if self.display.is_null() {
            return;
        }
        let name = std::ffi::CString::new(keysym_name).ok();
        let Some(name) = name else { return };
        let keysym = unsafe { xlib::XStringToKeysym(name.as_ptr()) };
        if keysym == 0 {
            return;
        }
        let code = unsafe { xlib::XKeysymToKeycode(self.display, keysym) };
        if code == 0 {
            return;
        }

        // Modifiers are held for the duration of the press, then released in
        // reverse order, so a chord reads exactly as it would on the machine.
        let mut held = Vec::new();
        for modifier in mods {
            let Ok(cname) = std::ffi::CString::new(modifier.as_str()) else { continue };
            let sym = unsafe { xlib::XStringToKeysym(cname.as_ptr()) };
            if sym == 0 {
                continue;
            }
            let mcode = unsafe { xlib::XKeysymToKeycode(self.display, sym) };
            if mcode == 0 {
                continue;
            }
            unsafe { xtest::XTestFakeKeyEvent(self.display, mcode as u32, xlib::True, 0) };
            held.push(mcode);
        }
        unsafe {
            xtest::XTestFakeKeyEvent(self.display, code as u32, xlib::True, 0);
            xtest::XTestFakeKeyEvent(self.display, code as u32, xlib::False, 0);
        }
        for mcode in held.into_iter().rev() {
            unsafe { xtest::XTestFakeKeyEvent(self.display, mcode as u32, xlib::False, 0) };
        }
        unsafe { xlib::XFlush(self.display) };
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if !self.image.is_null() {
            unsafe {
                if !self.shm.is_null() && !self.display.is_null() {
                    xshm::XShmDetach(self.display, self.shm);
                }
                // XDestroyImage frees the image and, on the shared-memory path,
                // the segment; the plain path's buffer came from malloc and is
                // freed by the same call through the image's own destructor.
                let _ = xlib::XDestroyImage(self.image);
                if !self.shm.is_null() {
                    let data = unsafe { (*self.shm).shmaddr };
                    if !data.is_null() {
                        unsafe { libc_shmdt(data as *mut c_void); }
                    }
                    unsafe { libc_free(self.shm as *mut c_void) };
                }
            }
        }
        if !self.display.is_null() {
            unsafe { xlib::XCloseDisplay(self.display) };
            self.display = ptr::null_mut();
        }
    }
}

struct SharedSegment {
    id: i32,
    data: *mut c_void,
    info: *mut xshm::XShmSegmentInfo,
}

/// `shmget` + `shmat`, declared here so the crate does not need a libc binding
/// for four calls.
unsafe fn libc_shmat(size: usize) -> Result<SharedSegment, ScreenError> {
    let info = libc_malloc(std::mem::size_of::<xshm::XShmSegmentInfo>()) as *mut xshm::XShmSegmentInfo;
    if info.is_null() {
        return Err(ScreenError::Shm("no memory for the segment record".to_string()));
    }
    let id = shmget(0, size, IPC_PRIVATE | IPC_CREAT | 0o600);
    if id < 0 {
        libc_free(info as *mut c_void);
        return Err(ScreenError::Shm("shmget failed".to_string()));
    }
    let data = shmat(id, ptr::null(), 0);
    if data == (-1isize) as *mut c_void {
        libc_free(info as *mut c_void);
        return Err(ScreenError::Shm("shmat failed".to_string()));
    }
    Ok(SharedSegment { id, data, info })
}

unsafe fn libc_shmdt(data: *mut c_void) {
    shmdt(data);
}

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: i32 = 0o1000;

extern "C" {
    fn shmget(key: i32, size: usize, flags: i32) -> i32;
    fn shmat(id: i32, addr: *const c_void, flags: i32) -> *mut c_void;
    fn shmdt(addr: *const c_void) -> i32;
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

unsafe fn libc_malloc(size: usize) -> *mut c_void {
    malloc(size)
}

unsafe fn libc_free(ptr: *mut c_void) {
    free(ptr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this guards: a display that refuses shared memory used to end the
    /// process, because Xlib's default handler exits. Opening a display that is
    /// not there must come back as an error, and the process must still be here.
    #[test]
    fn a_display_that_is_not_there_is_an_error_not_an_exit() {
        let result = Screen::open(":9999", 640, 480);
        assert!(matches!(result, Err(ScreenError::Open(_))));
        // Reaching this line at all is the assertion: the default handler would
        // have ended the test binary.
    }

    /// Same for a display name that cannot exist even as a socket path.
    #[test]
    fn a_nonsense_display_name_is_an_error_not_an_exit() {
        let result = Screen::open("not-a-display", 640, 480);
        assert!(result.is_err());
    }

    #[test]
    fn a_failed_request_is_recorded_once() {
        X_FAILED.store(true, Ordering::SeqCst);
        assert!(take_error(), "the mark is visible");
        assert!(!take_error(), "and cleared by reading it");
    }
}

/// Holds the X clipboard and answers requests for it.
///
/// A selection owner must be present to reply: the requesting client sends an
/// event and waits. That needs a connection with an event loop, which the
/// capture connection does not have — it is busy reading pixels — so this opens
/// its own on a thread of its own and leaves it there for as long as the engine
/// lives. The text is replaced from the outside under a mutex, and a condvar
/// wakes the loop so a new string is served immediately rather than at the next
/// poll.
struct ClipboardServer {
    shared: Arc<(Mutex<Option<String>>, Condvar)>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardServer {
    /// Starts a server already holding `text`.
    fn start(text: &str) -> Option<Self> {
        let shared = Arc::new((Mutex::new(Some(text.to_string())), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let ready = Arc::new((Mutex::new(false), Condvar::new()));
        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        let thread_ready = Arc::clone(&ready);

        let handle = std::thread::Builder::new()
            .name("aixodia-clipboard".to_string())
            .spawn(move || serve_selection(thread_shared, thread_stop, thread_ready))
            .ok()?;

        // Wait for the owner to be established, or the paste would be pressed
        // before there is anything to paste.
        let (lock, condvar) = &*ready;
        let started = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = condvar.wait_timeout_while(started, std::time::Duration::from_millis(500), |ready| !*ready);

        Some(Self {
            shared,
            stop,
            thread: Some(handle),
        })
    }

    /// Replaces the text the clipboard serves.
    fn set(&mut self, text: &str) {
        let (lock, condvar) = &*self.shared;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.to_string());
        condvar.notify_all();
    }
}

impl Drop for ClipboardServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let (_, condvar) = &*self.shared;
        condvar.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The selection loop: owns CLIPBOARD and answers requests until told to stop.
fn serve_selection(
    shared: Arc<(Mutex<Option<String>>, Condvar)>,
    stop: Arc<AtomicBool>,
    ready: Arc<(Mutex<bool>, Condvar)>,
) {
    let display = unsafe { xlib::XOpenDisplay(std::ptr::null()) };
    if display.is_null() {
        return;
    }
    unsafe {
        xlib::XSetErrorHandler(Some(note_error));
    }

    // A small window to own the selection against: the owner must be a window,
    // and it need never be mapped.
    let window = unsafe { xlib::XCreateSimpleWindow(display, xlib::XDefaultRootWindow(display), 0, 0, 1, 1, 0, 0, 0) };
    let clipboard = unsafe { xlib::XInternAtom(display, b"CLIPBOARD\0".as_ptr() as *const i8, xlib::False) };
    let utf8 = unsafe { xlib::XInternAtom(display, b"UTF8_STRING\0".as_ptr() as *const i8, xlib::False) };
    let targets = unsafe { xlib::XInternAtom(display, b"TARGETS\0".as_ptr() as *const i8, xlib::False) };

    unsafe {
        xlib::XSetSelectionOwner(display, clipboard, window, xlib::CurrentTime);
        xlib::XFlush(display);
    }
    {
        let (lock, condvar) = &*ready;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        condvar.notify_all();
    }

    while !stop.load(Ordering::SeqCst) {
        // Wait for a request, giving up often enough to notice `stop`.
        let pending = unsafe { xlib::XPending(display) };
        if pending == 0 {
            // Nothing to answer. The selection stays held — taking it again on
            // every pass would fight nobody but would make clients that cache
            // the owner re-ask on every tick, which is worse than waiting.
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }

        let mut event: xlib::XEvent = unsafe { std::mem::zeroed() };
        unsafe { xlib::XNextEvent(display, &mut event) };
        if unsafe { event.type_ } != SEL_NOTIFY {
            continue;
        }
        let request = unsafe { event.selection_request };
        let text = {
            let (lock, _) = &*shared;
            lock.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
        };

        let mut reply: xlib::XSelectionEvent = unsafe { std::mem::zeroed() };
        reply.type_ = SEL_NOTIFY;
        reply.display = display;
        reply.requestor = request.requestor;
        reply.selection = request.selection;
        reply.target = request.target;
        reply.time = request.time;

        if unsafe { request.target } == targets {
            // A client asking what formats are on offer.
            let offered = [utf8];
            unsafe {
                xlib::XChangeProperty(
                    display,
                    request.requestor,
                    request.property,
                    xlib::XA_ATOM,
                    32,
                    xlib::PropModeReplace,
                    offered.as_ptr() as *const u8,
                    1,
                );
            }
            reply.property = request.property;
        } else if unsafe { request.property } != 0 {
            let bytes = text.as_bytes();
            unsafe {
                xlib::XChangeProperty(
                    display,
                    request.requestor,
                    request.property,
                    utf8,
                    8,
                    xlib::PropModeReplace,
                    bytes.as_ptr(),
                    bytes.len() as i32,
                );
            }
            reply.property = request.property;
        }
        unsafe {
            xlib::XSendEvent(display, request.requestor, 0, 0, &mut reply as *mut _ as *mut xlib::XEvent);
            xlib::XFlush(display);
        }
    }

    unsafe {
        xlib::XCloseDisplay(display);
    }
}
