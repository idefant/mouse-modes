use crate::engine::{EngineEvent, EventCallback};
use crate::{EngineStatus, Hotkey, Settings};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct EngineHandle {
    input: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    settings: Arc<Mutex<Settings>>,
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
        let socket = std::path::Path::new("/run/mouse-modes-input.sock");
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(stream) => return Self::start_service(callback, settings, stream),
            Err(error)
                if std::path::Path::new("/usr/lib/systemd/system/mouse-modes-input.socket")
                    .exists() =>
            {
                return Err(format!(
                    "Не удалось подключиться к службе ввода Mouse Modes: {error}"
                ));
            }
            Err(_) => {}
        }
        // Development and portable builds retain explicit system authorization.
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        Self::start_with(
            callback,
            settings,
            executable,
            "pkexec".into(),
            std::env::args().any(|arg| arg == "--autostart"),
        )
    }

    fn start_service(
        callback: EventCallback,
        settings: Settings,
        stream: std::os::unix::net::UnixStream,
    ) -> Result<Self, String> {
        let mut output = stream.try_clone().map_err(|error| error.to_string())?;
        let state = Arc::new(Mutex::new(EngineStatus {
            available: false,
            capturing: false,
            message: "Подключение к службе ввода".into(),
        }));
        let handle = Self {
            input: Arc::new(Mutex::new(Some(Box::new(stream)))),
            settings: Arc::new(Mutex::new(settings.clone())),
            state: state.clone(),
        };
        // The service may reject an inactive session before this first write.
        // The supervisor below reconnects when the local session becomes active.
        let _ = handle.update_settings(settings);
        let worker_input = handle.input.clone();
        let worker_settings = handle.settings.clone();
        std::thread::spawn(move || loop {
            let mut last_error = None;
            for line in BufReader::new(output).lines().map_while(Result::ok) {
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
                        if let Ok(status) = serde_json::from_value::<EngineStatus>(event.payload) {
                            if !status.available {
                                last_error = Some(status.message.clone());
                            }
                            if let Ok(mut current) = state.lock() {
                                *current = status.clone();
                            }
                            callback(EngineEvent::Status(status));
                        }
                    }
                    _ => {}
                }
            }
            let status = EngineStatus {
                available: false,
                capturing: false,
                message: last_error.unwrap_or_else(|| "Ожидание подключения к службе ввода".into()),
            };
            if let Ok(mut current) = state.lock() {
                *current = status.clone();
            }
            callback(EngineEvent::Status(status));
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let Ok(mut input) = worker_input.lock() else {
                    return;
                };
                if input.is_none() {
                    return;
                }
                // Don't repeatedly start service instances while this user is
                // inactive. The service independently verifies the kernel peer.
                if !super::linux_service::current_session_is_active() {
                    continue;
                }
                let Ok(mut stream) =
                    std::os::unix::net::UnixStream::connect("/run/mouse-modes-input.sock")
                else {
                    continue;
                };
                let Ok(reader) = stream.try_clone() else {
                    continue;
                };
                let Ok(settings) = worker_settings.lock() else {
                    return;
                };
                if writeln!(
                    stream,
                    "{}",
                    json!({"command":"settings", "settings":*settings})
                )
                .and_then(|_| stream.flush())
                .is_err()
                {
                    continue;
                }
                *input = Some(Box::new(stream));
                output = reader;
                break;
            }
        });
        Ok(handle)
    }

    fn start_with(
        callback: EventCallback,
        settings: Settings,
        executable: std::path::PathBuf,
        launcher: std::path::PathBuf,
        autostart: bool,
    ) -> Result<Self, String> {
        let (child, input, output, errors) = spawn_helper(&launcher, &executable, &settings)?;
        let input: Arc<Mutex<Option<Box<dyn Write + Send>>>> =
            Arc::new(Mutex::new(Some(Box::new(input))));
        let current_settings = Arc::new(Mutex::new(settings));
        let worker_input = input.clone();
        let worker_settings = current_settings.clone();
        let retry_deadline = Instant::now() + Duration::from_secs(30);
        let state = Arc::new(Mutex::new(EngineStatus {
            available: false,
            message: "Ожидание системного разрешения на доступ к устройствам ввода".into(),
            capturing: false,
        }));
        let worker_state = state.clone();
        std::thread::spawn(move || {
            let mut child = child;
            let mut output = output;
            let mut errors = errors;
            loop {
                let mut last_error = None;
                let mut was_available = false;
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
                            was_available |= status.available;
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
                let no_agent = errors.join().unwrap_or(false);
                let mut input = match worker_input.lock() {
                    Ok(input) => input,
                    Err(_) => return,
                };
                if input.is_none() {
                    return;
                }
                // Retry only the explicit absence of an authentication agent.
                // Rejected credentials and user dismissal must not reopen prompts.
                if should_retry(
                    autostart,
                    no_agent,
                    was_available,
                    exit.as_ref().ok().and_then(|s| s.code()),
                    Instant::now() < retry_deadline,
                ) {
                    // Drop the dead pipe while holding the lock so settings updates
                    // cannot write to it. Shutdown must remain responsive while waiting.
                    drop(input);
                    std::thread::sleep(Duration::from_secs(1));
                    input = match worker_input.lock() {
                        Ok(input) => input,
                        Err(_) => return,
                    };
                    if input.is_none() {
                        return;
                    }
                    let settings = match worker_settings.lock() {
                        Ok(settings) => settings,
                        Err(_) => return,
                    };
                    if let Ok((next_child, next_input, next_output, next_errors)) =
                        spawn_helper(&launcher, &executable, &settings)
                    {
                        *input = Some(Box::new(next_input));
                        child = next_child;
                        output = next_output;
                        errors = next_errors;
                        continue;
                    }
                }
                input.take();
                drop(input);
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
                break;
            }
        });
        Ok(Self {
            input,
            settings: current_settings,
            state,
        })
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
        // Keep settings and pipe updates in the same order as helper retries.
        let mut input = self
            .input
            .lock()
            .map_err(|_| "Input helper lock poisoned")?;
        let mut current = self.settings.lock().map_err(|_| "Settings lock poisoned")?;
        let pipe = input.as_mut().ok_or("Input helper is stopped")?;
        writeln!(
            pipe,
            "{}",
            json!({"command":"settings", "settings":settings})
        )
        .map_err(|error| error.to_string())?;
        pipe.flush().map_err(|error| error.to_string())?;
        *current = settings;
        Ok(())
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
            if let Some(mut pipe) = input.take() {
                let _ = writeln!(pipe, "{}", json!({"command":"quit"}));
                let _ = pipe.flush();
            }
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

// Log stderr and recognize the transient missing-agent failure in a fixed
// locale. Other authorization failures must not trigger automatic prompts.
fn spawn_helper(
    launcher: &std::path::Path,
    executable: &std::path::Path,
    settings: &Settings,
) -> Result<
    (
        Child,
        ChildStdin,
        ChildStdout,
        std::thread::JoinHandle<bool>,
    ),
    String,
> {
    let mut child = Command::new(launcher)
        .arg("--disable-internal-agent")
        .arg(executable)
        .arg("--input-helper")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("LC_ALL", "C")
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Cannot start Linux input helper (pkexec): {error}"))?;
    let mut input = child.stdin.take().ok_or("Helper stdin unavailable")?;
    let output = child.stdout.take().ok_or("Helper stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("Helper stderr unavailable")?;
    let errors = std::thread::spawn(move || {
        let mut no_agent = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            no_agent |= line.contains("No authentication agent found.");
            eprintln!("Linux input helper: {line}");
        }
        no_agent
    });
    if let Err(error) = writeln!(
        input,
        "{}",
        json!({"command":"settings", "settings":settings})
    )
    .and_then(|_| input.flush())
    {
        // A fast authorization failure can close stdin before the first write.
        // Let the supervisor inspect the exit code and apply the retry policy.
        eprintln!("Cannot initialize Linux input helper: {error}");
    }
    Ok((child, input, output, errors))
}

fn should_retry(
    autostart: bool,
    no_agent: bool,
    was_available: bool,
    exit_code: Option<i32>,
    within_deadline: bool,
) -> bool {
    autostart && no_agent && !was_available && exit_code == Some(127) && within_deadline
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn service_transport_sends_settings_receives_status_and_quits() {
        use std::os::unix::net::UnixStream;
        let (client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            let mut writer = server.try_clone().unwrap();
            let mut reader = BufReader::new(server);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let settings: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(settings["command"], "settings");
            writeln!(writer, "{}", json!({"event":"engine-status","payload":{"available":true,"capturing":false,"message":"ready"}})).unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["command"],
                "quit"
            );
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let engine = EngineHandle::start_service(
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
            Settings::default(),
            client,
        )
        .unwrap();
        let EngineEvent::Status(status) = rx.recv_timeout(Duration::from_secs(3)).unwrap() else {
            panic!("Expected status")
        };
        assert!(status.available);
        engine.shutdown();
        worker.join().unwrap();
    }

    #[test]
    fn recovers_when_authorization_agent_becomes_ready() {
        let directory = std::env::temp_dir().join(format!(
            "mouse-modes-startup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let launcher = directory.join("pkexec");
        // Two failed boot attempts followed by a helper that acknowledges the
        // initial settings. It exits on EOF, just like the real input worker.
        std::fs::write(&launcher, r#"#!/bin/sh
cd -- "$(dirname -- "$0")" || exit 1
printf 'attempt\n' >> attempts
count=$(wc -l < attempts)
if [ "$count" -lt 3 ]; then
  echo 'Error executing command as another user: No authentication agent found.' >&2
  exit 127
fi
IFS= read -r settings || exit 1
printf '%s' "$settings" > settings.json
printf '%s\n' '{"event":"engine-status","payload":{"available":true,"capturing":false,"message":"ready"}}'
cat > /dev/null
"#).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let engine = EngineHandle::start_with(
            Arc::new(move |event| {
                if let EngineEvent::Status(status) = event {
                    let _ = tx.send(status);
                }
            }),
            Settings::default(),
            "/unused".into(),
            launcher,
            true,
        )
        .unwrap();
        let status = rx.recv_timeout(Duration::from_secs(8)).unwrap();
        assert!(status.available, "{}", status.message);
        assert_eq!(
            std::fs::read_to_string(directory.join("attempts"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        let settings: Value =
            serde_json::from_slice(&std::fs::read(directory.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["command"], "settings");
        assert_eq!(
            settings["settings"],
            serde_json::to_value(Settings::default()).unwrap()
        );
        engine.shutdown();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retries_only_startup_authorization_unavailability() {
        assert!(should_retry(true, true, false, Some(127), true));
        assert!(!should_retry(true, false, false, Some(127), true));
        assert!(!should_retry(true, true, false, Some(126), true));
        assert!(!should_retry(false, true, false, Some(127), true));
        assert!(!should_retry(true, true, true, Some(127), true));
        assert!(!should_retry(true, true, false, Some(127), false));
        assert!(!should_retry(true, true, false, Some(1), true));
        assert!(!should_retry(true, true, false, None, true));
    }
}
