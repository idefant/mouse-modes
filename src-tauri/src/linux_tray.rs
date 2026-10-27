//! libappindicator does not forward left clicks. Use StatusNotifierItem directly
//! so activation opens the window while right click keeps the context menu.
use ksni::{menu::StandardItem, MenuItem, TrayMethods};
use tauri::Manager;

struct LinuxTray {
    app: tauri::AppHandle,
    icon: ksni::Icon,
}

impl LinuxTray {
    fn open(&mut self) {
        let app = self.app.clone();
        let dispatcher = app.clone();
        let _ = dispatcher.run_on_main_thread(move || crate::show(&app));
    }
}

impl ksni::Tray for LinuxTray {
    fn id(&self) -> String {
        self.app.config().identifier.clone()
    }

    fn title(&self) -> String {
        "Mouse Modes".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.icon.clone()]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.open();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Настройки".into(),
                activate: Box::new(Self::open),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Выход".into(),
                activate: Box::new(|tray: &mut Self| tray.app.exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub(crate) fn setup(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let icon = tauri::include_image!("icons/tray.png");
    let mut pixels = icon.rgba().to_vec();
    // StatusNotifierItem expects ARGB32 in network byte order; Tauri uses RGBA.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.rotate_right(1);
    }
    let tray = LinuxTray {
        app: app.clone(),
        icon: ksni::Icon {
            width: icon.width() as i32,
            height: icon.height() as i32,
            data: pixels,
        },
    };
    let handle = tauri::async_runtime::block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tray.assume_sni_available(true).spawn(),
        )
        .await
    })??;
    // Keep the service alive, including while the panel/watcher is restarting.
    app.manage(handle);
    Ok(())
}
