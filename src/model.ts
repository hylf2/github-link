export type RouteMode = "auto" | "optimized_direct" | "upstream" | "plain_direct";

export type CandidateHealth = {
  address: string;
  tcp_ms: number | null;
  tls_ms: number | null;
  score: number;
  success_rate: number;
  last_error: string | null;
};

export type MirrorCandidate = {
  host: string;
  latency_ms: number | null;
  score: number;
  http_status: number | null;
  last_error: string | null;
};

export type DownloadAccelerationStatus = {
  enabled: boolean;
  best_mirror: string | null;
  accelerated_url: string | null;
  latency_ms: number | null;
  checked_at_unix_ms: number | null;
  candidates: MirrorCandidate[];
  last_error: string | null;
};

export type AccelerationResult = {
  original_url: string;
  accelerated_url: string;
  mirror: string;
  latency_ms: number;
  candidates: MirrorCandidate[];
};

export type DaemonStatus = {
  running: boolean;
  enabled: boolean;
  configured_mode: RouteMode;
  active_route: string;
  proxy_address: string;
  pac_url: string;
  direct_failures: number;
  last_error: string | null;
  updated_at_unix_ms: number;
  selected_ips: Record<string, string>;
  candidates: Record<string, CandidateHealth[]>;
  download_acceleration: DownloadAccelerationStatus;
};

export function healthScore(status: DaemonStatus | null) {
  if (!status) return 0;
  const best = Object.values(status.candidates).flatMap((items) => items.slice(0, 1));
  if (!best.length) return status.running ? 60 : 0;
  return Math.round(best.reduce((total, item) => total + item.score, 0) / best.length);
}

export function unwrapStatus(value: unknown): DaemonStatus {
  const response = value as { type?: string; status?: DaemonStatus };
  if (response.type === "status" && response.status) return response.status;
  throw new Error("Daemon returned an unexpected response");
}

export function unwrapAcceleration(value: unknown): AccelerationResult {
  const response = value as { type?: string; result?: AccelerationResult };
  if (response.type === "acceleration" && response.result) return response.result;
  const error = (value as { message?: string }).message;
  throw new Error(error || "Daemon returned an unexpected acceleration response");
}
