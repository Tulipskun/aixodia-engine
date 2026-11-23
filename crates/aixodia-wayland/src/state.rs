//! The compositor's state: every object a client holds, and what each opcode does.
//!
//! Wayland's shape is here. A client binds globals, creates surfaces, says "here
//! is a buffer" and commits; the compositor answers a commit with a configure
//! (that is the size the window will be) and, when a picture is on the screen,
//! with a frame callback. Input goes the other way: the compositor sends enter,
//! motion, button and key events to the surface under the pointer or holding the
//! focus, each with a serial the client can quote back.
//!
//! No picture is copied through a socket here: `shm` maps the client's own pages
//! and `compose` blits them into the frame the engine encodes.

use std::collections::HashMap;

use crate::keymap;
use crate::protocol::{Args, Writer};
use crate::server::{Client, Interface, GLOBALS};
use crate::shm::{Buffer, Pool, ARGB8888, XRGB8888};

/// A window as the compositor knows it: one toplevel surface that has been
/// committed with a buffer, and where it sits.
#[derive(Debug, Clone)]
pub struct Window {
    pub client: usize,
    pub surface: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub app_id: String,
    /// False until a buffer has been committed: an unmapped window is not drawn.
    pub mapped: bool,
}

/// What one object id means on one connection.
pub enum Object {
    Display,
    Registry,
    Callback,
    Compositor,
    Surface(Surface),
    Region,
    Shm,
    ShmPool(Pool),
    Buffer(Buffer),
    Seat,
    Pointer,
    Keyboard,
    Touch,
    Output,
    DataDeviceManager,
    DataDevice,
    DataSource,
    XdgWmBase,
    Positioner(Positioner),
    XdgSurface {
        surface: u32,
        window: Option<usize>,
        /// False until the first configure has gone out. The protocol puts that
        /// configure in answer to the client's initial commit, and a client that
        /// is configured before it ever committed is being answered before it
        /// asked.
        configured: bool,
    },
    XdgToplevel {
        xdg_surface: u32,
        title: String,
        app_id: String,
    },
    XdgPopup {
        xdg_surface: u32,
    },
    TextInputManager,
    TextInput {
        /// True between `enable` and `disable`: the client has a text field
        /// waiting for characters.
        enabled: bool,
    },
}

impl Object {
    fn interface(&self) -> Interface {
        match self {
            Self::Display => Interface::Display,
            Self::Registry => Interface::Registry,
            Self::Callback => Interface::Callback,
            Self::Compositor => Interface::Compositor,
            Self::Surface(_) => Interface::Surface,
            Self::Region => Interface::Region,
            Self::Shm => Interface::Shm,
            Self::ShmPool(_) => Interface::ShmPool,
            Self::Buffer(_) => Interface::Buffer,
            Self::Seat => Interface::Seat,
            Self::Pointer => Interface::Pointer,
            Self::Keyboard => Interface::Keyboard,
            Self::Touch => Interface::Touch,
            Self::Output => Interface::Output,
            Self::DataDeviceManager => Interface::DataDeviceManager,
            Self::DataDevice => Interface::DataDevice,
            Self::DataSource => Interface::DataSource,
            Self::XdgWmBase => Interface::XdgWmBase,
            Self::Positioner(_) => Interface::Positioner,
            Self::XdgSurface { .. } => Interface::XdgSurface,
            Self::XdgToplevel { .. } => Interface::XdgToplevel,
            Self::XdgPopup { .. } => Interface::XdgPopup,
            Self::TextInputManager => Interface::TextInputManager,
            Self::TextInput { .. } => Interface::TextInput,
        }
    }
}

/// Where a popup asked to be, in the terms the protocol gives.
///
/// A positioner is a form the client fills in: a rectangle to hang the popup off,
/// the size it wants, and an offset. Only the parts that decide *where* a popup
/// lands are kept; the gravity and constraint words are read and not remembered,
/// because this compositor has one screen and no edges to slide away from.
#[derive(Default, Clone, Copy, Debug)]
pub struct Positioner {
    pub anchor: (i32, i32, i32, i32),
    pub size: (i32, i32),
    pub offset: (i32, i32),
}

/// A menu or a dropdown: a surface that belongs to another surface, not to the
/// list of windows.
///
/// Chrome's address bar suggestions are a popup. Without one, the browser has no
/// place to put them and, measured here, died with a SIGSEGV while typing into an
/// omnibox that had opened one.
#[derive(Clone, Debug)]
pub struct Popup {
    pub client: usize,
    /// The `xdg_popup` object itself: the popup is configured through this, the
    /// surface through its `xdg_surface`.
    pub object: u32,
    pub surface: u32,
    pub xdg_surface: u32,
    pub parent: u32,
    pub positioner: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub mapped: bool,
}

/// What a surface has been told, and what it has not yet shown.
#[derive(Default)]
pub struct Surface {
    /// The buffer attached and waiting for a commit.
    pub pending_buffer: Option<u32>,
    /// The buffer on the screen now.
    pub buffer: Option<u32>,
    pub width: u32,
    pub height: u32,
    /// Callbacks waiting for the next frame, as ids on this connection.
    pub frame_callbacks: Vec<u32>,
    pub scale: i32,
    pub window: Option<usize>,
    /// False until the surface has been told which output it is on.
    pub entered: bool,
}

/// One connection and the objects it holds.
pub struct ClientState {
    pub client: Client,
    pub objects: HashMap<u32, Object>,
}

/// The pointer, as the compositor sees it.
#[derive(Debug, Default, Clone, Copy)]
pub struct Pointer {
    pub x: f64,
    pub y: f64,
    /// The surface the pointer is over, and the client that owns it.
    pub over: Option<(usize, u32)>,
    /// The cursor the client under the pointer asked us to draw.
    pub cursor: Option<PointerCursor>,
}

/// A cursor a client set on the pointer: which surface holds its pixels, and
/// which pixel of that surface sits on the pointer.
#[derive(Debug, Clone, Copy)]
pub struct PointerCursor {
    pub client: usize,
    pub surface: u32,
    pub hot_x: i32,
    pub hot_y: i32,
}

/// The pixels of the cursor on screen, and where they point.
///
/// A viewer draws this itself, and so does the app: the shape is the only honest
/// answer to "can text be typed here", and a name is not available to ask for.
#[derive(Debug, Clone)]
pub struct CursorImage {
    pub serial: u64,
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    /// ARGB, premultiplied, as the client drew it.
    pub argb: Vec<u32>,
}

/// The compositor.
pub struct State {
    pub width: u32,
    pub height: u32,
    /// ARGB pixels, row-major: the picture the engine encodes.
    pub frame: Vec<u32>,
    pub clients: Vec<ClientState>,
    pub serial: u32,
    /// Bottom first.
    pub windows: Vec<Window>,
    /// Menus and dropdowns, drawn above every window.
    pub popups: Vec<Popup>,
    pub pointer: Pointer,
    /// The cursor image last committed on the pointer's surface.
    pub cursor_image: Option<CursorImage>,
    /// Set by anything that must be recomposited before the next frame.
    pub dirty: bool,
    /// The surface the keyboard types into, and the client that owns it.
    pub keyboard_focus: Option<(usize, u32)>,
    keymap: Option<std::fs::File>,
    keymap_size: u32,
    /// The modifier bits the client has been told about: bit 0 shift, 1 caps
    /// lock, 2 control, 3 alt, 6 logo — the xkb indices the keymap in `keymap`
    /// actually uses.
    pub mods: u32,
    /// Buffers to release once the client's own requests for this turn have all
    /// been read.
    ///
    /// A release is the compositor talking about a buffer the client may already
    /// have thrown away: the destroy can be sitting in the same turn's buffer,
    /// later than the commit that freed it. Sending the release mid-turn is
    /// therefore a race with the client's own words — measured as a browser
    /// dying the instant it typed, right after a release went out to an id it had
    /// recycled. Sending it at the end of the turn, once every request has been
    /// read, is the same promise kept in the right order.
    pub releases: Vec<(usize, u32)>,
    /// Counters for the log: a compositor that is asked for buffers and never
    /// commits one is a compositor whose client is stuck, and the log should say.
    pub commits: u64,
    /// When the compositor started, for the time field of input events.
    started: std::time::Instant,
    pub configures: u64,
}

impl State {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            frame: vec![0xff10_1a2b; (width * height) as usize],
            clients: Vec::new(),
            serial: 0,
            windows: Vec::new(),
            popups: Vec::new(),
            releases: Vec::new(),
            pointer: Pointer::default(),
            cursor_image: None,
            dirty: true,
            keyboard_focus: None,
            keymap: None,
            keymap_size: 0,
            mods: 0,
            commits: 0,
            started: std::time::Instant::now(),
            configures: 0,
        }
    }

    /// Milliseconds since the compositor started.
    ///
    /// A key event carries a time, and a client uses it to tell a fresh press
    /// from a repeat: every event stamped zero is a client that computes a
    /// nonsense interval the first time it holds a key down.
    fn now_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }

    pub fn add_client(&mut self, client: Client) {
        // Every connection begins with `wl_display` on id 1, and the client is
        // allowed to use it before it has been told anything.
        let mut objects = HashMap::new();
        objects.insert(crate::server::DISPLAY_ID, Object::Display);
        self.clients.push(ClientState { client, objects });
    }

    /// Handles every whole message a client has buffered.
    pub fn handle_client(&mut self, index: usize) -> std::io::Result<usize> {
        let mut handled = 0;
        while let Some((object, opcode, body)) = self.clients[index].client.next_message()? {
            self.dispatch(index, object, opcode, &body)?;
            handled += 1;
        }
        Ok(handled)
    }

    fn dispatch(
        &mut self,
        index: usize,
        object: u32,
        opcode: u16,
        body: &[u8],
    ) -> std::io::Result<()> {
        let interface = match self.clients[index].objects.get(&object) {
            Some(object) => object.interface(),
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("request on unknown object {object}"),
                ))
            }
        };
        match (interface, opcode) {
            (Interface::Display, 0) => {
                let callback = Args::new(body).new_id();
                self.insert(index, callback, Object::Callback);
                let serial = self.next_serial();
                self.send(index, Writer::new(callback, 0).uint(serial).finish())?;
            }
            (Interface::Display, 1) => {
                let registry = Args::new(body).new_id();
                self.insert(index, registry, Object::Registry);
                for global in GLOBALS {
                    let event = Writer::new(registry, 0)
                        .uint(global.name)
                        .string(Some(global.interface))
                        .uint(global.version)
                        .finish();
                    self.send(index, event)?;
                }
            }
            (Interface::Registry, 0) => {
                let mut args = Args::new(body);
                let name = args.uint();
                let interface = args.string().unwrap_or_default();
                let version = args.uint();
                let id = args.new_id();
                self.bind(index, name, &interface, version, id)?;
            }
            (Interface::Compositor, 0) => {
                let surface = Args::new(body).new_id();
                self.insert(index, surface, Object::Surface(Surface::default()));
            }
            (Interface::Compositor, 1) => {
                let region = Args::new(body).new_id();
                self.insert(index, region, Object::Region);
            }
            (Interface::Shm, 0) => {
                let mut args = Args::new(body);
                let pool = args.new_id();
                let size = args.int();
                match self.clients[index].client.take_fd() {
                    Some(fd) if size > 0 => match Pool::map(fd, size as usize) {
                        Ok(mapped) => self.insert(index, pool, Object::ShmPool(mapped)),
                        Err(err) => {
                            tracing::warn!("wayland: a client's pool could not be mapped: {err}");
                            // SAFETY: the fd came from the client and was not mapped.
                            unsafe { libc::close(fd) };
                        }
                    },
                    Some(fd) => {
                        tracing::warn!("wayland: create_pool with a size of {size}");
                        unsafe { libc::close(fd) };
                    }
                    None => tracing::warn!("wayland: create_pool without a file descriptor"),
                }
            }
            (Interface::ShmPool, 2) => {
                let mut args = Args::new(body);
                let size = args.int().max(0) as usize;
                if let Some(Object::ShmPool(pool)) = self.clients[index].objects.get_mut(&object) {
                    match pool.resize(size) {
                        Ok(mapped) if mapped < size => tracing::warn!(
                            "wayland: pool {object} asked for {size} bytes but its file holds {mapped}"
                        ),
                        Ok(_) => {}
                        Err(err) => tracing::warn!("wayland: a pool could not grow: {err}"),
                    }
                }
            }
            (Interface::ShmPool, 0) => {
                let mut args = Args::new(body);
                let id = args.new_id();
                let offset = args.int().max(0) as usize;
                let width = args.int().max(0) as u32;
                let height = args.int().max(0) as u32;
                let stride = args.int().max(0) as usize;
                let format = args.uint();
                let buffer = Buffer {
                    pool: object,
                    offset,
                    width,
                    height,
                    stride,
                    format,
                };
                let pool_size = match self.clients[index].objects.get(&object) {
                    Some(Object::ShmPool(pool)) => pool.size(),
                    _ => 0,
                };
                if offset + buffer.required_bytes() > pool_size {
                    tracing::warn!(
                        "wayland: buffer {id} on pool {object} needs {} bytes at {offset} of a {pool_size}-byte pool",
                        buffer.required_bytes()
                    );
                } else {
                    self.insert(index, id, Object::Buffer(buffer));
                }
            }
            (Interface::Surface, opcode) => self.surface_request(index, object, opcode, body)?,
            // set_cursor(serial, surface, hotspot_x, hotspot_y): the client is
            // telling us what to draw, as a surface of its own. A null surface
            // means it has nothing to say and the caller's own pointer stands.
            (Interface::Pointer, 0) => {
                let mut args = Args::new(body);
                let _serial = args.uint();
                let surface = args.object();
                let hot_x = args.int();
                let hot_y = args.int();
                self.pointer.cursor = if surface == 0 {
                    None
                } else {
                    Some(PointerCursor {
                        client: index,
                        surface,
                        hot_x,
                        hot_y,
                    })
                };
            }
            // release(): the pointer object is going away; its cursor with it.
            (Interface::Pointer, 1) => {
                self.pointer.cursor = None;
            }
            (Interface::Seat, 0) => {
                let pointer = Args::new(body).new_id();
                self.insert(index, pointer, Object::Pointer);
                // A pointer is only ever told it is inside a surface it is
                // actually inside: `wl_pointer.enter` cannot carry a null
                // surface, and libwayland aborts the client when a compositor
                // sends one. A pointer with nothing under it says nothing.
                if let Some((client, surface)) = self.pointer.over {
                    if client == index {
                        let serial = self.next_serial();
                        let (x, y) = (self.pointer.x, self.pointer.y);
                        self.send(
                            index,
                            Writer::new(pointer, 0)
                                .uint(serial)
                                .object(surface)
                                .fixed(x)
                                .fixed(y)
                                .finish(),
                        )?;
                    }
                }
            }
            (Interface::Seat, 1) => {
                let keyboard = Args::new(body).new_id();
                self.insert(index, keyboard, Object::Keyboard);
                self.send_keymap(index, keyboard)?;
            }
            (Interface::Seat, 2) => {
                let touch = Args::new(body).new_id();
                self.insert(index, touch, Object::Touch);
            }
            (Interface::XdgWmBase, 2) => {
                let mut args = Args::new(body);
                let xdg_surface = args.new_id();
                let surface = args.object();
                self.insert(
                    index,
                    xdg_surface,
                    Object::XdgSurface {
                        surface,
                        window: None,
                        configured: false,
                    },
                );
            }
            (Interface::XdgWmBase, 1) => {
                let positioner = Args::new(body).new_id();
                self.insert(index, positioner, Object::Positioner(Positioner::default()));
            }
            (Interface::XdgSurface, 1) => {
                let toplevel = Args::new(body).new_id();
                self.insert(
                    index,
                    toplevel,
                    Object::XdgToplevel {
                        xdg_surface: object,
                        title: String::new(),
                        app_id: String::new(),
                    },
                );
            }
            (Interface::XdgSurface, 2) => {
                let mut args = Args::new(body);
                let popup = args.new_id();
                let parent = args.object();
                let positioner = args.object();
                let _x = args.int();
                let _y = args.int();
                self.insert(
                    index,
                    popup,
                    Object::XdgPopup {
                        xdg_surface: object,
                    },
                );
                let surface = match self.clients[index].objects.get(&object) {
                    Some(Object::XdgSurface { surface, .. }) => *surface,
                    _ => return Ok(()),
                };
                self.popups.push(Popup {
                    client: index,
                    object: popup,
                    surface,
                    xdg_surface: object,
                    parent,
                    positioner,
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                    mapped: false,
                });
            }
            (Interface::Positioner, opcode) => {
                let mut args = Args::new(body);
                let mut asked = match self.clients[index].objects.get(&object) {
                    Some(Object::Positioner(positioner)) => *positioner,
                    _ => Positioner::default(),
                };
                match opcode {
                    // set_size
                    1 => asked.size = (args.int(), args.int()),
                    // set_anchor_rect
                    2 => {
                        asked.anchor = (args.int(), args.int(), args.int(), args.int());
                    }
                    // set_offset
                    6 => asked.offset = (args.int(), args.int()),
                    // destroy, and the anchor/gravity/constraint words: read, not
                    // remembered, since one screen has no edge to slide away from.
                    0 => {
                        self.clients[index].objects.remove(&object);
                        return Ok(());
                    }
                    _ => {}
                }
                self.clients[index]
                    .objects
                    .insert(object, Object::Positioner(asked));
            }
            (Interface::XdgPopup, 0) => {
                self.clients[index].objects.remove(&object);
                self.popups.retain(|popup| popup.object != object);
                self.dirty = true;
            }
            // Destroys. A table that keeps what the client has thrown away starts
            // sending events about objects that no longer exist, and the first
            // thing that does is a buffer release for a buffer the client already
            // destroyed. Measured against a real browser, that was the last thing
            // it was told before it died.
            (Interface::Buffer, 0) | (Interface::Region, 0) => {
                self.clients[index].objects.remove(&object);
            }
            (Interface::Callback, 0) => {
                self.clients[index].objects.remove(&object);
                for held in self.clients[index].objects.values_mut() {
                    if let Object::Surface(surface) = held {
                        surface.frame_callbacks.retain(|id| *id != object);
                    }
                }
            }
            (Interface::XdgSurface, 0) => {
                self.clients[index].objects.remove(&object);
            }
            (Interface::XdgToplevel, 2) => {
                let title = Args::new(body).string().unwrap_or_default();
                if let Some(window) = self.toplevel_window(index, object) {
                    self.windows[window].title = title.clone();
                }
                if let Some(Object::XdgToplevel { title: slot, .. }) =
                    self.clients[index].objects.get_mut(&object)
                {
                    *slot = title;
                }
            }
            (Interface::XdgToplevel, 3) => {
                let app_id = Args::new(body).string().unwrap_or_default();
                if let Some(window) = self.toplevel_window(index, object) {
                    self.windows[window].app_id = app_id.clone();
                }
                if let Some(Object::XdgToplevel { app_id: slot, .. }) =
                    self.clients[index].objects.get_mut(&object)
                {
                    *slot = app_id;
                }
            }
            (Interface::XdgToplevel, 0) => {
                self.clients[index].objects.remove(&object);
            }
            // get_text_input: the manager's only request besides its own destroy,
            // and it is the second one on the wire. Measured: guessing it at
            // opcode 0 left the client asking for an object that never existed,
            // and this compositor dropped it for asking.
            (Interface::TextInputManager, 1) => {
                let text_input = Args::new(body).new_id();
                self.insert(index, text_input, Object::TextInput { enabled: false });
            }
            (Interface::TextInputManager, 0) => {
                self.clients[index].objects.remove(&object);
            }
            (Interface::TextInput, opcode) => {
                let mut args = Args::new(body);
                match opcode {
                    0 => {
                        self.clients[index].objects.remove(&object);
                    }
                    // enable and disable
                    1 | 2 => {
                        let enabled = opcode == 1;
                        if let Some(Object::TextInput { enabled: slot }) =
                            self.clients[index].objects.get_mut(&object)
                        {
                            *slot = enabled;
                        }
                        self.text_input_commit(index, object)?;
                    }
                    // set_surrounding_text(string, uint, uint)
                    3 => {
                        let _text = args.string();
                    }
                    // commit: the point at which a client's wishes take effect.
                    7 => self.text_input_commit(index, object)?,
                    _ => {}
                }
            }
            (Interface::DataDeviceManager, 0) => {
                let source = Args::new(body).new_id();
                self.insert(index, source, Object::DataSource);
            }
            (Interface::DataDeviceManager, 1) => {
                let device = Args::new(body).new_id();
                self.insert(index, device, Object::DataDevice);
            }
            // Everything else — a region's shape, a popup's grab, a data source's
            // offers, a keyboard release — is either not this compositor's
            // business or was answered when the object was created.
            (interface, opcode) => {
                tracing::trace!("wayland: {} request {opcode} ignored", interface.name());
            }
        }
        Ok(())
    }

    /// Creates the object a `bind` asks for, and sends whatever the interface has
    /// to say at creation.
    fn bind(
        &mut self,
        index: usize,
        name: u32,
        interface: &str,
        _version: u32,
        id: u32,
    ) -> std::io::Result<()> {
        let object = match interface {
            "wl_compositor" => Object::Compositor,
            "wl_shm" => Object::Shm,
            "wl_seat" => Object::Seat,
            "wl_output" => Object::Output,
            "wl_data_device_manager" => Object::DataDeviceManager,
            "xdg_wm_base" => Object::XdgWmBase,
            "zwp_text_input_manager_v3" => Object::TextInputManager,
            other => {
                tracing::warn!("wayland: bind of unknown global {other} ({name})");
                return Ok(());
            }
        };
        self.insert(index, id, object);
        match interface {
            "wl_shm" => {
                for format in [ARGB8888, XRGB8888] {
                    self.send(index, Writer::new(id, 0).uint(format).finish())?;
                }
            }
            "wl_seat" => {
                // Pointer and keyboard, and no touch: a phone's fingers arrive as
                // a pointer, and a second input path would be a second truth.
                self.send(index, Writer::new(id, 0).uint(3).finish())?;
                self.send(index, Writer::new(id, 1).string(Some("seat0")).finish())?;
            }
            "wl_output" => {
                self.send(
                    index,
                    Writer::new(id, 0)
                        .int(0)
                        .int(0)
                        .int(300)
                        .int(170)
                        .int(0)
                        .string(Some("aixodia"))
                        .string(Some("virtual"))
                        .int(0)
                        .finish(),
                )?;
                self.send(
                    index,
                    Writer::new(id, 1)
                        .uint(1)
                        .int(self.width as i32)
                        .int(self.height as i32)
                        .int(60000)
                        .finish(),
                )?;
                self.send(index, Writer::new(id, 3).int(1).finish())?;
                self.send(index, Writer::new(id, 2).finish())?;
            }
            _ => {}
        }
        Ok(())
    }

    fn surface_request(
        &mut self,
        index: usize,
        object: u32,
        opcode: u16,
        body: &[u8],
    ) -> std::io::Result<()> {
        match opcode {
            0 => {
                self.clients[index].objects.remove(&object);
                // A surface that is gone takes its window, its popup and its
                // keyboard with it. Left behind, they are records of a window
                // that cannot be drawn and a keyboard that types into nothing —
                // and once the client reuses the id, a window that draws someone
                // else's pixels.
                self.windows.retain(|window| window.surface != object);
                self.popups.retain(|popup| popup.surface != object);
                if let Some((client, surface)) = self.pointer.over {
                    if client == index && surface == object {
                        self.pointer.over = None;
                    }
                }
                if let Some((client, surface)) = self.keyboard_focus {
                    if client == index && surface == object {
                        self.keyboard_focus = None;
                    }
                }
                self.dirty = true;
            }
            // attach(buffer, x, y)
            1 => {
                let buffer = Args::new(body).object();
                if let Some(Object::Surface(surface)) = self.clients[index].objects.get_mut(&object)
                {
                    surface.pending_buffer = if buffer == 0 { None } else { Some(buffer) };
                }
            }
            // frame(new_id)
            3 => {
                let callback = Args::new(body).new_id();
                self.insert(index, callback, Object::Callback);
                if let Some(Object::Surface(surface)) = self.clients[index].objects.get_mut(&object)
                {
                    surface.frame_callbacks.push(callback);
                }
            }
            // commit()
            6 => self.commit(index, object)?,
            // set_buffer_scale(scale)
            8 => {
                let scale = Args::new(body).int().max(1);
                if let Some(Object::Surface(surface)) = self.clients[index].objects.get_mut(&object)
                {
                    surface.scale = scale;
                }
            }
            // damage, set_opaque_region, set_input_region, set_buffer_transform,
            // damage_buffer: whole-surface recomposition needs none of them.
            _ => {}
        }
        Ok(())
    }

    /// A commit is the client saying "this is what I have now".
    fn commit(&mut self, index: usize, surface_id: u32) -> std::io::Result<()> {
        self.commits += 1;
        self.enter_output(index, surface_id)?;
        let (pending_buffer, callbacks, window) = {
            let Some(Object::Surface(surface)) = self.clients[index].objects.get(&surface_id)
            else {
                return Ok(());
            };
            (
                surface.pending_buffer,
                surface.frame_callbacks.clone(),
                surface.window,
            )
        };
        // Answer the frame callbacks first: a client waiting to draw the next
        // frame is waiting on these.
        let serial = self.serial;
        for callback in &callbacks {
            // A callback the client has thrown away is not a callback: answering
            // it sends an event to an id the client may already have given to
            // something else, which is a listener looking at a proxy of the wrong
            // type and, measured against a real browser, a SIGSEGV.
            let still_there = matches!(
                self.clients[index].objects.get(callback),
                Some(Object::Callback)
            );
            if still_there {
                self.send(index, Writer::new(*callback, 0).uint(serial).finish())?;
            }
        }
        if let Some(Object::Surface(surface)) = self.clients[index].objects.get_mut(&surface_id) {
            surface.frame_callbacks.clear();
        }

        let Some(buffer_id) = pending_buffer else {
            // A commit with no buffer is a client saying it is ready to be told
            // what size to be — the first step of every toplevel, and the only
            // step of a popup.
            if let Some(window) = window {
                self.windows[window].mapped = false;
                self.dirty = true;
            }
            let xdg_surface = self.xdg_surface_of(index, surface_id);
            if let Some(popup) = self.popups.iter().position(|p| p.surface == surface_id) {
                self.configure_popup(index, popup)?;
            } else if let Some(xdg_surface) = xdg_surface {
                if self.take_unconfigured(index, xdg_surface) {
                    self.send_configure(index, xdg_surface)?;
                }
            }
            return Ok(());
        };
        let buffer = match self.clients[index].objects.get(&buffer_id) {
            Some(Object::Buffer(buffer)) => *buffer,
            _ => {
                tracing::warn!("wayland: commit attached buffer {buffer_id} that does not exist");
                return Ok(());
            }
        };
        let (previous, scale) = {
            let Some(Object::Surface(surface)) = self.clients[index].objects.get(&surface_id)
            else {
                return Ok(());
            };
            (surface.buffer, surface.scale.max(1))
        };
        if let Some(Object::Surface(surface)) = self.clients[index].objects.get_mut(&surface_id) {
            surface.buffer = Some(buffer_id);
            surface.width = buffer.width / scale as u32;
            surface.height = buffer.height / scale as u32;
        }
        if let Some(window) = window {
            self.windows[window].width = buffer.width / scale as u32;
            self.windows[window].height = buffer.height / scale as u32;
            self.windows[window].mapped = true;
        }
        if let Some(popup) = self.popups.iter_mut().find(|p| p.surface == surface_id) {
            popup.width = buffer.width / scale as u32;
            popup.height = buffer.height / scale as u32;
            popup.mapped = true;
        }
        // The cursor is the one surface whose pixels are not part of the picture:
        // it is drawn by whoever is looking, at the pointer, and it changes to
        // say what is under the pointer.
        self.capture_cursor(index, surface_id, buffer_id);
        // The buffer that was on screen before this one is free for the client to
        // draw into again; that is the whole of Wayland's buffer recycling.
        if let Some(previous) = previous {
            // Only a buffer the client still holds may be released: an id it has
            // already destroyed is an event to an object that is not there, and a
            // client that is told about its own dead object is entitled to treat
            // it as a protocol error.
            let held = matches!(
                self.clients[index].objects.get(&previous),
                Some(Object::Buffer(_))
            );
            if previous != buffer_id && held {
                self.releases.push((index, previous));
            }
        }
        self.dirty = true;
        Ok(())
    }

    /// Places a popup and tells it where it is.
    ///
    /// The protocol wants two events: `xdg_popup.configure` with the rectangle the
    /// compositor chose, and then `xdg_surface.configure` with its serial. A
    /// popup told only the second waits for the first forever, and a browser
    /// whose address-bar dropdown never appears is a browser whose typing stalls.
    fn configure_popup(&mut self, index: usize, popup: usize) -> std::io::Result<()> {
        let (xdg_surface, surface, positioner, parent) = {
            let popup = &self.popups[popup];
            (
                popup.xdg_surface,
                popup.surface,
                popup.positioner,
                popup.parent,
            )
        };
        let asked = match self.clients[index].objects.get(&positioner) {
            Some(Object::Positioner(positioner)) => *positioner,
            _ => Positioner::default(),
        };
        // The anchor rectangle is in the parent's coordinates, so the popup lands
        // in the parent's frame: the client says "the bottom of this field", not
        // "y=812 of the screen".
        let (parent_x, parent_y) = match self.toplevel_window(index, parent) {
            Some(window) => (self.windows[window].x, self.windows[window].y),
            None => (0, 0),
        };
        let width = if asked.size.0 > 0 {
            asked.size.0 as u32
        } else {
            240
        };
        let height = if asked.size.1 > 0 {
            asked.size.1 as u32
        } else {
            240
        };
        let x = parent_x + asked.anchor.0 + asked.anchor.2 + asked.offset.0;
        let y = parent_y + asked.anchor.1 + asked.anchor.3 + asked.offset.1;
        {
            let popup = &mut self.popups[popup];
            popup.x = x;
            popup.y = y;
            // A zero size in the configure means "whatever the client draws".
            if asked.size.0 > 0 {
                popup.width = width;
            }
            if asked.size.1 > 0 {
                popup.height = height;
            }
        }
        let _ = surface;
        self.take_unconfigured(index, xdg_surface);
        let xdg_popup = self.popups[popup].object;
        self.send(
            index,
            Writer::new(xdg_popup, 0)
                .int(x)
                .int(y)
                .int(width as i32)
                .int(height as i32)
                .finish(),
        )?;
        self.send_configure(index, xdg_surface)?;
        self.configures += 1;
        self.dirty = true;
        Ok(())
    }

    /// Tells a surface the size and state it has been given.
    fn send_configure(&mut self, index: usize, xdg_surface: u32) -> std::io::Result<()> {
        self.configures += 1;
        let serial = self.next_serial();
        self.send(index, Writer::new(xdg_surface, 0).uint(serial).finish())?;
        let toplevel = self.clients[index]
            .objects
            .iter()
            .find(|(_, object)| {
                matches!(object, Object::XdgToplevel { xdg_surface: s, .. } if *s == xdg_surface)
            })
            .map(|(id, _)| *id);
        if let Some(toplevel) = toplevel {
            // 0x0 is "you choose the size"; state 4 is "activated", which is what
            // makes a client draw its window as focused.
            let mut states = Writer::new(toplevel, 0).int(0).int(0);
            states = states.uint(4).uint(1); // array of one uint: 4 = activated
            for byte in [0u8, 0, 0, 0] {
                states = states.uint(byte as u32);
            }
            self.send(index, states.finish())?;
        }
        Ok(())
    }

    /// Moves the keyboard's focus and tells the keyboards on both sides of the
    /// change. Without `wl_keyboard.enter` a client never believes it is
    /// focused, and ignores every key: Chrome takes the keystrokes only after the
    /// surface it drew has been entered.
    fn set_keyboard_focus(&mut self, next: Option<(usize, u32)>) -> std::io::Result<()> {
        let previous = self.keyboard_focus;
        self.keyboard_focus = next;
        if previous == next {
            return Ok(());
        }
        if let Some((client, surface)) = previous {
            if let Some(keyboard) = self.keyboard_id(client) {
                let serial = self.next_serial();
                self.send(
                    client,
                    Writer::new(keyboard, 2)
                        .uint(serial)
                        .object(surface)
                        .finish(),
                )?;
            }
        }
        if let Some((client, surface)) = next {
            if let Some(keyboard) = self.keyboard_id(client) {
                let serial = self.next_serial();
                self.send(
                    client,
                    Writer::new(keyboard, 1)
                        .uint(serial)
                        .object(surface)
                        .array(&[])
                        .finish(),
                )?;
            }
        }
        Ok(())
    }

    fn send_keymap(&mut self, index: usize, keyboard: u32) -> std::io::Result<()> {
        if self.keymap.is_none() {
            let (file, size) = keymap::write()?;
            self.keymap_size = size;
            self.keymap = Some(file);
        }
        let fd = match &self.keymap {
            Some(file) => {
                use std::os::unix::io::AsRawFd;
                file.as_raw_fd()
            }
            None => return Ok(()),
        };
        self.send(
            index,
            Writer::new(keyboard, 0)
                .uint(1)
                .fd(fd)
                .uint(self.keymap_size)
                .finish(),
        )?;
        self.send(index, Writer::new(keyboard, 5).int(25).int(250).finish())
    }

    fn insert(&mut self, index: usize, id: u32, object: Object) {
        self.clients[index].objects.insert(id, object);
    }

    fn send(&mut self, index: usize, writer: Writer) -> std::io::Result<()> {
        tracing::trace!(
            "wayland: -> client {index} {} {:?}",
            writer.object_id(),
            writer.op()
        );
        self.clients[index].client.send(writer)
    }

    /// Tells a surface, once, which output it is drawn on.
    ///
    /// The protocol has this as `wl_surface.enter`, and a client uses it to know
    /// the screen's scale and refresh. A surface that has never been told is a
    /// surface with no output at all — measured against a real browser, the
    /// surface it made just before dying was exactly such a surface.
    fn enter_output(&mut self, index: usize, surface: u32) -> std::io::Result<()> {
        let entered = match self.clients[index].objects.get(&surface) {
            Some(Object::Surface(surface)) => surface.entered,
            _ => return Ok(()),
        };
        if entered {
            return Ok(());
        }
        let output = self.clients[index]
            .objects
            .iter()
            .find_map(|(id, object)| match object {
                Object::Output => Some(*id),
                _ => None,
            });
        if let Some(output) = output {
            self.send(index, Writer::new(surface, 0).object(output).finish())?;
        }
        if let Some(Object::Surface(surface)) = self.clients[index].objects.get_mut(&surface) {
            surface.entered = true;
        }
        Ok(())
    }

    /// The `xdg_surface` that wraps a `wl_surface`, if it has been made yet.
    fn xdg_surface_of(&self, index: usize, surface: u32) -> Option<u32> {
        self.clients[index]
            .objects
            .iter()
            .find_map(|(id, object)| match object {
                Object::XdgSurface { surface: s, .. } if *s == surface => Some(*id),
                _ => None,
            })
    }

    /// True the first time it is asked, so one commit is one configure.
    fn take_unconfigured(&mut self, index: usize, xdg_surface: u32) -> bool {
        match self.clients[index].objects.get_mut(&xdg_surface) {
            Some(Object::XdgSurface { configured, .. }) if !*configured => {
                *configured = true;
                true
            }
            _ => false,
        }
    }

    fn window_of(&self, index: usize, xdg_surface: u32) -> Option<usize> {
        match self.clients.get(index)?.objects.get(&xdg_surface) {
            Some(Object::XdgSurface { window, .. }) => *window,
            _ => None,
        }
    }

    fn toplevel_window(&self, index: usize, toplevel: u32) -> Option<usize> {
        match self.clients.get(index)?.objects.get(&toplevel) {
            Some(Object::XdgToplevel { xdg_surface, .. }) => self.window_of(index, *xdg_surface),
            _ => None,
        }
    }

    /// Gives every toplevel surface a window record, and carries a title or an
    /// app id across to it.
    pub fn reconcile(&mut self) {
        for index in 0..self.clients.len() {
            let toplevels: Vec<(u32, String, String)> = self.clients[index]
                .objects
                .values()
                .filter_map(|object| match object {
                    Object::XdgToplevel {
                        xdg_surface,
                        title,
                        app_id,
                    } => Some((*xdg_surface, title.clone(), app_id.clone())),
                    _ => None,
                })
                .collect();
            for (xdg_surface, title, app_id) in toplevels {
                let surface = match self.clients[index].objects.get(&xdg_surface) {
                    Some(Object::XdgSurface { surface, .. }) => *surface,
                    _ => continue,
                };
                match self.window_of(index, xdg_surface) {
                    Some(window) => {
                        self.windows[window].title = title;
                        self.windows[window].app_id = app_id;
                    }
                    None => {
                        // Placed a little off from the previous one, so two windows
                        // are two windows and not one.
                        let offset = (self.windows.len() as i32 % 6) * 24;
                        self.windows.push(Window {
                            client: index,
                            surface,
                            x: 40 + offset,
                            y: 40 + offset,
                            width: 800,
                            height: 560,
                            title,
                            app_id,
                            mapped: false,
                        });
                        let window = self.windows.len() - 1;
                        if let Some(Object::XdgSurface { window: slot, .. }) =
                            self.clients[index].objects.get_mut(&xdg_surface)
                        {
                            *slot = Some(window);
                        }
                        if let Some(Object::Surface(surface)) =
                            self.clients[index].objects.get_mut(&surface)
                        {
                            surface.window = Some(window);
                        }
                        self.dirty = true;
                    }
                }
            }
        }
    }

    /// Tells an input method what it needs to know and nothing more.
    ///
    /// The protocol wants `enter` and then a `done` with a serial, once per
    /// commit; a client waiting for `done` never shows its preedit or its caret.
    /// Nothing is committed back: characters arrive as keys.
    fn text_input_commit(&mut self, index: usize, text_input: u32) -> std::io::Result<()> {
        let enabled = matches!(
            self.clients[index].objects.get(&text_input),
            Some(Object::TextInput { enabled: true })
        );
        let focus = self
            .keyboard_focus
            .filter(|(client, _)| *client == index)
            .map(|(_, surface)| surface);
        // Both events name the surface: text-input-unstable-v3's `leave(o)` takes
        // one, unlike v1's. Measured by libwayland's own words, "message too short,
        // object (18), message leave(o)" — a client told with an empty event reads
        // the next message's bytes as a surface.
        match (enabled, focus) {
            (true, Some(surface)) => {
                self.send(index, Writer::new(text_input, 0).object(surface).finish())?;
            }
            (_, Some(surface)) => {
                self.send(index, Writer::new(text_input, 1).object(surface).finish())?;
            }
            (_, None) => {}
        }
        let serial = self.next_serial();
        self.send(index, Writer::new(text_input, 5).uint(serial).finish())?;
        Ok(())
    }

    /// Releases the buffers freed this turn, now that the client's own requests
    /// for the turn have been read.
    pub fn release_buffers(&mut self) -> std::io::Result<()> {
        let queued = std::mem::take(&mut self.releases);
        for (index, buffer) in queued {
            let held = matches!(
                self.clients.get(index).and_then(|c| c.objects.get(&buffer)),
                Some(Object::Buffer(_))
            );
            if held {
                self.send(index, Writer::new(buffer, 0).finish())?;
            }
        }
        Ok(())
    }

    /// Where every window is, and whether it is showing — a cheap fingerprint of
    /// what is on the screen for a pointer that is standing still.
    pub fn arrangement(&self) -> Vec<(i32, i32, u32, u32, bool)> {
        self.windows
            .iter()
            .map(|w| (w.x, w.y, w.width, w.height, w.mapped))
            .chain(
                self.popups
                    .iter()
                    .map(|p| (p.x, p.y, p.width, p.height, p.mapped)),
            )
            .collect()
    }

    /// Reads the pixels of a surface the pointer has named as its cursor.
    ///
    /// A cursor surface is an ordinary surface with an ordinary buffer, so this
    /// is the same read a window gets — kept separate because a cursor is one
    /// small image rather than part of the frame, and because nothing draws it
    /// here: the caller that has a pointer draws it.
    fn capture_cursor(&mut self, index: usize, surface_id: u32, buffer_id: u32) {
        let Some(cursor) = self.pointer.cursor else {
            return;
        };
        if cursor.client != index || cursor.surface != surface_id {
            return;
        }
        let Some(Object::Buffer(buffer)) = self.clients[index].objects.get(&buffer_id) else {
            return;
        };
        let Some(Object::ShmPool(pool)) = self.clients[index].objects.get(&buffer.pool) else {
            return;
        };
        let argb = buffer.pixels(pool);
        if argb.is_empty() {
            return;
        }
        let (width, height) = (buffer.width, buffer.height);
        // A serial that only moves when the picture does: two identical cursors
        // in a row are one cursor, and a viewer that treats them as two redraws
        // a pointer nobody moved.
        let unchanged = self
            .cursor_image
            .as_ref()
            .is_some_and(|image| image.argb == argb && image.width == width);
        let serial = if unchanged {
            self.cursor_image.as_ref().map(|i| i.serial).unwrap_or(0)
        } else {
            u64::from(self.next_serial())
        };
        self.cursor_image = Some(CursorImage {
            serial,
            width,
            height,
            hot_x: cursor.hot_x.max(0) as u32,
            hot_y: cursor.hot_y.max(0) as u32,
            argb,
        });
    }

    /// Draws every mapped window into the frame, bottom first.
    pub fn compose(&mut self) {
        self.frame.fill(0xff10_1a2b);
        for index in 0..self.windows.len() {
            let window = self.windows[index].clone();
            if !window.mapped {
                continue;
            }
            let Some(client) = self.clients.get(window.client) else {
                continue;
            };
            let Some(Object::Surface(surface)) = client.objects.get(&window.surface) else {
                continue;
            };
            let Some(buffer_id) = surface.buffer else {
                continue;
            };
            let Some(Object::Buffer(buffer)) = client.objects.get(&buffer_id) else {
                continue;
            };
            let Some(Object::ShmPool(pool)) = client.objects.get(&buffer.pool) else {
                continue;
            };
            buffer.blit(
                pool,
                None,
                &mut self.frame,
                self.width,
                self.height,
                window.x,
                window.y,
            );
        }
        for index in 0..self.popups.len() {
            let popup = self.popups[index].clone();
            if !popup.mapped {
                continue;
            }
            let Some(client) = self.clients.get(popup.client) else {
                continue;
            };
            let Some(Object::Surface(surface)) = client.objects.get(&popup.surface) else {
                continue;
            };
            let Some(buffer_id) = surface.buffer else {
                continue;
            };
            let Some(Object::Buffer(buffer)) = client.objects.get(&buffer_id) else {
                continue;
            };
            let Some(Object::ShmPool(pool)) = client.objects.get(&buffer.pool) else {
                continue;
            };
            buffer.blit(
                pool,
                None,
                &mut self.frame,
                self.width,
                self.height,
                popup.x,
                popup.y,
            );
        }
        self.dirty = false;
    }

    /// The topmost mapped window whose rectangle contains a point.
    pub fn window_at(&self, x: f64, y: f64) -> Option<(usize, u32)> {
        // Popups are above every window, so they are hit first.
        for popup in self.popups.iter().rev() {
            if !popup.mapped {
                continue;
            }
            if x >= popup.x as f64
                && y >= popup.y as f64
                && x < (popup.x + popup.width as i32) as f64
                && y < (popup.y + popup.height as i32) as f64
            {
                return Some((popup.client, popup.surface));
            }
        }
        for index in (0..self.windows.len()).rev() {
            let window = &self.windows[index];
            if !window.mapped {
                continue;
            }
            if x >= window.x as f64
                && y >= window.y as f64
                && x < (window.x + window.width as i32) as f64
                && y < (window.y + window.height as i32) as f64
            {
                return Some((window.client, window.surface));
            }
        }
        None
    }

    /// Moves the pointer and tells whichever surface it is over.
    pub fn pointer_motion(&mut self, x: f64, y: f64) -> std::io::Result<()> {
        self.pointer.x = x.clamp(0.0, self.width.saturating_sub(1) as f64);
        self.pointer.y = y.clamp(0.0, self.height.saturating_sub(1) as f64);
        let over = self.window_at(self.pointer.x, self.pointer.y);
        if over != self.pointer.over {
            if let Some((client, surface)) = self.pointer.over {
                if let Some(pointer) = self.pointer_id(client) {
                    let serial = self.next_serial();
                    self.send(
                        client,
                        Writer::new(pointer, 1)
                            .uint(serial)
                            .object(surface)
                            .finish(),
                    )?;
                }
            }
            self.pointer.over = over;
            if let Some((client, surface)) = over {
                if let Some(pointer) = self.pointer_id(client) {
                    let serial = self.next_serial();
                    let (px, py) = (self.pointer.x, self.pointer.y);
                    self.send(
                        client,
                        Writer::new(pointer, 0)
                            .uint(serial)
                            .object(surface)
                            .fixed(px)
                            .fixed(py)
                            .finish(),
                    )?;
                }
            }
            // The keyboard changes hands wherever the pointer goes, including
            // nowhere: leaving a window for the desktop is a leave for the
            // keyboard as well, or it types into a window nobody is looking at.
            self.set_keyboard_focus(over)?;
        }
        if let Some((client, _surface)) = over {
            if let Some(pointer) = self.pointer_id(client) {
                let (px, py) = (self.pointer.x, self.pointer.y);
                self.send(
                    client,
                    Writer::new(pointer, 2).uint(0).fixed(px).fixed(py).finish(),
                )?;
                self.send(client, Writer::new(pointer, 5).finish())?;
            }
        }
        Ok(())
    }

    /// A button, at the pointer's position.
    pub fn pointer_button(&mut self, button: u32, pressed: bool) -> std::io::Result<()> {
        let Some((client, _surface)) = self.pointer.over else {
            return Ok(());
        };
        let Some(pointer) = self.pointer_id(client) else {
            return Ok(());
        };
        let serial = self.next_serial();
        self.send(
            client,
            Writer::new(pointer, 3)
                .uint(serial)
                .uint(0)
                .uint(button)
                .uint(if pressed { 1 } else { 0 })
                .finish(),
        )?;
        self.send(client, Writer::new(pointer, 5).finish())
    }

    /// A scroll, in Wayland's own units: ten to a notch.
    pub fn pointer_axis(&mut self, notches: f64) -> std::io::Result<()> {
        self.pointer_axis_along(notches, true)
    }

    /// A scroll along one axis: the vertical one for a wheel, the horizontal one
    /// for a sideways two-finger drag.
    pub fn pointer_axis_along(&mut self, notches: f64, vertical: bool) -> std::io::Result<()> {
        let Some((client, _surface)) = self.pointer.over else {
            return Ok(());
        };
        let Some(pointer) = self.pointer_id(client) else {
            return Ok(());
        };
        self.send(
            client,
            Writer::new(pointer, 4)
                .uint(0)
                .uint(if vertical { 0 } else { 1 })
                .fixed(notches * 10.0)
                .finish(),
        )?;
        self.send(client, Writer::new(pointer, 5).finish())
    }

    /// A key, by evdev code, which is what Wayland's key event carries.
    pub fn key(&mut self, evdev: u32, pressed: bool) -> std::io::Result<()> {
        self.set_modifier(evdev, pressed);
        let Some((client, _surface)) = self.keyboard_focus else {
            return Ok(());
        };
        let Some(keyboard) = self.keyboard_id(client) else {
            return Ok(());
        };
        let serial = self.next_serial();
        self.send(
            client,
            Writer::new(keyboard, 3)
                .uint(serial)
                .uint(self.now_ms())
                .uint(evdev)
                .uint(if pressed { 1 } else { 0 })
                .finish(),
        )?;
        // The depressed-modifier word after every key: a client that missed one
        // event then still knows what is held, and shift a moment late is a
        // capital typed as a small letter.
        let serial = self.next_serial();
        self.send(
            client,
            Writer::new(keyboard, 4)
                .uint(serial)
                .uint(self.mods)
                .uint(0)
                .uint(0)
                .uint(0)
                .finish(),
        )
    }

    /// Tracks the eight modifier bits xkb's evdev keycodes produce. Only the ones
    /// our keymap binds are needed: shift, caps lock, control, alt and logo.
    fn set_modifier(&mut self, evdev: u32, pressed: bool) {
        let bit = match evdev {
            42 | 54 => 1 << 0,   // shift
            58 => 1 << 1,        // caps lock, a lock rather than a hold
            29 | 97 => 1 << 2,   // control
            56 | 100 => 1 << 3,  // alt
            125 | 126 => 1 << 6, // logo
            _ => return,
        };
        if pressed {
            self.mods |= bit;
        } else {
            self.mods &= !bit;
        }
    }

    fn pointer_id(&self, index: usize) -> Option<u32> {
        self.clients[index]
            .objects
            .iter()
            .find(|(_, object)| matches!(object, Object::Pointer))
            .map(|(id, _)| *id)
    }

    fn keyboard_id(&self, index: usize) -> Option<u32> {
        self.clients[index]
            .objects
            .iter()
            .find(|(_, object)| matches!(object, Object::Keyboard))
            .map(|(id, _)| *id)
    }

    /// Where the pointer is and what it is over, for a caller that draws a cursor
    /// or reports a hit.
    pub fn pointer_position(&self) -> (f64, f64) {
        (self.pointer.x, self.pointer.y)
    }
}
