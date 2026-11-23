//! The connection itself: a socket, a buffer of bytes that are not yet a
//! message, and the fds that arrived with them.
//!
//! This module owns no protocol opinions. `state` decides what each opcode
//! means; here a message is just an id, an opcode and a body.

use std::collections::VecDeque;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::protocol::{self, Writer};
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

/// The interface names Wayland gives its objects, for logging and dispatch.
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
    Touch,
    Output,
    DataDevice,
    DataSource,
    DataDeviceManager,
    XdgWmBase,
    Positioner,
    XdgSurface,
    XdgToplevel,
    XdgPopup,
}

impl Interface {
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
            Self::Touch => "wl_touch",
            Self::Output => "wl_output",
            Self::DataDevice => "wl_data_device",
            Self::DataSource => "wl_data_source",
            Self::DataDeviceManager => "wl_data_device_manager",
            Self::XdgWmBase => "xdg_wm_base",
            Self::Positioner => "xdg_positioner",
            Self::XdgSurface => "xdg_surface",
            Self::XdgToplevel => "xdg_toplevel",
            Self::XdgPopup => "xdg_popup",
        }
    }
}

/// One connected client and the bytes it has sent.
pub struct Client {
    stream: UnixStream,
    pending: Vec<u8>,
    fds: VecDeque<RawFd>,
}

impl Client {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        Ok(Self {
            stream,
            pending: Vec::with_capacity(8192),
            fds: VecDeque::new(),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Reads from the socket, if anything is there.
    ///
    /// Returns false at end of file, which an event loop treats as a client that
    /// has gone.
    pub fn fill(&mut self) -> io::Result<bool> {
        let (bytes, fds) = match sys::recv(self.fd(), 65536) {
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(true),
            Err(err) => return Err(err),
        };
        if bytes.is_empty() && fds.is_empty() {
            return Ok(false);
        }
        self.pending.extend_from_slice(&bytes);
        self.fds.extend(fds);
        Ok(true)
    }

    /// The next whole message, if one is buffered.
    pub fn next_message(&mut self) -> io::Result<Option<(u32, u16, Vec<u8>)>> {
        match protocol::take(&self.pending) {
            Ok(Some((message, used))) => {
                let taken = (message.object, message.opcode, message.body.to_vec());
                self.pending.drain(..used);
                Ok(Some(taken))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
        }
    }

    /// Sends one event.
    pub fn send(&mut self, writer: Writer) -> io::Result<()> {
        let bytes = writer.bytes();
        let mut sent = 0;
        while sent < bytes.len() {
            let written = sys::send(self.fd(), &bytes[sent..], &writer.fds)?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the client's socket took nothing",
                ));
            }
            sent += written;
        }
        Ok(())
    }

    /// Takes the next fd a client sent, in the order it sent them.
    pub fn take_fd(&mut self) -> Option<RawFd> {
        self.fds.pop_front()
    }

    /// Drops a client's socket without touching its fds, for a compositor that is
    /// shutting down.
    pub fn close(self) {}
}

/// Where clients connect: a directory and a socket name, which is what
/// `WAYLAND_DISPLAY` names.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
}

impl Server {
    /// Binds `dir/name`. A socket nobody is listening on is a stale one — a
    /// crashed predecessor — and is replaced; a socket with a listener behind it
    /// is left alone, because two compositors on one name is two compositors
    /// whose clients are talking to the wrong program.
    pub fn bind(dir: &Path, name: &str) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(name);
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{} is already in use", path.display()),
                ));
            }
            let _ = std::fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        tracing::info!("wayland: listening on {}", path.display());
        Ok(Self { listener, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    /// Accepts one client if one is waiting.
    pub fn accept(&mut self) -> io::Result<Option<Client>> {
        match self.listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(true)?;
                tracing::info!("wayland: client connected");
                Ok(Some(Client::new(stream)?))
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err),
        }
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
    use std::io::Read;

    #[test]
    fn a_message_split_across_two_reads_is_handled_once_it_is_whole() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut client = Client::new(ours).unwrap();
        let message = Writer::new(DISPLAY_ID, 0).uint(2).finish();
        let bytes = message.bytes();
        client.pending.extend_from_slice(&bytes[..5]);
        assert!(client.next_message().unwrap().is_none());
        client.pending.extend_from_slice(&bytes[5..]);
        let (object, opcode, body) = client.next_message().unwrap().unwrap();
        assert_eq!((object, opcode), (DISPLAY_ID, 0));
        assert_eq!(body.len(), 4);
        drop(theirs);
    }

    #[test]
    fn an_event_written_to_one_end_arrives_at_the_other() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut client = Client::new(ours).unwrap();
        client.send(Writer::new(2, 0).uint(7).finish()).unwrap();
        let mut header = [0u8; 8];
        theirs.read_exact(&mut header).unwrap();
        // object 2, opcode 0, then the length: 12 bytes in all.
        assert_eq!(&header[..8], &[2, 0, 0, 0, 0, 0, 12, 0]);
        let mut body = [0u8; 4];
        theirs.read_exact(&mut body).unwrap();
        assert_eq!(u32::from_le_bytes(body), 7);
    }
}
