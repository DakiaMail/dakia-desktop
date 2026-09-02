import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Account } from "./types";
import {
  ANALYTICS_ENDPOINT,
  createAnalyticsPayload,
  listenForAnalyticsConsent,
  reportUsageIfConsented,
  saveAnalyticsConsent,
  setAnalyticsConsent,
} from "./analytics";

const native = vi.hoisted(() => ({
  getVersion: vi.fn(),
  arch: vi.fn(),
  osType: vi.fn(),
  osVersion: vi.fn(),
}));
const events = vi.hoisted(() => ({
  emitTo: vi.fn(),
  listen: vi.fn(),
}));

vi.mock("@tauri-apps/api/app", () => ({ getVersion: native.getVersion }));
vi.mock("@tauri-apps/plugin-os", () => ({
  arch: native.arch,
  type: native.osType,
  version: native.osVersion,
}));
vi.mock("@tauri-apps/api/event", () => ({
  emitTo: events.emitTo,
  listen: events.listen,
}));

const accounts = [
  { provider_id: "gmail", enabled: true },
  { provider_id: "fastmail", enabled: true },
  { provider_id: "custom-imap", enabled: true },
  { provider_id: "outlook", enabled: false },
] as Account[];

describe("privacy-preserving usage analytics", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.restoreAllMocks();
    native.getVersion.mockResolvedValue("0.4.0");
    native.osType.mockResolvedValue("Darwin");
    native.osVersion.mockResolvedValue("15.6");
    native.arch.mockResolvedValue("aarch64");
    events.emitTo.mockResolvedValue(undefined);
    events.listen.mockResolvedValue(() => undefined);
  });

  it("does not read diagnostics or make a request before opt-in", async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);

    await expect(reportUsageIfConsented(accounts)).resolves.toBe(false);

    expect(fetchMock).not.toHaveBeenCalled();
    expect(native.getVersion).not.toHaveBeenCalled();
  });

  it("sends only the consented, coarse payload after opt-in", async () => {
    const fetchMock = vi.fn().mockResolvedValue({ ok: true });
    vi.stubGlobal("fetch", fetchMock);
    saveAnalyticsConsent(true);

    await expect(
      reportUsageIfConsented(accounts, new Date("2026-08-26T12:00:00Z")),
    ).resolves.toBe(true);

    expect(fetchMock).toHaveBeenCalledWith(
      ANALYTICS_ENDPOINT,
      expect.objectContaining({ method: "POST" }),
    );
    const body = JSON.parse(fetchMock.mock.calls[0][1].body);
    expect(body).toMatchObject({
      schema: 1,
      month: "2026-08",
      app_version: "0.4.0",
      os: "macos",
      os_version: "15.6",
      arch: "arm64",
      providers: ["fastmail", "gmail", "other"],
    });
    expect(body.rotating_install_token).toMatch(/^[a-f0-9]{32}$/);
    expect(JSON.stringify(body)).not.toContain("custom-imap");
    expect(JSON.stringify(body)).not.toContain("outlook");
  });

  it("broadcasts consent and treats events as notifications to reread the latest choice", async () => {
    let receive:
      ((event: { payload: { consent: "enabled" } }) => void) | undefined;
    events.listen.mockImplementation(async (_name, handler) => {
      receive = handler;
      return () => undefined;
    });
    const onChange = vi.fn();

    expect(setAnalyticsConsent(true).consent).toBe("enabled");
    expect(events.emitTo).toHaveBeenCalledWith(
      "main",
      "analytics-consent-changed",
      { consent: "enabled" },
    );
    expect(events.emitTo).toHaveBeenCalledWith(
      "settings",
      "analytics-consent-changed",
      { consent: "enabled" },
    );
    await listenForAnalyticsConsent(onChange);
    // Simulate a delayed old enable event arriving after a newer opt-out was
    // already persisted by another WebView.
    saveAnalyticsConsent(false);
    receive?.({ payload: { consent: "enabled" } });

    expect(onChange).toHaveBeenCalledWith({ consent: "disabled" });
    expect(localStorage.getItem("dakia.analytics")).toContain("disabled");
  });

  it("does not restore a reporting marker when consent changes before a response completes", async () => {
    let resolveFetch: ((value: { ok: boolean }) => void) | undefined;
    vi.stubGlobal(
      "fetch",
      vi.fn(
        () =>
          new Promise<{ ok: boolean }>((resolve) => {
            resolveFetch = resolve;
          }),
      ),
    );
    saveAnalyticsConsent(true);
    const reporting = reportUsageIfConsented(
      accounts,
      new Date("2026-08-26T12:00:00Z"),
    );
    await vi.waitFor(() => expect(resolveFetch).toBeTypeOf("function"));
    saveAnalyticsConsent(false);
    resolveFetch?.({ ok: true });

    await expect(reporting).resolves.toBe(false);
    expect(localStorage.getItem("dakia.analytics.last-report-at")).toBeNull();
  });

  it("makes the preview token-free until consent and deletes it on opt-out", async () => {
    const beforeConsent = await createAnalyticsPayload(
      accounts,
      false,
      new Date("2026-08-26T12:00:00Z"),
    );
    expect(beforeConsent.rotating_install_token).toBeUndefined();

    const consented = await createAnalyticsPayload(
      accounts,
      true,
      new Date("2026-08-26T12:00:00Z"),
    );
    expect(consented.rotating_install_token).toMatch(/^[a-f0-9]{32}$/);

    saveAnalyticsConsent(false);
    expect(
      localStorage.getItem("dakia.analytics.rotating-install-token.2026-08"),
    ).toBeNull();
  });
});
