import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  ArrowDownUp,
  ArrowLeftRight,
  Gauge,
  Keyboard,
  Mouse,
  Move,
  RotateCcw,
  X,
  Zap,
} from "lucide-react";
import {
  formatKey,
  hotkeySchema,
  modeKeys,
  settingsSchema,
  statusSchema,
} from "./settings";
import type { EngineStatus, Hotkey, Mode, Settings } from "./settings";
import styles from "./App.module.scss";
import { CaptureSession } from "./captureSession";
import { version } from "../package.json";
const modes = [
  {
    id: "slow",
    name: "Точное движение",
    description: "Замедлить курсор",
    icon: Gauge,
  },
  {
    id: "fast",
    name: "Быстрое движение",
    description: "Ускорить курсор",
    icon: Zap,
  },
  {
    id: "scrollHorizontal",
    name: "Горизонтальная прокрутка",
    description: "Движением мыши влево и вправо",
    icon: ArrowLeftRight,
  },
  {
    id: "scrollVertical",
    name: "Вертикальная прокрутка",
    description: "Движением мыши вверх и вниз",
    icon: ArrowDownUp,
  },
  {
    id: "scrollBoth",
    name: "Свободная прокрутка",
    description: "Прокрутка сразу по двум осям",
    icon: Move,
  },
] as const;
export default function App() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [status, setStatus] = useState<EngineStatus | null>(null);
  const [recording, setRecording] = useState<Mode | null>(null);
  const [error, setError] = useState("");
  const [preparing, setPreparing] = useState(false);
  const [busy, setBusy] = useState(false);
  const captureSession = useRef(new CaptureSession());
  const settingsRef = useRef<Settings | null>(null);
  const applyRef = useRef<(next: Settings) => Promise<void>>(async () => {});
  async function apply(next: Settings) {
    setBusy(true);
    setError("");
    try {
      const validated = settingsSchema.parse(next);
      const saved = settingsSchema.parse(
        await invoke("set_settings", { settings: validated }),
      );
      settingsRef.current = saved;
      setSettings(saved);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }
  applyRef.current = apply;
  useEffect(() => {
    function handleKeyDown(event: KeyboardEvent) {
      if (
        recording === null &&
        event.ctrlKey &&
        !event.altKey &&
        !event.shiftKey &&
        !event.metaKey &&
        event.code === "KeyW"
      ) {
        event.preventDefault();
        if (!event.repeat) {
          void getCurrentWindow()
            .close()
            .catch((e) => setError(String(e)));
        }
      }
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [recording]);
  useEffect(() => {
    let mounted = true;
    const cleanup: (() => void)[] = [];
    void (async () => {
      try {
        const unlisten = await listen<Hotkey>("hotkey-captured", (event) => {
          const mode = captureSession.current.complete();
          setPreparing(false);
          setRecording(null);
          if (mode && settingsRef.current) {
            const hotkey = hotkeySchema.parse(event.payload);
            void applyRef.current({
              ...settingsRef.current,
              hotkeys: { ...settingsRef.current.hotkeys, [mode]: hotkey },
            });
          }
        });
        if (!mounted) {
          unlisten();
          return;
        }
        cleanup.push(unlisten);
        const statusListener = await listen<EngineStatus>(
          "engine-status",
          (event) => {
            if (mounted) {
              const next = statusSchema.parse(event.payload);
              setStatus(next);
              const phase = captureSession.current.observe(next);
              if (phase === "active") setPreparing(false);
              if (phase === "ended") {
                setRecording(null);
                setPreparing(false);
              }
            }
          },
        );
        if (!mounted) {
          statusListener();
          return;
        }
        cleanup.push(statusListener);
        const loaded = settingsSchema.parse(await invoke("get_settings"));
        const engine = statusSchema.parse(await invoke("engine_status"));
        if (mounted) {
          settingsRef.current = loaded;
          setSettings(loaded);
          setStatus(engine);
        }
      } catch (e) {
        if (mounted) setError(String(e));
      }
    })();
    return () => {
      mounted = false;
      cleanup.forEach((fn) => fn());
    };
  }, []);
  async function capture(mode: Mode) {
    setError("");
    const captureId = captureSession.current.start(mode);
    setRecording(mode);
    setPreparing(true);
    try {
      await invoke("begin_capture");
      if (captureSession.current.acknowledge(captureId)) setPreparing(false);
    } catch (e) {
      if (!captureSession.current.isCurrent(captureId)) return;
      captureSession.current.cancel();
      setRecording(null);
      setPreparing(false);
      setError(String(e));
    }
  }
  async function cancel() {
    try {
      await invoke("cancel_capture");
      captureSession.current.cancel();
      setRecording(null);
      setPreparing(false);
    } catch (e) {
      setError(String(e));
    }
  }
  const configured = settings
    ? modeKeys.filter((mode) => settings.hotkeys[mode]).length
    : 0;
  return (
    <main className={styles.app}>
      <header className={styles.header}>
        <div className={styles.logo}>
          <Mouse size={25} strokeWidth={1.7} />
        </div>
        <div>
          <h1>Mouse Modes</h1>
          <p>Больше возможностей под рукой</p>
        </div>
        <span className={styles.badge}>УДЕРЖАНИЕ</span>
      </header>
      <section className={styles.intro}>
        <h2>Режимы мыши</h2>
        <p>
          Зажмите сочетание, чтобы включить режим. Отпустите — вернётся обычное
          движение.
        </p>
      </section>
      <div className={styles.modes}>
        {modes.map(({ id, name, description, icon: Icon }) => (
          <div
            className={`${styles.row} ${recording === id ? styles.recording : ""}`}
            key={id}
          >
            <Icon className={styles.modeIcon} size={20} />
            <div className={styles.label}>
              <h3>{name}</h3>
              <p>{description}</p>
            </div>
            <div className={styles.controls}>
              {settings?.hotkeys[id] && recording !== id ? (
                <span className={styles.chord}>
                  {settings.hotkeys[id]?.keys.map((key) => (
                    <kbd key={key}>{formatKey(key)}</kbd>
                  ))}
                </span>
              ) : (
                <span className={styles.empty}>
                  {recording === id
                    ? preparing
                      ? "Подготовка ввода…"
                      : "Зажмите сочетание…"
                    : "Не назначено"}
                </span>
              )}
              {recording === id ? (
                <button
                  className={styles.iconButton}
                  onClick={() => void cancel()}
                  title="Отменить ввод"
                  aria-label={`Отменить ввод: ${name}`}
                >
                  <X size={17} />
                </button>
              ) : (
                <>
                  <button
                    className={styles.iconButton}
                    disabled={
                      !settings ||
                      busy ||
                      recording !== null ||
                      !status?.available
                    }
                    onClick={() => void capture(id)}
                    title="Ввести сочетание"
                    aria-label={`Ввести сочетание: ${name}`}
                  >
                    <Keyboard size={19} />
                  </button>
                  {settings?.hotkeys[id] && (
                    <button
                      className={styles.iconButton}
                      disabled={busy || recording !== null}
                      onClick={() =>
                        void apply({
                          ...settings,
                          hotkeys: { ...settings.hotkeys, [id]: null },
                        })
                      }
                      title="Сбросить по умолчанию"
                      aria-label={`Сбросить: ${name}`}
                    >
                      <RotateCcw size={16} />
                    </button>
                  )}
                </>
              )}
            </div>
          </div>
        ))}
      </div>
      <p className={styles.hint}>
        Левые и правые Ctrl, Shift, Alt и Win различаются. Зажмите оба
        модификатора, чтобы подошла любая сторона. Ввод завершится, когда вы
        отпустите все клавиши.
      </p>
      <section className={styles.startup}>
        <div>
          <h3>Запускать при старте системы</h3>
          <p>Программа будет ждать в системном трее</p>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={settings?.autostart ?? false}
          aria-label="Запускать при старте системы"
          className={`${styles.switch} ${settings?.autostart ? styles.on : ""}`}
          disabled={!settings || busy || recording !== null}
          onClick={() =>
            settings &&
            void apply({ ...settings, autostart: !settings.autostart })
          }
        >
          <span />
        </button>
      </section>
      {error && (
        <div role="alert" className={styles.error}>
          {error}
        </div>
      )}
      <footer>
        <span className={styles.status}>
          <i className={status?.available ? styles.ready : ""} />
          {status?.message || "Подключение к устройствам…"}
        </span>
        <span>
          {configured} / 5 назначено{busy ? " · Применение…" : ""}
        </span>
      </footer>
      <p className={styles.version}>Mouse Modes v{version}</p>
    </main>
  );
}
