mod engine;
mod platform;
pub use engine::{EngineEvent, EngineStatus, Hotkey, Settings};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{Emitter, Manager};
use tauri_plugin_autostart::ManagerExt;
struct AppState {
    settings: Mutex<Settings>,
    engine: platform::EngineHandle,
    path: PathBuf,
}
#[tauri::command]
fn get_settings(state: tauri::State<AppState>) -> Result<Settings, String> {
    Ok(state.settings.lock().map_err(|e| e.to_string())?.clone())
}
#[tauri::command]
fn engine_status(state: tauri::State<AppState>) -> EngineStatus {
    state.engine.status()
}
#[tauri::command]
fn begin_capture(state: tauri::State<AppState>) -> Result<(), String> {
    state.engine.begin_capture()
}
#[tauri::command]
fn cancel_capture(state: tauri::State<AppState>) -> Result<(), String> {
    state.engine.cancel_capture()
}
fn save(path: &PathBuf, settings: &Settings) -> Result<(), String> {
    let directory = path.parent().ok_or("Нет каталога настроек")?;
    fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary).map_err(|e| e.to_string())?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let source: Vec<u16> = temporary
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let destination: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    fs::rename(temporary, path).map_err(|e| e.to_string())
}
#[tauri::command]
fn set_settings(
    app: tauri::AppHandle,
    state: tauri::State<AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    settings.validate()?;
    let mut current = state.settings.lock().map_err(|e| e.to_string())?;
    let previous = current.clone();
    let autostart = app.autolaunch();
    if settings.autostart != previous.autostart {
        if settings.autostart {
            autostart.enable()
        } else {
            autostart.disable()
        }
        .map_err(|e| e.to_string())?;
    }
    if let Err(error) = state
        .engine
        .update_settings(settings.clone())
        .and_then(|_| save(&state.path, &settings))
    {
        let _ = state.engine.update_settings(previous.clone());
        if previous.autostart {
            let _ = autostart.enable();
        } else {
            let _ = autostart.disable();
        }
        return Err(error);
    }
    *current = settings.clone();
    Ok(settings)
}
fn show(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
pub fn run() {
    let builder = tauri::Builder::default();
    // Tao's raw keyboard registration prevents our LL hook from receiving
    // keys while this app is focused. The input engine owns raw mouse input.
    #[cfg(windows)]
    let builder = builder.device_event_filter(tauri::DeviceEventFilter::Always);
    builder
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show(app)))
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .args(["--autostart"])
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            engine_status,
            begin_capture,
            cancel_capture
        ])
        .setup(|app| {
            if let Some(window) = app.get_webview_window("main") {
                window.set_icon(tauri::include_image!("icons/128x128.png"))?;
            }
            let path = app.path().app_config_dir()?.join("settings.json");
            let mut settings = match fs::read(&path) {
                Ok(data) => serde_json::from_slice::<Settings>(&data)
                    .map_err(|e| format!("Повреждены настройки {}: {e}", path.display()))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
                Err(e) => return Err(e.into()),
            };
            settings.validate()?;
            settings.autostart = app.autolaunch().is_enabled().unwrap_or(false);
            let handle = app.handle().clone();
            let callback = Arc::new(move |event: EngineEvent| match event {
                EngineEvent::Captured(hotkey) => {
                    let _ = handle.emit("hotkey-captured", hotkey);
                }
                EngineEvent::Status(status) => {
                    if !status.available {
                        // Failed background startup must be visible to the user.
                        show(&handle);
                    }
                    let _ = handle.emit("engine-status", status);
                }
            });
            if std::env::args().any(|arg| arg == "--autostart") {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            let engine = platform::EngineHandle::start(callback, settings.clone())?;
            app.manage(AppState {
                settings: Mutex::new(settings),
                engine,
                path,
            });
            let show_item =
                tauri::menu::MenuItem::with_id(app, "show", "Настройки", true, None::<&str>)?;
            let quit_item =
                tauri::menu::MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
            let menu = tauri::menu::Menu::with_items(app, &[&show_item, &quit_item])?;
            tauri::tray::TrayIconBuilder::new()
                .icon(tauri::include_image!("icons/tray.png"))
                .tooltip("Mouse Modes")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            #[cfg(windows)]
            if let tauri::WindowEvent::Focused(false) = event {
                if let Some(state) = window.app_handle().try_state::<AppState>() {
                    let _ = state.engine.cancel_capture();
                }
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                if let Some(state) = window.app_handle().try_state::<AppState>() {
                    let _ = state.engine.cancel_capture();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("Не удалось запустить Mouse Modes")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app.try_state::<AppState>() {
                    state.engine.shutdown();
                }
            }
        });
}

/// The privileged Linux input worker enters here before any GUI initialization.
#[cfg(target_os = "linux")]
pub fn run_input_helper() -> Result<(), String> {
    platform::run_helper()
}

#[cfg(target_os = "linux")]
pub fn run_input_service() -> Result<(), String> {
    platform::run_service()
}
