use crate::{AppConfig, backup_dir, git_include_path, ssh_include_path};
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const SSH_MARKER_START: &str = "# >>> github-link managed include >>>";
const SSH_MARKER_END: &str = "# <<< github-link managed include <<<";

pub fn install_tool_integrations(config: &AppConfig, cli_path: &Path) -> Result<()> {
    write_git_include(config)?;
    ensure_git_include_registered()?;
    write_ssh_include(cli_path)?;
    ensure_ssh_include_registered()?;
    Ok(())
}

pub fn restore_tool_integrations() -> Result<()> {
    let git_include = git_include_path();
    let _ = Command::new("git")
        .args([
            "config",
            "--global",
            "--fixed-value",
            "--unset-all",
            "include.path",
            &git_include.to_string_lossy(),
        ])
        .status();
    if git_include.exists() {
        fs::remove_file(&git_include)?;
    }

    let ssh_config = ssh_config_path();
    if ssh_config.exists() {
        backup_file(&ssh_config)?;
        let content = fs::read_to_string(&ssh_config)?;
        let cleaned = remove_marker_block(&content, SSH_MARKER_START, SSH_MARKER_END);
        atomic_write(&ssh_config, cleaned.as_bytes(), 0o600)?;
    }
    let ssh_include = ssh_include_path();
    if ssh_include.exists() {
        fs::remove_file(ssh_include)?;
    }
    Ok(())
}

fn write_git_include(config: &AppConfig) -> Result<()> {
    let path = git_include_path();
    let proxy = format!("http://127.0.0.1:{}", config.proxy_port);
    let hosts = [
        "github.com",
        "api.github.com",
        "gist.github.com",
        "raw.githubusercontent.com",
        "objects.githubusercontent.com",
        "codeload.github.com",
        "release-assets.githubusercontent.com",
        "github-releases.githubusercontent.com",
        "npm.pkg.github.com",
        "ghcr.io",
    ];
    let mut content = String::from("# Managed by GitHub Link. TLS verification remains enabled.\n");
    for host in hosts {
        content.push_str(&format!("[http \"https://{host}/\"]\n\tproxy = {proxy}\n"));
    }
    atomic_write(&path, content.as_bytes(), 0o600)
}

fn ensure_git_include_registered() -> Result<()> {
    let include = git_include_path();
    let output = Command::new("git")
        .args(["config", "--global", "--get-all", "include.path"])
        .output()
        .context("read global Git includes")?;
    let registered = String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| Path::new(line) == include.as_path());
    if registered {
        return Ok(());
    }
    let global = dirs::home_dir().unwrap_or_default().join(".gitconfig");
    if global.exists() {
        backup_file(&global)?;
    }
    let status = Command::new("git")
        .args([
            "config",
            "--global",
            "--add",
            "include.path",
            &include.to_string_lossy(),
        ])
        .status()?;
    anyhow::ensure!(status.success(), "failed to register Git include");
    Ok(())
}

fn write_ssh_include(cli_path: &Path) -> Result<()> {
    let include = ssh_include_path();
    let command = shell_quote(&cli_path.to_string_lossy());
    let content = format!(
        "# Managed by GitHub Link\nHost github.com\n    HostName github.com\n    User git\n    ProxyCommand {command} ssh-tunnel %h %p\n    ConnectTimeout 10\n    ServerAliveInterval 20\n    ServerAliveCountMax 3\n"
    );
    atomic_write(&include, content.as_bytes(), 0o600)
}

fn ensure_ssh_include_registered() -> Result<()> {
    let path = ssh_config_path();
    if path.exists() {
        backup_file(&path)?;
    }
    let original = fs::read_to_string(&path).unwrap_or_default();
    let cleaned = remove_marker_block(&original, SSH_MARKER_START, SSH_MARKER_END);
    let block = format!(
        "{SSH_MARKER_START}\nInclude {}\n{SSH_MARKER_END}\n",
        ssh_include_path().display()
    );
    atomic_write(&path, format!("{block}{cleaned}").as_bytes(), 0o600)
}

fn ssh_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".ssh")
        .join("config")
}

fn backup_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    fs::create_dir_all(backup_dir())?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("config");
    fs::copy(path, backup_dir().join(format!("{name}.{timestamp}.bak")))?;
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    fs::write(&temp, bytes)?;
    fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
    fs::rename(temp, path)?;
    Ok(())
}

fn remove_marker_block(content: &str, start: &str, end: &str) -> String {
    let mut result = Vec::new();
    let mut inside = false;
    for line in content.lines() {
        if line.trim() == start {
            inside = true;
            continue;
        }
        if line.trim() == end {
            inside = false;
            continue;
        }
        if !inside {
            result.push(line);
        }
    }
    let mut value = result.join("\n");
    if !value.is_empty() {
        value.push('\n');
    }
    value
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_managed_ssh_block() {
        let source = format!(
            "Host work\n  HostName x\n{SSH_MARKER_START}\nInclude x\n{SSH_MARKER_END}\nHost home\n"
        );
        let result = remove_marker_block(&source, SSH_MARKER_START, SSH_MARKER_END);
        assert!(result.contains("Host work"));
        assert!(result.contains("Host home"));
        assert!(!result.contains("Include x"));
    }
}
