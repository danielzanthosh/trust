use serde::Deserialize;

use std::env;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[derive(Clone, Debug)]
pub(crate) struct WebBridgeDaemonState {
    pub(crate) running: bool,
    pub(crate) extension_connected: Option<bool>,
    pub(crate) message: String,
}

#[derive(Deserialize)]
struct WebBridgeStatus {
    running: bool,
    extension_connected: Option<bool>,
}

pub(crate) fn ensure_webbridge_daemon_started() -> Result<WebBridgeDaemonState, String> {
    let binary = resolve_webbridge_binary().ok_or_else(|| {
        "Kimi WebBridge CLI was not found. Install it from https://www.kimi.com/features/webbridge, then retry. Expected path: ~/.kimi-webbridge/bin/kimi-webbridge".to_string()
    })?;

    match read_webbridge_status(&binary) {
        Ok(status) if status.running => {
            let message = if status.extension_connected == Some(false) {
                "Kimi WebBridge daemon is running, but the browser extension is not connected. Open your browser or install/enable the extension, then retry.".to_string()
            } else {
                "Kimi WebBridge daemon is running.".to_string()
            };

            Ok(WebBridgeDaemonState {
                running: true,
                extension_connected: status.extension_connected,
                message,
            })
        }

        Ok(_) => start_webbridge_daemon(&binary),

        Err(status_error) => match start_webbridge_daemon(&binary) {
            Ok(state) => Ok(state),
            Err(start_error) => Err(format!(
                "Failed to check or start Kimi WebBridge. Status error: {}. Start error: {}",
                status_error, start_error
            )),
        },
    }
}

fn resolve_webbridge_binary() -> Option<PathBuf> {
    if let Ok(path) = env::var("KIMI_WEBBRIDGE_BIN") {
        let path = PathBuf::from(path.trim());
        if path.exists() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();

    if let Some(home) = home_dir() {
        let base = home
            .join(".kimi-webbridge")
            .join("bin")
            .join("kimi-webbridge");
        candidates.push(base.clone());

        if cfg!(windows) {
            candidates.push(base.with_extension("exe"));
            candidates.push(base.with_extension("cmd"));
            candidates.push(base.with_extension("bat"));
        }
    }

    candidates.into_iter().find(|path| path.exists())
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn read_webbridge_status(binary: &PathBuf) -> Result<WebBridgeStatus, String> {
    let output = Command::new(binary)
        .arg("status")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("failed to run status: {}", error))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        return Err(format!(
            "status exited with {}: {}{}",
            output.status,
            stdout.trim(),
            stderr.trim()
        ));
    }

    serde_json::from_str::<WebBridgeStatus>(stdout.trim()).map_err(|error| {
        format!(
            "failed to parse status JSON: {}. Output: {}{}",
            error,
            stdout.trim(),
            stderr.trim()
        )
    })
}

fn start_webbridge_daemon(binary: &PathBuf) -> Result<WebBridgeDaemonState, String> {
    let output = Command::new(binary)
        .arg("start")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("failed to run start: {}", error))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        return Err(format!(
            "start exited with {}: {}{}",
            output.status,
            stdout.trim(),
            stderr.trim()
        ));
    }

    thread::sleep(Duration::from_millis(500));

    match read_webbridge_status(binary) {
        Ok(status) => {
            let message = if status.running && status.extension_connected == Some(false) {
                "Started Kimi WebBridge daemon. The browser extension is not connected yet; open your browser or enable the extension before browser actions.".to_string()
            } else if status.running {
                "Started Kimi WebBridge daemon.".to_string()
            } else {
                "Kimi WebBridge start command completed, but status still reports running: false."
                    .to_string()
            };

            Ok(WebBridgeDaemonState {
                running: status.running,
                extension_connected: status.extension_connected,
                message,
            })
        }

        Err(error) => Ok(WebBridgeDaemonState {
            running: true,
            extension_connected: None,
            message: format!(
                "Kimi WebBridge start command completed, but status could not be verified: {}",
                error
            ),
        }),
    }
}
