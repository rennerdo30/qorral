//! Qorral's window. `qorral-gui --snapshot out.png` renders it offscreen.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args = match qorral_app::parse_gui_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("qorral-gui: {e}");
            std::process::exit(2);
        }
    };
    std::process::exit(qorral_app::run_gui(args));
}
