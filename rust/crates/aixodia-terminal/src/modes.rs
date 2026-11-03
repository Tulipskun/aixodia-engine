//! Remembering the DEC private modes a program leaves on.
//!
//! Port of `io/terminal/modes.go`. A session only keeps the tail of its output,
//! so a full-screen program that set its mouse mode and alternate screen long
//! before that tail would come back to a client believing it is at a plain
//! prompt. These helpers replay the modes, trim the ring on a parser boundary,
//! and strip the device queries that must not be replayed.

/// The state of the small ANSI parser, carried across reads.
#[derive(Debug, Default)]
pub struct ModeTracker {
    on: std::collections::HashMap<i64, bool>,
    state: u8,
    params: Vec<u8>,
    bad: bool,
    string_osc: bool,
    saw_esc: bool,
}

const MODE_GROUND: u8 = 0;
const MODE_ESCAPE: u8 = 1;
const MODE_CSI: u8 = 2;
const MODE_STRING: u8 = 3;

fn tracked(mode: i64) -> bool {
    matches!(
        mode,
        1 | 7
            | 25
            | 47
            | 1000
            | 1002
            | 1003
            | 1004
            | 1005
            | 1006
            | 1015
            | 1016
            | 1047
            | 1049
            | 2004
    )
}

/// Alternate screen first so the ring is drawn into it, then the input modes.
const REPLAY_ORDER: [i64; 15] = [
    1049, 1047, 47, 1, 7, 25, 1000, 1002, 1003, 1004, 1005, 1006, 1015, 1016, 2004,
];

impl ModeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one chunk of pty output through the parser.
    pub fn scan(&mut self, bytes: &[u8]) {
        for &c in bytes {
            match self.state {
                MODE_GROUND => {
                    if c == 0x1b {
                        self.state = MODE_ESCAPE;
                    }
                }
                MODE_ESCAPE => match c {
                    b'[' => {
                        self.state = MODE_CSI;
                        self.params.clear();
                        self.bad = false;
                    }
                    b']' | b'P' | b'_' | b'^' | b'X' => {
                        self.state = MODE_STRING;
                        self.string_osc = c == b']';
                        self.saw_esc = false;
                    }
                    0x1b => {}
                    _ => self.state = MODE_GROUND,
                },
                MODE_CSI => match c {
                    0x30..=0x3f => {
                        if self.params.len() < 64 {
                            self.params.push(c);
                        }
                    }
                    0x20..=0x2f => {
                        // An intermediate byte (a query such as `ESC[?1016$p`),
                        // not a mode change.
                        self.bad = true;
                    }
                    0x18 | 0x1a => self.state = MODE_GROUND,
                    0x40..=0x7e => {
                        self.finish_csi(c);
                        self.state = MODE_GROUND;
                    }
                    _ => self.state = MODE_GROUND,
                },
                MODE_STRING => {
                    if self.saw_esc {
                        self.saw_esc = false;
                        if c == b'\\' {
                            self.state = MODE_GROUND;
                        } else if c == 0x1b {
                            self.saw_esc = true;
                        }
                    } else if c == 0x1b {
                        self.saw_esc = true;
                    } else if c == 0x07 && self.string_osc {
                        self.state = MODE_GROUND;
                    }
                }
                _ => self.state = MODE_GROUND,
            }
        }
    }

    fn finish_csi(&mut self, final_byte: u8) {
        if self.bad || (final_byte != b'h' && final_byte != b'l') {
            return;
        }
        if self.params.len() < 2 || self.params[0] != b'?' {
            return;
        }
        let on = final_byte == b'h';
        let rest = String::from_utf8_lossy(&self.params[1..]).into_owned();
        for part in rest.split(';') {
            let Ok(mode) = part.parse::<i64>() else {
                continue;
            };
            if tracked(mode) {
                self.on.insert(mode, on);
            }
        }
    }

    /// Whether the program is drawing on the alternate screen.
    pub fn alt_screen(&self) -> bool {
        self.on.get(&1049).copied().unwrap_or(false)
            || self.on.get(&1047).copied().unwrap_or(false)
            || self.on.get(&47).copied().unwrap_or(false)
    }

    /// The sequences that put a fresh emulator into the modes still left on.
    pub fn replay(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for mode in REPLAY_ORDER {
            let Some(&on) = self.on.get(&mode) else {
                continue;
            };
            match mode {
                25 => {
                    if !on {
                        out.extend_from_slice(b"\x1b[?25l");
                    }
                }
                7 => {
                    if !on {
                        out.extend_from_slice(b"\x1b[?7l");
                    }
                }
                _ => {
                    if on {
                        out.extend_from_slice(format!("\x1b[?{mode}h").as_bytes());
                    }
                }
            }
        }
        out
    }
}

/// Keeps the newest `cap` bytes of a session's output, cutting only where the
/// parser is in the ground state and on a UTF-8 boundary.
pub fn trim_ring(ring: &mut Vec<u8>, cap: usize) {
    if ring.len() <= cap {
        return;
    }
    let cut = ring.len() - cap;
    let mut start = ground_boundary(ring, cut);
    if start >= ring.len() {
        ring.clear();
        return;
    }
    while start < ring.len() && ring[start] & 0xC0 == 0x80 {
        start += 1;
    }
    ring.drain(..start);
}

/// The largest index at most `limit` at which `bytes` begins in ground state.
fn ground_boundary(bytes: &[u8], limit: usize) -> usize {
    const GROUND: u8 = 0;
    const ESC: u8 = 1;
    const CSI: u8 = 2;
    const STR: u8 = 3;
    const STR_ESC: u8 = 4;

    let mut state = GROUND;
    let mut osc = false;
    let mut best = 0;
    for (i, &c) in bytes.iter().enumerate() {
        match state {
            GROUND => {
                if c == 0x1b {
                    state = ESC;
                }
            }
            ESC => match c {
                b'[' => state = CSI,
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    state = STR;
                    osc = c == b']';
                }
                0x1b => {}
                _ => state = GROUND,
            },
            CSI => {
                if (0x40..=0x7e).contains(&c) {
                    state = GROUND;
                }
            }
            STR => {
                if c == 0x1b {
                    state = STR_ESC;
                } else if c == 0x07 && osc {
                    state = GROUND;
                }
            }
            STR_ESC => {
                if c == b'\\' {
                    state = GROUND;
                } else if c != 0x1b {
                    state = STR;
                }
            }
            _ => state = GROUND,
        }
        if state == GROUND && i < limit {
            best = i + 1;
        }
    }
    best
}

/// Removes the sequences that ask the terminal a question (DA and cursor
/// reports). Replaying them makes the client answer a second time, and the late
/// answer arrives as if it had been typed.
pub fn strip_queries(bytes: &[u8]) -> Vec<u8> {
    if !bytes.contains(&0x1b) {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b || i + 1 >= bytes.len() || bytes[i + 1] != b'[' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let mut j = i + 2;
        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        match bytes[j] {
            b'c' | b'n' => i = j + 1,
            _ => {
                out.extend_from_slice(&bytes[i..=j]);
                i = j + 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_only_the_modes_left_on() {
        let mut tracker = ModeTracker::new();
        tracker.scan(b"\x1b[?1049h\x1b[?1000h\x1b[?25l");
        assert!(tracker.alt_screen());
        assert_eq!(
            tracker.replay(),
            b"\x1b[?1049h\x1b[?25l\x1b[?1000h".to_vec()
        );
        tracker.scan(b"\x1b[?1049l");
        assert!(!tracker.alt_screen());
    }

    #[test]
    fn ignores_queries_and_split_sequences() {
        let mut tracker = ModeTracker::new();
        tracker.scan(b"\x1b[?1016$p\x1b[?10");
        tracker.scan(b"02h");
        assert_eq!(tracker.replay(), b"\x1b[?1002h".to_vec());
    }

    #[test]
    fn trims_on_a_boundary() {
        let mut ring = b"abcdef\x1b[31mred".to_vec();
        trim_ring(&mut ring, 6);
        assert_eq!(ring, b"\x1b[31mred");
    }

    #[test]
    fn strips_only_questions() {
        assert_eq!(
            strip_queries(b"a\x1b[1;2cb\x1b[31mc\x1b[6n"),
            b"ab\x1b[31mc".to_vec()
        );
    }
}
