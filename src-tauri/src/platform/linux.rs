use crate::engine::{EngineEvent, EventCallback};
use crate::{EngineStatus, Hotkey, Settings};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

// Embedded at build time so installed apps don't depend on the source checkout.
const HELPER: &str = include_str!("../../../linux/mouse_modes_helper.py");

pub struct EngineHandle {
    input: Mutex<Option<ChildStdin>>,
    state: Arc<Mutex<EngineStatus>>,
}

#[derive(Deserialize)]
struct HelperEvent {
    event: String,
    payload: Value,
}

impl EngineHandle {
    pub fn start(callback: EventCallback, settings: Settings) -> Result<Self, String> {
        settings.validate()?;
        // The helper itself is passed as a python -I -c argument, not loaded from
        // a user-writable file with persistent root authorization. Polkit asks
        // for this exact invocation; no permissive polkit rule is installed.
        let mut command = Command::new("pkexec");
        command.args(["/usr/bin/python3", "-I", "-u", "-c", HELPER]);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Cannot start Linux input helper (pkexec): {error}"))?;
        let input = child.stdin.take().ok_or("Helper stdin unavailable")?;
        let output = child.stdout.take().ok_or("Helper stdout unavailable")?;
        let state = Arc::new(Mutex::new(EngineStatus {
            available: false,
            message: "Ожидание системного разрешения на доступ к устройствам ввода".into(),
            capturing: false,
        }));
        let worker_state = state.clone();
        std::thread::spawn(move || {
            let mut last_error = None;
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                let Ok(event) = serde_json::from_str::<HelperEvent>(&line) else {
                    continue;
                };
                match event.event.as_str() {
                    "hotkey-captured" => {
                        if let Ok(hotkey) = serde_json::from_value::<Hotkey>(event.payload) {
                            callback(EngineEvent::Captured(hotkey));
                        }
                    }
                    "engine-status" => {
                        let payload = event.payload;
                        let status = EngineStatus {
                            available: payload["available"].as_bool().unwrap_or(false),
                            capturing: payload["capturing"].as_bool().unwrap_or(false),
                            message: payload["message"]
                                .as_str()
                                .unwrap_or("Input helper protocol error")
                                .into(),
                        };
                        if !status.available {
                            last_error = Some(status.message.clone());
                        }
                        if let Ok(mut state) = worker_state.lock() {
                            *state = status.clone();
                        }
                        callback(EngineEvent::Status(status));
                    }
                    _ => {}
                }
            }
            let exit = child.wait();
            let message = last_error.unwrap_or_else(|| match exit {
                Ok(status) if status.code() == Some(126) || status.code() == Some(127) => {
                    "Разрешение на доступ к вводу отклонено или недоступно. Перезапустите программу для повторной авторизации."
                        .into()
                }
                Ok(status) => {
                    format!("Служба ввода Linux завершилась ({status}); перезапустите программу")
                }
                Err(error) => format!("Служба ввода Linux завершилась: {error}"),
            });
            let status = EngineStatus {
                available: false,
                capturing: false,
                message,
            };
            if let Ok(mut state) = worker_state.lock() {
                *state = status.clone();
            }
            callback(EngineEvent::Status(status));
        });
        let handle = Self {
            input: Mutex::new(Some(input)),
            state,
        };
        handle.update_settings(settings)?;
        Ok(handle)
    }

    fn send(&self, value: Value) -> Result<(), String> {
        let mut guard = self
            .input
            .lock()
            .map_err(|_| "Input helper lock poisoned")?;
        let input = guard.as_mut().ok_or("Input helper is stopped")?;
        writeln!(input, "{value}")
            .map_err(|error| format!("Cannot communicate with Linux input helper: {error}"))?;
        input.flush().map_err(|error| error.to_string())
    }

    pub fn update_settings(&self, settings: Settings) -> Result<(), String> {
        settings.validate()?;
        self.send(json!({"command":"settings", "settings":settings}))
    }

    pub fn begin_capture(&self) -> Result<(), String> {
        if !self.status().available {
            return Err("Разрешите доступ к устройствам ввода перед записью сочетания".into());
        }
        self.send(json!({"command":"capture"}))?;
        // Do not tell the UI it can record until EVIOCGRAB suppression is active.
        // Waiting for acknowledgement prevents the first chord from reaching apps.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if self.status().capturing {
                return Ok(());
            }
            if !self.status().available {
                return Err(self.status().message);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = self.cancel_capture();
        Err("Служба ввода не подтвердила запись; отпустите все клавиши и повторите".into())
    }

    pub fn cancel_capture(&self) -> Result<(), String> {
        self.send(json!({"command":"cancel"}))
    }

    pub fn shutdown(&self) {
        // Closing the only writer triggers EOF even if the GUI aborts. The helper
        // releases forwarded keys, closes virtual devices and releases grabs.
        if let Ok(mut input) = self.input.lock() {
            input.take();
        }
    }

    pub fn status(&self) -> EngineStatus {
        self.state
            .lock()
            .map(|state| state.clone())
            .unwrap_or(EngineStatus {
                available: false,
                capturing: false,
                message: "Состояние службы ввода недоступно".into(),
            })
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
