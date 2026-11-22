//! Asks the display for a frame, over and over, and says whether the answer
//! changes when the screen does.
//!
//! The screen's transport sends only what differs from the frame before, so a
//! capture that keeps handing back an older picture is invisible in the code and
//! obvious here: two connections, one kept open and one opened fresh each time,
//! and a hash of what each one saw.

fn hash(pixels: &[u8]) -> String {
    // FNV-1a, enough to tell two pictures apart without a dependency.
    let mut value: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in pixels {
        value ^= *byte as u64;
        value = value.wrapping_mul(0x100_0000_01b3);
    }
    format!("{value:016x}")
}

fn main() {
    let display = match aixodia_screen::display::ensure() {
        Ok(display) => display,
        Err(err) => {
            eprintln!("no display: {}", err.0);
            return;
        }
    };
    let input = match aixodia_screen::Input::open(&display) {
        Ok(input) => input,
        Err(err) => {
            eprintln!("cannot open {display}: {err}");
            return;
        }
    };
    let (width, height) = input.display_size();
    let mut kept = match aixodia_screen::Screen::open(&display, width, height) {
        Ok(screen) => screen,
        Err(err) => {
            eprintln!("cannot capture {display}: {err}");
            return;
        }
    };
    for round in 0..6 {
        let kept_frame = kept
            .frame()
            .map(|frame| hash(&frame.pixels))
            .unwrap_or_default();
        // A second connection, opened just now, as the comparison.
        let fresh = aixodia_screen::Screen::open(&display, width, height)
            .and_then(|mut screen| screen.frame())
            .map(|frame| hash(&frame.pixels))
            .unwrap_or_default();
        println!(
            "round {round}: kept {kept_frame} fresh {fresh} same {}",
            kept_frame == fresh
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}
