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

#[cfg(target_os = "linux")]
mod linux_native;
#[cfg(target_os = "linux")]
pub use linux_native::run_helper;

#[cfg(target_os = "linux")]
mod linux_service;
#[cfg(target_os = "linux")]
pub use linux_service::run_service;
