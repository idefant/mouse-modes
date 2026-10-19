#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::EngineHandle;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::EngineHandle;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
compile_error!("Mouse Modes supports Linux and Windows");
