//! Serves the display over RFB, so a stock VNC viewer can be pointed at it.
//!
//! Run with `cargo run -p aixodia-screen --example vnc -- 5900`, then connect a
//! viewer to `127.0.0.1:5900`. It exists to be connected to: what a viewer sees
//! and what its clicks do cannot be checked from inside this crate.

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|port| port.parse().ok())
        .unwrap_or(5900);
    match aixodia_screen::rfb::start(aixodia_screen::rfb::Config {
        port,
        quality: 75,
        tile: 64,
    }) {
        Ok(port) => println!("rfb: listening on 127.0.0.1:{port}"),
        Err(err) => {
            eprintln!("rfb: could not listen: {err}");
            return;
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(600));
}
