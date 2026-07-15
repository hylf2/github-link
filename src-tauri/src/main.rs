use github_link_core::{
    AppConfig, ControlRequest, ControlResponse, RouteMode, UpstreamConfig, UpstreamKind,
    config_path, socket_path,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command as TokioCommand;

const DAEMON_LABEL: &str = "com.fengc.github-linkd";
const DAEMON_RETRY_ATTEMPTS: usize = 20;
const DAEMON_RETRY_DELAY: Duration = Duration::from_millis(200);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsInput {
    upstream_kind: Option<String>,
    upstream_host: Option<String>,
    upstream_port: Option<u16>,
    refresh_seconds: u64,
}

#[tauri::command]
async fn get_status(app: AppHandle) -> Result<serde_json::Value, String> {
    response_value(send_with_recovery(&app, ControlRequest::Status).await)
}

#[tauri::command]
fn get_config() -> Result<serde_json::Value, String> {
    let config = AppConfig::load_or_create(&config_path()).map_err(|error| error.to_string())?;
    serde_json::to_value(config).map_err(|error| error.to_string())
}

#[tauri::command]
async fn probe(app: AppHandle) -> Result<serde_json::Value, String> {
    response_value(send_with_recovery(&app, ControlRequest::Probe { host: None }).await)
}

#[tauri::command]
async fn set_route(app: AppHandle, mode: String) -> Result<serde_json::Value, String> {
    let mode = mode
        .parse::<RouteMode>()
        .map_err(|error| error.to_string())?;
    response_value(send_with_recovery(&app, ControlRequest::SetRoute { mode }).await)
}

#[tauri::command]
async fn save_settings(app: AppHandle, input: SettingsInput) -> Result<serde_json::Value, String> {
    let mut config =
        AppConfig::load_or_create(&config_path()).map_err(|error| error.to_string())?;
    config.refresh_seconds = input.refresh_seconds.clamp(30, 3_600);
    config.upstream = match (
        input.upstream_kind.as_deref(),
        input.upstream_host,
        input.upstream_port,
    ) {
        (Some(kind), Some(host), Some(port)) if !host.trim().is_empty() => {
            let kind = match kind {
                "http" => UpstreamKind::Http,
                "socks5" => UpstreamKind::Socks5,
                _ => return Err("Unsupported upstream type".into()),
            };
            Some(UpstreamConfig {
                kind,
                host: host.trim().to_owned(),
                port,
                enabled: true,
            })
        }
        _ => None,
    };
    config
        .save(&config_path())
        .map_err(|error| error.to_string())?;
    response_value(send_with_recovery(&app, ControlRequest::Reload).await)
}

#[tauri::command]
fn run_cli(app: AppHandle, action: String) -> Result<String, String> {
    let cli = bundled_binary(&app, "github-link").map_err(|error| error.to_string())?;
    let output = Command::new(cli)
        .arg(action)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

#[tauri::command]
fn open_network_settings() -> Result<(), String> {
    Command::new("open")
        .arg("x-apple.systempreferences:com.apple.Network-Settings.extension")
        .status()
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn send_with_recovery(
    app: &AppHandle,
    request: ControlRequest,
) -> Result<ControlResponse, String> {
    let path = socket_path();
    match send_once(&path, request.clone()).await {
        Ok(response) => return Ok(response),
        Err(initial_error) => {
            let config = AppConfig::load_or_create(&config_path())
                .map_err(|error| format!("Daemon unavailable: {initial_error}; {error}"))?;
            if !config.enabled {
                return Err(format!("Daemon unavailable: {initial_error}"));
            }
        }
    }

    if kickstart_daemon().await.is_ok()
        && let Ok(response) = retry_send(
            &path,
            request.clone(),
            DAEMON_RETRY_ATTEMPTS / 2,
            DAEMON_RETRY_DELAY,
        )
        .await
    {
        return Ok(response);
    }

    reinstall_daemon(app).await?;
    retry_send(&path, request, DAEMON_RETRY_ATTEMPTS, DAEMON_RETRY_DELAY)
        .await
        .map_err(|error| format!("Daemon unavailable after automatic recovery: {error}"))
}

async fn kickstart_daemon() -> Result<(), String> {
    let uid = command_output("id", &["-u"]).await?;
    let target = format!("gui/{uid}/{DAEMON_LABEL}");
    let kickstart = TokioCommand::new("launchctl")
        .args(["kickstart", "-k", &target])
        .status()
        .await
        .map_err(|error| format!("failed to restart daemon: {error}"))?;
    if kickstart.success() {
        Ok(())
    } else {
        Err(format!("launchctl could not kickstart {DAEMON_LABEL}"))
    }
}

async fn reinstall_daemon(app: &AppHandle) -> Result<(), String> {
    let cli = bundled_binary(app, "github-link").map_err(|error| error.to_string())?;
    let output = TokioCommand::new(cli)
        .arg("enable")
        .output()
        .await
        .map_err(|error| format!("failed to reinstall daemon: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if message.is_empty() {
            "failed to reinstall daemon".into()
        } else {
            message
        })
    }
}

async fn command_output(program: &str, args: &[&str]) -> Result<String, String> {
    let output = TokioCommand::new(program)
        .args(args)
        .output()
        .await
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("{program} exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn retry_send(
    path: &Path,
    request: ControlRequest,
    attempts: usize,
    delay: Duration,
) -> Result<ControlResponse, String> {
    let mut last_error = "daemon did not become ready".to_owned();
    for attempt in 0..attempts {
        match send_once(path, request.clone()).await {
            Ok(response) => return Ok(response),
            Err(error) => last_error = error,
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(delay).await;
        }
    }
    Err(last_error)
}

async fn send_once(path: &Path, request: ControlRequest) -> Result<ControlResponse, String> {
    let mut stream = UnixStream::connect(path)
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(
            serde_json::to_string(&request)
                .map_err(|error| error.to_string())?
                .as_bytes(),
        )
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|error| error.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .await
        .map_err(|error| error.to_string())?;
    if line.is_empty() {
        return Err("daemon closed the control connection without a response".into());
    }
    serde_json::from_str(&line).map_err(|error| error.to_string())
}

fn response_value(response: Result<ControlResponse, String>) -> Result<serde_json::Value, String> {
    serde_json::to_value(response?).map_err(|error| error.to_string())
}

fn bundled_binary(app: &AppHandle, name: &str) -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let sibling = current.parent().unwrap_or(Path::new(".")).join(name);
    if sibling.exists() {
        return Ok(sibling);
    }
    let resource = app.path().resource_dir()?.join(name);
    if resource.exists() {
        return Ok(resource);
    }
    let nested = app
        .path()
        .resource_dir()?
        .join("bundle-resources")
        .join(name);
    if nested.exists() {
        return Ok(nested);
    }
    let tauri_parent_resource = app
        .path()
        .resource_dir()?
        .join("_up_")
        .join("bundle-resources")
        .join(name);
    anyhow::ensure!(
        tauri_parent_resource.exists(),
        "missing bundled binary: {}",
        tauri_parent_resource.display()
    );
    Ok(tauri_parent_resource)
}

fn show_window(window: Option<WebviewWindow>) {
    if let Some(window) = window {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let open = MenuItem::with_id(app, "open", "Open GitHub Link", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().expect("app icon"))
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_window(app.get_webview_window("main")),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_window(tray.app_handle().get_webview_window("main"));
                    }
                })
                .build(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_config,
            probe,
            set_route,
            save_settings,
            run_cli,
            open_network_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running GitHub Link");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir_in;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn retries_until_control_socket_is_ready() {
        let target_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        let directory = tempdir_in(target_dir).unwrap();
        let path = directory.path().join("control.sock");
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let listener = UnixListener::bind(server_path).unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut request)
                .await
                .unwrap();
            assert!(request.contains("\"command\":\"status\""));
            stream
                .write_all(b"{\"type\":\"ok\",\"message\":\"ready\"}\n")
                .await
                .unwrap();
        });

        let response = retry_send(&path, ControlRequest::Status, 10, Duration::from_millis(10))
            .await
            .unwrap();
        assert!(matches!(response, ControlResponse::Ok { message } if message == "ready"));
        server.await.unwrap();
    }
}
