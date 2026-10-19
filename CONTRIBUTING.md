# Сборка и разработка

Нужны Node.js 22.12+ и Rust stable. На Windows установите Visual Studio C++ Build Tools и WebView2. На Linux нужны заголовки GTK3, WebKitGTK 4.1, AppIndicator и OpenSSL, а для запуска — polkit и `uinput`.

Зависимости Arch/Omarchy: `webkit2gtk-4.1 libappindicator-gtk3 openssl polkit`. Зависимости Ubuntu приведены в [CI](.github/workflows/check.yml).

```sh
npm ci
npm run tauri dev
```

Автономная сборка с встроенным интерфейсом:

```sh
npm run tauri build -- --no-bundle
```

Исполняемый файл будет в `src-tauri/target/release/`. Для локальной debug-сборки добавьте `--debug`.

Установщик для текущей ОС:

```sh
npm run tauri build -- --bundles deb      # Linux
npm run tauri build -- --bundles nsis     # Windows
```

## Проверки

```sh
npm run check
npm test
cargo test --manifest-path src-tauri/Cargo.toml
```

CI проверяет Linux и Windows отдельно. Проверки на одной ОС не заменяют живую проверку другой.

## Linux: обработка ввода

Нативный Rust-обработчик запускается тем же исполняемым файлом с `--input-helper` до инициализации интерфейса. GUI вызывает его через polkit и передаёт только настройки и команды записи по унаследованным pipe. Данные произвольных нажатий не отправляются в интерфейс и не сохраняются.

Обнаружение устройств и создание виртуальных устройств выполняются вне потока обработки движения. При пустых назначениях мышь использует обычный путь системы. С назначениями сохраняется непрерывный relay, чтобы нажатие хоткея во время перетаскивания не теряло состояние кнопок. Клавиатура захватывается исключительно на время записи сочетания. Поддерживаются составные HID-клавиатуры с непозиционными абсолютными осями, например громкостью.
