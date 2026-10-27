use anyhow::{Context, Result};
use bytes::Bytes;
use github_link_core::{
    AccelerationResult, AppConfig, DaemonStatus, IpSelector, MirrorAccelerator, MirrorCandidate,
    RouteMode, UpstreamConfig, UpstreamKind, now_unix_ms,
};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio_socks::tcp::Socks5Stream;
use tracing::{info, warn};

#[derive(Clone)]
pub struct ProxyRuntime {
    pub config: Arc<RwLock<AppConfig>>,
    pub selector: IpSelector,
    pub accelerator: MirrorAccelerator,
    pub status: Arc<RwLock<DaemonStatus>>,
}

impl ProxyRuntime {
    pub fn new(config: AppConfig) -> Result<Self> {
        let mut initial_status = DaemonStatus::new(
            config.enabled,
            config.route_mode,
            config.proxy_port,
            config.pac_port,
        );
        initial_status.download_acceleration.enabled = config.download_acceleration.enabled;
        let status = Arc::new(RwLock::new(initial_status));
        let config = Arc::new(RwLock::new(config));
        let selector = IpSelector::new(config.clone())?;
        let accelerator = MirrorAccelerator::new()?;
        Ok(Self {
            config,
            selector,
            accelerator,
            status,
        })
    }

    pub async fn run_proxy(self) -> Result<()> {
        let port = self.config.read().await.proxy_port;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("bind CONNECT proxy on 127.0.0.1:{port}"))?;
        info!(port, "CONNECT proxy listening");
        loop {
            let (stream, peer) = listener.accept().await?;
            if !peer.ip().is_loopback() {
                warn!(%peer, "rejected non-loopback proxy client");
                continue;
            }
            let runtime = self.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| handle_proxy(request, runtime.clone()));
                if let Err(error) = http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await
                {
                    warn!(%error, "proxy connection ended with error");
                }
            });
        }
    }

    pub async fn run_pac(self) -> Result<()> {
        let port = self.config.read().await.pac_port;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("bind PAC server on 127.0.0.1:{port}"))?;
        info!(port, "PAC server listening");
        loop {
            let (stream, peer) = listener.accept().await?;
            if !peer.ip().is_loopback() {
                continue;
            }
            let runtime = self.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| handle_pac(request, runtime.clone()));
                if let Err(error) = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                {
                    warn!(%error, "PAC connection ended with error");
                }
            });
        }
    }

    pub async fn probe_host(&self, host: &str) -> Result<()> {
        let health = self.selector.probe(host, 443).await?;
        let mut status = self.status.write().await;
        if let Some(best) = health.iter().find(|item| item.success_rate > 0.0) {
            status
                .selected_ips
                .insert(host.to_owned(), best.address.clone());
        }
        status.candidates.insert(host.to_owned(), health);
        status.last_error = None;
        status.touch();
        Ok(())
    }

    pub async fn refresh_status(&self) {
        let snapshot = self.selector.snapshot().await;
        let mut status = self.status.write().await;
        status.candidates = snapshot;
        status.touch();
    }

    pub async fn refresh_mirrors(&self) -> Result<Vec<MirrorCandidate>> {
        let config = self.config.read().await.clone();
        let result = self.accelerator.refresh(&config).await;
        let mut status = self.status.write().await;
        status.download_acceleration.enabled = config.download_acceleration.enabled;
        status.download_acceleration.checked_at_unix_ms = Some(now_unix_ms());
        match result {
            Ok(candidates) => {
                status.download_acceleration.best_mirror =
                    candidates.first().map(|item| item.host.clone());
                status.download_acceleration.candidates = candidates.clone();
                status.download_acceleration.last_error = None;
                status.touch();
                Ok(candidates)
            }
            Err(error) => {
                status.download_acceleration.last_error = Some(error.to_string());
                status.touch();
                Err(error)
            }
        }
    }

    pub async fn accelerate_download(&self, input: &str) -> Result<AccelerationResult> {
        let config = self.config.read().await.clone();
        let result = self.accelerator.accelerate(&config, input).await;
        let mut status = self.status.write().await;
        status.download_acceleration.enabled = config.download_acceleration.enabled;
        status.download_acceleration.checked_at_unix_ms = Some(now_unix_ms());
        match result {
            Ok(result) => {
                status.download_acceleration.best_mirror = Some(result.mirror.clone());
                status.download_acceleration.accelerated_url = Some(result.accelerated_url.clone());
                status.download_acceleration.latency_ms = Some(result.latency_ms);
                status.download_acceleration.candidates = result.candidates.clone();
                status.download_acceleration.last_error = None;
                status.touch();
                Ok(result)
            }
            Err(error) => {
                status.download_acceleration.last_error = Some(error.to_string());
                status.touch();
                Err(error)
            }
        }
    }

    async fn connect_target(&self, host: &str, port: u16) -> Result<TcpStream> {
        let config = self.config.read().await.clone();
        let result = match config.route_mode {
            RouteMode::OptimizedDirect => self
                .optimized_direct(host, port)
                .await
                .map(|value| (value, "optimized_direct")),
            RouteMode::PlainDirect => plain_direct(host, port)
                .await
                .map(|value| (value, "plain_direct")),
            RouteMode::Upstream => self
                .upstream(host, port, config.upstream.as_ref())
                .await
                .map(|value| (value, "upstream")),
            RouteMode::Auto => match self.optimized_direct(host, port).await {
                Ok(stream) => Ok((stream, "optimized_direct")),
                Err(direct_error) => {
                    self.record_failure(&direct_error).await;
                    match self.upstream(host, port, config.upstream.as_ref()).await {
                        Ok(stream) => Ok((stream, "upstream")),
                        Err(upstream_error) => Err(anyhow::anyhow!(
                            "direct failed: {direct_error}; upstream failed: {upstream_error}"
                        )),
                    }
                }
            },
        };

        match result {
            Ok((stream, route)) => {
                let mut status = self.status.write().await;
                status.active_route = route.to_owned();
                status.direct_failures = if route == "optimized_direct" {
                    0
                } else {
                    status.direct_failures
                };
                status.last_error = None;
                status.touch();
                Ok(stream)
            }
            Err(error) => {
                self.record_failure(&error).await;
                Err(error)
            }
        }
    }

    async fn optimized_direct(&self, host: &str, port: u16) -> Result<TcpStream> {
        match self.selector.connect(host, port).await {
            Ok((stream, address)) => {
                let mut status = self.status.write().await;
                status
                    .selected_ips
                    .insert(format!("{host}:{port}"), address.ip().to_string());
                Ok(stream)
            }
            Err(error) if host.eq_ignore_ascii_case("github.com") && port == 22 => {
                let (stream, address) = self.selector.connect("ssh.github.com", 443).await?;
                let mut status = self.status.write().await;
                status
                    .selected_ips
                    .insert("github.com:22".into(), address.ip().to_string());
                status.last_error = Some(format!(
                    "SSH 22 unavailable, using ssh.github.com:443 ({error})"
                ));
                Ok(stream)
            }
            Err(error) => Err(error),
        }
    }

    async fn upstream(
        &self,
        host: &str,
        port: u16,
        upstream: Option<&UpstreamConfig>,
    ) -> Result<TcpStream> {
        let upstream = upstream
            .filter(|item| item.enabled)
            .context("no enabled upstream configured")?;
        match upstream.kind {
            UpstreamKind::Http => connect_http_upstream(upstream, host, port).await,
            UpstreamKind::Socks5 => {
                let proxy = (upstream.host.as_str(), upstream.port);
                Ok(Socks5Stream::connect(proxy, (host, port))
                    .await?
                    .into_inner())
            }
        }
    }

    async fn record_failure(&self, error: &anyhow::Error) {
        let mut status = self.status.write().await;
        status.direct_failures = status.direct_failures.saturating_add(1);
        status.last_error = Some(error.to_string());
        status.touch();
    }
}

async fn handle_proxy(
    mut request: Request<Incoming>,
    runtime: ProxyRuntime,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if request.method() != Method::CONNECT {
        return Ok(text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "CONNECT only",
        ));
    }
    let Some(authority) = request.uri().authority().cloned() else {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "missing CONNECT authority",
        ));
    };
    let host = authority.host().to_owned();
    let port = authority.port_u16().unwrap_or(443);
    if !runtime.config.read().await.allows(&host, port) {
        return Ok(text_response(
            StatusCode::FORBIDDEN,
            "target is not on the GitHub allowlist",
        ));
    }

    let on_upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let result = async {
            let upgraded = on_upgrade.await?;
            let mut client = TokioIo::new(upgraded);
            if port == 443 {
                relay_tls_with_retry(&mut client, &runtime, &host, port).await?;
            } else {
                let mut upstream = runtime.connect_target(&host, port).await?;
                copy_bidirectional(&mut client, &mut upstream).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            runtime.record_failure(&error).await;
            warn!(%host, port, %error, "CONNECT tunnel failed");
        }
    });
    Ok(text_response(StatusCode::OK, ""))
}

async fn relay_tls_with_retry<S>(
    client: &mut S,
    runtime: &ProxyRuntime,
    host: &str,
    port: u16,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut tls_header = [0_u8; 5];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut tls_header))
        .await
        .context("client did not send TLS ClientHello header")??;
    anyhow::ensure!(
        tls_header[0] == 0x16,
        "first tunneled record is not a TLS handshake"
    );
    let payload_len = u16::from_be_bytes([tls_header[3], tls_header[4]]) as usize;
    anyhow::ensure!(
        payload_len <= 32 * 1024,
        "TLS ClientHello record is too large"
    );
    let mut client_hello = Vec::with_capacity(5 + payload_len);
    client_hello.extend_from_slice(&tls_header);
    client_hello.resize(5 + payload_len, 0);
    tokio::time::timeout(
        Duration::from_secs(5),
        client.read_exact(&mut client_hello[5..]),
    )
    .await
    .context("client did not finish TLS ClientHello")??;

    let mut errors = Vec::new();
    for attempt in 0..2 {
        if attempt > 0 {
            runtime.selector.invalidate(host, port).await;
        }
        let result = async {
            let mut upstream = runtime.connect_target(host, port).await?;
            upstream.write_all(&client_hello).await?;
            let mut server_hello = vec![0_u8; 32 * 1024];
            let length =
                tokio::time::timeout(Duration::from_secs(3), upstream.read(&mut server_hello))
                    .await
                    .context("upstream TLS first-byte timeout")??;
            anyhow::ensure!(length > 0, "upstream closed before TLS ServerHello");
            client.write_all(&server_hello[..length]).await?;
            copy_bidirectional(client, &mut upstream).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                runtime.record_failure(&error).await;
                errors.push(error);
            }
        }
    }
    Err(anyhow::anyhow!(
        "TLS tunnel failed after candidate replay: {}",
        errors
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

async fn handle_pac(
    request: Request<Incoming>,
    runtime: ProxyRuntime,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let response = match request.uri().path() {
        "/proxy.pac" | "/" => {
            let script = runtime.config.read().await.pac_script();
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/x-ns-proxy-autoconfig")
                .header("cache-control", "no-store")
                .body(Full::new(Bytes::from(script)))
                .unwrap()
        }
        "/health" => {
            let status = runtime.status.read().await.clone();
            let body = serde_json::to_vec(&status).unwrap_or_default();
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(body)))
                .unwrap()
        }
        _ => text_response(StatusCode::NOT_FOUND, "not found"),
    };
    Ok(response)
}

async fn plain_direct(host: &str, port: u16) -> Result<TcpStream> {
    match TcpStream::connect((host, port)).await {
        Ok(stream) => Ok(stream),
        Err(error) if host.eq_ignore_ascii_case("github.com") && port == 22 => {
            TcpStream::connect(("ssh.github.com", 443))
                .await
                .map_err(Into::into)
        }
        Err(error) => Err(error.into()),
    }
}

async fn connect_http_upstream(
    upstream: &UpstreamConfig,
    host: &str,
    port: u16,
) -> Result<TcpStream> {
    let mut stream = TcpStream::connect((upstream.host.as_str(), upstream.port)).await?;
    let request = format!(
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Connection: Keep-Alive\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut header = Vec::with_capacity(512);
    let mut byte = [0_u8; 1];
    while header.len() < 8_192 && !header.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut byte).await?;
        if read == 0 {
            anyhow::bail!("upstream proxy closed before CONNECT response");
        }
        header.push(byte[0]);
    }
    let first_line = String::from_utf8_lossy(&header)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    if !first_line.contains(" 200 ") {
        anyhow::bail!("upstream CONNECT rejected: {first_line}");
    }
    Ok(stream)
}

fn text_response(status: StatusCode, text: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::copy_from_slice(text.as_bytes())))
        .unwrap()
}

pub fn proxy_address(config: &AppConfig) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], config.proxy_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_upstream_connect_is_validated() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            assert!(String::from_utf8_lossy(&request).starts_with("CONNECT github.com:443"));
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
        });
        let upstream = UpstreamConfig {
            kind: UpstreamKind::Http,
            host: "127.0.0.1".into(),
            port,
            enabled: true,
        };
        let stream = connect_http_upstream(&upstream, "github.com", 443)
            .await
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap().port(), port);
    }
}
