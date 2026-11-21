//! What the machine's screen looks like when the engine has nothing else to do.
//!
//! Run with `cargo run -p aixodia-screen --example desktop`, then look at the
//! display it prints: the desktop colour, the bar along the bottom, and one
//! window opened from that bar. It exists to be looked at, because a screen is a
//! picture and a test can only check arithmetic.

fn main() {
    let display = match aixodia_screen::display::ensure() {
        Ok(display) => display,
        Err(err) => {
            eprintln!("no display: {}", err.0);
            return;
        }
    };
    println!("display: {display}");
    // One window from the bar, so the picture has a title bar, a task button and
    // a piece of a terminal on it rather than an empty desktop.
    aixodia_screen::panel::launch("terminal", &display);
    // The window manager and the bar live on their own threads, so this only has
    // to stay alive long enough for a look.
    std::thread::sleep(std::time::Duration::from_secs(120));
}
