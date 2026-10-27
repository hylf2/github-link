use anyhow::{Context, Result};
use clap::Parser;
use github_link_core::{
    AppConfig, ControlRequest, ControlResponse, config_path, log_dir, socket_path,
};
use github_link_proxy::ProxyRuntime;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "github-linkd",
    version,
    about = "GitHub Link local proxy daemon"
)]
struct Args {
    #[arg(long, default_value_os_t = config_path())]
    config: PathBuf,
    #[arg(long, default_value_os_t = socket_path())]
    socket: PathBuf,
    #[arg(long, default_value_os_t = log_dir())]
    log_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    fs::create_dir_all(&args.log_dir)?;
    let file = tracing_appender::rolling::daily(&args.log_dir, "github-linkd.log");
    let (writer, _guard) = tracing_appender::non_blocking(file);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(writer)
        .with_ansi(false)
        .init();

    let config = AppConfig::load_or_create(&args.config)?;
    let runtime = ProxyRuntime::new(config)?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let proxy = tokio::spawn(runtime.clone().run_proxy());
    let pac = tokio::spawn(runtime.clone().run_pac());
    let control = tokio::spawn(run_control(
        args.socket.clone(),
        args.config.clone(),
        runtime.clone(),
        shutdown_tx.clone(),
    ));
    let refresh = tokio::spawn(run_refresh(runtime.clone(), shutdown_rx.clone()));
    let mirror_refresh = tokio::spawn(run_mirror_refresh(runtime.clone(), shutdown_rx.clone()));

    info!(config = %args.config.display(), socket = %args.socket.display(), "GitHub Link daemon started");
    tokio::select! {
        result = proxy => result??,
        result = pac => result??,
        result = control => result??,
        _ = wait_for_shutdown(shutdown_rx) => {},
        _ = tokio::signal::ctrl_c() => {
            info!("received shutdown signal");
        }
    }
    let _ = shutdown_tx.send(true);
    refresh.abort();
    mirror_refresh.abort();
    if args.socket.exists() {
        let _ = fs::remove_file(&args.socket);
    }
    Ok(())
}

async fn run_control(
    path: PathBuf,
    config_path: PathBuf,
    runtime: ProxyRuntime,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind control socket {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let runtime = runtime.clone();
        let config_path = config_path.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_control(stream, config_path, runtime, shutdown).await {
                warn!(%error, "control request failed");
            }
        });
    }
}

async fn handle_control(
    stream: UnixStream,
    config_path: PathBuf,
    runtime: ProxyRuntime,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let line = lines.next_line().await?.context("empty control request")?;
    let request: ControlRequest = serde_json::from_str(&line)?;
    let response = match request {
        ControlRequest::Status => {
            runtime.refresh_status().await;
            ControlResponse::Status {
                status: runtime.status.read().await.clone(),
            }
        }
        ControlRequest::Probe { host } => {
            let hosts = host.map(|value| vec![value]).unwrap_or_else(|| {
                vec![
                    "github.com".into(),
                    "api.github.com".into(),
                    "raw.githubusercontent.com".into(),
                ]
            });
            let mut errors = Vec::new();
            for host in hosts {
                if let Err(error) = runtime.probe_host(&host).await {
                    errors.push(format!("{host}: {error}"));
                }
            }
            if errors.is_empty() {
                ControlResponse::Status {
                    status: runtime.status.read().await.clone(),
                }
            } else {
                ControlResponse::Error {
                    message: errors.join("; "),
                }
            }
        }
        ControlRequest::RefreshMirrors => match runtime.refresh_mirrors().await {
            Ok(candidates) => ControlResponse::Mirrors { candidates },
            Err(error) => ControlResponse::Error {
                message: error.to_string(),
            },
        },
        ControlRequest::Accelerate { url } => match runtime.accelerate_download(&url).await {
            Ok(result) => ControlResponse::Acceleration { result },
            Err(error) => ControlResponse::Error {
                message: error.to_string(),
            },
        },
        ControlRequest::SetRoute { mode } => {
            {
                let mut config = runtime.config.write().await;
                config.route_mode = mode;
                config.save(&config_path)?;
            }
            let mut status = runtime.status.write().await;
            status.configured_mode = mode;
            status.active_route = mode.to_string();
            status.touch();
            ControlResponse::Ok {
                message: format!("route set to {mode}"),
            }
        }
        ControlRequest::Reload => {
            let config = AppConfig::load_or_create(&config_path)?;
            *runtime.config.write().await = config.clone();
            let mut status = runtime.status.write().await;
            status.enabled = config.enabled;
            status.configured_mode = config.route_mode;
            status.download_acceleration.enabled = config.download_acceleration.enabled;
            status.touch();
            drop(status);
            if config.download_acceleration.enabled {
                let refresh_runtime = runtime.clone();
                tokio::spawn(async move {
                    if let Err(error) = refresh_runtime.refresh_mirrors().await {
                        warn!(%error, "download mirror refresh after reload failed");
                    }
                });
            }
            ControlResponse::Ok {
                message: "configuration reloaded".into(),
            }
        }
        ControlRequest::Shutdown => {
            let _ = shutdown.send(true);
            ControlResponse::Ok {
                message: "daemon shutting down".into(),
            }
        }
    };
    writer
        .write_all(serde_json::to_string(&response)?.as_bytes())
        .await?;
    writer.write_all(b"\n").await?;
    Ok(())
}

async fn run_refresh(runtime: ProxyRuntime, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    loop {
        let seconds = runtime.config.read().await.refresh_seconds;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(seconds)) => {
                if let Err(error) = runtime.probe_host("github.com").await {
                    error!(%error, "background GitHub probe failed");
                }
            }
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

async fn run_mirror_refresh(
    runtime: ProxyRuntime,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        let config = runtime.config.read().await.clone();
        if config.download_acceleration.enabled
            && let Err(error) = runtime.refresh_mirrors().await
        {
            error!(%error, "background download mirror probe failed");
        }
        let seconds = config.download_acceleration.refresh_seconds.max(60);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(seconds)) => {}
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}
