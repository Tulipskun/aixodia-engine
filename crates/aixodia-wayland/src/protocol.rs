//! Wayland's wire format, written out by hand.
//!
//! Every message begins with two words: a 32-bit object id, and a second word
//! whose low half is the opcode and whose high half is the length that counts
//! the header itself. On a little-endian machine that puts the opcode's two
//! bytes before the length's, which is the one detail of this format that is
//! easy to get backwards — and silent when you do, because a reader and a writer
//! that swap it in the same way agree with each other and with nothing else. The argument types are the interesting part and there are only
//! eight of them: int, uint, fixed (a 24.8 number), string (length plus bytes
//! plus a terminator, padded to four), object, new_id, array, and fd — and an fd
//! is not in the bytes at all, it rides beside them in a Unix socket's control
//! message.
//!
//! This module is only that format. Which interface an opcode belongs to, and
//! what to do about it, lives in `server`.

/// One decoded message: enough to dispatch on, and its bytes to read arguments
/// from.
#[derive(Debug)]
pub struct Message<'a> {
    pub object: u32,
    pub opcode: u16,
    pub body: &'a [u8],
}

/// What is wrong with a message that cannot be decoded.
#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    /// Fewer bytes than the header promises, or fewer than the header.
    Incomplete,
    /// A length smaller than the header, or not a multiple of four.
    BadLength(u16),
    /// A string or array that runs past its message.
    Overrun,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomplete => write!(f, "the message is incomplete"),
            Self::BadLength(length) => write!(f, "the length {length} is not a message length"),
            Self::Overrun => write!(f, "an argument runs past the end of its message"),
        }
    }
}

/// Takes one message off the front of `bytes`, and says how many bytes it used.
///
/// `Ok(None)` means the buffer holds only part of a message, which is ordinary:
/// a socket read can stop anywhere. Anything else is a protocol error, because a
/// client that sends a half message cannot be resynchronised with.
pub fn take(bytes: &[u8]) -> Result<Option<(Message<'_>, usize)>, WireError> {
    if bytes.len() < 8 {
        return Ok(None);
    }
    let object = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    // The second word is `size << 16 | opcode`, so the opcode comes first.
    let opcode = u16::from_le_bytes([bytes[4], bytes[5]]);
    let size = u16::from_le_bytes([bytes[6], bytes[7]]);
    if size < 8 || size % 4 != 0 {
        return Err(WireError::BadLength(size));
    }
    let size = size as usize;
    if bytes.len() < size {
        return Ok(None);
    }
    Ok(Some((
        Message {
            object,
            opcode,
            body: &bytes[8..size],
        },
        size,
    )))
}

/// Reads the typed arguments of a decoded message.
///
/// The reader does not know the interface, so each getter is the caller's
/// statement about what the next argument is. A getter that runs out returns the
/// zero of its type rather than panicking: a malformed message from a client is
/// the client's problem, and `finished` says when it happened.
pub struct Args<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Args<'a> {
    pub fn new(body: &'a [u8]) -> Self {
        Self { bytes: body, at: 0 }
    }

    pub fn uint(&mut self) -> u32 {
        if self.at + 4 > self.bytes.len() {
            self.at = self.bytes.len();
            return 0;
        }
        let value = u32::from_le_bytes([
            self.bytes[self.at],
            self.bytes[self.at + 1],
            self.bytes[self.at + 2],
            self.bytes[self.at + 3],
        ]);
        self.at += 4;
        value
    }

    pub fn int(&mut self) -> i32 {
        self.uint() as i32
    }

    /// A 24.8 fixed-point number, as Wayland defines them.
    pub fn fixed(&mut self) -> f64 {
        self.int() as f64 / 256.0
    }

    /// A string: a length that counts the terminator, the bytes, the terminator,
    /// and padding to four. Wayland strings may be `NULL` (length 0).
    pub fn string(&mut self) -> Option<String> {
        let length = self.uint() as usize;
        if length == 0 {
            return None;
        }
        let end = self.at + length;
        if end > self.bytes.len() {
            self.at = self.bytes.len();
            return None;
        }
        let raw = &self.bytes[self.at..end];
        let text = raw
            .split(|byte| *byte == 0)
            .next()
            .unwrap_or_default()
            .to_vec();
        self.at = (end + 3) & !3;
        if self.at > self.bytes.len() {
            self.at = self.bytes.len();
        }
        Some(String::from_utf8_lossy(&text).into_owned())
    }

    /// An array: a length that does not count a terminator, then the bytes,
    /// padded to four.
    pub fn array(&mut self) -> Vec<u8> {
        let length = self.uint() as usize;
        let end = self.at + length;
        if end > self.bytes.len() {
            self.at = self.bytes.len();
            return Vec::new();
        }
        let value = self.bytes[self.at..end].to_vec();
        self.at = (end + 3) & !3;
        if self.at > self.bytes.len() {
            self.at = self.bytes.len();
        }
        value
    }

    pub fn object(&mut self) -> u32 {
        self.uint()
    }

    pub fn new_id(&mut self) -> u32 {
        self.uint()
    }

    /// True when the message said more than the caller read.
    pub fn finished(&self) -> bool {
        self.at >= self.bytes.len()
    }
}

/// Builds one message. Fds are collected beside the bytes and sent with them.
#[derive(Default)]
pub struct Writer {
    bytes: Vec<u8>,
    pub fds: Vec<i32>,
}

impl Writer {
    /// Starts a message to `object` with `opcode`'s interface method. The size
    /// field is filled in by `finish`, because it is a header field that counts
    /// the whole message.
    pub fn new(object: u32, opcode: u16) -> Self {
        let mut bytes = Vec::with_capacity(32);
        bytes.extend_from_slice(&object.to_le_bytes());
        bytes.extend_from_slice(&opcode.to_le_bytes());
        // The length is written by `finish`, once the arguments are in.
        bytes.extend_from_slice(&0u16.to_le_bytes());
        Self {
            bytes,
            fds: Vec::new(),
        }
    }

    pub fn uint(mut self, value: u32) -> Self {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn int(self, value: i32) -> Self {
        self.uint(value as u32)
    }

    pub fn fixed(self, value: f64) -> Self {
        self.int((value * 256.0) as i32)
    }

    pub fn object(self, id: u32) -> Self {
        self.uint(id)
    }

    pub fn string(mut self, value: Option<&str>) -> Self {
        match value {
            None => self.uint(0),
            Some(text) => {
                let with_nul = text.len() + 1;
                self.bytes
                    .extend_from_slice(&(with_nul as u32).to_le_bytes());
                self.bytes.extend_from_slice(text.as_bytes());
                self.bytes.push(0);
                while self.bytes.len() % 4 != 0 {
                    self.bytes.push(0);
                }
                self
            }
        }
    }

    pub fn array(mut self, value: &[u8]) -> Self {
        self.bytes
            .extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.bytes.extend_from_slice(value);
        while self.bytes.len() % 4 != 0 {
            self.bytes.push(0);
        }
        self
    }

    /// Attaches an fd to this message. The caller keeps ownership of it; the
    /// send duplicates it into the socket.
    ///
    /// An fd takes a place in the message's signature and **no space in its
    /// body**: the four bytes some protocol descriptions promise are not read by
    /// libwayland, and writing them shifts every argument after the fd by one.
    /// Measured both ways against libwayland-client — with the extra bytes a
    /// `wl_keyboard.keymap` of 195 bytes was read as a keymap of zero.
    pub fn fd(mut self, fd: i32) -> Self {
        self.fds.push(fd);
        self
    }

    pub fn finish(mut self) -> Self {
        let size = self.bytes.len() as u16;
        self.bytes[6..8].copy_from_slice(&size.to_le_bytes());
        self
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The object an event is sent to, for a log that says what was said.
    pub fn object_id(&self) -> u32 {
        u32::from_le_bytes([self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3]])
    }

    /// The event's opcode on that object.
    pub fn op(&self) -> u16 {
        u16::from_le_bytes([self.bytes[4], self.bytes[5]])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_taken_off_the_front_and_reports_its_length() {
        let message = Writer::new(7, 3).uint(0x11223344).finish();
        let (taken, used) = take(message.bytes()).unwrap().unwrap();
        assert_eq!(used, 12);
        assert_eq!(taken.object, 7);
        assert_eq!(taken.opcode, 3);
        let mut args = Args::new(taken.body);
        assert_eq!(args.uint(), 0x11223344);
        assert!(args.finished());
    }

    #[test]
    fn a_file_descriptor_travels_beside_the_body_and_not_in_it() {
        // wl_keyboard.keymap is (format, fd, size). Measured against
        // libwayland-client reading a real compositor's message: the fd takes a
        // place in the signature but *no space in the body*. Three uints of body
        // there and the client reads the size out of the wrong place — a keymap
        // of zero bytes, and the client's keyboard state never comes up.
        let message = Writer::new(21, 0).uint(1).fd(9).uint(1024).finish();
        assert_eq!(message.bytes().len(), 16, "header, the format and the size");
        assert_eq!(message.fds, vec![9]);
        let (taken, used) = take(message.bytes()).unwrap().unwrap();
        assert_eq!(used, 16);
        let mut args = Args::new(taken.body);
        assert_eq!(args.uint(), 1, "the format");
        assert_eq!(args.uint(), 1024, "the size lands where the client looks");
        assert!(args.finished());
    }

    #[test]
    fn half_a_message_is_not_an_error_it_is_just_early() {
        let message = Writer::new(9, 1).uint(5).finish();
        assert!(take(&message.bytes()[..6]).unwrap().is_none());
        assert!(take(&message.bytes()[..11]).unwrap().is_none());
    }

    #[test]
    fn a_length_that_cannot_be_a_message_is_an_error() {
        let mut bytes = Writer::new(1, 1).uint(0).finish().bytes().to_vec();
        bytes[6..8].copy_from_slice(&7u16.to_le_bytes());
        assert!(matches!(take(&bytes), Err(WireError::BadLength(7))));
    }

    #[test]
    fn a_real_client_s_hello_message_reads_as_it_was_sent() {
        // The first 24 bytes libwayland's client sends, taken from a real
        // connection: get_registry(new id 2), then sync(new id 3). The opcode's
        // two bytes come before the length's, and reading them the other way
        // round makes the length 1 — which is how this cost an afternoon.
        let hello = [
            0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x0c, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x03, 0x00, 0x00, 0x00,
        ];
        let (first, used) = take(&hello).unwrap().unwrap();
        assert_eq!((first.object, first.opcode, used), (1, 1, 12));
        let mut args = Args::new(first.body);
        assert_eq!(args.new_id(), 2);
        let (second, used) = take(&hello[used..]).unwrap().unwrap();
        assert_eq!((second.object, second.opcode, used), (1, 0, 12));
        let mut args = Args::new(second.body);
        assert_eq!(args.new_id(), 3);
    }

    #[test]
    fn a_string_carries_its_terminator_and_pads_to_four() {
        let message = Writer::new(1, 0).string(Some("wl_shm")).uint(9).finish();
        // 8 header + (4 length + 7 bytes, padded to 12) + 4 = 24
        assert_eq!(message.bytes().len(), 24);
        let (taken, _) = take(message.bytes()).unwrap().unwrap();
        let mut args = Args::new(taken.body);
        assert_eq!(args.string().as_deref(), Some("wl_shm"));
        assert_eq!(args.uint(), 9);
        assert!(args.finished());
    }

    #[test]
    fn a_null_string_is_a_zero_length() {
        let message = Writer::new(1, 0).string(None).finish();
        let (taken, _) = take(message.bytes()).unwrap().unwrap();
        let mut args = Args::new(taken.body);
        assert_eq!(args.string(), None);
    }

    #[test]
    fn a_fixed_number_survives_the_round_trip() {
        let message = Writer::new(1, 0).fixed(1.5).finish();
        let (taken, _) = take(message.bytes()).unwrap().unwrap();
        let mut args = Args::new(taken.body);
        assert_eq!(args.fixed(), 1.5);
    }

    #[test]
    fn an_argument_past_the_end_reads_as_zero_and_does_not_panic() {
        let message = Writer::new(1, 0).uint(3).finish();
        let (taken, _) = take(message.bytes()).unwrap().unwrap();
        let mut args = Args::new(taken.body);
        assert_eq!(args.uint(), 3);
        assert_eq!(args.uint(), 0);
        assert!(args.finished());
        assert_eq!(args.string(), None);
    }

    #[test]
    fn two_messages_in_one_read_are_both_found() {
        let mut bytes = Writer::new(1, 0).uint(1).finish().bytes().to_vec();
        bytes.extend_from_slice(Writer::new(2, 0).uint(2).finish().bytes());
        let (first, used) = take(&bytes).unwrap().unwrap();
        assert_eq!(first.object, 1);
        let (second, _) = take(&bytes[used..]).unwrap().unwrap();
        assert_eq!(second.object, 2);
    }
}
