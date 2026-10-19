#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--input-helper")) {
        if let Err(error) = mouse_modes_lib::run_input_helper() {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    mouse_modes_lib::run();
}
