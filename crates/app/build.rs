//! Gives the Windows programs their icon, which Explorer, the Start menu and
//! the taskbar read from the program file itself.
//!
//! `assets/icon.png` is eframe's own icon without the empty margin it comes
//! with, which made it smaller on the taskbar than the icons beside it.
//! `assets/icon.ico` is the same at the sizes Windows asks for. The window
//! shows the PNG in its corner.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        winresource::WindowsResource::new()
            .set_icon("assets/icon.ico")
            .compile()
            .expect("the icon cannot be put into the program");
    }
}
