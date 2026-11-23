//! The server side of Wayland: who is connected, which objects they hold, and
//! what each opcode means.
//!
//! Wayland names its interfaces and their opcodes in XML files. This module is
//! the part of that contract this compositor implements, written out as Rust:
//! every opcode arrives as a number on an object, so the object's interface plus
//! the opcode is the whole dispatch.
//!
//! The first milestone is deliberately small — a display, a registry and its
//! callbacks — because that is the part every client starts with, and a client
//! that cannot get past `wl_display.sync` never draws anything at all.

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::protocol::{self, Args, Writer};
use crate::sys;

/// The id the protocol reserves for `wl_display` on every connection.
pub const DISPLAY_ID: u32 = 1;

/// A global this compositor advertises: what a client can `bind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Global {
    pub name: u32,
    pub interface: &'static str,
    pub version: u32,
}

/// What a client can bind, and the versions this compositor really implements.
pub const GLOBALS: &[Global] = &[
    Global {
        name: 1,
        interface: "wl_compositor",
        version: 4,
    },
    Global {
        name: 2,
        interface: "wl_shm",
        version: 1,
    },
    Global {
        name: 3,
        interface: "wl_seat",
        version: 5,
    },
    Global {
        name: 4,
        interface: "wl_output",
        version: 3,
    },
    Global {
        name: 5,
        interface: "wl_data_device_manager",
        version: 3,
    },
    Global {
        name: 6,
        interface: "xdg_wm_base",
        version: 2,
    },
];

/// What an object id means on this connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interface {
    Display,
    Registry,
    Callback,
    Compositor,
    Surface,
    Region,
    Shm,
    ShmPool,
    Buffer,
    Seat,
    Pointer,
    Keyboard,
    Output,
    DataDeviceManager,
    XdgWmBase,
    XdgSurface,
    XdgToplevel,
}

impl Interface {
    /// The name Wayland gives the interface, used in logs and in `bind`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Display => "wl_display",
            Self::Registry => "wl_registry",
            Self::Callback => "wl_callback",
            Self::Compositor => "wl_compositor",
            Self::Surface => "wl_surface",
            Self::Region => "wl_region",
            Self::Shm => "wl_shm",
            Self::ShmPool => "wl_shm_pool",
            Self::Buffer => "wl_buffer",
            Self::Seat => "wl_seat",
            Self::Pointer => "wl_pointer",
            Self::Keyboard => "wl_keyboard",
            Self::Output => "wl_output",
            Self::DataDeviceManager => "wl_data_device_manager",
            Self::XdgWmBase => "xdg_wm_base",
            Self::XdgSurface => "xdg_surface",
            Self::XdgToplevel => "xdg_toplevel",
        }
    }

    /// Whether the interface is a global a client binds by name.
    pub fn from_global(name: &str) -> Option<Self> {
        Some(match name {
            "wl_compositor" => Self::Compositor,
            "wl_shm" => Self::Shm,
            "wl_seat" => Self::Seat,
            "wl_output" => Self::Output,
            "wl_data_device_manager" => Self::DataDeviceManager,
            "xdg_wm_base" => Self::XdgWmBase,
            _ => return None,
        })
    }
}

/// One connected client: its socket, its objects, and the bytes it has sent so
/// far that do not yet make a message.
pub struct Client {
    stream: UnixStream,
    objects: HashMap<u32, Interface>,
    pending: Vec<u8>,
    fds: Vec<RawFd>,
}

impl Client {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(false)?;
        let mut objects = HashMap::new();
        objects.insert(DISPLAY_ID, Interface::Display);
        Ok(Self {
            stream,
            objects,
            pending: Vec::with_capacity(4096),
            fds: Vec::new(),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// The interface behind an id, for a caller that wants to know what a client
    /// holds.
    pub fn interface_of(&self, id: u32) -> Option<Interface> {
        self.objects.get(&id).copied()
    }

    /// Reads what the socket has and handles every whole message in it.
    ///
    /// Returns how many messages were handled, so an event loop can tell "the
    /// client is quiet" from "the client spoke".
    pub fn read_and_handle(&mut self, serial: &mut u32) -> io::Result<usize> {
        let (bytes, fds) = sys::recv(self.fd(), 65536)?;
        if bytes.is_empty() && fds.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the client closed the connection",
            ));
        }
        self.pending.extend_from_slice(&bytes);
        self.fds.extend(fds);
        self.handle_pending(serial)
    }

    /// Handles bytes a caller already has, which is what the tests use and what a
    /// future in-process client would use.
    pub fn handle_bytes(&mut self, bytes: &[u8], serial: &mut u32) -> io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        self.handle_pending(serial)
    }

    /// Handles every whole message already buffered.
    ///
    /// A message's body is copied out before dispatch because dispatch writes to
    /// the same client: a borrow of the buffer and a call that appends to it
    /// cannot both be alive.
    fn handle_pending(&mut self, serial: &mut u32) -> io::Result<usize> {
        let mut handled = 0;
        loop {
            let step = match protocol::take(&self.pending) {
                Ok(Some((message, used))) => {
                    Some((message.object, message.opcode, message.body.to_vec(), used))
                }
                Ok(None) => None,
                Err(err) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
                }
            };
            let Some((object, opcode, body, used)) = step else {
                break;
            };
            self.pending.drain(..used);
            self.dispatch(object, opcode, &body, serial)?;
            handled += 1;
        }
        Ok(handled)
    }

    fn dispatch(
        &mut self,
        object: u32,
        opcode: u16,
        body: &[u8],
        serial: &mut u32,
    ) -> io::Result<()> {
        let interface = match self.objects.get(&object) {
            Some(interface) => *interface,
            None => {
                // An id this connection never created. Wayland requires an error
                // event and a close here; the connection is not trusted after it.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("request on unknown object {object}"),
                ));
            }
        };
        match (interface, opcode) {
            // wl_display.sync(new_id)
            (Interface::Display, 0) => {
                let callback = Args::new(body).new_id();
                self.insert(callback, Interface::Callback);
                *serial += 1;
                self.send(Writer::new(callback, 0).uint(*serial).finish())?;
            }
            // wl_display.get_registry(new_id)
            (Interface::Display, 1) => {
                let registry = Args::new(body).new_id();
                self.insert(registry, Interface::Registry);
                for global in GLOBALS {
                    self.send(
                        Writer::new(registry, 0)
                            .uint(global.name)
                            .string(Some(global.interface))
                            .uint(global.version)
                            .finish(),
                    )?;
                }
            }
            // wl_registry.bind(name, interface, version, new_id)
            (Interface::Registry, 0) => {
                let mut args = Args::new(body);
                let name = args.uint();
                let interface = args.string().unwrap_or_default();
                let version = args.uint();
                let id = args.new_id();
                match Interface::from_global(&interface) {
                    Some(bound) => {
                        tracing::debug!(
                            "wayland: client bound {interface} v{version} (global {name}) as {id}"
                        );
                        self.insert(id, bound);
                    }
                    None => {
                        tracing::warn!("wayland: client asked for unknown global {interface}");
                    }
                }
            }
            // wl_callback is a one-shot object with no requests.
            (Interface::Callback, opcode) => {
                tracing::debug!("wayland: ignored callback request {opcode}");
            }
            (interface, opcode) => {
                tracing::debug!(
                    "wayland: {} request {opcode} is not implemented yet",
                    interface.name()
                );
            }
        }
        Ok(())
    }

    fn insert(&mut self, id: u32, interface: Interface) {
        self.objects.insert(id, interface);
    }

    fn send(&mut self, writer: Writer) -> io::Result<()> {
        let bytes = writer.bytes();
        let sent = sys::send(self.fd(), bytes, &writer.fds)?;
        if sent != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "the socket took only part of a message",
            ));
        }
        Ok(())
    }

    /// Takes the fds this client has sent, consumed in order by pool creation.
    pub fn take_fds(&mut self) -> Vec<RawFd> {
        std::mem::take(&mut self.fds)
    }

    /// Reads anything the client has sent without blocking, for an event loop
    /// that polls first.
    pub fn drain(&mut self) -> io::Result<()> {
        let mut sink = Vec::new();
        match self.stream.read_to_end(&mut sink) {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(err) => Err(err),
        }
    }
}

/// Where clients connect: a directory and a socket name, which is what
/// `WAYLAND_DISPLAY` names.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    pub serial: u32,
}

impl Server {
    /// Binds `dir/name` and refuses to start if something is already listening
    /// there: a compositor that steals another's socket is a compositor whose
    /// clients are talking to the wrong program.
    pub fn bind(dir: &Path, name: &str) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(name);
        if path.exists() {
            // A socket nobody is listening on is stale — a crashed predecessor.
            match UnixStream::connect(&path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("{} is already in use", path.display()),
                    ))
                }
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        let listener = UnixListener::bind(&path)?;
        tracing::info!("wayland: listening on {}", path.display());
        Ok(Self {
            listener,
            path,
            serial: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accepts one client. Blocking; an event loop polls the listener first.
    pub fn accept(&mut self) -> io::Result<Client> {
        let (stream, _) = self.listener.accept()?;
        tracing::info!("wayland: client connected");
        Client::new(stream)
    }

    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// A client and a compositor on the two ends of one socket pair, so the
    /// tests speak the real wire format instead of calling functions.
    fn pair() -> (Client, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        (Client::new(ours).unwrap(), theirs)
    }

    /// Reads one message from the client side of the pair.
    fn next_event(theirs: &mut UnixStream) -> (u32, u16, Vec<u8>) {
        let mut header = [0u8; 8];
        theirs.read_exact(&mut header).unwrap();
        let object = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let size = u16::from_le_bytes([header[4], header[5]]) as usize;
        let opcode = u16::from_le_bytes([header[6], header[7]]);
        let mut body = vec![0u8; size - 8];
        if !body.is_empty() {
            theirs.read_exact(&mut body).unwrap();
        }
        (object, opcode, body)
    }

    #[test]
    fn a_sync_is_answered_with_a_done_callback() {
        let (mut client, mut theirs) = pair();
        let mut serial = 0;
        // wl_display.sync(new_id 2)
        client
            .handle_bytes(
                &Writer::new(DISPLAY_ID, 0).uint(2).finish().bytes(),
                &mut serial,
            )
            .unwrap();
        let (object, opcode, body) = next_event(&mut theirs);
        assert_eq!(object, 2, "the callback is the id the client asked for");
        assert_eq!(opcode, 0, "wl_callback.done");
        let mut args = Args::new(&body);
        assert_eq!(args.uint(), 1, "the first serial");
        assert_eq!(client.interface_of(2), Some(Interface::Callback));
    }

    #[test]
    fn asking_for_the_registry_answers_with_every_global() {
        let (mut client, mut theirs) = pair();
        let mut serial = 0;
        client
            .handle_bytes(
                &Writer::new(DISPLAY_ID, 1).uint(3).finish().bytes(),
                &mut serial,
            )
            .unwrap();
        let mut seen = Vec::new();
        for _ in 0..GLOBALS.len() {
            let (object, opcode, body) = next_event(&mut theirs);
            assert_eq!(object, 3);
            assert_eq!(opcode, 0, "wl_registry.global");
            let mut args = Args::new(&body);
            let name = args.uint();
            let interface = args.string().unwrap_or_default();
            let version = args.uint();
            seen.push((name, interface, version));
        }
        assert!(seen
            .iter()
            .any(|(_, interface, _)| interface == "wl_compositor"));
        assert!(seen.iter().any(|(_, interface, _)| interface == "wl_shm"));
        assert!(seen
            .iter()
            .any(|(_, interface, _)| interface == "xdg_wm_base"));
    }

    #[test]
    fn binding_a_global_makes_the_new_id_mean_that_interface() {
        let (mut client, mut theirs) = pair();
        let mut serial = 0;
        client
            .handle_bytes(
                &Writer::new(DISPLAY_ID, 1).uint(3).finish().bytes(),
                &mut serial,
            )
            .unwrap();
        for _ in 0..GLOBALS.len() {
            next_event(&mut theirs);
        }
        let bind = Writer::new(3, 0)
            .uint(1)
            .string(Some("wl_compositor"))
            .uint(4)
            .uint(9)
            .finish();
        client.handle_bytes(bind.bytes(), &mut serial).unwrap();
        assert_eq!(client.interface_of(9), Some(Interface::Compositor));
    }

    #[test]
    fn a_request_on_an_id_that_was_never_created_is_refused() {
        let (mut client, _theirs) = pair();
        let mut serial = 0;
        let err = client
            .handle_bytes(&Writer::new(77, 0).uint(1).finish().bytes(), &mut serial)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_message_split_across_two_reads_is_handled_once_it_is_whole() {
        let (mut client, mut theirs) = pair();
        let mut serial = 0;
        let message = Writer::new(DISPLAY_ID, 0).uint(2).finish();
        let bytes = message.bytes();
        assert_eq!(client.handle_bytes(&bytes[..5], &mut serial).unwrap(), 0);
        assert_eq!(client.handle_bytes(&bytes[5..], &mut serial).unwrap(), 1);
        let (object, opcode, _) = next_event(&mut theirs);
        assert_eq!((object, opcode), (2, 0));
    }
}
