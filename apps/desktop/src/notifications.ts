import {
  isPermissionGranted,
  onAction,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import type { PluginListener } from "@tauri-apps/api/core";
import { api } from "./api";
import type { MailSummary, NotificationSettings } from "./types";

const SETTINGS_KEY = "dakia.notifications";
const PERMISSION_ASKED_KEY = "dakia.notifications.permission-asked";

export const defaultNotificationSettings: NotificationSettings = {
  enabled: true,
  soundEnabled: true,
  showPreview: true,
};

const isTauri = () => "__TAURI_INTERNALS__" in window;

export function readNotificationSettings(): NotificationSettings {
  try {
    return {
      ...defaultNotificationSettings,
      ...JSON.parse(localStorage.getItem(SETTINGS_KEY) ?? "{}"),
    };
  } catch {
    return defaultNotificationSettings;
  }
}

export function saveNotificationSettings(settings: NotificationSettings) {
  localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings));
}

export async function notificationPermissionGranted() {
  return isTauri() ? isPermissionGranted() : false;
}

export async function requestNotificationAccess() {
  if (!isTauri()) return false;
  const granted =
    (await isPermissionGranted()) || (await requestPermission()) === "granted";
  localStorage.setItem(PERMISSION_ASKED_KEY, "true");
  return granted;
}

export async function requestInitialNotificationAccess(
  settings: NotificationSettings,
) {
  if (
    !settings.enabled ||
    localStorage.getItem(PERMISSION_ASKED_KEY) === "true"
  )
    return notificationPermissionGranted();
  return requestNotificationAccess();
}

type NotificationCopy = {
  newMail: string;
  oneGeneric: string;
  many: (count: number) => string;
  manyBody: (count: number) => string;
};

export function buildNewMailNotification(
  messages: MailSummary[],
  showPreview: boolean,
  copy: NotificationCopy,
) {
  if (messages.length === 1) {
    const message = messages[0];
    return {
      title: showPreview
        ? message.from_name || message.from_address
        : copy.newMail,
      body: showPreview ? message.subject || copy.oneGeneric : copy.oneGeneric,
      extra: {
        accountId: message.account_id,
        messageId: message.id,
        count: 1,
      },
    };
  }
  return {
    title: copy.many(messages.length),
    body: copy.manyBody(messages.length),
    extra: { count: messages.length },
  };
}

export async function sendNewMailNotification(
  messages: MailSummary[],
  settings: NotificationSettings,
  copy: NotificationCopy,
) {
  if (
    !isTauri() ||
    !settings.enabled ||
    !messages.length ||
    !(await isPermissionGranted())
  )
    return false;
  const notification = buildNewMailNotification(
    messages,
    settings.showPreview,
    copy,
  );
  if (isMac()) {
    await api.sendDesktopNotification({
      title: notification.title,
      body: notification.body,
      accountId:
        typeof notification.extra.accountId === "string"
          ? notification.extra.accountId
          : undefined,
      messageId:
        typeof notification.extra.messageId === "string"
          ? notification.extra.messageId
          : undefined,
      count: notification.extra.count,
      sound: settings.soundEnabled ? notificationSound() : undefined,
    });
  } else {
    sendNotification({
      ...notification,
      group: "new-mail",
      autoCancel: true,
      sound: settings.soundEnabled ? notificationSound() : undefined,
    });
  }
  return true;
}

export async function sendTestNotification(
  settings: NotificationSettings,
  title: string,
  body: string,
) {
  if (!(await requestNotificationAccess())) return false;
  sendNotification({
    title,
    body,
    group: "new-mail",
    autoCancel: true,
    sound: settings.soundEnabled ? notificationSound() : undefined,
    extra: { count: 0 },
  });
  return true;
}

export function onNotificationAction(
  handler: (extra: Record<string, unknown>) => void,
): Promise<PluginListener | (() => void)> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return onAction((notification) => handler(notification.extra ?? {}));
}

function notificationSound() {
  const platform = navigator.platform.toLowerCase();
  if (platform.includes("mac")) return "Ping";
  if (platform.includes("linux")) return "message-new-instant";
  return "Default";
}

function isMac() {
  return navigator.platform.toLowerCase().includes("mac");
}
