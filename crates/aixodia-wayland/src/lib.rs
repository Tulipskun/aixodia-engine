//! A Wayland compositor written from the protocol up.
//!
//! X11 is the older protocol and it shows: a separate display server, a window
//! manager that is a second program talking to it, a client library linked into
//! whatever wants a picture, and a capture path that copies the server's
//! framebuffer across a socket. Wayland folds the server and the window manager
//! into one program — the compositor — and hands each client's pixels over
//! shared memory, so the compositor owns the only copy.
//!
//! Writing that compositor here, rather than linking `wlroots`, is the same
//! decision as writing the RFB server: the protocol is public and small enough
//! to hold in your head, while the library that wraps it drags in a dozen shared
//! objects (and, for the GPU path, needs a device the engine's host may not have).
//! The dependencies of this crate are `libc` and `tracing`; nothing here needs a
//! Wayland package installed.
//!
//! `protocol` is the wire format, `sys` is the sendmsg/recvmsg that carries an fd
//! beside the bytes, and `server` is the object model and the dispatch.

pub mod protocol;
pub mod server;
pub mod sys;

pub use server::{Client, Global, Interface, Server, GLOBALS};
