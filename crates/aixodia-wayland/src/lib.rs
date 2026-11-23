//! A Wayland compositor written from the protocol up.
//!
//! X11 is the older arrangement and it shows: a separate display server, a window
//! manager that is a second program talking to it, a client library linked into
//! every program that wants a picture, and a capture path that copies the
//! server's framebuffer back out across a socket. Wayland folds the display
//! server and the window manager into one program — the compositor — and hands
//! each client's pixels over shared memory, so the compositor owns the only copy
//! of the picture it already has to encode.
//!
//! `wlroots` would bring that with eleven shared libraries and a GPU its renderer
//! needs; this host has no `/dev/dri` at all. So the compositor is written from
//! the protocol, the way the RFB server was: this crate depends on `libc` and
//! `tracing`, and nothing in it needs a Wayland package installed.
//!
//! * `protocol` — the wire format, eight argument types and all.
//! * `sys` — the sendmsg/recvmsg that carries a file descriptor beside the bytes.
//! * `server` — connections, the message pump, and the globals on offer.
//! * `state` — the object model, what each opcode does, and compositing.
//! * `shm` — the client's shared-memory pool, mapped and blitted.
//! * `keymap` — the xkb keymap a keyboard is sent, so typing produces text.

pub mod keymap;
pub mod protocol;
pub mod server;
pub mod shm;
pub mod state;
pub mod sys;

use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

pub use server::{Global, GLOBALS};
pub use state::{State, Window};

/// A compositor: the socket clients connect to, and the picture they draw into.
pub struct Compositor {
    server: server::Server,
    pub state: State,
    /// What was last written to the log, so a change is logged once and an idle
    /// pane is silent.
    reported: Option<(usize, usize, bool)>,
}

impl Compositor {
    /// Binds `dir/name` and gives the output a size. Clients find it through
    /// `WAYLAND_DISPLAY=name` and `XDG_RUNTIME_DIR=dir`, which is what a
    /// launcher unsets and sets before starting a program.
    pub fn new(dir: &Path, name: &str, width: u32, height: u32) -> io::Result<Self> {
        let server = server::Server::bind(dir, name)?;
        Ok(Self {
            server,
            state: State::new(width, height),
            reported: None,
        })
    }

    pub fn path(&self) -> &Path {
        self.server.path()
    }

    pub fn size(&self) -> (u32, u32) {
        (self.state.width, self.state.height)
    }

    /// The picture, as ARGB.
    pub fn frame(&self) -> &[u32] {
        &self.state.frame
    }

    /// Takes a connection the caller already has, which is what a test does and
    /// what a program launched with an inherited socket would need.
    pub fn adopt(&mut self, stream: UnixStream) -> io::Result<()> {
        stream.set_nonblocking(true)?;
        self.state.add_client(server::Client::new(stream)?);
        Ok(())
    }

    /// Waits up to `timeout_ms` for something to happen and then does it.
    ///
    /// Returns true when the picture changed, which is the only thing a caller
    /// that encodes frames cares about.
    pub fn poll(&mut self, timeout_ms: i32) -> io::Result<bool> {
        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(self.state.clients.len() + 1);
        fds.push(libc::pollfd {
            fd: self.server.listener().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for client in &self.state.clients {
            fds.push(libc::pollfd {
                fd: client.client.fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // SAFETY: the array is valid for its length and poll does not keep it.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(err);
        }
        self.pump(!fds.is_empty() && fds[0].revents != 0)
    }

    /// Handles whatever is already waiting, without blocking.
    pub fn pump_ready(&mut self) -> io::Result<bool> {
        self.pump(true)
    }

    fn pump(&mut self, accept: bool) -> io::Result<bool> {
        let arrangement = self.state.arrangement();
        if accept {
            while let Some(client) = self.server.accept()? {
                self.state.add_client(client);
            }
        }
        // Read and handle per client; a client that has gone is dropped with its
        // windows.
        let mut gone = Vec::new();
        for index in 0..self.state.clients.len() {
            let alive = self.state.clients[index].client.fill()?;
            if !alive {
                gone.push(index);
                continue;
            }
            if let Err(err) = self.state.handle_client(index) {
                tracing::info!("wayland: dropping a client: {err}");
                gone.push(index);
            }
        }
        for index in gone.into_iter().rev() {
            self.remove_client(index);
        }
        // Whatever a client could not take last turn is offered again now. A
        // composer that keeps this inside the write path is a composer that stalls
        // the whole display behind one slow reader.
        for index in 0..self.state.clients.len() {
            if let Err(err) = self.state.clients[index].client.flush() {
                tracing::info!("wayland: a client stopped reading: {err}");
                self.remove_client(index);
                break;
            }
        }
        self.state.release_buffers()?;
        self.state.reconcile();
        // A window that appears under a pointer that has not moved still takes the
        // pointer, and with it the keyboard. Without this, a viewer that points at
        // the middle of an empty screen before the browser has drawn has to point
        // again — and a viewer that only then sends keys (ctrl+L, a URL) has
        // typed into nothing.
        if arrangement != self.state.arrangement() {
            let (x, y) = (self.state.pointer.x, self.state.pointer.y);
            self.state.pointer_motion(x, y)?;
        }
        let report = (
            self.state.clients.len(),
            self.state.windows.len(),
            self.state.dirty,
        );
        if self.reported != Some(report) {
            self.reported = Some(report);
            tracing::debug!(
                "wayland: {} clients, {} windows, dirty={}",
                report.0,
                report.1,
                report.2
            );
        }
        if self.state.dirty {
            self.state.compose();
            return Ok(true);
        }
        Ok(false)
    }

    /// Forgets a client, and every window that belonged to it.
    ///
    /// Window indices are positions in a vector that other objects point at, so
    /// the survivors are rebuilt and the ids that pointed at them are corrected
    /// rather than left dangling.
    fn remove_client(&mut self, index: usize) {
        self.state.clients.remove(index);
        // Windows are positions in a vector that objects point at, so the
        // survivors are rebuilt and every pointer to a window is rewritten from
        // what each surface now is, rather than shifted by arithmetic.
        let mut kept = Vec::with_capacity(self.state.windows.len());
        for window in self.state.windows.drain(..) {
            if window.client == index {
                continue;
            }
            let client = if window.client > index {
                window.client - 1
            } else {
                window.client
            };
            kept.push(Window { client, ..window });
        }
        self.state.windows = kept;
        // A popup belongs to a client the same way a window does, including the
        // indices of the clients that are left.
        self.state.popups.retain(|popup| popup.client != index);
        for popup in &mut self.state.popups {
            if popup.client > index {
                popup.client -= 1;
            }
        }
        for (client_index, client) in self.state.clients.iter_mut().enumerate() {
            let windows: Vec<(u32, usize)> = self
                .state
                .windows
                .iter()
                .enumerate()
                .filter(|(_, window)| window.client == client_index)
                .map(|(position, window)| (window.surface, position))
                .collect();
            for object in client.objects.values_mut() {
                if let state::Object::XdgSurface {
                    surface,
                    window,
                    configured: _,
                } = object
                {
                    *window = windows
                        .iter()
                        .find(|(surface_id, _)| surface_id == surface)
                        .map(|(_, position)| *position);
                }
            }
        }
        if let Some((client, _surface)) = self.state.pointer.over {
            if client >= index {
                self.state.pointer.over = None;
            }
        }
        if let Some((client, _surface)) = self.state.keyboard_focus {
            if client >= index {
                self.state.keyboard_focus = None;
            }
        }
        self.state.dirty = true;
    }

    /// Moves the pointer, as a phone's finger would.
    pub fn pointer_motion(&mut self, x: f64, y: f64) -> io::Result<()> {
        self.state.pointer_motion(x, y)
    }

    /// A pointer button. Wayland's numbers are evdev's: 1 left, 2 middle, 3 right.
    pub fn pointer_button(&mut self, button: u32, pressed: bool) -> io::Result<()> {
        self.state.pointer_button(button, pressed)
    }

    /// A scroll, in notches.
    pub fn pointer_axis(&mut self, notches: f64) -> io::Result<()> {
        self.state.pointer_axis(notches)
    }

    /// A scroll sideways, in notches.
    pub fn pointer_axis_across(&mut self, notches: f64) -> io::Result<()> {
        self.state.pointer_axis_along(notches, false)
    }

    /// Where the pointer is, in the frame's coordinates.
    pub fn pointer_position(&self) -> (f64, f64) {
        (self.state.pointer.x, self.state.pointer.y)
    }

    /// A key, by evdev code.
    pub fn key(&mut self, evdev: u32, pressed: bool) -> io::Result<()> {
        self.state.key(evdev, pressed)
    }

    /// A window's title, for a taskbar the engine draws itself.
    pub fn windows(&self) -> &[Window] {
        &self.state.windows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Args, Writer};
    use crate::sys;
    use std::io::Read;
    use std::os::unix::io::AsRawFd;

    /// Reads one event from the client end of the connection.
    fn next_event(theirs: &UnixStream) -> (u32, u16, Vec<u8>) {
        let mut reader = theirs;
        let mut header = [0u8; 8];
        reader.read_exact(&mut header).unwrap();
        let object = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let opcode = u16::from_le_bytes([header[4], header[5]]);
        let size = u16::from_le_bytes([header[6], header[7]]) as usize;
        let mut body = vec![0u8; size - 8];
        if !body.is_empty() {
            reader.read_exact(&mut body).unwrap();
        }
        (object, opcode, body)
    }

    /// Reads one event if one is waiting, and gives up rather than blocking.
    fn next_message(theirs: &UnixStream) -> Option<(u32, u16, Vec<u8>)> {
        use std::io::Read;
        theirs.set_nonblocking(true).ok()?;
        let mut header = [0u8; 8];
        let mut reader = theirs;
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => {
                theirs.set_nonblocking(false).ok()?;
                return None;
            }
        }
        let object = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let opcode = u16::from_le_bytes([header[4], header[5]]);
        let size = u16::from_le_bytes([header[6], header[7]]) as usize;
        let mut body = vec![0u8; size - 8];
        if !body.is_empty() {
            reader.read_exact(&mut body).ok()?;
        }
        theirs.set_nonblocking(false).ok()?;
        Some((object, opcode, body))
    }

    /// Sends one request with an optional fd beside it.
    fn request(theirs: &UnixStream, writer: Writer) {
        let bytes = writer.bytes();
        sys::send(theirs.as_raw_fd(), bytes, &writer.fds).unwrap();
    }

    /// A compositor and a client on the two ends of one socket.
    fn setup(size: u32) -> (Compositor, UnixStream) {
        // A socket of its own per call: tests run in parallel, and two of them
        // binding one path is `Address already in use`.
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("aixodia-wl-{}-{size}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut compositor = Compositor::new(&dir, "wayland-0", size, size).unwrap();
        let (ours, theirs) = UnixStream::pair().unwrap();
        compositor.adopt(ours).unwrap();
        (compositor, theirs)
    }

    /// Pumps the compositor until it has handled what the test sent.
    fn settle(compositor: &mut Compositor) {
        compositor.pump_ready().unwrap();
    }

    #[test]
    fn a_client_that_does_the_whole_handshake_gets_its_pixels_into_the_frame() {
        let (mut compositor, theirs) = setup(8);
        // wl_display.get_registry(2), and read the globals it answers with.
        request(&theirs, Writer::new(1, 1).uint(2).finish());
        settle(&mut compositor);
        let mut globals = Vec::new();
        loop {
            let (object, opcode, body) = next_event(&theirs);
            assert_eq!((object, opcode), (2, 0));
            let mut args = Args::new(&body);
            let _name = args.uint();
            globals.push(args.string().unwrap_or_default());
            if globals.len() == server::GLOBALS.len() {
                break;
            }
        }
        assert!(globals.iter().any(|name| name == "wl_shm"));

        // Bind compositor, shm, seat and xdg_wm_base.
        for (name, interface, id) in [
            (1u32, "wl_compositor", 3u32),
            (2, "wl_shm", 4),
            (3, "wl_seat", 5),
            (6, "xdg_wm_base", 6),
        ] {
            request(
                &theirs,
                Writer::new(2, 0)
                    .uint(name)
                    .string(Some(interface))
                    .uint(4)
                    .uint(id)
                    .finish(),
            );
        }
        settle(&mut compositor);

        // A surface, an xdg_surface and a toplevel on it.
        request(&theirs, Writer::new(3, 0).uint(7).finish());
        request(&theirs, Writer::new(6, 2).uint(8).object(7).finish());
        request(&theirs, Writer::new(8, 1).uint(9).finish());
        settle(&mut compositor);
        // The commit that asks to be configured.
        request(&theirs, Writer::new(7, 6).finish());
        settle(&mut compositor);

        // A pool holding an 8x8 picture, and a buffer into it.
        let pool = tempfile_pool(8, 8, 0xff00_ff00);
        let message = Writer::new(4, 0)
            .uint(10)
            .int(8 * 8 * 4)
            .fd(pool.as_raw_fd())
            .finish();
        request(&theirs, message);
        request(
            &theirs,
            Writer::new(10, 0)
                .uint(11)
                .int(0)
                .int(8)
                .int(8)
                .int(8 * 4)
                .uint(0)
                .finish(),
        );
        settle(&mut compositor);
        // Attach it and commit: this is the frame the client wants shown.
        request(&theirs, Writer::new(7, 1).object(11).int(0).int(0).finish());
        request(&theirs, Writer::new(7, 6).finish());
        settle(&mut compositor);

        assert_eq!(compositor.state.windows.len(), 1);
        let window = &compositor.state.windows[0];
        assert!(window.mapped, "the window has a buffer and is mapped");
        assert_eq!((window.width, window.height), (8, 8));
        // The window sits at (40, 40) by default, which is outside this 8x8
        // output, so the frame is background — the window is real but off screen.
        assert!(compositor
            .state
            .frame
            .iter()
            .all(|pixel| *pixel == 0xff10_1a2b));
        // Move it into view and the pixels arrive.
        compositor.state.windows[0].x = 0;
        compositor.state.windows[0].y = 0;
        compositor.state.dirty = true;
        compositor.state.compose();
        assert_eq!(compositor.state.frame[0], 0xff00_ff00);
    }

    /// A shared-memory pool with every pixel one colour.
    fn tempfile_pool(width: u32, height: u32, colour: u32) -> std::fs::File {
        use std::io::Write;
        let name = c"aixodia-test-pool";
        // SAFETY: a C string literal and the documented memfd flags.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        // SAFETY: the fd is fresh and ownership moves into the File.
        let mut file = unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(fd) };
        let mut bytes = Vec::new();
        for _ in 0..width * height {
            bytes.extend_from_slice(&colour.to_le_bytes());
        }
        file.write_all(&bytes).unwrap();
        file
    }

    #[test]
    fn a_sync_is_answered_and_a_surface_can_be_created() {
        let (mut compositor, theirs) = setup(4);
        // The registry first: a callback has no bind, and a bind on a callback
        // is the protocol error this test exists to not make.
        request(&theirs, Writer::new(1, 1).uint(2).finish());
        settle(&mut compositor);
        for _ in 0..server::GLOBALS.len() {
            let (object, opcode, _) = next_event(&theirs);
            assert_eq!((object, opcode), (2, 0));
        }
        request(
            &theirs,
            Writer::new(2, 0)
                .uint(1)
                .string(Some("wl_compositor"))
                .uint(4)
                .uint(3)
                .finish(),
        );
        settle(&mut compositor);
        request(&theirs, Writer::new(3, 0).uint(4).finish());
        settle(&mut compositor);
        assert!(matches!(
            compositor.state.clients[0].objects.get(&4),
            Some(state::Object::Surface(_))
        ));

        // And a sync after all that is answered with its own serial.
        request(&theirs, Writer::new(1, 0).uint(5).finish());
        settle(&mut compositor);
        let (object, opcode, body) = next_event(&theirs);
        assert_eq!((object, opcode), (5, 0));
        assert!(Args::new(&body).uint() >= 1);
    }

    /// A compositor with one client that already has a pointer and a keyboard,
    /// and one window under (0,0)-(64,64), so input can be pointed at it.
    fn with_input(size: u32) -> (Compositor, UnixStream) {
        let (mut compositor, theirs) = setup(size);
        compositor.state.clients[0]
            .objects
            .insert(20, state::Object::Pointer);
        compositor.state.clients[0]
            .objects
            .insert(21, state::Object::Keyboard);
        compositor.state.windows.push(Window {
            client: 0,
            surface: 7,
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            title: "test".into(),
            app_id: "test".into(),
            mapped: true,
        });
        (compositor, theirs)
    }

    #[test]
    fn a_keyboard_says_when_it_enters_a_window_and_when_it_leaves() {
        let (mut compositor, theirs) = with_input(128);
        compositor.pointer_motion(10.0, 10.0).unwrap();
        // wl_pointer enter is opcode 0 on the pointer; wl_keyboard enter is opcode
        // 1 on the keyboard. Both are sent, and the keyboard's names a surface —
        // a null one is a protocol error.
        let mut keyboard_entered = None;
        while let Some((object, opcode, body)) = next_message(&theirs) {
            if object == 21 && opcode == 1 {
                let mut args = Args::new(&body);
                let _serial = args.uint();
                keyboard_entered = Some(args.object());
            }
        }
        assert_eq!(keyboard_entered, Some(7), "the keyboard entered our window");

        // Leaving for the desktop tells the keyboard too.
        compositor.pointer_motion(120.0, 120.0).unwrap();
        let mut left = false;
        while let Some((object, opcode, _body)) = next_message(&theirs) {
            if object == 21 && opcode == 2 {
                left = true;
            }
        }
        assert!(left, "the keyboard was told the pointer left the window");
    }

    #[test]
    fn shift_is_reported_as_the_modifier_the_keymap_binds() {
        let (mut compositor, theirs) = with_input(128);
        compositor.pointer_motion(10.0, 10.0).unwrap();
        // Read past the enter, then press shift: 42 is the left shift keycode.
        while next_message(&theirs).is_some() {}
        compositor.key(42, true).unwrap();
        assert_eq!(modifiers_sent(&theirs), Some(1), "shift is modifier bit 0");
        compositor.key(42, false).unwrap();
        assert_eq!(
            modifiers_sent(&theirs),
            Some(0),
            "and releasing it clears the bit"
        );
    }

    /// The depressed-modifier word of the last `wl_keyboard.modifiers` event.
    fn modifiers_sent(theirs: &UnixStream) -> Option<u32> {
        let mut depressed = None;
        while let Some((object, opcode, body)) = next_message(theirs) {
            if object == 21 && opcode == 4 {
                let mut args = Args::new(&body);
                let _serial = args.uint();
                depressed = Some(args.uint());
            }
        }
        depressed
    }

    #[test]
    fn a_key_with_no_focus_goes_nowhere() {
        let (mut compositor, theirs) = with_input(128);
        // The pointer is outside the window, so the keyboard has no surface.
        compositor.pointer_motion(120.0, 120.0).unwrap();
        while next_message(&theirs).is_some() {}
        compositor.key(30, true).unwrap();
        assert!(
            next_message(&theirs).is_none(),
            "nothing was sent to anyone"
        );
    }

    #[test]
    fn the_pointer_reports_the_window_under_it_and_the_focus_follows() {
        let (mut compositor, theirs) = setup(64);
        // A window record by hand: this test is about hit testing, not about the
        // handshake the other test already proves.
        compositor.state.windows.push(Window {
            client: 0,
            surface: 7,
            x: 10,
            y: 10,
            width: 20,
            height: 20,
            title: "test".into(),
            app_id: "test".into(),
            mapped: true,
        });
        compositor.pointer_motion(15.0, 15.0).unwrap();
        assert_eq!(compositor.state.pointer.over, Some((0, 7)));
        compositor.pointer_motion(60.0, 60.0).unwrap();
        assert_eq!(compositor.state.pointer.over, None);
        drop(theirs);
    }
}
