import { describe, expect, it } from "vitest";
import { DaemonStatus, healthScore, unwrapStatus } from "./model";

function status(scores: number[]): DaemonStatus {
  return {
    running: true,
    enabled: true,
    configured_mode: "auto",
    active_route: "optimized_direct",
    proxy_address: "127.0.0.1:38457",
    pac_url: "http://127.0.0.1:38458/proxy.pac",
    direct_failures: 0,
    last_error: null,
    updated_at_unix_ms: 1,
    selected_ips: {},
    candidates: Object.fromEntries(scores.map((score, index) => [
      `host-${index}:443`,
      [{ address: "1.1.1.1", tcp_ms: 20, tls_ms: 30, score, success_rate: 1, last_error: null }],
    ])),
  };
}

describe("healthScore", () => {
  it("averages the best candidate for every endpoint", () => {
    expect(healthScore(status([90, 80, 70]))).toBe(80);
  });

  it("uses a neutral running score before the first probe", () => {
    expect(healthScore(status([]))).toBe(60);
  });
});

describe("unwrapStatus", () => {
  it("returns status responses", () => {
    const value = status([95]);
    expect(unwrapStatus({ type: "status", status: value })).toBe(value);
  });

  it("rejects malformed daemon responses", () => {
    expect(() => unwrapStatus({ type: "ok" })).toThrow(/unexpected response/);
  });
});
