//! Runs the compositor on its own and writes the picture it composited, so the
//! question "did a client draw anything" can be answered by looking at pixels
//! rather than by reading a log.
//!
//!   cargo run --example compositor_probe -- /tmp/wl-probe 1280 720 /tmp/out.ppm 30
//!
//! Then start a client against it:
//!   XDG_RUNTIME_DIR=/tmp/wl-probe WAYLAND_DISPLAY=wayland-0 chromium --ozone-platform=wayland

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "/tmp/wl-probe".into()));
    let width = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1280u32);
    let height = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(720u32);
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/tmp/wl-frame.ppm".into()));
    let seconds = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30u64);

    let mut compositor = aixodia_wayland::Compositor::new(&dir, "wayland-0", width, height)?;
    println!(
        "compositor on {} ({}x{}) — WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR={}",
        compositor.path().display(),
        width,
        height,
        dir.display()
    );
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut frames = 0u32;
    while Instant::now() < deadline {
        if !compositor.poll(100)? {
            continue;
        }
        frames += 1;
        let mut ppm = Vec::new();
        ppm.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
        for pixel in compositor.frame() {
            ppm.push(((pixel >> 16) & 0xff) as u8);
            ppm.push(((pixel >> 8) & 0xff) as u8);
            ppm.push((pixel & 0xff) as u8);
        }
        std::fs::File::create(&out)?.write_all(&ppm)?;
        for window in compositor.windows() {
            println!(
                "frame {frames}: window {:?} at ({}, {}) {}x{} mapped={}",
                window.title, window.x, window.y, window.width, window.height, window.mapped
            );
        }
        if compositor.windows().is_empty() {
            println!("frame {frames}: no windows");
        }
    }
    println!("{frames} frames in {seconds}s");
    Ok(())
}
