//! What the pointer's cursor is, and when it changes.
//!
//! Run against the engine's display with the pointer moved between a window that
//! asks for a text cursor and the bare root: the serial changes when the cursor
//! does, and the bitmap that comes back is a real image.
//!
//! ```text
//! DISPLAY=:90 cargo run -p aixodia-screen --example cursor_probe
//! ```

fn main() {
    let name = std::env::var("DISPLAY").unwrap_or_else(|_| ":90".to_string());
    let input = aixodia_screen::Input::open(&name).expect("a display to ask");
    let rounds = std::env::var("ROUNDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(40);
    let mut last: Option<u64> = None;
    // A shape is easier to name from its picture than from its size: an I-beam
    // and a hand can be the same number of pixels. `MAP=1` prints the alpha as a
    // coarse grid so a rule about shapes can be argued from what X hands over.
    let map = std::env::var("MAP").is_ok();
    for _ in 0..rounds {
        match input.cursor_shape() {
            Some(now) => {
                if last != Some(now.serial) {
                    let opaque = now.rgba.chunks(4).filter(|p| p[3] > 0).count();
                    println!(
                        "serial {} {}x{} hot {}x{} opaque pixels {}",
                        now.serial, now.width, now.height, now.hot_x, now.hot_y, opaque
                    );
                    if map {
                        for y in 0..now.height {
                            let mut line = String::new();
                            for x in 0..now.width {
                                let i = ((y * now.width + x) * 4 + 3) as usize;
                                line.push(if now.rgba[i] > 127 { '#' } else { '.' });
                            }
                            println!("{line}");
                        }
                    }
                    last = Some(now.serial);
                }
            }
            None => println!("no cursor to read"),
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
