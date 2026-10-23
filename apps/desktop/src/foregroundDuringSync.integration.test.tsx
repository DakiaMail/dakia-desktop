import { MantineProvider } from "@mantine/core";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import type { ComposeSeed, OutboxEvent } from "./composeWindow";
import type { Account, MailSummary } from "./types";
import { groupMessages } from "./threads";

const mocks = vi.hoisted(() => {
  class SendMessageError extends Error {
    constructor(
      readonly kind:
        "not_submitted" | "delivery_uncertain" | "sent_copy_failed",
    ) {
      super("safe send failure");
    }
  }
  const account = {
    id: "account-1",
    email: "reader@example.test",
    account_name: "Fictional inbox",
    display_name: "Fictional Reader",
    provider_id: "fastmail",
    auth: { type: "password" as const, username: "reader@example.test" },
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
  const message = {
    id: "cached-message",
    account_id: account.id,
    mailbox: "INBOX",
    uid: 41,
    mailbox_uid_validity: 77,
    thread_id: "cached-thread",
    subject: "Cached mail during provider sync",
    from_address: "sender@example.test",
    to_addresses: account.email,
    received_at: "2026-10-21T08:00:00Z",
    snippet: "This message is already local",
    body_text: "",
    is_read: true,
    is_flagged: false,
    has_attachments: false,
  } satisfies MailSummary;
  const outboxHandlers = new Set<(event: OutboxEvent) => void>();
  let composeSeed: ComposeSeed = {};
  const noopListener = vi.fn(async () => () => undefined);
  return {
    SendMessageError,
    account,
    message,
    outboxHandlers,
    getComposeSeed: () => composeSeed,
    setComposeSeed: (seed: ComposeSeed) => {
      composeSeed = seed;
    },
    api: {
      accounts: vi.fn(async () => [account]),
      action: vi.fn(async () => undefined),
      aiAvailable: vi.fn(async () => false),
      classifyPending: vi.fn(async () => 0),
      configureTray: vi.fn(async () => undefined),
      content: vi.fn(async () => ({
        body_text: "Cached provider body remains readable",
        attachments: [],
      })),
      draft: vi.fn(async () => ""),
      forwardAttachments: vi.fn(async () => []),
      mailRebuildStatus: vi.fn(async () => []),
      readDroppedFiles: vi.fn(async () => []),
      readSendRecoverySeed: vi.fn(),
      acknowledgeSendRecoverySeed: vi.fn(async () => undefined),
      recordNotificationDelivered: vi.fn(async () => undefined),
      search: vi.fn(async () => ({
        conversations: groupMessages([message]),
        nextCursor: null,
      })),
      searchRemote: vi.fn(async () => ({
        messages: [],
        limitReached: false,
      })),
      send: vi.fn(),
      setCategory: vi.fn(async () => undefined),
      setRead: vi.fn(async () => undefined),
      setStarred: vi.fn(async () => undefined),
      showAccountContextMenu: vi.fn(async () => undefined),
      smartInbox: vi.fn(async () => ({ sections: [] })),
      starredCount: vi.fn(async () => 0),
      startRealtimeSync: vi.fn(async () => undefined),
      sync: vi.fn(),
      trashMessagesFromSender: vi.fn(async () => ({
        matched: 0,
        moved: 0,
        failed: 0,
      })),
      unsubscribe: vi.fn(async () => ({ kind: "completed" })),
    },
    closeComposeWindow: vi.fn(async () => undefined),
    discardComposeSeed: vi.fn(async () => undefined),
    notifyOutbox: vi.fn(async (event: OutboxEvent) => {
      for (const handler of outboxHandlers) handler(event);
    }),
    onOutboxChanged: vi.fn(async (handler: (event: OutboxEvent) => void) => {
      outboxHandlers.add(handler);
      return () => outboxHandlers.delete(handler);
    }),
    openComposeWindow: vi.fn((seed: ComposeSeed) => {
      composeSeed = seed;
    }),
    hasNativeComposeSeedHandoff: vi.fn(() => false),
    hasNativeComposeRecovery: vi.fn(() => false),
    composeSeedFromSendRecovery: vi.fn(),
    readNativeComposeRecoveryKey: vi.fn(),
    notifyComposeWindowReady: vi.fn(async () => undefined),
    onComposeTarget: vi.fn(async () => () => undefined),
    rememberComposeSeed: vi.fn(() => true),
    persistComposeSeed: vi.fn(async (seed: ComposeSeed) => {
      composeSeed = seed;
    }),
    noopListener,
    showNativeMessage: vi.fn(async () => undefined),
    windowApi: {
      isFocused: vi.fn(async () => true),
      setFocus: vi.fn(async () => undefined),
      show: vi.fn(async () => undefined),
      startDragging: vi.fn(async () => undefined),
    },
  };
});

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => mocks.windowApi,
}));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    listen: vi.fn(async () => () => undefined),
    onDragDropEvent: vi.fn(async () => () => undefined),
  }),
}));
vi.mock("./api", () => ({
  api: mocks.api,
  SendMessageError: mocks.SendMessageError,
}));
vi.mock("./composeWindow", () => ({
  closeComposeWindow: mocks.closeComposeWindow,
  discardComposeSeed: mocks.discardComposeSeed,
  notifyOutbox: mocks.notifyOutbox,
  onComposeSent: mocks.noopListener,
  onOutboxChanged: mocks.onOutboxChanged,
  openComposeWindow: mocks.openComposeWindow,
  hasNativeComposeRecovery: mocks.hasNativeComposeRecovery,
  hasNativeComposeSeedHandoff: mocks.hasNativeComposeSeedHandoff,
  composeSeedFromSendRecovery: mocks.composeSeedFromSendRecovery,
  readNativeComposeRecoveryKey: mocks.readNativeComposeRecoveryKey,
  notifyComposeWindowReady: mocks.notifyComposeWindowReady,
  onComposeTarget: mocks.onComposeTarget,
  rememberComposeSeed: mocks.rememberComposeSeed,
  persistComposeSeed: mocks.persistComposeSeed,
  readComposeSeed: mocks.getComposeSeed,
  readDatabaseComposeSeed: vi.fn(async () => undefined),
}));
vi.mock("./features", () => ({ AI_FEATURES_VISIBLE: true }));
vi.mock("./feedback", () => ({
  createFeedbackComposeSeed: vi.fn(async () => ({
    accountId: mocks.account.id,
    to: "support@dakiamail.com",
    subject: "",
    body: "",
  })),
}));
vi.mock("./nativeFeedback", () => ({
  confirmNativeAction: vi.fn(async () => false),
  showNativeMessage: mocks.showNativeMessage,
}));
vi.mock("./nativeSubscriptions", () => ({
  subscribeNativeGroup: vi.fn(async () => () => undefined),
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
vi.mock("./readerWindow", () => ({
  onReaderWindowFailed: mocks.noopListener,
  onReaderWindowMutated: mocks.noopListener,
  openReaderWindow: vi.fn(async () => undefined),
}));
vi.mock("./realtimeEvents", () => ({
  subscribeRealtimeEvents: vi.fn(async () => () => undefined),
}));
vi.mock("./nativeWindows", () => ({
  onAccountConnected: mocks.noopListener,
  onAccountRemoved: mocks.noopListener,
  onAccountUpdated: mocks.noopListener,
  onDesktopNotificationAction: mocks.noopListener,
  onMailIndexRebuilt: mocks.noopListener,
  onMailRebuildFinished: mocks.noopListener,
  onMailRebuildProgress: mocks.noopListener,
  onMailSyncState: mocks.noopListener,
  onNativeMenuAction: mocks.noopListener,
  onNotificationSettingsChanged: mocks.noopListener,
  onSettingsChanged: mocks.noopListener,
  openAccountWindow: vi.fn(async () => undefined),
  openSettingsWindow: vi.fn(async () => undefined),
  openSettingsWindowForAccount: vi.fn(async () => undefined),
}));
vi.mock("./updater", () => ({
  checkForUpdate: vi.fn(async () => null),
  downloadUpdate: vi.fn(async () => undefined),
  installUpdateAndRelaunch: vi.fn(async () => undefined),
}));

import App from "./App";
import { ComposeApp } from "./ComposeApp";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

describe("foreground reading and composing during provider synchronization", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.outboxHandlers.clear();
    mocks.setComposeSeed({});
    localStorage.clear();
    localStorage.setItem("dakia.analytics", '{"consent":"disabled"}');
    localStorage.setItem("dakia.mail-list-view", "list");
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    mocks.api.accounts.mockResolvedValue([mocks.account]);
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([mocks.message]),
      nextCursor: null,
    });
    mocks.api.content.mockResolvedValue({
      body_text: "Cached provider body remains readable",
      attachments: [],
    });
    mocks.persistComposeSeed.mockImplementation(async (seed: ComposeSeed) => {
      mocks.setComposeSeed(seed);
    });
    mocks.notifyOutbox.mockImplementation(async (event: OutboxEvent) => {
      for (const handler of mocks.outboxHandlers) handler(event);
    });
  });

  it("keeps cached Reader and optimistic Compose work responsive while sync is pending", async () => {
    const synchronization = deferred<{
      syncedCount: number;
      newMessages: MailSummary[];
    }>();
    const submission = deferred<string>();
    mocks.api.sync.mockReturnValue(synchronization.promise);
    mocks.api.send.mockReturnValue(submission.promise);

    const app = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const subject = await screen.findByText(mocks.message.subject);
    await waitFor(() => expect(mocks.onOutboxChanged).toHaveBeenCalledOnce());

    fireEvent.click(screen.getByRole("button", { name: "Sync" }));
    await waitFor(() =>
      expect(mocks.api.sync).toHaveBeenCalledWith(
        mocks.account.id,
        expect.any(Function),
        false,
      ),
    );

    fireEvent.click(subject.closest("button")!);
    expect(
      await screen.findByText("Cached provider body remains readable"),
    ).toBeVisible();
    expect(mocks.api.content).toHaveBeenCalledWith(mocks.message.id, 77);

    fireEvent.click(screen.getByRole("button", { name: "Compose" }));
    expect(mocks.openComposeWindow).toHaveBeenCalledWith({
      accountId: mocks.account.id,
    });

    render(<ComposeApp />);
    await screen.findByRole("button", { name: /^Send/ });
    fireEvent.change(screen.getByLabelText("To"), {
      target: { value: "recipient@example.test" },
    });
    fireEvent.change(screen.getByLabelText("Subject"), {
      target: { value: "Draft remains usable during sync" },
    });
    const body = screen.getByLabelText("Write your message…");
    body.innerHTML = "<p>Fictional body</p>";
    fireEvent.input(body);

    const send = screen.getByRole("button", { name: /^Send/ });
    fireEvent.click(send);

    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    expect(mocks.persistComposeSeed).toHaveBeenCalledWith(
      expect.objectContaining({
        accountId: mocks.account.id,
        subject: "Draft remains usable during sync",
        bodyHtml: "<p>Fictional body</p>",
        sendState: "sending",
      }),
    );
    expect(screen.getByLabelText("1 email sending")).toBeVisible();
    expect(mocks.api.sync).toHaveBeenCalledOnce();

    fireEvent.click(screen.getByRole("button", { name: /^Outbox/ }));
    expect(
      await screen.findByText("Draft remains usable during sync"),
    ).toBeVisible();
    act(() => {
      fireEvent.click(send);
      fireEvent.keyDown(window, { key: "Enter", metaKey: true });
    });
    await act(async () => Promise.resolve());
    expect(mocks.api.send).toHaveBeenCalledOnce();

    submission.resolve("queued");
    await waitFor(() =>
      expect(mocks.notifyOutbox).toHaveBeenCalledWith(
        expect.objectContaining({ phase: "finished" }),
      ),
    );
    expect(
      screen.queryByText("Draft remains usable during sync"),
    ).not.toBeInTheDocument();

    synchronization.resolve({ syncedCount: 0, newMessages: [] });
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Sync" })).toBeEnabled(),
    );
    app.unmount();
  });
});
