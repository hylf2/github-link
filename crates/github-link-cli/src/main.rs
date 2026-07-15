use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use github_link_core::{
    AppConfig, ControlRequest, ControlResponse, RouteMode, app_support_dir, config_path,
    install_tool_integrations, restore_tool_integrations, socket_path,
};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};

#[derive(Debug, Parser)]
#[command(
    name = "github-link",
    version,
    about = "Secure GitHub connectivity for macOS"
)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Enable,
    Disable,
    Status {
        #[arg(long)]
        json: bool,
    },
    Probe {
        host: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Route {
        mode: RouteMode,
    },
    Exec {
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    Doctor {
        #[arg(long)]
        json: bool,
    },
    Restore,
    #[command(hide = true)]
    SshTunnel {
        host: String,
        port: u16,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Commands::Enable => enable().await,
        Commands::Disable => disable().await,
        Commands::Status { json } => print_response(send(ControlRequest::Status).await?, json),
        Commands::Probe { host, json } => {
            print_response(send(ControlRequest::Probe { host }).await?, json)
        }
        Commands::Route { mode } => {
            print_response(send(ControlRequest::SetRoute { mode }).await?, false)
        }
        Commands::Exec { command } => exec_command(command),
        Commands::Doctor { json } => doctor(json).await,
        Commands::Restore => {
            restore_tool_integrations()?;
            println!("Git and SSH integrations restored");
            Ok(())
        }
        Commands::SshTunnel { host, port } => ssh_tunnel(&host, port).await,
    }
}

async fn enable() -> Result<()> {
    let mut config = AppConfig::load_or_create(&config_path())?;
    config.enabled = true;
    config.save(&config_path())?;
    let (cli, daemon) = install_runtime_binaries()?;
    install_tool_integrations(&config, &cli)?;
    install_launch_agent(&daemon)?;
    println!("GitHub Link enabled");
    println!("PAC URL: http://127.0.0.1:{}/proxy.pac", config.pac_port);
    Ok(())
}

async fn disable() -> Result<()> {
    let _ = send(ControlRequest::Shutdown).await;
    unload_launch_agent()?;
    let mut config = AppConfig::load_or_create(&config_path())?;
    config.enabled = false;
    config.save(&config_path())?;
    restore_tool_integrations()?;
    println!("GitHub Link disabled and tool integrations restored");
    Ok(())
}

async fn send(request: ControlRequest) -> Result<ControlResponse> {
    let mut stream = UnixStream::connect(socket_path())
        .await
        .context("daemon is not running; run `github-link enable`")?;
    stream
        .write_all(serde_json::to_string(&request)?.as_bytes())
        .await?;
    stream.write_all(b"\n").await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    Ok(serde_json::from_str(&line)?)
}

fn print_response(response: ControlResponse, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    match response {
        ControlResponse::Status { status } => {
            println!("running: {}", status.running);
            println!("mode: {}", status.configured_mode);
            println!("active route: {}", status.active_route);
            println!("proxy: {}", status.proxy_address);
            println!("PAC: {}", status.pac_url);
            if let Some(error) = status.last_error {
                println!("last error: {error}");
            }
        }
        ControlResponse::Ok { message } => println!("{message}"),
        ControlResponse::Error { message } => anyhow::bail!(message),
    }
    Ok(())
}

fn exec_command(command: Vec<String>) -> Result<()> {
    let (program, args) = command.split_first().context("missing command")?;
    let config = AppConfig::load_or_create(&config_path())?;
    let proxy = format!("http://127.0.0.1:{}", config.proxy_port);
    let status = Command::new(program)
        .args(args)
        .env("HTTP_PROXY", &proxy)
        .env("HTTPS_PROXY", &proxy)
        .env("http_proxy", &proxy)
        .env("https_proxy", &proxy)
        .status()?;
    std::process::exit(status.code().unwrap_or(1));
}

async fn doctor(json: bool) -> Result<()> {
    let config = AppConfig::load_or_create(&config_path())?;
    let daemon = send(ControlRequest::Status).await.ok();
    let report = serde_json::json!({
        "config": config_path(),
        "socket": socket_path(),
        "daemon_reachable": daemon.is_some(),
        "proxy_port": config.proxy_port,
        "pac_port": config.pac_port,
        "git_include": github_link_core::git_include_path(),
        "ssh_include": github_link_core::ssh_include_path(),
        "tls_interception": false,
        "sslverify_modified": false
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("daemon reachable: {}", daemon.is_some());
        println!("proxy: 127.0.0.1:{}", config.proxy_port);
        println!("PAC: http://127.0.0.1:{}/proxy.pac", config.pac_port);
        println!("TLS interception: disabled");
    }
    Ok(())
}

async fn ssh_tunnel(host: &str, port: u16) -> Result<()> {
    let config = AppConfig::load_or_create(&config_path())?;
    let mut stream = TcpStream::connect(("127.0.0.1", config.proxy_port)).await?;
    let request = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    while header.len() < 8_192 && !header.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await? == 0 {
            anyhow::bail!("proxy closed connection");
        }
        header.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&header);
    anyhow::ensure!(
        status.lines().next().unwrap_or_default().contains(" 200 "),
        "proxy rejected SSH tunnel: {status}"
    );
    let (mut reader, mut writer) = stream.into_split();
    let upload = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        tokio::io::copy(&mut stdin, &mut writer).await
    });
    let mut stdout = tokio::io::stdout();
    tokio::io::copy(&mut reader, &mut stdout).await?;
    upload.abort();
    Ok(())
}

fn install_launch_agent(daemon: &Path) -> Result<()> {
    let plist = launch_agent_path();
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>com.fengc.github-linkd</string><key>ProgramArguments</key><array><string>{}</string><string>--config</string><string>{}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>StandardOutPath</key><string>{}/launchd.out.log</string><key>StandardErrorPath</key><string>{}/launchd.err.log</string></dict></plist>\n",
        xml_escape(&daemon.to_string_lossy()),
        xml_escape(&config_path().to_string_lossy()),
        xml_escape(&github_link_core::log_dir().to_string_lossy()),
        xml_escape(&github_link_core::log_dir().to_string_lossy())
    );
    fs::create_dir_all(github_link_core::log_dir())?;
    fs::write(&plist, content)?;
    fs::set_permissions(&plist, fs::Permissions::from_mode(0o600))?;
    let domain = format!("gui/{}", uid()?);
    let _ = Command::new("launchctl")
        .args(["bootout", &domain, &plist.to_string_lossy()])
        .status();
    let status = Command::new("launchctl")
        .args(["bootstrap", &domain, &plist.to_string_lossy()])
        .status()?;
    anyhow::ensure!(status.success(), "launchctl bootstrap failed");
    Ok(())
}

fn unload_launch_agent() -> Result<()> {
    let plist = launch_agent_path();
    if plist.exists() {
        let domain = format!("gui/{}", uid()?);
        let _ = Command::new("launchctl")
            .args(["bootout", &domain, &plist.to_string_lossy()])
            .status();
        fs::remove_file(plist)?;
    }
    Ok(())
}

fn install_runtime_binaries() -> Result<(PathBuf, PathBuf)> {
    let current = env::current_exe()?;
    let sibling = current
        .parent()
        .unwrap_or(Path::new("."))
        .join("github-linkd");
    anyhow::ensure!(
        sibling.exists(),
        "missing sibling daemon binary: {}",
        sibling.display()
    );
    let bin_dir = app_support_dir().join("bin");
    fs::create_dir_all(&bin_dir)?;
    let installed_cli = bin_dir.join("github-link");
    let installed_daemon = bin_dir.join("github-linkd");
    copy_executable_if_needed(&current, &installed_cli)?;
    copy_executable_if_needed(&sibling, &installed_daemon)?;
    Ok((installed_cli, installed_daemon))
}

fn copy_executable_if_needed(source: &Path, target: &Path) -> Result<()> {
    let same_file = source.canonicalize().ok() == target.canonicalize().ok();
    if !same_file {
        fs::copy(source, target)?;
    }
    fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn launch_agent_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(app_support_dir)
        .join("Library/LaunchAgents/com.fengc.github-linkd.plist")
}

fn uid() -> Result<String> {
    let output = Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
