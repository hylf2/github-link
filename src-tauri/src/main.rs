use github_link_core::{
    AppConfig, ControlRequest, ControlResponse, RouteMode, UpstreamConfig, UpstreamKind,
    config_path, socket_path,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewWindow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsInput {
    upstream_kind: Option<String>,
    upstream_host: Option<String>,
    upstream_port: Option<u16>,
    refresh_seconds: u64,
}

#[tauri::command]
async fn get_status() -> Result<serde_json::Value, String> {
    response_value(send(ControlRequest::Status).await)
}

#[tauri::command]
fn get_config() -> Result<serde_json::Value, String> {
    let config = AppConfig::load_or_create(&config_path()).map_err(|error| error.to_string())?;
    serde_json::to_value(config).map_err(|error| error.to_string())
}

#[tauri::command]
async fn probe() -> Result<serde_json::Value, String> {
    response_value(send(ControlRequest::Probe { host: None }).await)
}

#[tauri::command]
async fn set_route(mode: String) -> Result<serde_json::Value, String> {
    let mode = mode
        .parse::<RouteMode>()
        .map_err(|error| error.to_string())?;
    response_value(send(ControlRequest::SetRoute { mode }).await)
}

#[tauri::command]
async fn save_settings(input: SettingsInput) -> Result<serde_json::Value, String> {
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
    response_value(send(ControlRequest::Reload).await)
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

async fn send(request: ControlRequest) -> Result<ControlResponse, String> {
    let mut stream = UnixStream::connect(socket_path())
        .await
        .map_err(|error| format!("Daemon unavailable: {error}"))?;
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
