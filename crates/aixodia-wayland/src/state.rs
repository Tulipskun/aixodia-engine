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
    Positioner,
    XdgSurface {
        surface: u32,
        window: Option<usize>,
    },
    XdgToplevel {
        xdg_surface: u32,
        title: String,
        app_id: String,
    },
    XdgPopup {
        xdg_surface: u32,
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
            Self::Positioner => Interface::Positioner,
            Self::XdgSurface { .. } => Interface::XdgSurface,
            Self::XdgToplevel { .. } => Interface::XdgToplevel,
            Self::XdgPopup { .. } => Interface::XdgPopup,
        }
    }
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
    pub pointer: Pointer,
    /// Set by anything that must be recomposited before the next frame.
    pub dirty: bool,
    /// The keyboard's focus, and the client that owns it.
    pub focus: Option<(usize, u32)>,
    keymap: Option<std::fs::File>,
    keymap_size: u32,
    /// Counters for the log: a compositor that is asked for buffers and never
    /// commits one is a compositor whose client is stuck, and the log should say.
    pub commits: u64,
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
            pointer: Pointer::default(),
            dirty: true,
            focus: None,
            keymap: None,
            keymap_size: 0,
            commits: 0,
            configures: 0,
        }
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
                        "wayland: buffer {id} needs {} bytes at {offset} of a {pool_size}-byte pool",
                        buffer.required_bytes()
                    );
                } else {
                    self.insert(index, id, Object::Buffer(buffer));
                }
            }
            (Interface::Surface, opcode) => self.surface_request(index, object, opcode, body)?,
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
                    },
                );
                self.send_configure(index, xdg_surface)?;
            }
            (Interface::XdgWmBase, 1) => {
                // xdg_positioner: popups are placed where the client asks, so the
                // positioner is only the token the protocol requires.
                let positioner = Args::new(body).new_id();
                self.insert(index, positioner, Object::Positioner);
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
                let popup = Args::new(body).new_id();
                self.insert(
                    index,
                    popup,
                    Object::XdgPopup {
                        xdg_surface: object,
                    },
                );
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
                if let Some((client, surface)) = self.pointer.over {
                    if client == index && surface == object {
                        self.pointer.over = None;
                    }
                }
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
            self.send(index, Writer::new(*callback, 0).uint(serial).finish())?;
        }
        if let Some(Object::Surface(surface)) = self.clients[index].objects.get_mut(&surface_id) {
            surface.frame_callbacks.clear();
        }

        let Some(buffer_id) = pending_buffer else {
            // A commit with no buffer is a client saying it is ready to be told
            // what size to be — the first step of every toplevel.
            if let Some(window) = window {
                self.windows[window].mapped = false;
                self.dirty = true;
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
        // The buffer that was on screen before this one is free for the client to
        // draw into again; that is the whole of Wayland's buffer recycling.
        if let Some(previous) = previous {
            if previous != buffer_id {
                self.send(index, Writer::new(previous, 0).finish())?;
            }
        }
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
        self.clients[index].client.send(writer)
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
        self.dirty = false;
    }

    /// The topmost mapped window whose rectangle contains a point.
    pub fn window_at(&self, x: f64, y: f64) -> Option<(usize, u32)> {
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
                self.focus = over;
            }
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
                .uint(0)
                .fixed(notches * 10.0)
                .finish(),
        )?;
        self.send(client, Writer::new(pointer, 5).finish())
    }

    /// A key, by evdev code, which is what Wayland's key event carries.
    pub fn key(&mut self, evdev: u32, pressed: bool) -> std::io::Result<()> {
        let Some((client, _surface)) = self.focus.or(self.pointer.over) else {
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
                .uint(0)
                .uint(evdev)
                .uint(if pressed { 1 } else { 0 })
                .finish(),
        )
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
