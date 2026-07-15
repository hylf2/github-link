use crate::{AppConfig, CandidateHealth, is_public_ip};
use anyhow::{Context, Result};
use futures::future::join_all;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsConnector;

#[derive(Clone)]
pub struct IpSelector {
    config: Arc<RwLock<AppConfig>>,
    client: reqwest::Client,
    cache: Arc<RwLock<HashMap<String, CacheEntry>>>,
}

#[derive(Clone)]
struct CacheEntry {
    candidates: Vec<ScoredAddress>,
    expires_at: Instant,
}

#[derive(Clone)]
struct ScoredAddress {
    address: SocketAddr,
    health: CandidateHealth,
}

#[derive(Debug, Deserialize)]
struct DohResponse {
    #[serde(rename = "Answer", default)]
    answers: Vec<DohAnswer>,
}

#[derive(Debug, Deserialize)]
struct DohAnswer {
    #[serde(rename = "type")]
    record_type: u16,
    data: String,
}

impl IpSelector {
    pub fn new(config: Arc<RwLock<AppConfig>>) -> Result<Self> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(2))
            .user_agent("GitHubLink/0.1")
            .build()?;
        Ok(Self {
            config,
            client,
            cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub async fn connect(&self, host: &str, port: u16) -> Result<(TcpStream, SocketAddr)> {
        let candidates = self.candidates(host, port, false).await?;
        match self.race(candidates).await {
            Ok((stream, address, tcp_ms)) => {
                self.mark_connection_success(host, port, address, tcp_ms)
                    .await;
                Ok((stream, address))
            }
            Err(first_error) => {
                self.invalidate(host, port).await;
                let refreshed = self.candidates(host, port, true).await?;
                let (stream, address, tcp_ms) = self
                    .race(refreshed)
                    .await
                    .with_context(|| format!("initial candidates failed: {first_error}"))?;
                self.mark_connection_success(host, port, address, tcp_ms)
                    .await;
                Ok((stream, address))
            }
        }
    }

    pub async fn probe(&self, host: &str, port: u16) -> Result<Vec<CandidateHealth>> {
        let candidates = self.candidates(host, port, true).await?;
        Ok(candidates.into_iter().map(|item| item.health).collect())
    }

    pub async fn snapshot(&self) -> BTreeMap<String, Vec<CandidateHealth>> {
        self.cache
            .read()
            .await
            .iter()
            .map(|(host, entry)| {
                (
                    host.clone(),
                    entry
                        .candidates
                        .iter()
                        .map(|item| item.health.clone())
                        .collect(),
                )
            })
            .collect()
    }

    pub async fn invalidate(&self, host: &str, port: u16) {
        self.cache.write().await.remove(&cache_key(host, port));
    }

    async fn candidates(&self, host: &str, port: u16, force: bool) -> Result<Vec<ScoredAddress>> {
        let key = cache_key(host, port);
        let cached = if force {
            None
        } else {
            self.cache
                .read()
                .await
                .get(&key)
                .filter(|entry| entry.expires_at > Instant::now() && !entry.candidates.is_empty())
                .cloned()
        };
        if let Some(entry) = cached {
            return Ok(entry.candidates);
        }

        let config = self.config.read().await.clone();
        let addresses = self.resolve_all(host, port, &config).await?;
        if addresses.is_empty() {
            anyhow::bail!("no public IP address resolved for {host}");
        }

        let mut tasks = JoinSet::new();
        for address in addresses {
            let host = host.to_owned();
            tasks.spawn(async move { probe_address(&host, address).await });
        }

        let mut scored = Vec::new();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(item) => scored.push(item),
                Err(error) => tracing::warn!(%error, host, "candidate probe task failed"),
            }
        }
        if scored.is_empty() {
            anyhow::bail!("all candidate probe tasks failed for {host}");
        }
        scored.sort_by(|a, b| b.health.score.total_cmp(&a.health.score));
        scored.truncate(3);

        let entry = CacheEntry {
            candidates: scored.clone(),
            expires_at: Instant::now() + Duration::from_secs(config.cache_seconds),
        };
        self.cache.write().await.insert(key, entry);
        Ok(scored)
    }

    async fn resolve_all(
        &self,
        host: &str,
        port: u16,
        config: &AppConfig,
    ) -> Result<Vec<SocketAddr>> {
        let mut addresses = HashSet::new();
        if let Ok(Ok(system)) = timeout(
            Duration::from_secs(2),
            tokio::net::lookup_host((host, port)),
        )
        .await
        {
            addresses.extend(system.filter(|address| is_public_ip(address.ip())));
        }

        let queries = config.doh_providers.iter().flat_map(|provider| {
            [1_u16, 28_u16].map(|record_type| self.query_doh(provider, host, record_type, port))
        });
        for result in join_all(queries).await.into_iter().flatten() {
            addresses.extend(result);
        }

        let mut values: Vec<_> = addresses.into_iter().collect();
        values.sort_by_key(|address| (!address.is_ipv6(), address.ip()));
        Ok(values)
    }

    async fn query_doh(
        &self,
        provider: &str,
        host: &str,
        record_type: u16,
        port: u16,
    ) -> Result<Vec<SocketAddr>> {
        let response = self
            .client
            .get(provider)
            .query(&[
                ("name", host),
                ("type", if record_type == 1 { "A" } else { "AAAA" }),
            ])
            .header("accept", "application/dns-json")
            .send()
            .await?
            .error_for_status()?
            .json::<DohResponse>()
            .await?;
        Ok(response
            .answers
            .into_iter()
            .filter(|answer| answer.record_type == record_type)
            .filter_map(|answer| answer.data.parse::<IpAddr>().ok())
            .filter(|ip| is_public_ip(*ip))
            .map(|ip| SocketAddr::new(ip, port))
            .collect())
    }

    async fn race(&self, candidates: Vec<ScoredAddress>) -> Result<(TcpStream, SocketAddr, u64)> {
        let mut tasks = JoinSet::new();
        for (index, candidate) in candidates.into_iter().take(2).enumerate() {
            tasks.spawn(async move {
                sleep(Duration::from_millis(index as u64 * 150)).await;
                let started = Instant::now();
                let stream = timeout(
                    Duration::from_secs(5),
                    TcpStream::connect(candidate.address),
                )
                .await
                .context("TCP connect timeout")??;
                Ok::<_, anyhow::Error>((
                    stream,
                    candidate.address,
                    started.elapsed().as_millis() as u64,
                ))
            });
        }
        while let Some(result) = tasks.join_next().await {
            if let Ok(Ok(value)) = result {
                tasks.abort_all();
                return Ok(value);
            }
        }
        anyhow::bail!("all candidate IP connections failed")
    }

    async fn mark_connection_success(
        &self,
        host: &str,
        port: u16,
        address: SocketAddr,
        tcp_ms: u64,
    ) {
        let key = cache_key(host, port);
        if let Some(entry) = self.cache.write().await.get_mut(&key)
            && let Some(candidate) = entry
                .candidates
                .iter_mut()
                .find(|candidate| candidate.address == address)
        {
            candidate.health.tcp_ms = Some(tcp_ms);
            candidate.health.score = connection_score(tcp_ms, candidate.health.tls_ms);
            candidate.health.success_rate = 1.0;
            candidate.health.last_error = None;
            entry
                .candidates
                .sort_by(|a, b| b.health.score.total_cmp(&a.health.score));
        }
    }
}

async fn probe_address(host: &str, address: SocketAddr) -> ScoredAddress {
    let started = Instant::now();
    let tcp = timeout(Duration::from_secs(5), TcpStream::connect(address)).await;
    let Ok(Ok(stream)) = tcp else {
        return failed_candidate(address, "TCP connection failed");
    };
    let tcp_ms = started.elapsed().as_millis() as u64;

    if address.port() != 443 {
        return healthy_candidate(address, tcp_ms, None);
    }

    let tls_started = Instant::now();
    let mut roots = RootCertStore::empty();
    let certs = rustls_native_certs::load_native_certs();
    for cert in certs.certs {
        let _ = roots.add(cert);
    }
    let tls_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let Ok(server_name) = ServerName::try_from(host.to_owned()) else {
        return failed_candidate(address, "invalid TLS server name");
    };
    let connector = TlsConnector::from(Arc::new(tls_config));
    match timeout(
        Duration::from_secs(5),
        connector.connect(server_name, stream),
    )
    .await
    {
        Ok(Ok(_)) => healthy_candidate(
            address,
            tcp_ms,
            Some(tls_started.elapsed().as_millis() as u64),
        ),
        _ => failed_candidate(address, "TLS certificate or handshake failed"),
    }
}

fn healthy_candidate(address: SocketAddr, tcp_ms: u64, tls_ms: Option<u64>) -> ScoredAddress {
    let score = connection_score(tcp_ms, tls_ms);
    ScoredAddress {
        address,
        health: CandidateHealth {
            address: address.ip().to_string(),
            tcp_ms: Some(tcp_ms),
            tls_ms,
            score,
            success_rate: 1.0,
            last_error: None,
        },
    }
}

fn connection_score(tcp_ms: u64, tls_ms: Option<u64>) -> f64 {
    let total = tcp_ms + tls_ms.unwrap_or_default();
    (100.0 - (total.min(3_000) as f64 / 30.0)).max(1.0)
}

fn failed_candidate(address: SocketAddr, error: &str) -> ScoredAddress {
    ScoredAddress {
        address,
        health: CandidateHealth {
            address: address.ip().to_string(),
            tcp_ms: None,
            tls_ms: None,
            score: 0.0,
            success_rate: 0.0,
            last_error: Some(error.to_owned()),
        },
    }
}

fn cache_key(host: &str, port: u16) -> String {
    format!("{}:{port}", host.to_ascii_lowercase())
}
