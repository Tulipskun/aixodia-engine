//! Sending and receiving beside a Unix socket, because an fd cannot be in a byte
//! stream.
//!
//! Wayland passes file descriptors — a client's shared-memory pool, the keymap
//! the compositor hands a keyboard — as `SCM_RIGHTS` control messages attached to
//! an ordinary `sendmsg`/`recvmsg`. A Rust `UnixStream` cannot carry them, so
//! these two functions do the syscalls themselves. This is the only unsafe code
//! in the crate, and it is confined to moving bytes and fds between the kernel
//! and a `Vec`.

use std::io;
use std::os::unix::io::RawFd;

/// Sends one buffer, with `fds` attached if there are any.
pub fn send(fd: RawFd, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // Room for the maximum a single message may carry; Wayland never needs more
    // than eight and the kernel caps it at 253 anyway.
    let mut control = [0u8; 64];
    let mut message = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: std::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    if !fds.is_empty() {
        message.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        message.msg_controllen = unsafe { libc::CMSG_SPACE((fds.len() * 4) as u32) } as usize;
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&message) };
        if cmsg.is_null() {
            return Err(io::Error::other("no room for the file descriptors"));
        }
        unsafe {
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as u32) as usize;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr() as *const u8,
                libc::CMSG_DATA(cmsg),
                fds.len() * 4,
            );
        }
    }
    let sent = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sent as usize)
}

/// Reads up to `capacity` bytes and any fds that came with them.
pub fn recv(fd: RawFd, capacity: usize) -> io::Result<(Vec<u8>, Vec<RawFd>)> {
    let mut bytes = vec![0u8; capacity];
    let mut control = [0u8; 256];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let mut message = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: control.as_mut_ptr() as *mut libc::c_void,
        msg_controllen: control.len(),
        msg_flags: 0,
    };
    let read = unsafe { libc::recvmsg(fd, &mut message, libc::MSG_CMSG_CLOEXEC) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    bytes.truncate(read as usize);
    let mut fds = Vec::new();
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&message) };
    while !cmsg.is_null() {
        unsafe {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let length = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(cmsg);
                for index in 0..length / 4 {
                    let mut fd = 0i32;
                    std::ptr::copy_nonoverlapping(
                        data.add(index * 4),
                        &mut fd as *mut i32 as *mut u8,
                        4,
                    );
                    fds.push(fd);
                }
            }
            cmsg = libc::CMSG_NXTHDR(&message, cmsg);
        }
    }
    Ok((bytes, fds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    #[test]
    fn a_file_descriptor_survives_a_round_trip_beside_the_bytes() {
        let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        let bytes = b"with an fd";
        assert_eq!(
            send(left.as_raw_fd(), bytes, &[file.as_raw_fd()]).unwrap(),
            bytes.len()
        );
        let (received, fds) = recv(right.as_raw_fd(), 64).unwrap();
        assert_eq!(received, bytes);
        assert_eq!(fds.len(), 1);
        // It is a real descriptor: closing it is not an error.
        assert_eq!(unsafe { libc::close(fds[0]) }, 0);
    }

    #[test]
    fn bytes_with_no_fd_come_back_as_bytes() {
        let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
        send(left.as_raw_fd(), b"plain", &[]).unwrap();
        let (received, fds) = recv(right.as_raw_fd(), 64).unwrap();
        assert_eq!(received, b"plain");
        assert!(fds.is_empty());
    }
}
