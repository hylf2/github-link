import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Activity,
  Check,
  Clipboard,
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
import { DaemonStatus, RouteMode, healthScore, unwrapStatus } from "./model";

type AppConfig = {
  route_mode: RouteMode;
  refresh_seconds: number;
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
};

const mockConfig: AppConfig = {
  route_mode: "auto",
  refresh_seconds: 300,
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
        },
      });
    });
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
