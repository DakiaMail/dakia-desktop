import { getVersion } from "@tauri-apps/api/app";
import { emitTo, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { arch, version as osVersion } from "@tauri-apps/plugin-os";
import type { Account } from "./types";

export const ANALYTICS_ENDPOINT = "https://analytics.dakiamail.com/v1/usage";
export const ANALYTICS_PRIVACY_URL =
  "https://github.com/DakiaMail/dakia-desktop/blob/main/docs/usage-analytics.md";
export const ANALYTICS_SETTINGS_KEY = "dakia.analytics";
const LAST_REPORT_KEY = "dakia.analytics.last-report-at";
const TOKEN_PREFIX = "dakia.analytics.rotating-install-token.";
const REPORT_INTERVAL_MS = 7 * 24 * 60 * 60 * 1000;
let reportInFlight: Promise<boolean> | undefined;
let activeReportController: AbortController | undefined;

export type AnalyticsConsent = "unknown" | "enabled" | "disabled";

export type AnalyticsSettings = {
  consent: AnalyticsConsent;
};

export type AnalyticsPayload = {
  schema: 1;
  month: string;
  rotating_install_token?: string;
  app_version: string;
  os: "macos";
  os_version: string;
  arch: "arm64" | "x64" | "other";
  providers: AnalyticsProvider[];
};

export type AnalyticsProvider =
  "gmail" | "outlook" | "fastmail" | "icloud" | "yahoo" | "other";

type AnalyticsEnvironment = {
  appVersion?: string;
  osVersion?: string;
  architecture?: string;
};

export function readAnalyticsSettings(): AnalyticsSettings {
  try {
    const value = JSON.parse(
      localStorage.getItem(ANALYTICS_SETTINGS_KEY) ?? "{}",
    );
    if (value?.consent === "enabled" || value?.consent === "disabled") {
      return { consent: value.consent };
    }
  } catch {
    // An invalid local value must never turn analytics on.
  }
  return { consent: "unknown" };
}

export function saveAnalyticsConsent(enabled: boolean): AnalyticsSettings {
  const settings: AnalyticsSettings = {
    consent: enabled ? "enabled" : "disabled",
  };
  localStorage.setItem(ANALYTICS_SETTINGS_KEY, JSON.stringify(settings));
  if (!enabled) {
    activeReportController?.abort();
    clearAnalyticsLocalData();
  }
  return settings;
}

/**
 * Persist locally and notify every Dakia WebView. Each WebView owns its own
 * JavaScript realm, so localStorage events alone cannot reliably stop an
 * in-flight report in the main window when consent changes in Settings.
 */
export function setAnalyticsConsent(enabled: boolean): AnalyticsSettings {
  const settings = saveAnalyticsConsent(enabled);
  void Promise.allSettled([
    emitTo("main", "analytics-consent-changed", settings),
    emitTo("settings", "analytics-consent-changed", settings),
  ]);
  return settings;
}

export function listenForAnalyticsConsent(
  onChange: (settings: AnalyticsSettings) => void,
): Promise<UnlistenFn> {
  return listen<AnalyticsSettings>("analytics-consent-changed", () => {
    // The event is only an invalidation signal. localStorage is shared by the
    // app's same-origin WebViews and is the latest persisted choice; trusting
    // an event payload could let a delayed older event undo a newer opt-out.
    const settings = readAnalyticsSettings();
    if (!analyticsEnabled(settings)) {
      activeReportController?.abort();
      clearAnalyticsLocalData();
    }
    onChange(settings);
  });
}

export function analyticsEnabled(settings = readAnalyticsSettings()): boolean {
  return settings.consent === "enabled";
}

export function analyticsProvider(providerId: string): AnalyticsProvider {
  switch (providerId.trim().toLowerCase()) {
    case "gmail":
    case "google":
      return "gmail";
    case "outlook":
    case "microsoft":
    case "office365":
      return "outlook";
    case "fastmail":
      return "fastmail";
    case "icloud":
    case "apple":
      return "icloud";
    case "yahoo":
      return "yahoo";
    default:
      return "other";
  }
}

export function analyticsProviders(accounts: Account[]): AnalyticsProvider[] {
  return [
    ...new Set(
      accounts
        .filter((account) => account.enabled)
        .map((account) => analyticsProvider(account.provider_id)),
    ),
  ].sort();
}

export function analyticsMonth(now = new Date()): string {
  return `${now.getUTCFullYear()}-${String(now.getUTCMonth() + 1).padStart(2, "0")}`;
}

export async function loadAnalyticsEnvironment(): Promise<AnalyticsEnvironment> {
  const read = async (
    value: () => string | null | undefined | Promise<string | null | undefined>,
  ) => {
    try {
      return (await value()) ?? undefined;
    } catch {
      return undefined;
    }
  };
  const [appVersion, detectedOsVersion, architecture] = await Promise.all([
    read(getVersion),
    read(osVersion),
    read(arch),
  ]);
  return { appVersion, osVersion: detectedOsVersion, architecture };
}

export async function createAnalyticsPayload(
  accounts: Account[],
  includeToken: boolean,
  now = new Date(),
): Promise<AnalyticsPayload> {
  const environment = await loadAnalyticsEnvironment();
  const month = analyticsMonth(now);
  return {
    schema: 1,
    month,
    ...(includeToken ? { rotating_install_token: tokenForMonth(month) } : {}),
    app_version: environment.appVersion ?? "unknown",
    // Dakia currently ships as a macOS desktop app. Do not report the raw OS name.
    os: "macos",
    os_version: environment.osVersion ?? "unknown",
    arch: normalizeArchitecture(environment.architecture),
    providers: analyticsProviders(accounts),
  };
}

/**
 * Sends at most one aggregate heartbeat per seven days, and only after an
 * explicit opt-in. There is deliberately no queue: opting out leaves no
 * pending report that could be sent later.
 */
export async function reportUsageIfConsented(
  accounts: Account[],
  now = new Date(),
): Promise<boolean> {
  if (!analyticsEnabled() || !reportDue(now)) return false;
  if (reportInFlight) return reportInFlight;

  const send = async () => {
    const payload = await createAnalyticsPayload(accounts, true, now);
    // Consent can change while native diagnostics are loading.
    if (!analyticsEnabled()) return false;
    const controller = new AbortController();
    activeReportController = controller;
    try {
      const response = await fetch(ANALYTICS_ENDPOINT, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(payload),
        signal: controller.signal,
      });
      if (!response.ok) return false;
      // A different WebView may have withdrawn consent while the request was
      // completing. Never recreate the local reporting marker after opt-out.
      if (!analyticsEnabled()) return false;
      localStorage.setItem(LAST_REPORT_KEY, now.toISOString());
      return true;
    } catch {
      return false;
    } finally {
      if (activeReportController === controller)
        activeReportController = undefined;
    }
  };
  reportInFlight = send().finally(() => {
    reportInFlight = undefined;
  });
  return reportInFlight;
}

function reportDue(now: Date): boolean {
  const last = localStorage.getItem(LAST_REPORT_KEY);
  if (!last) return true;
  const lastTime = Date.parse(last);
  return (
    !Number.isFinite(lastTime) || now.getTime() - lastTime >= REPORT_INTERVAL_MS
  );
}

function tokenForMonth(month: string): string {
  const key = `${TOKEN_PREFIX}${month}`;
  const existing = localStorage.getItem(key);
  if (existing) return existing;
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  const token = Array.from(bytes, (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
  localStorage.setItem(key, token);
  return token;
}

function clearAnalyticsLocalData() {
  localStorage.removeItem(LAST_REPORT_KEY);
  for (let index = localStorage.length - 1; index >= 0; index -= 1) {
    const key = localStorage.key(index);
    if (key?.startsWith(TOKEN_PREFIX)) localStorage.removeItem(key);
  }
}

function normalizeArchitecture(
  value: string | undefined,
): AnalyticsPayload["arch"] {
  switch (value?.toLowerCase()) {
    case "aarch64":
    case "arm64":
      return "arm64";
    case "x86_64":
    case "x64":
      return "x64";
    default:
      return "other";
  }
}
