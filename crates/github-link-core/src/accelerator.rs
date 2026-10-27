use crate::{AppConfig, DownloadAccelerationConfig, MirrorCandidate};
use anyhow::{Context, Result};
use futures::future::join_all;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use url::{Host, Url};

const LATENCY_TEST_URL: &str = "https://raw.githubusercontent.com/microsoft/terminal/refs/heads/main/res/terminal/images/SmallTile.scale-125.png";
const ACTUAL_PROBE_LIMIT: usize = 6;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccelerationResult {
    pub original_url: String,
    pub accelerated_url: String,
    pub mirror: String,
    pub latency_ms: u64,
    pub candidates: Vec<MirrorCandidate>,
}

#[derive(Debug, Default)]
struct MirrorCache {
    refreshed_at: Option<Instant>,
    candidates: Vec<MirrorCandidate>,
}

#[derive(Clone)]
pub struct MirrorAccelerator {
    client: reqwest::Client,
    cache: Arc<RwLock<MirrorCache>>,
}

impl MirrorAccelerator {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(format!("GitHubLink/{}", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::limited(8))
            .build()
            .context("build download accelerator HTTP client")?;
        Ok(Self {
            client,
            cache: Arc::new(RwLock::new(MirrorCache::default())),
        })
    }

    pub async fn candidates(&self, config: &AppConfig) -> Result<Vec<MirrorCandidate>> {
        let settings = &config.download_acceleration;
        anyhow::ensure!(
            settings.enabled,
            "download acceleration is disabled in settings"
        );
        let cache = self.cache.read().await;
        let fresh = cache.refreshed_at.is_some_and(|instant| {
            instant.elapsed() < Duration::from_secs(settings.refresh_seconds.max(60))
        });
        let candidates = cache.candidates.clone();
        drop(cache);
        if fresh {
            return Ok(candidates);
        }
        self.refresh(config).await
    }

    pub async fn refresh(&self, config: &AppConfig) -> Result<Vec<MirrorCandidate>> {
        let settings = &config.download_acceleration;
        anyhow::ensure!(
            settings.enabled,
            "download acceleration is disabled in settings"
        );
        let timeout = Duration::from_secs(settings.timeout_seconds.clamp(2, 20));
        let discovered = match discover_nodes(&self.client, &settings.site_url, timeout).await {
            Ok(nodes) => nodes,
            Err(error) => {
                tracing::warn!(%error, "failed to discover GitHub mirror nodes");
                Vec::new()
            }
        };
        let nodes = merged_nodes(settings, discovered);
        let mut candidates = probe_mirrors(&self.client, nodes, timeout).await;
        candidates.sort_by(compare_candidates);
        let mut cache = self.cache.write().await;
        cache.candidates = candidates.clone();
        cache.refreshed_at = Some(Instant::now());
        Ok(candidates)
    }

    pub async fn accelerate(&self, config: &AppConfig, input: &str) -> Result<AccelerationResult> {
        let original_url = normalize_github_download_url(input)?;
        let candidates = self.candidates(config).await?;
        let timeout =
            Duration::from_secs(config.download_acceleration.timeout_seconds.clamp(2, 20));
        let selected: Vec<MirrorCandidate> = candidates
            .iter()
            .filter(|item| item.score > 0.0)
            .take(ACTUAL_PROBE_LIMIT)
            .cloned()
            .collect();

        if selected.is_empty() {
            anyhow::bail!("no healthy GitHub download mirrors are available");
        }

        let tested = join_all(selected.iter().map(|candidate| {
            probe_accelerated_url(
                &self.client,
                candidate.host.clone(),
                original_url.clone(),
                timeout,
            )
        }))
        .await;
        let mut measured_candidates = tested.clone();
        measured_candidates.sort_by(compare_candidates);

        let best = tested
            .iter()
            .filter(|item| item.score > 0.0)
            .min_by_key(|item| item.latency_ms.unwrap_or(u64::MAX))
            .cloned()
            .context("all selected GitHub download mirrors failed for this URL")?;
        let accelerated_url = mirror_url(&best.host, &original_url)?;

        let mut cache = self.cache.write().await;
        for candidate in &mut cache.candidates {
            if let Some(measured) = tested.iter().find(|item| item.host == candidate.host) {
                *candidate = measured.clone();
            }
        }
        cache.candidates.sort_by(compare_candidates);
        drop(cache);

        Ok(AccelerationResult {
            original_url,
            accelerated_url,
            mirror: best.host,
            latency_ms: best.latency_ms.unwrap_or_default(),
            candidates: measured_candidates,
        })
    }
}

pub fn normalize_github_download_url(input: &str) -> Result<String> {
    let trimmed = input.trim();
    anyhow::ensure!(!trimmed.is_empty(), "GitHub download URL is empty");
    let candidate = if trimmed.starts_with("https://") {
        trimmed.to_owned()
    } else if trimmed.starts_with("http://") {
        anyhow::bail!("only HTTPS GitHub download URLs are accepted");
    } else {
        format!("https://{trimmed}")
    };
    let mut url = Url::parse(&candidate).context("invalid GitHub download URL")?;
    anyhow::ensure!(url.scheme() == "https", "only HTTPS URLs are accepted");
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URLs containing credentials are not accepted"
    );
    let host = url
        .host_str()
        .context("GitHub download URL has no host")?
        .to_ascii_lowercase();
    let path = url.path().to_ascii_lowercase();
    let supported = match host.as_str() {
        "github.com" => ["/releases/download/", "/archive/", "/raw/", "/blob/"]
            .iter()
            .any(|segment| path.contains(segment)),
        "raw.githubusercontent.com" | "gist.githubusercontent.com" | "codeload.github.com" => true,
        _ => false,
    };
    anyhow::ensure!(
        supported,
        "only public GitHub release, archive, raw, gist, or codeload URLs are supported"
    );
    url.set_fragment(None);
    Ok(url.to_string())
}

pub fn mirror_url(mirror: &str, original_url: &str) -> Result<String> {
    let host = normalize_mirror_host(mirror)?;
    let parsed = Url::parse(&format!("https://{host}/{original_url}"))
        .with_context(|| format!("build accelerated URL for mirror {host}"))?;
    Ok(parsed.to_string())
}

pub fn is_accelerated_github_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if url.scheme() != "https" || url.username() != "" || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if normalize_mirror_host(host).is_err() {
        return false;
    }
    normalize_github_download_url(url.path().strip_prefix('/').unwrap_or_default()).is_ok()
}

pub fn discover_nodes_from_html(html: &str) -> Vec<String> {
    static NODE_PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = NODE_PATTERN.get_or_init(|| {
        Regex::new(r"\[(?:贡献|测绘)\]</span></span><span[^>]*>([^<]+)</span>")
            .expect("valid mirror discovery regex")
    });
    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    for captures in pattern.captures_iter(html) {
        let Some(raw) = captures.get(1).map(|value| value.as_str().trim()) else {
            continue;
        };
        let Ok(host) = normalize_mirror_host(raw) else {
            continue;
        };
        if seen.insert(host.clone()) {
            nodes.push(host);
        }
    }
    nodes
}

async fn discover_nodes(
    client: &reqwest::Client,
    site_url: &str,
    timeout_duration: Duration,
) -> Result<Vec<String>> {
    let url = Url::parse(site_url).context("invalid mirror discovery site URL")?;
    anyhow::ensure!(
        url.scheme() == "https",
        "mirror discovery site must use HTTPS"
    );
    let response = tokio::time::timeout(timeout_duration, client.get(url).send())
        .await
        .context("mirror discovery site timed out")?
        .context("request mirror discovery site")?;
    anyhow::ensure!(
        response.status().is_success(),
        "mirror discovery site returned HTTP {}",
        response.status()
    );
    let html = tokio::time::timeout(timeout_duration, response.text())
        .await
        .context("mirror discovery page body timed out")?
        .context("read mirror discovery page")?;
    let nodes = discover_nodes_from_html(&html);
    anyhow::ensure!(!nodes.is_empty(), "mirror discovery found no nodes");
    Ok(nodes)
}

fn merged_nodes(settings: &DownloadAccelerationConfig, discovered: Vec<String>) -> Vec<String> {
    let limit = settings.max_nodes.clamp(1, 32);
    let mut seen = HashSet::new();
    settings
        .nodes
        .iter()
        .chain(discovered.iter())
        .filter_map(|value| normalize_mirror_host(value).ok())
        .filter(|host| seen.insert(host.clone()))
        .take(limit)
        .collect()
}

async fn probe_mirrors(
    client: &reqwest::Client,
    nodes: Vec<String>,
    timeout_duration: Duration,
) -> Vec<MirrorCandidate> {
    join_all(
        nodes
            .into_iter()
            .map(|host| probe_mirror_latency(client, host, timeout_duration)),
    )
    .await
}

async fn probe_mirror_latency(
    client: &reqwest::Client,
    host: String,
    timeout_duration: Duration,
) -> MirrorCandidate {
    let url = format!("https://{host}/{LATENCY_TEST_URL}");
    let started = Instant::now();
    let result = tokio::time::timeout(timeout_duration, async {
        let response = client
            .get(&url)
            .header(reqwest::header::CACHE_CONTROL, "no-cache")
            .send()
            .await?;
        let status = response.status().as_u16();
        anyhow::ensure!(response.status().is_success(), "HTTP {status}");
        let body = response.bytes().await?;
        anyhow::ensure!(!body.is_empty(), "empty response");
        Ok::<u16, anyhow::Error>(status)
    })
    .await;

    match result {
        Ok(Ok(status)) => {
            let latency_ms = elapsed_ms(started);
            MirrorCandidate {
                host,
                latency_ms: Some(latency_ms),
                score: mirror_score(latency_ms),
                http_status: Some(status),
                last_error: None,
            }
        }
        Ok(Err(error)) => failed_candidate(host, started, error.to_string()),
        Err(_) => failed_candidate(host, started, "probe timed out".into()),
    }
}

async fn probe_accelerated_url(
    client: &reqwest::Client,
    host: String,
    original_url: String,
    timeout_duration: Duration,
) -> MirrorCandidate {
    let url = match mirror_url(&host, &original_url) {
        Ok(url) => url,
        Err(error) => return failed_candidate(host, Instant::now(), error.to_string()),
    };
    let started = Instant::now();
    let result = tokio::time::timeout(timeout_duration, async {
        let response = client
            .get(&url)
            .header(reqwest::header::RANGE, "bytes=0-1023")
            .header(reqwest::header::CACHE_CONTROL, "no-cache")
            .send()
            .await?;
        let status = response.status();
        anyhow::ensure!(status.is_success(), "HTTP {}", status.as_u16());
        anyhow::ensure!(
            response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .is_none_or(|value| value != "0"),
            "empty response"
        );
        Ok::<u16, anyhow::Error>(status.as_u16())
    })
    .await;

    match result {
        Ok(Ok(status)) => {
            let latency_ms = elapsed_ms(started);
            MirrorCandidate {
                host,
                latency_ms: Some(latency_ms),
                score: mirror_score(latency_ms),
                http_status: Some(status),
                last_error: None,
            }
        }
        Ok(Err(error)) => failed_candidate(host, started, error.to_string()),
        Err(_) => failed_candidate(host, started, "download probe timed out".into()),
    }
}

fn normalize_mirror_host(value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches('.');
    anyhow::ensure!(!value.is_empty(), "empty mirror host");
    anyhow::ensure!(
        !value.contains(['/', '?', '#', '@', ':']),
        "mirror must be a bare hostname"
    );
    let url = Url::parse(&format!("https://{value}/")).context("invalid mirror hostname")?;
    anyhow::ensure!(
        url.host()
            .is_some_and(|host| matches!(host, Host::Domain(_))),
        "mirror hostname must be a DNS name"
    );
    anyhow::ensure!(
        url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
        "mirror must not include a path, query, or fragment"
    );
    let host = url
        .host_str()
        .context("mirror hostname is missing")?
        .to_ascii_lowercase();
    anyhow::ensure!(
        host != "localhost"
            && !host.ends_with(".localhost")
            && !host.ends_with(".local")
            && host.contains('.'),
        "local or invalid mirror hostname"
    );
    Ok(host)
}

fn failed_candidate(host: String, started: Instant, error: String) -> MirrorCandidate {
    MirrorCandidate {
        host,
        latency_ms: Some(elapsed_ms(started)),
        score: 0.0,
        http_status: None,
        last_error: Some(error),
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn mirror_score(latency_ms: u64) -> f64 {
    let penalty = (latency_ms as f64 / 40.0).min(100.0);
    (100.0 - penalty).max(0.0)
}

fn compare_candidates(left: &MirrorCandidate, right: &MirrorCandidate) -> std::cmp::Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| left.latency_ms.cmp(&right.latency_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_public_download_urls() {
        assert_eq!(
            normalize_github_download_url(
                "github.com/hylf2/github-link/releases/download/v0.1.1/file.zip"
            )
            .unwrap(),
            "https://github.com/hylf2/github-link/releases/download/v0.1.1/file.zip"
        );
        assert!(normalize_github_download_url("https://api.github.com/user").is_err());
        assert!(normalize_github_download_url("http://github.com/a/b/archive/main.zip").is_err());
        assert!(
            normalize_github_download_url("https://token@github.com/a/b/archive/main.zip").is_err()
        );
    }

    #[test]
    fn discovers_and_deduplicates_site_nodes() {
        let html = r#"
          <span>[贡献]</span></span><span class="host">cdn.example.com</span>
          <span>[测绘]</span></span><span class="host">gh.example.net</span>
          <span>[贡献]</span></span><span class="host">cdn.example.com</span>
        "#;
        assert_eq!(
            discover_nodes_from_html(html),
            vec!["cdn.example.com", "gh.example.net"]
        );
    }

    #[test]
    fn builds_and_validates_accelerated_urls() {
        let value = mirror_url(
            "gh-proxy.com",
            "https://github.com/a/b/releases/download/v1/file.zip",
        )
        .unwrap();
        assert_eq!(
            value,
            "https://gh-proxy.com/https://github.com/a/b/releases/download/v1/file.zip"
        );
        assert!(is_accelerated_github_url(&value));
        assert!(!is_accelerated_github_url(
            "https://gh-proxy.com/https://api.github.com/user"
        ));
    }

    #[tokio::test]
    #[ignore = "requires network access to github.akams.cn and public mirror nodes"]
    async fn live_site_smoke_selects_release_mirror() {
        let mut config = AppConfig::default();
        config.download_acceleration.enabled = true;
        config.download_acceleration.max_nodes = 10;
        config.download_acceleration.timeout_seconds = 10;
        let accelerator = MirrorAccelerator::new().unwrap();
        let result = accelerator
            .accelerate(
                &config,
                "https://github.com/hylf2/github-link/releases/download/v0.1.1/GitHub-Link-v0.1.1-macos-arm64.zip",
            )
            .await
            .unwrap();
        eprintln!(
            "selected mirror={} latency_ms={} candidates={}",
            result.mirror,
            result.latency_ms,
            result.candidates.len()
        );
        assert!(is_accelerated_github_url(&result.accelerated_url));
        assert!(result.latency_ms > 0);
    }
}
