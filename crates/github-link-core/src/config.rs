use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

pub const DEFAULT_PROXY_PORT: u16 = 38_457;
pub const DEFAULT_PAC_PORT: u16 = 38_458;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RouteMode {
    #[default]
    Auto,
    OptimizedDirect,
    Upstream,
    PlainDirect,
}

impl std::fmt::Display for RouteMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Auto => "auto",
            Self::OptimizedDirect => "optimized_direct",
            Self::Upstream => "upstream",
            Self::PlainDirect => "plain_direct",
        };
        f.write_str(value)
    }
}

impl std::str::FromStr for RouteMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "optimized" | "optimized_direct" => Ok(Self::OptimizedDirect),
            "upstream" => Ok(Self::Upstream),
            "direct" | "plain_direct" => Ok(Self::PlainDirect),
            _ => anyhow::bail!("unknown route mode: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamKind {
    Http,
    Socks5,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    pub kind: UpstreamKind,
    pub host: String,
    pub port: u16,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub route_mode: RouteMode,
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
    #[serde(default = "default_pac_port")]
    pub pac_port: u16,
    #[serde(default = "default_domains")]
    pub domains: Vec<String>,
    #[serde(default = "default_doh_providers")]
    pub doh_providers: Vec<String>,
    #[serde(default)]
    pub upstream: Option<UpstreamConfig>,
    #[serde(default = "default_refresh_seconds")]
    pub refresh_seconds: u64,
    #[serde(default = "default_cache_seconds")]
    pub cache_seconds: u64,
    #[serde(default = "default_quarantine_seconds")]
    pub quarantine_seconds: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: schema_version(),
            enabled: false,
            route_mode: RouteMode::Auto,
            proxy_port: default_proxy_port(),
            pac_port: default_pac_port(),
            domains: default_domains(),
            doh_providers: default_doh_providers(),
            upstream: None,
            refresh_seconds: default_refresh_seconds(),
            cache_seconds: default_cache_seconds(),
            quarantine_seconds: default_quarantine_seconds(),
        }
    }
}

impl AppConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            let value = Self::default();
            value.save(path)?;
            return Ok(value);
        }
        let raw =
            fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("toml.tmp");
        fs::write(&temp, toml::to_string_pretty(self)?)?;
        fs::rename(&temp, path)?;
        Ok(())
    }

    pub fn allows(&self, host: &str, port: u16) -> bool {
        let port_allowed = port == 443 || port == 22;
        port_allowed
            && self
                .domains
                .iter()
                .any(|pattern| domain_matches(pattern, host))
    }

    pub fn pac_script(&self) -> String {
        let mut rules = String::new();
        for domain in &self.domains {
            rules.push_str(&format!(
                "  if (shExpMatch(host, '{}')) return proxy;\n",
                domain.replace('\'', "")
            ));
        }
        format!(
            "function FindProxyForURL(url, host) {{\n  var proxy = 'PROXY 127.0.0.1:{}';\n  if (url.substring(0, 6) !== 'https:') return 'DIRECT';\n{}  return 'DIRECT';\n}}\n",
            self.proxy_port, rules
        )
    }
}

pub fn app_support_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("GitHubLink")
}

pub fn config_path() -> PathBuf {
    app_support_dir().join("config.toml")
}

pub fn socket_path() -> PathBuf {
    app_support_dir().join("control.sock")
}

pub fn log_dir() -> PathBuf {
    app_support_dir().join("logs")
}

pub fn backup_dir() -> PathBuf {
    app_support_dir().join("backups")
}

pub fn git_include_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("github-link")
        .join("gitconfig")
}

pub fn ssh_include_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
        .join("config.d")
        .join("github-link")
}

pub fn domain_matches(pattern: &str, host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let pattern = pattern.trim_end_matches('.').to_ascii_lowercase();
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return host != suffix && host.ends_with(&format!(".{suffix}"));
    }
    host == pattern
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.octets()[0] == 0)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (ip.segments()[0] & 0xfe00) == 0xfc00
        || (ip.segments()[0] & 0xffc0) == 0xfe80)
}

fn schema_version() -> u32 {
    1
}
fn default_true() -> bool {
    true
}
fn default_proxy_port() -> u16 {
    DEFAULT_PROXY_PORT
}
fn default_pac_port() -> u16 {
    DEFAULT_PAC_PORT
}
fn default_refresh_seconds() -> u64 {
    300
}
fn default_cache_seconds() -> u64 {
    600
}
fn default_quarantine_seconds() -> u64 {
    60
}

fn default_doh_providers() -> Vec<String> {
    vec![
        "https://doh.pub/dns-query".into(),
        "https://dns.alidns.com/resolve".into(),
        "https://cloudflare-dns.com/dns-query".into(),
    ]
}

fn default_domains() -> Vec<String> {
    vec![
        "github.com",
        "api.github.com",
        "gist.github.com",
        "ssh.github.com",
        "*.github.com",
        "*.github.io",
        "*.githubassets.com",
        "*.githubusercontent.com",
        "*.githubapp.com",
        "ghcr.io",
        "*.ghcr.io",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_requires_a_subdomain() {
        assert!(domain_matches(
            "*.githubusercontent.com",
            "raw.githubusercontent.com"
        ));
        assert!(!domain_matches("*.github.com", "github.com"));
    }

    #[test]
    fn private_addresses_are_rejected() {
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("10.0.0.1".parse().unwrap()));
        assert!(is_public_ip("140.82.113.3".parse().unwrap()));
    }
}
