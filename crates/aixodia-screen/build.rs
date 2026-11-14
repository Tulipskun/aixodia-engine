//! Links the X libraries this crate calls into.
//!
//! The `x11` crate declares the symbols but does not link anything, and a
//! dynamic link is what we want: the package depends on libX11 being present,
//! not on a particular build of it.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-lib=dylib=X11");
        println!("cargo:rustc-link-lib=dylib=Xext");
        println!("cargo:rustc-link-lib=dylib=Xtst");
        println!("cargo:rerun-if-changed=build.rs");
    }
}
