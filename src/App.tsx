import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Activity,
  Check,
  Clipboard,
  Download,
  ExternalLink,
  Gauge,
  GitBranch,
  Network,
  Power,
  RefreshCw,
  Router,
  Settings,
  ShieldCheck,
  Terminal,
  TriangleAlert,
  Wifi,
} from "lucide-react";
import {
  AccelerationResult,
  DaemonStatus,
  RouteMode,
  healthScore,
  unwrapAcceleration,
  unwrapStatus,
} from "./model";

type AppConfig = {
  route_mode: RouteMode;
  refresh_seconds: number;
  download_acceleration: {
    enabled: boolean;
    site_url: string;
    nodes: string[];
    timeout_seconds: number;
    max_nodes: number;
    refresh_seconds: number;
  };
  upstream: null | {
    kind: "http" | "socks5";
    host: string;
    port: number;
    enabled: boolean;
  };
};

const mockStatus: DaemonStatus = {
  running: true,
  enabled: true,
  configured_mode: "auto",
  active_route: "optimized_direct",
  proxy_address: "127.0.0.1:38457",
  pac_url: "http://127.0.0.1:38458/proxy.pac",
  direct_failures: 0,
  last_error: null,
  updated_at_unix_ms: Date.now(),
  selected_ips: {
    "github.com:443": "140.82.113.4",
    "raw.githubusercontent.com:443": "185.199.108.133",
  },
  candidates: {
    "github.com:443": [
      { address: "140.82.113.4", tcp_ms: 62, tls_ms: 88, score: 95, success_rate: 1, last_error: null },
      { address: "140.82.112.4", tcp_ms: 81, tls_ms: 102, score: 93, success_rate: 1, last_error: null },
    ],
    "api.github.com:443": [
      { address: "140.82.113.6", tcp_ms: 70, tls_ms: 91, score: 94, success_rate: 1, last_error: null },
    ],
    "raw.githubusercontent.com:443": [
      { address: "185.199.108.133", tcp_ms: 74, tls_ms: 96, score: 94, success_rate: 1, last_error: null },
    ],
  },
  download_acceleration: {
    enabled: true,
    best_mirror: "gh-proxy.com",
    accelerated_url: "https://gh-proxy.com/https://github.com/a/b/archive/main.zip",
    latency_ms: 420,
    checked_at_unix_ms: Date.now(),
    candidates: [
      { host: "gh-proxy.com", latency_ms: 420, score: 89.5, http_status: 206, last_error: null },
      { host: "cdn.akaere.online", latency_ms: 780, score: 80.5, http_status: 206, last_error: null },
    ],
    last_error: null,
  },
};

const mockConfig: AppConfig = {
  route_mode: "auto",
  refresh_seconds: 300,
  download_acceleration: {
    enabled: false,
    site_url: "https://github.akams.cn/",
    nodes: [],
    timeout_seconds: 8,
    max_nodes: 16,
    refresh_seconds: 600,
  },
  upstream: null,
};

function isTauri() {
  return "__TAURI_INTERNALS__" in window;
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  return invoke<T>(command, args);
}

const routeLabels: Record<RouteMode, string> = {
  auto: "自动",
  optimized_direct: "优化直连",
  upstream: "上游代理",
  plain_direct: "普通直连",
};

export default function App() {
  const [tab, setTab] = useState<"status" | "settings">("status");
  const [status, setStatus] = useState<DaemonStatus | null>(null);
  const [config, setConfig] = useState<AppConfig>(mockConfig);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [upstreamType, setUpstreamType] = useState<"none" | "http" | "socks5">("none");
  const [upstreamHost, setUpstreamHost] = useState("");
  const [upstreamPort, setUpstreamPort] = useState("7890");
  const [refreshSeconds, setRefreshSeconds] = useState("300");
  const [downloadEnabled, setDownloadEnabled] = useState(false);
  const [accelerationSite, setAccelerationSite] = useState("https://github.akams.cn/");
  const [downloadUrl, setDownloadUrl] = useState("");
  const [acceleration, setAcceleration] = useState<AccelerationResult | null>(null);

  const load = useCallback(async () => {
    try {
      if (!isTauri()) {
        setStatus(mockStatus);
        setConfig(mockConfig);
        return;
      }
      const [statusValue, configValue] = await Promise.all([
        call<unknown>("get_status"),
        call<AppConfig>("get_config"),
      ]);
      const nextStatus = unwrapStatus(statusValue);
      setStatus(nextStatus);
      setConfig(configValue);
      setError(null);
    } catch (reason) {
      setStatus(null);
      setError(String(reason));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
    const timer = window.setInterval(() => void load(), 5_000);
    return () => window.clearInterval(timer);
  }, [load]);

  useEffect(() => {
    setRefreshSeconds(String(config.refresh_seconds));
    setUpstreamType(config.upstream?.kind ?? "none");
    setUpstreamHost(config.upstream?.host ?? "");
    setUpstreamPort(String(config.upstream?.port ?? 7890));
    setDownloadEnabled(config.download_acceleration.enabled);
    setAccelerationSite(config.download_acceleration.site_url);
  }, [config]);

  const currentScore = useMemo(() => healthScore(status), [status]);
  const candidateRows = useMemo(
    () => Object.entries(status?.candidates ?? {}).flatMap(([host, items]) =>
      items.slice(0, 1).map((item) => ({ host, ...item })),
    ),
    [status],
  );

  async function action(task: () => Promise<void>) {
    setBusy(true);
    try {
      await task();
      await load();
      setError(null);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(false);
    }
  }

  async function changeRoute(mode: RouteMode) {
    if (!isTauri()) {
      setStatus((current) => current ? { ...current, configured_mode: mode, active_route: mode } : current);
      return;
    }
    await action(async () => { await call("set_route", { mode }); });
  }

  async function runProbe() {
    if (!isTauri()) return;
    await action(async () => { await call("probe"); });
  }

  async function toggleEnabled() {
    if (!isTauri()) {
      setStatus((current) => current ? { ...current, enabled: !current.enabled } : current);
      return;
    }
    const command = status?.enabled ? "disable" : "enable";
    await action(async () => { await call("run_cli", { action: command }); });
  }

  async function saveSettings() {
    if (!isTauri()) return;
    await action(async () => {
      await call("save_settings", {
        input: {
          upstreamKind: upstreamType === "none" ? null : upstreamType,
          upstreamHost: upstreamType === "none" ? null : upstreamHost,
          upstreamPort: upstreamType === "none" ? null : Number(upstreamPort),
          refreshSeconds: Number(refreshSeconds),
          downloadAccelerationEnabled: downloadEnabled,
          downloadAccelerationSite: accelerationSite,
          downloadAccelerationMaxNodes: config.download_acceleration.max_nodes,
        },
      });
    });
  }

  async function accelerateDownload() {
    if (!downloadUrl.trim()) return;
    if (!isTauri()) {
      setAcceleration(mockStatus.download_acceleration.accelerated_url ? {
        original_url: downloadUrl,
        accelerated_url: mockStatus.download_acceleration.accelerated_url,
        mirror: mockStatus.download_acceleration.best_mirror ?? "gh-proxy.com",
        latency_ms: mockStatus.download_acceleration.latency_ms ?? 420,
        candidates: mockStatus.download_acceleration.candidates,
      } : null);
      return;
    }
    await action(async () => {
      const value = await call<unknown>("accelerate_download", { url: downloadUrl.trim() });
      setAcceleration(unwrapAcceleration(value));
    });
  }

  async function openAcceleratedUrl() {
    if (!acceleration) return;
    if (isTauri()) {
      await call("open_accelerated_url", { url: acceleration.accelerated_url });
    } else {
      window.open(acceleration.accelerated_url, "_blank", "noopener,noreferrer");
    }
  }

  const enabled = status?.enabled ?? false;
  const stateClass = !status ? "offline" : currentScore >= 85 ? "healthy" : currentScore >= 55 ? "degraded" : "offline";

  return (
    <main className="app-shell">
      <header className="topbar">
        <div className="brand-lockup">
          <div className={`brand-mark ${stateClass}`} aria-hidden="true">
            <span /><i /><b />
          </div>
          <div>
            <h1>GitHub Link</h1>
            <p>{enabled ? "连接保护已启用" : "连接保护未启用"}</p>
          </div>
        </div>
        <button className={`power-button ${enabled ? "on" : ""}`} onClick={toggleEnabled} disabled={busy} title={enabled ? "停用" : "启用"}>
          <Power size={18} />
        </button>
      </header>

      <nav className="tabs" aria-label="视图">
        <button className={tab === "status" ? "active" : ""} onClick={() => setTab("status")}><Activity size={16} />状态</button>
        <button className={tab === "settings" ? "active" : ""} onClick={() => setTab("settings")}><Settings size={16} />设置</button>
      </nav>

      {error && <div className="error-banner"><TriangleAlert size={16} /><span>{error}</span></div>}

      {tab === "status" ? (
        <section className="content">
          <div className="health-band">
            <div className={`score-ring ${stateClass}`}><strong>{loading ? "--" : currentScore}</strong><span>连接评分</span></div>
            <div className="health-summary">
              <div><span>当前线路</span><strong>{status ? routeLabels[status.configured_mode] : "Daemon 离线"}</strong></div>
              <div><span>实际路径</span><strong>{status?.active_route.replaceAll("_", " ") ?? "不可用"}</strong></div>
              <div><span>连续失败</span><strong>{status?.direct_failures ?? 0}</strong></div>
            </div>
          </div>

          <div className="section-block">
            <div className="section-title"><div><Router size={17} /><h2>线路模式</h2></div><button className="icon-button" onClick={runProbe} disabled={busy} title="立即检测"><RefreshCw size={16} className={busy ? "spin" : ""} /></button></div>
            <div className="segmented-control">
              {(Object.keys(routeLabels) as RouteMode[]).map((mode) => (
                <button key={mode} className={status?.configured_mode === mode ? "active" : ""} onClick={() => changeRoute(mode)}>{routeLabels[mode]}</button>
              ))}
            </div>
          </div>

          <div className="section-block">
            <div className="section-title"><div><Gauge size={17} /><h2>目标节点</h2></div><span>{candidateRows.length} 个域名</span></div>
            <div className="endpoint-table">
              <div className="endpoint-head"><span>Endpoint</span><span>选中 IP</span><span>TCP</span><span>TLS</span></div>
              {candidateRows.length ? candidateRows.map((row) => (
                <div className="endpoint-row" key={row.host}>
                  <span><i className={row.last_error ? "dot bad" : "dot"} />{row.host.replace(":443", "")}</span>
                  <code>{row.address}</code>
                  <span>{row.tcp_ms ?? "--"} ms</span>
                  <span>{row.tls_ms ?? "--"} ms</span>
                </div>
              )) : <div className="empty-row">运行一次检测以生成节点数据</div>}
            </div>
          </div>

          <div className="section-block download-accelerator">
            <div className="section-title">
              <div><Download size={17} /><h2>GitHub 下载加速</h2></div>
              <span>{status?.download_acceleration.enabled ? "已启用" : "未启用"}</span>
            </div>
            {status?.download_acceleration.enabled ? (
              <>
                <div className="mirror-input">
                  <input
                    value={downloadUrl}
                    onChange={(event) => setDownloadUrl(event.target.value)}
                    placeholder="粘贴 Release / Archive / Raw 公开链接"
                    onKeyDown={(event) => { if (event.key === "Enter") void accelerateDownload(); }}
                  />
                  <button className="primary-button" onClick={accelerateDownload} disabled={busy || !downloadUrl.trim()}>
                    {busy ? "测速中" : "选择最快节点"}
                  </button>
                </div>
                {acceleration ? (
                  <div className="mirror-result">
                    <div><span>最佳镜像</span><strong>{acceleration.mirror}</strong><em>{acceleration.latency_ms} ms</em></div>
                    <code>{acceleration.accelerated_url}</code>
                    <div className="inline-actions">
                      <button className="secondary-button" onClick={() => navigator.clipboard.writeText(acceleration.accelerated_url)}><Clipboard size={15} />复制链接</button>
                      <button className="secondary-button" onClick={openAcceleratedUrl}><ExternalLink size={15} />打开下载</button>
                    </div>
                  </div>
                ) : (
                  <div className="empty-row">
                    {status.download_acceleration.best_mirror
                      ? `当前缓存节点：${status.download_acceleration.best_mirror}`
                      : "粘贴公开下载链接后，会从站点节点和内置候选节点中选择最快线路"}
                  </div>
                )}
                {status.download_acceleration.candidates.length > 0 && (
                  <div className="mirror-candidates">
                    {status.download_acceleration.candidates.slice(0, 3).map((candidate) => (
                      <div key={candidate.host}>
                        <i className={candidate.last_error ? "dot bad" : "dot"} />
                        <span>{candidate.host}</span>
                        <strong>{candidate.last_error ? "失败" : `${candidate.latency_ms ?? "--"} ms`}</strong>
                      </div>
                    ))}
                  </div>
                )}
              </>
            ) : (
              <div className="empty-row">在设置中启用后，仅用于公开下载链接</div>
            )}
            <p className="security-note">镜像源来自 github.akams.cn 及内置候选，证书由第三方提供；不会携带 GitHub Token，不用于 API、登录或私有仓库。</p>
          </div>

          <div className="security-strip"><ShieldCheck size={18} /><div><strong>端到端 TLS</strong><span>不安装 CA，不解密流量，不关闭证书验证</span></div></div>
        </section>
      ) : (
        <section className="content settings-view">
          <div className="section-block">
            <div className="section-title"><div><Network size={17} /><h2>本地接入</h2></div></div>
            <label className="field-label">PAC 地址</label>
            <div className="copy-field"><code>{status?.pac_url ?? "http://127.0.0.1:38458/proxy.pac"}</code><button className="icon-button" onClick={() => navigator.clipboard.writeText(status?.pac_url ?? "http://127.0.0.1:38458/proxy.pac")} title="复制"><Clipboard size={16} /></button></div>
            <button className="secondary-button" onClick={() => isTauri() && call("open_network_settings")}><Wifi size={16} />打开 macOS 网络设置</button>
          </div>

          <div className="section-block form-grid">
            <div className="section-title full"><div><GitBranch size={17} /><h2>上游代理</h2></div></div>
            <label><span>类型</span><select value={upstreamType} onChange={(event) => setUpstreamType(event.target.value as typeof upstreamType)}><option value="none">不使用</option><option value="http">HTTP CONNECT</option><option value="socks5">SOCKS5</option></select></label>
            <label><span>主机</span><input value={upstreamHost} onChange={(event) => setUpstreamHost(event.target.value)} placeholder="127.0.0.1" disabled={upstreamType === "none"} /></label>
            <label><span>端口</span><input type="number" min="1" max="65535" value={upstreamPort} onChange={(event) => setUpstreamPort(event.target.value)} disabled={upstreamType === "none"} /></label>
            <label><span>后台检测</span><select value={refreshSeconds} onChange={(event) => setRefreshSeconds(event.target.value)}><option value="60">1 分钟</option><option value="300">5 分钟</option><option value="600">10 分钟</option><option value="1800">30 分钟</option></select></label>
            <button className="primary-button full" onClick={saveSettings} disabled={busy}><Check size={16} />保存设置</button>
          </div>

          <div className="section-block form-grid">
            <div className="section-title full"><div><Download size={17} /><h2>下载加速</h2></div></div>
            <label className="checkbox-field full">
              <input type="checkbox" checked={downloadEnabled} onChange={(event) => setDownloadEnabled(event.target.checked)} />
              <span>启用第三方 GitHub 下载镜像</span>
            </label>
            <label className="full"><span>节点发现页面</span><input value={accelerationSite} onChange={(event) => setAccelerationSite(event.target.value)} placeholder="https://github.akams.cn/" /></label>
            <div className="field-hint full">默认关闭。仅处理 Release、Archive、Raw、Gist 和 Codeload 公开下载，不代理 GitHub API 或登录请求。</div>
            <button className="primary-button full" onClick={saveSettings} disabled={busy}><Check size={16} />保存下载加速设置</button>
          </div>

          <div className="section-block command-list">
            <div className="section-title"><div><Terminal size={17} /><h2>开发工具</h2></div></div>
            <div><span>HTTP 代理</span><code>{status?.proxy_address ?? "127.0.0.1:38457"}</code></div>
            <div><span>Git / SSH</span><strong>{enabled ? "已接管" : "未接管"}</strong></div>
            <div><span>TLS 中间人</span><strong>禁用</strong></div>
          </div>
        </section>
      )}
    </main>
  );
}
