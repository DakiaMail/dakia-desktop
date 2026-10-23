import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  buildNewMailNotification,
  onNotificationAction,
  sendNewMailNotification,
  sendTestNotification,
} from "./notifications";
import type { MailSummary } from "./types";

const mocks = vi.hoisted(() => ({
  isPermissionGranted: vi.fn(),
  onAction: vi.fn(),
  requestPermission: vi.fn(),
  sendNotification: vi.fn(),
  sendDesktopNotification: vi.fn(),
}));

vi.mock("@tauri-apps/plugin-notification", () => ({
  isPermissionGranted: mocks.isPermissionGranted,
  onAction: mocks.onAction,
  requestPermission: mocks.requestPermission,
  sendNotification: mocks.sendNotification,
}));

vi.mock("./api", () => ({
  api: {
    sendDesktopNotification: mocks.sendDesktopNotification,
  },
}));

const copy = {
  newMail: "New email",
  oneGeneric: "You have a new message",
  many: (count: number) => `${count} new emails`,
  manyBody: (count: number) => `${count} messages arrived`,
};

const message = (id: string): MailSummary => ({
  id,
  account_id: "account",
  mailbox: "INBOX",
  uid: Number(id),
  thread_id: id,
  subject: `Subject ${id}`,
  from_name: "Mara",
  from_address: "mara@example.com",
  to_addresses: "me@example.com",
  received_at: "2026-07-19T10:00:00Z",
  snippet: "Preview",
  body_text: "Body",
  is_read: false,
  is_flagged: false,
  has_attachments: false,
});

const privateMessage = (): MailSummary => ({
  ...message("1"),
  message_id: "<private-message@example.com>",
  subject: "Private medical appointment",
  from_name: "Private Sender",
  from_address: "private@example.com",
  to_addresses: "private-account-address@example.com",
  cc_addresses: "private-cc@example.com",
  bcc_addresses: "private-bcc@example.com",
  reply_to_addresses: "private-reply-to@example.com",
  snippet: "Sensitive preview text",
  body_text: "Sensitive body text",
});

const privateContent =
  /Private medical appointment|Private Sender|private@example\.com|private-account-address@example\.com|private-cc@example\.com|private-bcc@example\.com|private-reply-to@example\.com|Sensitive preview text|Sensitive body text/;
const privateFieldsOutsidePreview =
  /private@example\.com|private-account-address@example\.com|private-cc@example\.com|private-bcc@example\.com|private-reply-to@example\.com|Sensitive preview text|Sensitive body text/;

beforeEach(() => {
  vi.clearAllMocks();
  Object.defineProperty(window, "__TAURI_INTERNALS__", {
    configurable: true,
    value: {},
  });
  mocks.isPermissionGranted.mockResolvedValue(true);
  mocks.sendDesktopNotification.mockResolvedValue(undefined);
});

describe("new-mail notification copy", () => {
  it("shows the sender and subject for one message", () => {
    expect(
      buildNewMailNotification(
        [{ ...message("1"), message_id: "<message-1@example.com>" }],
        true,
        copy,
      ),
    ).toMatchObject({
      title: "Mara",
      body: "Subject 1",
      extra: {
        accountId: "account",
        messageId: "1",
        rfcMessageId: "<message-1@example.com>",
        threadId: "1",
        count: 1,
      },
    });
  });

  it("hides message metadata when previews are disabled", () => {
    const message = privateMessage();

    expect(buildNewMailNotification([message], false, copy)).toEqual({
      title: "New email",
      body: "You have a new message",
      extra: {
        accountId: "account",
        messageId: "1",
        rfcMessageId: "<private-message@example.com>",
        threadId: "1",
        count: 1,
      },
    });
    expect(
      JSON.stringify(buildNewMailNotification([message], false, copy)),
    ).not.toMatch(privateContent);
  });

  it("batches several messages into one summary", () => {
    expect(
      buildNewMailNotification([message("1"), message("2")], true, copy),
    ).toEqual({
      title: "2 new emails",
      body: "2 messages arrived",
      extra: { count: 2 },
    });
  });
});

describe("notification action subscriptions", () => {
  it("uses Dakia's native macOS action bridge without registering the unavailable plugin listener", async () => {
    vi.spyOn(navigator, "platform", "get").mockReturnValue("MacIntel");

    const listener = await onNotificationAction(() => undefined);

    expect(listener).toBeTypeOf("function");
    expect(mocks.onAction).not.toHaveBeenCalled();
  });

  it("registers plugin actions on supported Tauri runtimes", async () => {
    vi.spyOn(navigator, "platform", "get").mockReturnValue("Linux x86_64");
    const unregister = vi.fn();
    const received: Array<Record<string, unknown>> = [];
    mocks.onAction.mockImplementationOnce(async (handler) => {
      handler({ extra: { accountId: "account" } });
      return { unregister };
    });

    const listener = await onNotificationAction((extra) =>
      received.push(extra),
    );

    expect(mocks.onAction).toHaveBeenCalledOnce();
    expect(received).toEqual([{ accountId: "account" }]);
    expect(listener).toEqual({ unregister });
  });

  it("recovers only when the action listener command is unavailable", async () => {
    vi.spyOn(navigator, "platform", "get").mockReturnValue("Linux x86_64");
    mocks.onAction.mockRejectedValueOnce(
      new Error(
        "notification.registerListener not allowed. Command not found.",
      ),
    );

    await expect(onNotificationAction(() => undefined)).resolves.toBeTypeOf(
      "function",
    );

    mocks.onAction.mockRejectedValueOnce(
      new Error("notification permission denied"),
    );
    await expect(onNotificationAction(() => undefined)).rejects.toThrow(
      "notification permission denied",
    );
  });
});

describe("new-mail notification delivery privacy", () => {
  it("does not call either notification boundary when notifications are disabled", async () => {
    const delivered = await sendNewMailNotification(
      [privateMessage()],
      { enabled: false, showPreview: false, soundEnabled: true },
      copy,
    );

    expect(delivered).toBe(false);
    expect(mocks.isPermissionGranted).not.toHaveBeenCalled();
    expect(mocks.sendDesktopNotification).not.toHaveBeenCalled();
    expect(mocks.sendNotification).not.toHaveBeenCalled();
  });

  it("does not call either notification boundary when permission is denied", async () => {
    mocks.isPermissionGranted.mockResolvedValue(false);

    const delivered = await sendNewMailNotification(
      [privateMessage()],
      { enabled: true, showPreview: false, soundEnabled: true },
      copy,
    );

    expect(delivered).toBe(false);
    expect(mocks.isPermissionGranted).toHaveBeenCalledOnce();
    expect(mocks.sendDesktopNotification).not.toHaveBeenCalled();
    expect(mocks.sendNotification).not.toHaveBeenCalled();
  });

  it("rechecks delivery permission after the native permission query resolves", async () => {
    let resolvePermission: ((granted: boolean) => void) | undefined;
    mocks.isPermissionGranted.mockImplementationOnce(
      () =>
        new Promise<boolean>((resolve) => {
          resolvePermission = resolve;
        }),
    );
    let stillAllowed = true;
    const delivery = sendNewMailNotification(
      [privateMessage()],
      { enabled: true, showPreview: true, soundEnabled: true },
      copy,
      () => stillAllowed,
    );
    await vi.waitFor(() => expect(resolvePermission).toBeTypeOf("function"));

    stillAllowed = false;
    resolvePermission?.(true);

    await expect(delivery).resolves.toBe(false);
    expect(mocks.sendDesktopNotification).not.toHaveBeenCalled();
    expect(mocks.sendNotification).not.toHaveBeenCalled();
  });

  it("keeps private message content out of the native macOS payload when previews are off", async () => {
    vi.spyOn(navigator, "platform", "get").mockReturnValue("MacIntel");

    const delivered = await sendNewMailNotification(
      [privateMessage()],
      { enabled: true, showPreview: false, soundEnabled: true },
      copy,
    );

    expect(delivered).toBe(true);
    expect(mocks.sendDesktopNotification).toHaveBeenCalledWith({
      title: "New email",
      body: "You have a new message",
      accountId: "account",
      messageId: "1",
      rfcMessageId: "<private-message@example.com>",
      threadId: "1",
      count: 1,
      sound: "Ping",
    });
    expect(
      JSON.stringify(mocks.sendDesktopNotification.mock.calls),
    ).not.toMatch(privateContent);
    expect(mocks.sendNotification).not.toHaveBeenCalled();
  });

  it("keeps private message content out of the plugin payload when previews are off", async () => {
    vi.spyOn(navigator, "platform", "get").mockReturnValue("Linux x86_64");

    const delivered = await sendNewMailNotification(
      [privateMessage()],
      { enabled: true, showPreview: false, soundEnabled: false },
      copy,
    );

    expect(delivered).toBe(true);
    expect(mocks.sendNotification).toHaveBeenCalledWith({
      title: "New email",
      body: "You have a new message",
      extra: {
        accountId: "account",
        messageId: "1",
        rfcMessageId: "<private-message@example.com>",
        threadId: "1",
        count: 1,
      },
      group: "new-mail",
      autoCancel: true,
      sound: undefined,
    });
    expect(JSON.stringify(mocks.sendNotification.mock.calls)).not.toMatch(
      privateContent,
    );
    expect(mocks.sendDesktopNotification).not.toHaveBeenCalled();
  });

  it("does not add recipient or body data to a preview-enabled plugin payload", async () => {
    vi.spyOn(navigator, "platform", "get").mockReturnValue("Linux x86_64");

    const delivered = await sendNewMailNotification(
      [privateMessage()],
      { enabled: true, showPreview: true, soundEnabled: false },
      copy,
    );

    expect(delivered).toBe(true);
    expect(mocks.sendNotification).toHaveBeenCalledWith({
      title: "Private Sender",
      body: "Private medical appointment",
      extra: {
        accountId: "account",
        messageId: "1",
        rfcMessageId: "<private-message@example.com>",
        threadId: "1",
        count: 1,
      },
      group: "new-mail",
      autoCancel: true,
      sound: undefined,
    });
    expect(JSON.stringify(mocks.sendNotification.mock.calls)).not.toMatch(
      privateFieldsOutsidePreview,
    );
  });
});

describe("test notification identity", () => {
  const settings = { enabled: true, showPreview: true, soundEnabled: false };

  it("uses the guarded native notification path on macOS", async () => {
    vi.spyOn(navigator, "platform", "get").mockReturnValue("MacIntel");
    expect(await sendTestNotification(settings, "Test", "Notification")).toBe(
      true,
    );
    expect(mocks.sendDesktopNotification).toHaveBeenCalledWith({
      title: "Test",
      body: "Notification",
      count: 0,
      sound: undefined,
    });
    expect(mocks.sendNotification).not.toHaveBeenCalled();
  });

  it("propagates native identity failure without a plugin fallback", async () => {
    vi.spyOn(navigator, "platform", "get").mockReturnValue("MacIntel");
    mocks.sendDesktopNotification.mockRejectedValueOnce(
      new Error("Notifications unavailable"),
    );
    await expect(
      sendTestNotification(settings, "Test", "Notification"),
    ).rejects.toThrow("Notifications unavailable");
    expect(mocks.sendNotification).not.toHaveBeenCalled();
  });

  it("keeps the plugin path on other supported platforms", async () => {
    vi.spyOn(navigator, "platform", "get").mockReturnValue("Linux x86_64");
    expect(await sendTestNotification(settings, "Test", "Notification")).toBe(
      true,
    );
    expect(mocks.sendNotification).toHaveBeenCalledWith({
      title: "Test",
      body: "Notification",
      group: "new-mail",
      autoCancel: true,
      sound: undefined,
      extra: { count: 0 },
    });
    expect(mocks.sendDesktopNotification).not.toHaveBeenCalled();
  });

  it("does not send when notification permission is denied", async () => {
    vi.spyOn(navigator, "platform", "get").mockReturnValue("MacIntel");
    mocks.isPermissionGranted.mockResolvedValueOnce(false);
    mocks.requestPermission.mockResolvedValueOnce("denied");
    expect(await sendTestNotification(settings, "Test", "Notification")).toBe(
      false,
    );
    expect(mocks.sendDesktopNotification).not.toHaveBeenCalled();
    expect(mocks.sendNotification).not.toHaveBeenCalled();
  });
});
