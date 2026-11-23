//! Types through the engine's own input path and says where the keys went.
//!
//! ```text
//! DISPLAY=:90 cargo run -p aixodia-screen --example type_probe -- 659 122 "hello"
//! ```
//!
//! The point is to separate one question from another: "did the keys reach the
//! machine at all" is this program's answer, and "did the phone send them" is the
//! viewer's. A program that takes keys in one window and not another decides
//! which of the two is at fault.

fn main() {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":90".to_string());
    let mut args = std::env::args().skip(1);
    let x: i32 = args
        .next()
        .and_then(|v| v.parse().ok())
        .expect("an x to click at");
    let y: i32 = args
        .next()
        .and_then(|v| v.parse().ok())
        .expect("a y to click at");
    let text = args.next().unwrap_or_else(|| "hello".to_string());

    let mut screen = aixodia_screen::Screen::open(&display, 1280, 720).expect("the display");
    screen.move_pointer(x, y);
    screen.button(aixodia_screen::Button::Left, true);
    screen.button(aixodia_screen::Button::Left, false);
    std::thread::sleep(std::time::Duration::from_millis(300));
    screen.type_text(&text).expect("the keys to go somewhere");
    println!("typed {text:?} at {x},{y}");
}
