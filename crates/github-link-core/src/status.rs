use crate::RouteMode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CandidateHealth {
    pub address: String,
    pub tcp_ms: Option<u64>,
    pub tls_ms: Option<u64>,
    pub score: f64,
    pub success_rate: f64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MirrorCandidate {
    pub host: String,
    pub latency_ms: Option<u64>,
    pub score: f64,
    pub http_status: Option<u16>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DownloadAccelerationStatus {
    pub enabled: bool,
    pub best_mirror: Option<String>,
    pub accelerated_url: Option<String>,
    pub latency_ms: Option<u64>,
    pub checked_at_unix_ms: Option<u64>,
    pub candidates: Vec<MirrorCandidate>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub enabled: bool,
    pub configured_mode: RouteMode,
    pub active_route: String,
    pub proxy_address: String,
    pub pac_url: String,
    pub direct_failures: u32,
    pub last_error: Option<String>,
    pub updated_at_unix_ms: u64,
    pub selected_ips: BTreeMap<String, String>,
    pub candidates: BTreeMap<String, Vec<CandidateHealth>>,
    #[serde(default)]
    pub download_acceleration: DownloadAccelerationStatus,
}

impl DaemonStatus {
    pub fn new(enabled: bool, mode: RouteMode, proxy_port: u16, pac_port: u16) -> Self {
        Self {
            running: true,
            enabled,
            configured_mode: mode,
            active_route: mode.to_string(),
            proxy_address: format!("127.0.0.1:{proxy_port}"),
            pac_url: format!("http://127.0.0.1:{pac_port}/proxy.pac"),
            direct_failures: 0,
            last_error: None,
            updated_at_unix_ms: now_ms(),
            selected_ips: BTreeMap::new(),
            candidates: BTreeMap::new(),
            download_acceleration: DownloadAccelerationStatus::default(),
        }
    }

    pub fn touch(&mut self) {
        self.updated_at_unix_ms = now_ms();
    }
}

pub fn now_unix_ms() -> u64 {
    now_ms()
}

fn now_ms() -> u64 {
    let value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_status_serializes_for_tauri_ipc() {
        let status = DaemonStatus::new(true, RouteMode::Auto, 38_457, 38_458);
        let value = serde_json::to_value(status).unwrap();
        assert!(value["updated_at_unix_ms"].is_u64());
    }
}
