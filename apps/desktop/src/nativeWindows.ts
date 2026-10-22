import { emitTo, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getAllWebviewWindows } from "@tauri-apps/api/webviewWindow";
import type {
  Account,
  AccountConnection,
  AiSettings,
  MailArrival,
  MailHydrated,
  MailRebuildProgress,
  MailRebuildFinished,
  NotificationSettings,
  NotificationAction,
  RealtimeSyncStatus,
} from "./types";

export type NativeView = "account" | "settings";

const isTauri = () => "__TAURI_INTERNALS__" in window;

export async function openSettingsWindow() {
  await openSettingsWindowForAccount();
}

export async function openAccountWindow() {
  await openUtilityWindow("account");
}

export async function openSettingsWindowForAccount(accountId?: string) {
  const existing = isTauri()
    ? (await getAllWebviewWindows()).find(
        (candidate) => candidate.label === "settings",
      )
    : undefined;
  if (existing) {
    await existing.show();
    await existing.setFocus();
    if (accountId) await emitTo("settings", "select-account", accountId);
    return;
  }
  await openUtilityWindow("settings", accountId);
}

async function openUtilityWindow(view: NativeView, accountId?: string) {
  const url = `/?view=${view}${
    accountId ? `&accountId=${encodeURIComponent(accountId)}` : ""
  }`;
  if (!isTauri()) {
    const [width, height] = view === "settings" ? [760, 560] : [620, 720];
    window.open(
      url,
      view,
      `popup=yes,width=${width},height=${height},resizable=yes,scrollbars=no`,
    );
    return;
  }

  const existing = (await getAllWebviewWindows()).find(
    (candidate) => candidate.label === view,
  );
  if (existing) {
    await existing.show();
    await existing.setFocus();
    return;
  }

  try {
    if (view === "settings") {
      await invoke("open_settings_window", { accountId });
    } else {
      await invoke("open_account_window");
    }
  } catch (error) {
    console.error(`Could not open ${view} window`, error);
  }
}

export async function closeNativeWindow() {
  if (isTauri()) await getCurrentWindow().close();
  else window.close();
}

export function onNativeMenuAction(
  handler: (action: string) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<string>("menu-action", (event) => handler(event.payload));
}

export function onAccountConnected(
  handler: (connection: AccountConnection) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<AccountConnection>("account-connected", (event) =>
    handler(event.payload),
  );
}

export function onAccountUpdated(
  handler: (account: Account) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<Account>("account-updated", (event) => handler(event.payload));
}

export function onAccountRemoved(
  handler: (event: { accountId: string }) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<{ accountId: string }>("account-removed", (event) =>
    handler(event.payload),
  );
}

export function onSettingsChanged(
  handler: (settings: AiSettings) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<AiSettings>("settings-changed", (event) =>
    handler(event.payload),
  );
}

export function onSettingsAccountSelected(
  handler: (accountId: string) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<string>("select-account", (event) => handler(event.payload));
}

export function onNotificationSettingsChanged(
  handler: (settings: NotificationSettings) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<NotificationSettings>(
    "notification-settings-changed",
    (event) => handler(event.payload),
  );
}

export function onDesktopNotificationAction(
  handler: (notification: NotificationAction) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen("notification-action", (event) =>
    handler(event.payload as NotificationAction),
  );
}

export function onMailArrived(
  handler: (arrival: MailArrival) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<MailArrival>("mail-arrived", (event) => handler(event.payload));
}

export function onMailHydrated(
  handler: (hydrated: MailHydrated) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<MailHydrated>("mail-hydrated", (event) =>
    handler(event.payload),
  );
}

export function onMailChanged(
  handler: (accountId: string) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<{ accountId: string }>("mail-changed", (event) =>
    handler(event.payload.accountId),
  );
}

export function onMailIndexRebuilt(
  handler: (accountId: string) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<{ accountId: string }>("mail-index-rebuilt", (event) =>
    handler(event.payload.accountId),
  );
}

export function onMailRebuildProgress(
  handler: (progress: MailRebuildProgress) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<MailRebuildProgress>("mail-rebuild-progress", (event) =>
    handler(event.payload),
  );
}

export function onMailRebuildFinished(
  handler: (result: MailRebuildFinished) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<MailRebuildFinished>("mail-rebuild-finished", (event) =>
    handler(event.payload),
  );
}

export function onMailSyncState(
  handler: (status: RealtimeSyncStatus) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<RealtimeSyncStatus>("mail-sync-state", (event) =>
    handler(event.payload),
  );
}

export async function notifyAccountConnected(connection: AccountConnection) {
  if (isTauri()) {
    await Promise.allSettled([
      emitTo("main", "account-connected", connection),
      emitTo("settings", "account-connected", connection),
    ]);
  }
}

export async function notifyAccountUpdated(account: Account) {
  if (isTauri()) await emitTo("main", "account-updated", account);
}

export async function notifySettingsChanged(settings: AiSettings) {
  if (isTauri())
    await emitTo("main", "settings-changed", { ...settings, apiKey: "" });
}

export async function notifyNotificationSettingsChanged(
  settings: NotificationSettings,
) {
  if (isTauri())
    await emitTo("main", "notification-settings-changed", settings);
}
