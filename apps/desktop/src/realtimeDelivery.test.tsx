import { MantineProvider } from "@mantine/core";
import { act, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import type { Account, MailCursor, MailSummary, MailThread } from "./types";

type TauriEvent = { payload: unknown };
type TauriListener = (event: TauriEvent) => void;

const eventListeners = vi.hoisted(() => new Map<string, TauriListener[]>());
const mocks = vi.hoisted(() => {
  const account = {
    id: "account-1",
    email: "me@example.test",
    account_name: "Inbox",
    display_name: "Me",
    provider_id: "fastmail",
    auth: { type: "password" as const, username: "me@example.test" },
    imap_host: "imap.example.test",
    imap_port: 993,
    imap_security: "tls" as const,
    smtp_host: "smtp.example.test",
    smtp_port: 465,
    smtp_security: "tls" as const,
    archive_mailbox: "Archive",
    spam_mailbox: "Spam",
    enabled: true,
  } satisfies Account;
  const unlisten = () => undefined;
  return {
    account,
    api: {
      action: vi.fn(async () => undefined),
      aiAvailable: vi.fn(async () => false),
      accounts: vi.fn(async (): Promise<Account[]> => [account]),
      classifyPending: vi.fn(async () => 0),
      configureTray: vi.fn(async () => undefined),
      content: vi.fn(async () => ({ body_text: "", attachments: [] })),
      mailRebuildStatus: vi.fn(async () => []),
      recordNotificationDelivered: vi.fn(async () => undefined),
      search: vi.fn(
        async (): Promise<{
          conversations: MailThread[];
          nextCursor: MailCursor | null;
        }> => ({ conversations: [], nextCursor: null }),
      ),
      smartInbox: vi.fn(async () => ({ sections: [] })),
      searchRemote: vi.fn(async () => ({
        messages: [],
        limitReached: false,
      })),
      setRead: vi.fn(async () => undefined),
      setStarred: vi.fn(async () => undefined),
      starredCount: vi.fn(async () => 0),
      startRealtimeSync: vi.fn(async () => undefined),
      sync: vi.fn(async () => ({ syncedCount: 0, newMessages: [] })),
      terminalCommandStatus: vi.fn(async () => ({ installed: false })),
      trashMessagesFromSender: vi.fn(async () => ({
        matched: 0,
        moved: 0,
        failed: 0,
      })),
      unsubscribe: vi.fn(async () => ({ kind: "completed" })),
    },
    windowApi: {
      isFocused: vi.fn(async () => true),
      setFocus: vi.fn(async () => undefined),
      show: vi.fn(async () => undefined),
      startDragging: vi.fn(async () => undefined),
    },
    noopListener: vi.fn(async () => unlisten),
  };
});

vi.mock("@tauri-apps/api/event", () => ({
  emitTo: vi.fn(async () => undefined),
  listen: vi.fn(async (event: string, handler: TauriListener) => {
    const handlers = eventListeners.get(event) ?? [];
    handlers.push(handler);
    eventListeners.set(event, handlers);
    return () => {
      const current = eventListeners.get(event) ?? [];
      eventListeners.set(
        event,
        current.filter((candidate) => candidate !== handler),
      );
    };
  }),
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => mocks.windowApi,
}));

vi.mock("./api", () => ({ api: mocks.api }));
vi.mock("./composeWindow", () => ({
  onComposeSent: mocks.noopListener,
  onOutboxChanged: mocks.noopListener,
  openComposeWindow: vi.fn(),
}));
vi.mock("./feedback", () => ({
  createFeedbackComposeSeed: vi.fn(async () => ({
    accountId: mocks.account.id,
    to: "support@dakiamail.com",
    subject: "",
    body: "",
  })),
}));
vi.mock("./readerWindow", () => ({
  onReaderWindowFailed: mocks.noopListener,
  onReaderWindowMutated: mocks.noopListener,
  openReaderWindow: vi.fn(async () => undefined),
}));
vi.mock("./nativeFeedback", () => ({
  confirmNativeAction: vi.fn(async () => false),
  showNativeMessage: vi.fn(async () => undefined),
}));
vi.mock("./notifications", () => ({
  onNotificationAction: mocks.noopListener,
  readNotificationSettings: () => ({
    enabled: true,
    soundEnabled: true,
    showPreview: true,
  }),
  requestInitialNotificationAccess: vi.fn(async () => undefined),
  sendNewMailNotification: vi.fn(async () => false),
}));
vi.mock("./updater", () => ({
  checkForUpdate: vi.fn(async () => null),
  downloadUpdate: vi.fn(async () => undefined),
  installUpdateAndRelaunch: vi.fn(async () => undefined),
}));

// The realtime functions remain production code. They route lower-level Tauri
// events into App through nativeWindows and realtimeEvents unchanged.
vi.mock("./nativeWindows", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./nativeWindows")>();
  return {
    ...actual,
    onAccountConnected: mocks.noopListener,
    onAccountRemoved: mocks.noopListener,
    onAccountUpdated: mocks.noopListener,
    onDesktopNotificationAction: mocks.noopListener,
    onMailIndexRebuilt: mocks.noopListener,
    onMailRebuildFinished: mocks.noopListener,
    onMailRebuildProgress: mocks.noopListener,
    onNativeMenuAction: mocks.noopListener,
    onNotificationSettingsChanged: mocks.noopListener,
    onSettingsChanged: mocks.noopListener,
    openAccountWindow: vi.fn(async () => undefined),
    openSettingsWindow: vi.fn(async () => undefined),
    openSettingsWindowForAccount: vi.fn(async () => undefined),
  };
});

import App from "./App";
import { groupMessages } from "./threads";

const nodeEnv = (
  globalThis as typeof globalThis & {
    process?: { env?: Record<string, string | undefined> };
  }
).process?.env;
const expectedNonce =
  import.meta.env.VITE_DAKIA_REALTIME_DELIVERY_NONCE ||
  nodeEnv?.VITE_DAKIA_REALTIME_DELIVERY_NONCE;
const encodedArtifact =
  import.meta.env.VITE_DAKIA_REALTIME_DELIVERY_ARTIFACT ||
  nodeEnv?.VITE_DAKIA_REALTIME_DELIVERY_ARTIFACT;
const artifact = encodedArtifact ? JSON.parse(atob(encodedArtifact)) : null;

function asMailSummary(value: unknown): MailSummary {
  return value as MailSummary;
}

describe.skipIf(!encodedArtifact || !expectedNonce)(
  "scripted provider cycle to production native sink to App",
  () => {
    beforeEach(() => {
      eventListeners.clear();
      vi.clearAllMocks();
      localStorage.clear();
      localStorage.setItem("dakia.mail-list-view", "list");
      Object.defineProperty(window, "__TAURI_INTERNALS__", {
        configurable: true,
        value: {},
      });
      if (artifact?.arrival?.accountId)
        mocks.account.id = artifact.arrival.accountId;
      mocks.api.search.mockResolvedValue({
        conversations: [],
        nextCursor: null,
      });
    });

    it("renders the exact MockRuntime arrival through the real native event adapter", async () => {
      expect(artifact).not.toBeNull();
      expect(expectedNonce).toBeTruthy();
      expect(artifact.nonce).toBe(expectedNonce);
      expect(artifact.arrival.accountId).toBe(mocks.account.id);
      expect(artifact.arrival.messages).toHaveLength(1);
      expect(artifact.arrival.messages[0]).toMatchObject({
        account_id: artifact.arrival.accountId,
        mailbox_uid_validity: 424242,
        subject: "Provider runtime arrival",
      });

      render(
        <MantineProvider>
          <App />
        </MantineProvider>,
      );
      await waitFor(() =>
        expect(eventListeners.get("mail-arrived")).toHaveLength(1),
      );

      const runtimeMessage = asMailSummary(artifact.arrival.messages[0]);
      mocks.api.search.mockResolvedValue({
        conversations: groupMessages([runtimeMessage]),
        nextCursor: null,
      });
      act(() => {
        eventListeners.get("mail-arrived")![0]({
          payload: artifact.arrival,
        });
      });

      expect(await screen.findByText(runtimeMessage.subject)).toBeVisible();
      expect(mocks.api.search).toHaveBeenLastCalledWith(
        "",
        [mocks.account.id],
        "INBOX",
        false,
        false,
        100,
        null,
      );
    });
  },
);
