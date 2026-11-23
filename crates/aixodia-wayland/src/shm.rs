//! Client pixels: the shared-memory pool they are written into, and the buffer
//! they are read out of.
//!
//! Wayland never copies a client's framebuffer to the compositor through a
//! socket. The client creates a pool, sends the compositor its fd, and maps the
//! same pages itself; a buffer is then an offset and a geometry into that pool.
//! The compositor reads those pages directly, which is why there is no capture
//! step here at all — the pixels are already in memory we can see.

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// The pixel formats a client may hand over. Wayland's numbers, which are
/// `DRM_FORMAT_*` values.
pub const ARGB8888: u32 = 0;
pub const XRGB8888: u32 = 1;

/// A mapped shared-memory pool.
pub struct Pool {
    /// Kept so the mapping has an owner, and so a resize can ask the file how big
    /// it really is.
    _fd: OwnedFd,
    address: *mut u8,
    size: usize,
}

impl Pool {
    /// Maps `fd` read-only as far as this compositor is concerned: clients write,
    /// we read, and nothing here ever writes into a client's pages.
    ///
    /// The mapping is `MAP_SHARED` because that is the whole point — the client's
    /// writes have to be visible here without a copy.
    pub fn map(fd: RawFd, size: usize) -> io::Result<Self> {
        if size == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "an empty pool"));
        }
        // SAFETY: `fd` came from the client and is owned by this compositor from
        // here on; the mapping is checked against MAP_FAILED and unmapped in Drop.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the fd is valid and not used anywhere else; ownership moves in.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self {
            _fd: owned,
            address: address as *mut u8,
            size,
        })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Grows the mapping to what the client has grown its file to.
    ///
    /// A client that needs more room sends `wl_shm_pool.resize` and later
    /// offsets buffers past our old mapping. Ignoring that is not a cosmetic
    /// complaint: measured against a real browser, the pool went 2304 → 587520
    /// bytes while the mapping stayed at 2304, so the next buffer at offset
    /// 211968 was refused and the browser kept committing a buffer that did not
    /// exist. The file itself is asked how big it is, because a mapping beyond a
    /// file's end is a SIGBUS on the first read, not an error return.
    pub fn resize(&mut self, size: usize) -> io::Result<usize> {
        // SAFETY: stat is ours and the fd is a plain descriptor.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(self._fd.as_raw_fd(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let grown = size.min(stat.st_size.max(0) as usize);
        if grown <= self.size {
            return Ok(self.size);
        }
        // SAFETY: the old mapping is ours, the new size is within the file, and
        // MAP_FAILED is checked. MREMAP_MAYMOVE lets the kernel pick a place.
        let address = unsafe {
            libc::mremap(
                self.address as *mut libc::c_void,
                self.size,
                grown,
                libc::MREMAP_MAYMOVE,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        self.address = address as *mut u8;
        self.size = grown;
        Ok(self.size)
    }

    /// The pool's bytes. A caller must not hold this across a `resize`.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is at least `size` bytes and lives as long as self.
        unsafe { std::slice::from_raw_parts(self.address, self.size) }
    }
}

// SAFETY: the mapping is owned by this pool, never written, and only read
// through `bytes`, so sending one to another thread sends an address that is
// still valid and still ours. Nothing else can unmap it: `Drop` is the only
// unmapper, and it needs the pool.
unsafe impl Send for Pool {}
unsafe impl Sync for Pool {}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: the address and size are exactly what mmap returned and asked
        // for, and no slice outlives this.
        unsafe {
            libc::munmap(self.address as *mut libc::c_void, self.size);
        }
    }
}

/// A rectangle of client pixels: where in the pool, and how it is laid out.
#[derive(Debug, Clone, Copy)]
pub struct Buffer {
    pub pool: u32,
    pub offset: usize,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: u32,
}

impl Buffer {
    /// Copies the buffer into an ARGB framebuffer at `(x, y)`, clipped to it.
    ///
    /// Returns the rectangle actually touched, which is what a caller has to
    /// repaint: a window half off the screen only changes half of it.
    // The frame's geometry and where the buffer goes are all needed here; a
    // struct to carry six numbers would be a struct nobody else uses.
    #[allow(clippy::too_many_arguments)]
    /// The buffer's pixels as words, row by row.
    ///
    /// `blit` writes into a frame; this hands the pixels to a caller that is not
    /// drawing them into the picture at all — a cursor is drawn by whoever is
    /// holding the pointer, not by the compositor.
    pub fn pixels(&self, pool: &Pool) -> Vec<u32> {
        let bytes = pool.bytes();
        let mut out = Vec::with_capacity((self.width as usize) * (self.height as usize));
        for row in 0..self.height {
            for column in 0..self.width {
                let at = row as usize * self.stride + column as usize * 4 + self.offset;
                if at + 4 > bytes.len() {
                    return out;
                }
                out.push(u32::from_le_bytes([
                    bytes[at],
                    bytes[at + 1],
                    bytes[at + 2],
                    bytes[at + 3],
                ]));
            }
        }
        out
    }

    pub fn blit(
        &self,
        pool: &Pool,
        source: Option<(u32, u32, u32, u32)>,
        frame: &mut [u32],
        frame_width: u32,
        frame_height: u32,
        x: i32,
        y: i32,
    ) -> Option<(i32, i32, i32, i32)> {
        let bytes = pool.bytes();
        let (sx, sy, sw, sh) = match source {
            Some(rect) => rect,
            None => (0, 0, self.width, self.height),
        };
        if sw == 0 || sh == 0 || self.stride == 0 {
            return None;
        }
        let mut touched: Option<(i32, i32, i32, i32)> = None;
        for row in 0..sh {
            let destination_y = y + (sy + row) as i32;
            if destination_y < 0 || destination_y >= frame_height as i32 {
                continue;
            }
            for column in 0..sw {
                let destination_x = x + (sx + column) as i32;
                if destination_x < 0 || destination_x >= frame_width as i32 {
                    continue;
                }
                let pixel =
                    (sy + row) as usize * self.stride + (sx + column) as usize * 4 + self.offset;
                if pixel + 4 > bytes.len() {
                    continue;
                }
                let value = u32::from_le_bytes([
                    bytes[pixel],
                    bytes[pixel + 1],
                    bytes[pixel + 2],
                    bytes[pixel + 3],
                ]);
                // Both formats are the same four bytes; xrgb8888 simply does not
                // promise an alpha, and an opaque window is the usual case, so the
                // unused byte is taken as opaque rather than as nothing.
                let argb = if self.format == XRGB8888 {
                    value | 0xff00_0000
                } else {
                    value
                };
                frame[destination_y as usize * frame_width as usize + destination_x as usize] =
                    argb;
                touched = Some(match touched {
                    None => (destination_x, destination_y, 1, 1),
                    Some((tx, ty, tw, th)) => {
                        let left = tx.min(destination_x);
                        let top = ty.min(destination_y);
                        let right = (tx + tw).max(destination_x + 1);
                        let bottom = (ty + th).max(destination_y + 1);
                        (left, top, right - left, bottom - top)
                    }
                });
            }
        }
        touched
    }

    /// The number of bytes this buffer needs from its pool.
    pub fn required_bytes(&self) -> usize {
        self.stride * self.height as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    /// A pool backed by a real file, because that is what a client sends: a
    /// sealed memfd or a shared temporary file.
    fn pool_with(pixels: &[u32]) -> (std::fs::File, Pool) {
        // A file of this test's own: tests run in parallel, and a shared path is
        // two tests writing over each other's pixels.
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let name = format!(
            "/tmp/aixodia-shm-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&name)
            .unwrap();
        let _ = std::fs::remove_file(&name);
        file.set_len(pixels.len() as u64 * 4).unwrap();
        use std::io::Write;
        let mut writer = &file;
        let bytes: Vec<u8> = pixels.iter().flat_map(|p| p.to_le_bytes()).collect();
        writer.write_all(&bytes).unwrap();
        let fd = unsafe { libc::dup(file.as_raw_fd()) };
        let pool = Pool::map(fd, pixels.len() * 4).unwrap();
        (file, pool)
    }

    #[test]
    fn a_buffer_is_copied_into_the_frame_at_the_place_it_is_asked_for() {
        let (_file, pool) = pool_with(&[0xff11_2233, 0xff44_5566, 0xff77_8899, 0xffaa_bbcc]);
        let buffer = Buffer {
            pool: 1,
            offset: 0,
            width: 2,
            height: 2,
            stride: 8,
            format: ARGB8888,
        };
        let mut frame = vec![0u32; 4 * 4];
        let touched = buffer
            .blit(&pool, None, &mut frame, 4, 4, 1, 1)
            .expect("a 2x2 buffer has pixels");
        assert_eq!(touched, (1, 1, 2, 2));
        assert_eq!(frame[5], 0xff11_2233);
        assert_eq!(frame[6], 0xff44_5566);
        assert_eq!(frame[9], 0xff77_8899);
        assert_eq!(frame[10], 0xffaa_bbcc);
        assert_eq!(frame[0], 0, "nothing outside the buffer was touched");
    }

    #[test]
    fn a_window_half_off_the_screen_only_touches_the_half_that_fits() {
        let (_file, pool) = pool_with(&[0xff00_00ff; 4]);
        let buffer = Buffer {
            pool: 1,
            offset: 0,
            width: 2,
            height: 2,
            stride: 8,
            format: ARGB8888,
        };
        let mut frame = vec![0u32; 2 * 2];
        let touched = buffer.blit(&pool, None, &mut frame, 2, 2, -1, -1).unwrap();
        assert_eq!(touched, (0, 0, 1, 1));
        assert_eq!(frame[0], 0xff00_00ff);
        assert_eq!(frame[3], 0, "the corner outside the screen is untouched");
    }

    #[test]
    fn a_damaged_rectangle_copies_only_that_rectangle() {
        let (_file, pool) = pool_with(&[1, 2, 3, 4, 5, 6]);
        let buffer = Buffer {
            pool: 1,
            offset: 0,
            width: 3,
            height: 2,
            stride: 12,
            format: ARGB8888,
        };
        let mut frame = vec![0u32; 3 * 2];
        let touched = buffer
            .blit(&pool, Some((1, 1, 2, 1)), &mut frame, 3, 2, 0, 0)
            .unwrap();
        assert_eq!(touched, (1, 1, 2, 1));
        assert_eq!(frame[4], 5, "the pixel at (1, 1)");
        assert_eq!(frame[5], 6);
        assert_eq!(frame[0], 0, "the undamaged pixel was not copied");
    }

    #[test]
    fn an_xrgb_buffer_is_taken_as_opaque() {
        let (_file, pool) = pool_with(&[0x0012_3456]);
        let buffer = Buffer {
            pool: 1,
            offset: 0,
            width: 1,
            height: 1,
            stride: 4,
            format: XRGB8888,
        };
        let mut frame = vec![0u32; 1];
        buffer.blit(&pool, None, &mut frame, 1, 1, 0, 0).unwrap();
        assert_eq!(frame[0], 0xff12_3456);
    }
}
