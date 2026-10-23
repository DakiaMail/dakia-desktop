import { MantineProvider } from "@mantine/core";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import fixture from "../testdata/tauri-contracts/high-risk.json";
import "./i18n";
import App from "./App";
import type { OutboxEvent } from "./composeWindow";
import { groupMessages } from "./threads";
import type {
  Account,
  AccountConnection,
  MailArrival,
  MailRebuildFinished,
  MailRebuildProgress,
  MailSummary,
  RemoteSearchResult,
  SmartInboxPage,
  MailThreadPage,
} from "./types";

const smartInboxPage = (
  sections: SmartInboxPage["sections"],
): SmartInboxPage => ({ sections });
const remoteSearchResult = (
  messages: MailSummary[],
  limitReached = false,
): RemoteSearchResult => ({ messages, limitReached });

const mocks = vi.hoisted(() => {
  const unlisten = () => undefined;
  const outboxHandlers: Array<(event: OutboxEvent) => void> = [];
  const rebuildProgressHandlers: Array<
    (progress: MailRebuildProgress) => void
  > = [];
  const rebuildFinishedHandlers: Array<(result: MailRebuildFinished) => void> =
    [];
  const hydratedHandlers: Array<() => void> = [];
  const mailChangedHandlers: Array<() => void> = [];
  const mailArrivedHandlers: Array<(arrival: MailArrival) => void> = [];
  const nativeMenuHandlers: Array<(action: string) => void> = [];
  const notificationActionHandlers: Array<
    (extra: Record<string, unknown>) => void | Promise<void>
  > = [];
  const desktopNotificationActionHandlers: Array<
    (extra: Record<string, unknown>) => void | Promise<void>
  > = [];
  const readerMutationHandlers: Array<() => void> = [];
  const readerFailureHandlers: Array<
    (failure: { accountId: string }) => void | Promise<void>
  > = [];
  const accountRemovedHandlers: Array<(event: { accountId: string }) => void> =
    [];
  const accountUpdatedHandlers: Array<(account: Account) => void> = [];
  const accountConnectedHandlers: Array<
    (connection: AccountConnection) => void
  > = [];
  const account = {
    id: "account-1",
    email: "me@example.com",
    account_name: "Inbox",
    display_name: "Me",
    provider_id: "fastmail",
    auth: { type: "password" as const, username: "me@example.com" },
    imap_host: "imap.example.com",
    imap_port: 993,
    imap_security: "tls" as const,
    smtp_host: "smtp.example.com",
    smtp_port: 465,
    smtp_security: "tls" as const,
    archive_mailbox: "Archive",
    spam_mailbox: "Spam",
    enabled: true,
  } satisfies Account;
  const message = {
    id: "message-1",
    account_id: "account-1",
    mailbox: "INBOX",
    uid: 1,
    mailbox_uid_validity: 77,
    thread_id: "thread-1",
    subject: "Unread thread",
    from_address: "sender@example.com",
    to_addresses: "me@example.com",
    received_at: "2026-07-19T10:00:00Z",
    snippet: "Preview",
    body_text: "Message body",
    is_read: false,
    is_flagged: false,
    has_attachments: false,
  } satisfies MailSummary;
  return {
    account,
    message,
    api: {
      listSendRecoveries: vi.fn(
        async (
          _ids: string[],
          _cursor?: string,
        ): Promise<import("./types").SendRecoveryPage> => ({
          items: [],
          next_cursor: null,
          total_count: 0,
        }),
      ),
      openSendRecoveryWindow: vi.fn(async () => "compose-recovery"),
      action: vi.fn(async () => undefined),
      aiAvailable: vi.fn(async () => false),
      accounts: vi.fn(async (): Promise<Account[]> => [account]),
      classifyPending: vi.fn(async () => 0),
      configureTray: vi.fn(async () => undefined),
      content: vi.fn(
        async (
          _messageId: string,
          _expectedUidValidity: number | null = null,
        ): Promise<import("./types").MessageContent> => ({
          body_text: "Message body",
          attachments: [],
        }),
      ),
      mailRebuildStatus: vi.fn(async () => []),
      recordNotificationDelivered: vi.fn(async () => undefined),
      search: vi.fn(async () => ({
        conversations: groupMessages([message]),
        nextCursor: null as import("./types").MailCursor | null,
      })),
      smartInbox: vi.fn(async (): Promise<SmartInboxPage> => ({
        sections: [],
      })),
      searchRemote: vi.fn(async (): Promise<RemoteSearchResult> => ({
        messages: [],
        limitReached: false,
      })),
      showEmailAddressContextMenu: vi.fn(async () => undefined),
      setRead: vi.fn(async () => undefined),
      setCategory: vi.fn(async () => undefined),
      setStarred: vi.fn(async () => undefined),
      starredCount: vi.fn(async () => 0),
      startRealtimeSync: vi.fn(async () => undefined),
      sync: vi.fn(async () => ({ syncedCount: 0, newMessages: [] })),
      unsubscribe: vi.fn(
        async (): Promise<import("./api").UnsubscribeResult> => ({
          kind: "completed",
          cleanupTarget: {
            accountId: "account-1",
            senderName: "Sender",
            senderAddress: "sender@example.com",
          },
        }),
      ),
      trashMessagesFromSender: vi.fn(async () => ({
        matched: 0,
        moved: 0,
        failed: 0,
      })),
    },
    windowApi: {
      show: vi.fn(async () => undefined),
      setFocus: vi.fn(async () => undefined),
      isFocused: vi.fn(async () => true),
      startDragging: vi.fn(async () => undefined),
    },
    checkForUpdate: vi.fn(async () => null),
    downloadUpdate: vi.fn(async () => undefined),
    installUpdateAndRelaunch: vi.fn(async () => undefined),
    openAccountWindow: vi.fn(async () => undefined),
    showNativeMessage: vi.fn(async () => undefined),
    confirmNativeAction: vi.fn(async () => false),
    requestInitialNotificationAccess: vi.fn(async () => undefined),
    sendNewMailNotification: vi.fn(async () => false),
    openComposeWindow: vi.fn(),
    createFeedbackComposeSeed: vi.fn(async (accountId, locale) => ({
      accountId,
      to: "support@dakiamail.com",
      subject: "Dakia feedback",
      body: `Language: ${locale}`,
    })),
    openReaderWindow: vi.fn(async () => undefined),
    noopListener: vi.fn(async () => unlisten),
    outboxHandlers,
    onOutboxChanged: vi.fn(async (handler: (event: OutboxEvent) => void) => {
      outboxHandlers.push(handler);
      return () => {
        const index = outboxHandlers.indexOf(handler);
        if (index >= 0) outboxHandlers.splice(index, 1);
      };
    }),
    onNativeMenuAction: vi.fn(async (handler: (action: string) => void) => {
      nativeMenuHandlers.push(handler);
      return unlisten;
    }),
    onAccountRemoved: vi.fn(
      async (handler: (event: { accountId: string }) => void) => {
        accountRemovedHandlers.push(handler);
        return unlisten;
      },
    ),
    onAccountUpdated: vi.fn(async (handler: (account: Account) => void) => {
      accountUpdatedHandlers.push(handler);
      return unlisten;
    }),
    onAccountConnected: vi.fn(
      async (handler: (connection: AccountConnection) => void) => {
        accountConnectedHandlers.push(handler);
        return unlisten;
      },
    ),
    accountConnectedHandlers,
    accountRemovedHandlers,
    accountUpdatedHandlers,
    rebuildProgressHandlers,
    rebuildFinishedHandlers,
    hydratedHandlers,
    mailChangedHandlers,
    mailArrivedHandlers,
    nativeMenuHandlers,
    notificationActionHandlers,
    desktopNotificationActionHandlers,
    readerMutationHandlers,
    readerFailureHandlers,
    onReaderWindowMutated: vi.fn(async (handler: () => void) => {
      readerMutationHandlers.push(handler);
      return unlisten;
    }),
    onReaderWindowFailed: vi.fn(
      async (
        handler: (failure: { accountId: string }) => void | Promise<void>,
      ) => {
        readerFailureHandlers.push(handler);
        return unlisten;
      },
    ),
    onNotificationAction: vi.fn(
      async (
        handler: (extra: Record<string, unknown>) => void | Promise<void>,
      ) => {
        notificationActionHandlers.push(handler);
        return unlisten;
      },
    ),
    onDesktopNotificationAction: vi.fn(
      async (
        handler: (extra: Record<string, unknown>) => void | Promise<void>,
      ) => {
        desktopNotificationActionHandlers.push(handler);
        return unlisten;
      },
    ),
    onMailRebuildProgress: vi.fn(
      async (handler: (progress: MailRebuildProgress) => void) => {
        rebuildProgressHandlers.push(handler);
        return unlisten;
      },
    ),
    onMailRebuildFinished: vi.fn(
      async (handler: (result: MailRebuildFinished) => void) => {
        rebuildFinishedHandlers.push(handler);
        return unlisten;
      },
    ),
    onMailHydrated: vi.fn(async (handler: () => void) => {
      hydratedHandlers.push(handler);
      return unlisten;
    }),
    onMailChanged: vi.fn(async (handler: () => void) => {
      mailChangedHandlers.push(handler);
      return unlisten;
    }),
    onMailArrived: vi.fn(async (handler: (arrival: MailArrival) => void) => {
      mailArrivedHandlers.push(handler);
      return unlisten;
    }),
  };
});

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => mocks.windowApi,
}));

vi.mock("./api", () => ({
  api: {
    ...mocks.api,
  },
}));

vi.mock("./composeWindow", () => ({
  onComposeSent: mocks.noopListener,
  onOutboxChanged: mocks.onOutboxChanged,
  openComposeWindow: mocks.openComposeWindow,
}));

vi.mock("./feedback", () => ({
  createFeedbackComposeSeed: mocks.createFeedbackComposeSeed,
}));

vi.mock("./readerWindow", () => ({
  onReaderWindowFailed: mocks.onReaderWindowFailed,
  onReaderWindowMutated: mocks.onReaderWindowMutated,
  openReaderWindow: mocks.openReaderWindow,
}));

vi.mock("./nativeFeedback", () => ({
  confirmNativeAction: mocks.confirmNativeAction,
  showNativeMessage: mocks.showNativeMessage,
}));

vi.mock("./notifications", () => ({
  onNotificationAction: mocks.onNotificationAction,
  readNotificationSettings: () => ({
    enabled: true,
    soundEnabled: true,
    showPreview: true,
  }),
  requestInitialNotificationAccess: mocks.requestInitialNotificationAccess,
  sendNewMailNotification: mocks.sendNewMailNotification,
}));

vi.mock("./nativeWindows", () => ({
  onAccountConnected: mocks.onAccountConnected,
  onAccountRemoved: mocks.onAccountRemoved,
  onAccountUpdated: mocks.onAccountUpdated,
  onMailArrived: mocks.onMailArrived,
  onMailChanged: mocks.onMailChanged,
  onMailHydrated: mocks.onMailHydrated,
  onMailIndexRebuilt: mocks.noopListener,
  onMailRebuildProgress: mocks.onMailRebuildProgress,
  onMailRebuildFinished: mocks.onMailRebuildFinished,
  onMailSyncState: mocks.noopListener,
  onDesktopNotificationAction: mocks.onDesktopNotificationAction,
  onNativeMenuAction: mocks.onNativeMenuAction,
  onNotificationSettingsChanged: mocks.noopListener,
  onSettingsChanged: mocks.noopListener,
  openAccountWindow: mocks.openAccountWindow,
  openSettingsWindow: vi.fn(async () => undefined),
  openSettingsWindowForAccount: vi.fn(async () => undefined),
}));

vi.mock("./updater", () => ({
  checkForUpdate: mocks.checkForUpdate,
  downloadUpdate: mocks.downloadUpdate,
  installUpdateAndRelaunch: mocks.installUpdateAndRelaunch,
}));

function encodeNativeMenuAddress(address: string) {
  const bytes = new TextEncoder().encode(address);
  return btoa(String.fromCharCode(...bytes))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "");
}

describe("App read state", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.outboxHandlers.length = 0;
    mocks.api.listSendRecoveries.mockResolvedValue({
      items: [],
      next_cursor: null,
      total_count: 0,
    });
    mocks.api.openSendRecoveryWindow.mockResolvedValue("compose-recovery");
    mocks.rebuildProgressHandlers.length = 0;
    mocks.rebuildFinishedHandlers.length = 0;
    mocks.hydratedHandlers.length = 0;
    mocks.mailChangedHandlers.length = 0;
    mocks.mailArrivedHandlers.length = 0;
    mocks.nativeMenuHandlers.length = 0;
    mocks.notificationActionHandlers.length = 0;
    mocks.desktopNotificationActionHandlers.length = 0;
    mocks.readerMutationHandlers.length = 0;
    mocks.readerFailureHandlers.length = 0;
    mocks.openReaderWindow.mockClear();
    mocks.accountRemovedHandlers.length = 0;
    mocks.accountUpdatedHandlers.length = 0;
    mocks.accountConnectedHandlers.length = 0;
    localStorage.clear();
    mocks.api.accounts.mockResolvedValue([mocks.account]);
    mocks.api.action.mockResolvedValue(undefined);
    localStorage.setItem("dakia.mail-list-view", "list");
    mocks.api.classifyPending.mockResolvedValue(0);
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([mocks.message]),
      nextCursor: null,
    });
    mocks.api.smartInbox.mockResolvedValue(smartInboxPage([]));
    mocks.api.starredCount.mockResolvedValue(0);
    mocks.api.content.mockResolvedValue({
      body_text: "Message body",
      attachments: [],
    });
  });

  it("offers a durable failed send from Outbox after the child completion event", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await waitFor(() => expect(mocks.outboxHandlers.length).toBe(1));
    await waitFor(() =>
      expect(mocks.api.listSendRecoveries).toHaveBeenCalledWith(
        [mocks.account.id],
        undefined,
      ),
    );
    const attempt = "aa38536f-69f3-4ca1-9911-d68a98019637";
    const saved = {
      account_id: mocks.account.id,
      attempt_id: attempt,
      subject: "Recover this exact submitted message",
      status: "sent_copy_failed" as const,
      updated_at: "2026-10-22T00:00:00Z",
    };
    act(() =>
      mocks.outboxHandlers[0]({
        phase: "sending",
        message: {
          ...mocks.message,
          id: `outbox-${mocks.account.id}-${attempt}`,
          mailbox: "Outbox",
          subject: saved.subject,
        },
      }),
    );
    fireEvent.click(screen.getByRole("button", { name: /^Outbox/ }));
    await screen.findByText(saved.subject);
    mocks.api.listSendRecoveries.mockResolvedValue({
      items: [saved],
      next_cursor: null,
      total_count: 1,
    });
    act(() =>
      mocks.outboxHandlers[0]({
        phase: "finished",
        id: `outbox-${mocks.account.id}-${attempt}`,
      }),
    );
    const recovery = await screen.findByRole("region", {
      name: "Needs attention",
    });
    expect(
      within(recovery).getByText(
        "Sent, but the Sent copy couldn’t be saved. Do not send it again.",
      ),
    ).toBeVisible();
    expect(screen.getByLabelText("1 email needs attention")).toBeVisible();
    expect(screen.getAllByText(saved.subject)).toHaveLength(1);
    expect(screen.queryByText("No matching mail")).not.toBeInTheDocument();
    fireEvent.click(
      within(recovery).getByRole("button", {
        name: `Review message: ${saved.subject}`,
      }),
    );
    expect(mocks.api.openSendRecoveryWindow).toHaveBeenCalledWith(
      mocks.account.id,
      attempt,
    );
    expect(mocks.api.action).not.toHaveBeenCalled();
  });

  it("shows a retry as sending while retaining another account's equal attempt", async () => {
    const other = {
      ...mocks.account,
      id: "account-2",
      account_name: "Other inbox",
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, other]);
    const attempt = "aa38536f-69f3-4ca1-9911-d68a98019637";
    const first = {
      account_id: mocks.account.id,
      attempt_id: attempt,
      subject: "Retry in the first account",
      status: "not_submitted" as const,
      updated_at: "2026-10-22T00:00:00Z",
    };
    const second = {
      ...first,
      account_id: other.id,
      subject: "Keep the other account's recovery",
    };
    mocks.api.listSendRecoveries.mockResolvedValue({
      items: [first, second],
      next_cursor: null,
      total_count: 2,
    });
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await waitFor(() => expect(mocks.outboxHandlers).toHaveLength(1));
    fireEvent.click(screen.getByRole("button", { name: /^Outbox/ }));
    await screen.findByRole("button", {
      name: `Review message: ${first.subject}`,
    });
    const id = `outbox-${mocks.account.id}-${attempt}`;
    act(() =>
      mocks.outboxHandlers[0]({
        phase: "sending",
        message: {
          ...mocks.message,
          id,
          thread_id: id,
          mailbox: "Outbox",
          subject: first.subject,
        },
      }),
    );
    expect(
      screen.queryByRole("button", {
        name: `Review message: ${first.subject}`,
      }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: `Review message: ${second.subject}` }),
    ).toBeVisible();
    expect(screen.getAllByText(first.subject)).toHaveLength(1);
    let resolve!: (page: import("./types").SendRecoveryPage) => void;
    mocks.api.listSendRecoveries.mockReturnValue(
      new Promise((done) => {
        resolve = done;
      }),
    );
    act(() => mocks.outboxHandlers[0]({ phase: "finished", id }));
    expect(
      screen.queryByRole("button", {
        name: `Review message: ${first.subject}`,
      }),
    ).not.toBeInTheDocument();
    expect(screen.getByText("Loading saved messages…")).toBeVisible();
    await act(async () =>
      resolve({
        items: [{ ...first, status: "sent_copy_failed" }, second],
        next_cursor: null,
        total_count: 2,
      }),
    );
    const recovered = screen.getByRole("region", { name: "Needs attention" });
    expect(
      within(recovered).getByRole("button", {
        name: `Review message: ${first.subject}`,
      }),
    ).toHaveTextContent(
      "Sent, but the Sent copy couldn’t be saved. Do not send it again.",
    );
    expect(
      within(recovered).getByRole("button", {
        name: `Review message: ${second.subject}`,
      }),
    ).toHaveTextContent("Not sent. You can review and try again.");
  });

  it("can page past active sends when an empty recovery page still has a cursor", async () => {
    const saved = {
      account_id: mocks.account.id,
      attempt_id: "83153cc6-aa7a-456d-a1c6-70dc77802ae0",
      subject: "Older saved message behind active sends",
      status: "delivery_uncertain" as const,
      updated_at: "2026-10-22T00:00:00Z",
    };
    mocks.api.listSendRecoveries.mockImplementation(
      async (_ids: string[], cursor?: string) =>
        cursor
          ? { items: [saved], next_cursor: null, total_count: 1 }
          : { items: [], next_cursor: "after-100-active", total_count: 1 },
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.api.listSendRecoveries).toHaveBeenCalled(),
    );
    fireEvent.click(screen.getByRole("button", { name: /^Outbox/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Load more" }));
    expect(await screen.findByText(saved.subject)).toBeVisible();
    expect(mocks.api.listSendRecoveries).toHaveBeenLastCalledWith(
      [mocks.account.id],
      "after-100-active",
    );
    expect(screen.queryByText("No matching mail")).not.toBeInTheDocument();
  });

  it("shows unavailable recovery storage as an error instead of an empty Outbox", async () => {
    mocks.api.listSendRecoveries.mockRejectedValue(new Error("disk fault"));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.api.listSendRecoveries).toHaveBeenCalled(),
    );
    fireEvent.click(screen.getByRole("button", { name: /^Outbox/ }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Saved messages couldn’t be loaded. Try again.",
    );
    expect(screen.queryByText("No matching mail")).not.toBeInTheDocument();
    mocks.api.listSendRecoveries.mockResolvedValue({
      items: [],
      next_cursor: null,
      total_count: 0,
    });
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await waitFor(() =>
      expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
    );
  });

  it("observes an existing account connection without invoking a rebuild", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.accountConnectedHandlers.length).toBeGreaterThan(0),
    );
    mocks.api.sync.mockClear();

    act(() => {
      mocks.accountConnectedHandlers.at(-1)!({
        account: { ...mocks.account, id: "existing-account" },
        reusedExistingAccount: true,
      });
    });

    await waitFor(() =>
      expect(screen.getByText("Unread thread")).toBeVisible(),
    );
    expect(mocks.api.sync).not.toHaveBeenCalled();
  });

  it("does not depend on the account-connected listener to start a fresh rebuild", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.accountConnectedHandlers.length).toBeGreaterThan(0),
    );
    mocks.api.sync.mockClear();

    act(() => {
      mocks.accountConnectedHandlers.at(-1)!({
        account: { ...mocks.account, id: "new-account" },
        reusedExistingAccount: false,
      });
    });

    await waitFor(() =>
      expect(screen.getByText("Unread thread")).toBeVisible(),
    );
    expect(mocks.api.sync).not.toHaveBeenCalled();
  });

  it("does not probe an AI provider while AI features are hidden", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    expect(mocks.api.aiAvailable).not.toHaveBeenCalled();
  });

  it("opens an editable support composer from the sidebar feedback action", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const feedback = await screen.findByRole("button", { name: "Feedback" });
    await waitFor(() => expect(feedback).toBeEnabled());
    fireEvent.click(feedback);

    await waitFor(() =>
      expect(mocks.createFeedbackComposeSeed).toHaveBeenCalledWith(
        "account-1",
        "en",
      ),
    );
    expect(mocks.openComposeWindow).toHaveBeenCalledWith({
      accountId: "account-1",
      to: "support@dakiamail.com",
      subject: "Dakia feedback",
      body: "Language: en",
    });
  });

  it("uses the currently selected account as the feedback sender", async () => {
    const workAccount: Account = {
      ...mocks.account,
      id: "account-2",
      email: "work@example.com",
      account_name: "Work",
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, workAccount]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "Work" }));
    const feedback = screen.getByRole("button", { name: "Feedback" });
    await waitFor(() => expect(feedback).toBeEnabled());
    fireEvent.click(feedback);

    await waitFor(() =>
      expect(mocks.createFeedbackComposeSeed).toHaveBeenCalledWith(
        "account-2",
        "en",
      ),
    );
  });

  it("routes feedback through account setup when no sender account exists", async () => {
    mocks.api.accounts.mockResolvedValue([]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const feedback = await screen.findByRole("button", { name: "Feedback" });
    await waitFor(() => expect(feedback).toBeEnabled());
    await waitFor(() => expect(mocks.openAccountWindow).toHaveBeenCalledOnce());
    mocks.openAccountWindow.mockClear();
    mocks.showNativeMessage.mockClear();

    fireEvent.click(feedback);

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "New message",
        "Connect an account before composing.",
        "warning",
      ),
    );
    await waitFor(() => expect(mocks.openAccountWindow).toHaveBeenCalledOnce());
    expect(mocks.createFeedbackComposeSeed).not.toHaveBeenCalled();
    expect(mocks.openComposeWindow).not.toHaveBeenCalled();
  });

  it("does not route feedback to account setup before accounts finish loading", async () => {
    let resolveAccounts: ((accounts: Account[]) => void) | undefined;
    mocks.api.accounts.mockImplementationOnce(
      () =>
        new Promise<Account[]>((resolve) => {
          resolveAccounts = resolve;
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const feedback = screen.getByRole("button", { name: "Feedback" });
    expect(feedback).toBeDisabled();
    fireEvent.click(feedback);
    expect(mocks.openAccountWindow).not.toHaveBeenCalled();

    act(() => resolveAccounts?.([mocks.account]));
    await waitFor(() => expect(feedback).toBeEnabled());
    fireEvent.click(feedback);
    await waitFor(() =>
      expect(mocks.createFeedbackComposeSeed).toHaveBeenCalledWith(
        "account-1",
        "en",
      ),
    );
  });

  it("shows only one error prompt when repeated update menu events share a failed check", async () => {
    mocks.checkForUpdate
      .mockResolvedValueOnce(null)
      .mockRejectedValueOnce(new TypeError("Load failed"));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() => expect(mocks.nativeMenuHandlers).not.toHaveLength(0));
    const menuHandler = mocks.nativeMenuHandlers.at(-1)!;

    act(() => {
      for (let index = 0; index < 20; index += 1) {
        menuHandler("check-for-updates");
      }
    });

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Could not check for updates",
        "TypeError: Load failed",
        "error",
      ),
    );
    expect(mocks.checkForUpdate).toHaveBeenCalledTimes(2);
    expect(mocks.showNativeMessage).toHaveBeenCalledTimes(1);
  });

  it("unlistens when menu listener setup finishes after its effect was replaced", async () => {
    let finishListenerSetup: ((unlisten: () => undefined) => void) | undefined;
    const staleUnlisten = vi.fn(() => undefined);
    mocks.onNativeMenuAction.mockImplementationOnce(
      () =>
        new Promise<() => undefined>((resolve) => {
          finishListenerSetup = resolve;
        }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    expect(mocks.onNativeMenuAction.mock.calls.length).toBeGreaterThan(1);

    act(() => finishListenerSetup?.(staleUnlisten));

    await waitFor(() => expect(staleUnlisten).toHaveBeenCalledOnce());
  });

  it("moves keyboard and native-menu search focus to the visible search field", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("textbox", { name: "Search mail" });
    search.blur();
    fireEvent.keyDown(document.documentElement, { key: "/" });
    expect(search).toHaveFocus();

    search.blur();
    await waitFor(() =>
      expect(mocks.nativeMenuHandlers.length).toBeGreaterThan(0),
    );
    act(() => mocks.nativeMenuHandlers.at(-1)!("search"));
    expect(search).toHaveFocus();
  });

  it("marks an unread conversation as read when opened", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = await screen.findByText("Unread thread");
    fireEvent.click(row.closest("button")!);

    await waitFor(() =>
      expect(mocks.api.setRead).toHaveBeenCalledWith("message-1", true, 77),
    );
  });

  it("shows mark-as-read immediately while the server mutation is pending", async () => {
    let finishRead: (() => void) | undefined;
    mocks.api.setRead.mockImplementationOnce(
      () =>
        new Promise<undefined>((resolve) => {
          finishRead = () => resolve(undefined);
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = (await screen.findByText("Unread thread")).closest("button")!;
    fireEvent.click(row);

    await waitFor(() => expect(row).toHaveAttribute("data-unread", "false"));
    expect(finishRead).toBeTypeOf("function");
    act(() => finishRead?.());
  });

  it("restores the affected row when a read mutation fails in the same view", async () => {
    let rejectRead: ((error: Error) => void) | undefined;
    mocks.api.setRead.mockImplementationOnce(
      () =>
        new Promise<undefined>((_resolve, reject) => {
          rejectRead = reject;
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = (await screen.findByText("Unread thread")).closest("button")!;
    fireEvent.click(row);
    await waitFor(() => expect(row).toHaveAttribute("data-unread", "false"));

    await act(async () => {
      rejectRead!(new Error("same-view read failed"));
      await Promise.resolve();
    });

    expect(row).toHaveAttribute("data-unread", "true");
  });

  it("does not undo a newer star while restoring a failed read mutation", async () => {
    let rejectRead: ((error: Error) => void) | undefined;
    let finishStar: (() => void) | undefined;
    mocks.api.setRead.mockImplementationOnce(
      () =>
        new Promise<undefined>((_resolve, reject) => {
          rejectRead = reject;
        }),
    );
    mocks.api.setStarred.mockImplementationOnce(
      () =>
        new Promise<undefined>((resolve) => {
          finishStar = () => resolve(undefined);
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    await waitFor(() => expect(rejectRead).toBeTypeOf("function"));
    const starButton = screen
      .getAllByRole("button", { name: "Star conversation" })
      .find((item) => item.tagName === "SPAN");
    expect(starButton).toBeDefined();
    fireEvent.click(starButton!);
    await waitFor(() => expect(finishStar).toBeTypeOf("function"));
    expect(starButton).toHaveAttribute("aria-label", "Remove star");
    expect(starButton).toHaveAttribute("data-active", "true");

    await act(async () => {
      rejectRead!(new Error("older read failed"));
      await Promise.resolve();
    });

    expect(starButton).toHaveAttribute("aria-label", "Remove star");
    expect(starButton).toHaveAttribute("data-active", "true");
    act(() => finishStar?.());
  });

  it("still restores a failed read after a same-view refresh also fails", async () => {
    let rejectRead: ((error: Error) => void) | undefined;
    mocks.api.setRead.mockImplementationOnce(
      () =>
        new Promise<undefined>((_resolve, reject) => {
          rejectRead = reject;
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = (await screen.findByText("Unread thread")).closest("button")!;
    fireEvent.click(row);
    await waitFor(() => expect(rejectRead).toBeTypeOf("function"));
    mocks.api.search.mockRejectedValueOnce(new Error("refresh failed"));
    act(() => mocks.mailChangedHandlers.at(-1)!());
    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "refresh failed",
        "error",
      ),
    );

    await act(async () => {
      rejectRead!(new Error("read failed"));
      await Promise.resolve();
    });

    expect(row).toHaveAttribute("data-unread", "true");
  });

  it("keeps the latest read intent when an older opposing mutation fails late", async () => {
    let rejectOlder: ((error: Error) => void) | undefined;
    let finishLatest: (() => void) | undefined;
    mocks.api.setRead
      .mockImplementationOnce(
        () =>
          new Promise<undefined>((_resolve, reject) => {
            rejectOlder = reject;
          }),
      )
      .mockImplementationOnce(
        () =>
          new Promise<undefined>((resolve) => {
            finishLatest = () => resolve(undefined);
          }),
      );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    const markUnread = await screen.findByRole("button", {
      name: "Mark as unread",
    });
    expect(rejectOlder).toBeTypeOf("function");

    fireEvent.click(markUnread);

    expect(
      await screen.findByRole("button", { name: "Mark as read" }),
    ).toBeVisible();
    expect(finishLatest).toBeTypeOf("function");

    await act(async () => {
      rejectOlder!(new Error("older mark-read failed"));
      await Promise.resolve();
    });

    expect(screen.getByRole("button", { name: "Mark as read" })).toBeVisible();
    expect(screen.queryByText("Could not mark 1 message as read")).toBeNull();
    act(() => finishLatest?.());
  });

  it("does not restore removed-account mail when an in-flight mutation fails", async () => {
    let rejectRead: ((error: Error) => void) | undefined;
    mocks.api.setRead.mockImplementationOnce(
      () =>
        new Promise<undefined>((_resolve, reject) => {
          rejectRead = reject;
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    await waitFor(() => expect(rejectRead).toBeTypeOf("function"));
    await waitFor(() =>
      expect(mocks.accountRemovedHandlers.length).toBeGreaterThan(0),
    );

    act(() => {
      mocks.accountRemovedHandlers.at(-1)!({ accountId: "account-1" });
    });
    expect(
      await screen.findByText("Bring your inboxes together"),
    ).toBeVisible();

    await act(async () => {
      rejectRead!(new Error("late provider failure"));
      await Promise.resolve();
    });

    expect(screen.queryByText("Unread thread")).not.toBeInTheDocument();
    expect(screen.getByText("Bring your inboxes together")).toBeVisible();
    expect(screen.queryByText("Could not mark 1 message as read")).toBeNull();
  });

  it("does not restore an old regular-list snapshot after leaving and returning to an account", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "second@example.com",
      account_name: "Second inbox",
    };
    const secondMessage = {
      ...mocks.message,
      id: "message-2",
      account_id: secondAccount.id,
      thread_id: "thread-2",
      subject: "Second account unread",
    };
    const refreshedFirstMessage = {
      ...mocks.message,
      id: "fresh-message-1",
      thread_id: "fresh-thread-1",
      subject: "Fresh first-account state",
    };
    let firstAccountRefreshed = false;
    let rejectRead: ((error: Error) => void) | undefined;
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.search.mockImplementation(async (...args: unknown[]) => ({
      conversations: groupMessages(
        (args[1] as string[]).length === 1
          ? (args[1] as string[])[0] === secondAccount.id
            ? [secondMessage]
            : firstAccountRefreshed
              ? [refreshedFirstMessage]
              : [mocks.message]
          : [mocks.message],
      ),
      nextCursor: null,
    }));
    mocks.api.setRead.mockImplementationOnce(
      () =>
        new Promise<undefined>((_resolve, reject) => {
          rejectRead = reject;
        }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.click(await screen.findByTitle(mocks.account.email));
    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    await waitFor(() => expect(rejectRead).toBeTypeOf("function"));
    fireEvent.click(screen.getByTitle(secondAccount.email));
    expect(await screen.findByText(secondMessage.subject)).toBeVisible();
    firstAccountRefreshed = true;
    fireEvent.click(await screen.findByTitle(mocks.account.email));
    expect(
      await screen.findByText(refreshedFirstMessage.subject),
    ).toBeVisible();

    await act(async () => {
      rejectRead!(new Error("account A read failed late"));
      await Promise.resolve();
    });

    expect(screen.getByText(refreshedFirstMessage.subject)).toBeVisible();
    expect(screen.queryByText("Unread thread")).not.toBeInTheDocument();
  });

  it("does not restore an old Smart Inbox snapshot after leaving and returning to an account", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "second@example.com",
      account_name: "Second inbox",
    };
    const firstMessage = { ...mocks.message, category: "people" as const };
    const secondMessage = {
      ...mocks.message,
      id: "message-2",
      account_id: secondAccount.id,
      thread_id: "thread-2",
      subject: "Second Smart unread",
      category: "people" as const,
    };
    const refreshedFirstMessage = {
      ...firstMessage,
      id: "fresh-message-1",
      thread_id: "fresh-thread-1",
      subject: "Fresh first-account Smart state",
    };
    let firstAccountRefreshed = false;
    let rejectRead: ((error: Error) => void) | undefined;
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.smartInbox.mockImplementation(async (...args: unknown[]) => {
      const accountIds = args[0] as string[];
      const message =
        accountIds.length === 1
          ? accountIds[0] === secondAccount.id
            ? secondMessage
            : firstAccountRefreshed
              ? refreshedFirstMessage
              : firstMessage
          : firstMessage;
      return smartInboxPage([
        {
          id: "people",
          conversations: groupMessages([message]),
          nextCursor: null,
        },
      ]);
    });
    mocks.api.setRead.mockImplementationOnce(
      () =>
        new Promise<undefined>((_resolve, reject) => {
          rejectRead = reject;
        }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.click(await screen.findByTitle(mocks.account.email));
    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    await waitFor(() => expect(rejectRead).toBeTypeOf("function"));
    fireEvent.click(screen.getByTitle(secondAccount.email));
    expect(await screen.findByText(secondMessage.subject)).toBeVisible();
    firstAccountRefreshed = true;
    fireEvent.click(await screen.findByTitle(mocks.account.email));
    expect(
      await screen.findByText(refreshedFirstMessage.subject),
    ).toBeVisible();

    await act(async () => {
      rejectRead!(new Error("account A read failed late"));
      await Promise.resolve();
    });

    expect(screen.getByText(refreshedFirstMessage.subject)).toBeVisible();
    expect(screen.queryByText("Unread thread")).not.toBeInTheDocument();
  });

  it("archives consecutive conversations immediately without waiting for the network", async () => {
    const messages = [1, 2, 3].map((uid) => ({
      ...mocks.message,
      id: `message-${uid}`,
      uid,
      thread_id: `thread-${uid}`,
      subject: `Conversation ${uid}`,
      received_at: `2026-07-19T1${3 - uid}:00:00Z`,
    }));
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages(messages),
      nextCursor: null,
    });
    const finishActions: Array<() => void> = [];
    mocks.api.action.mockImplementation(
      () =>
        new Promise<undefined>((resolve) => {
          finishActions.push(() => resolve(undefined));
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Conversation 1")).closest("button")!,
    );
    fireEvent.keyDown(document.documentElement, { key: "e" });
    await waitFor(() =>
      expect(screen.queryByText("Conversation 1")).not.toBeInTheDocument(),
    );
    const searchesBeforeRefresh = mocks.api.search.mock.calls.length;
    act(() => mocks.mailChangedHandlers.at(-1)?.());
    await waitFor(() =>
      expect(mocks.api.search.mock.calls.length).toBeGreaterThan(
        searchesBeforeRefresh,
      ),
    );
    expect(screen.queryByText("Conversation 1")).not.toBeInTheDocument();

    fireEvent.keyDown(document.documentElement, { key: "e" });
    await waitFor(() =>
      expect(screen.queryByText("Conversation 2")).not.toBeInTheDocument(),
    );
    fireEvent.keyDown(document.documentElement, { key: "e" });

    await waitFor(() => {
      expect(screen.queryByText("Conversation 3")).not.toBeInTheDocument();
      expect(mocks.api.action).toHaveBeenCalledTimes(3);
    });
    expect(
      (mocks.api.action.mock.calls as unknown[][]).map((call) => [
        call[2],
        call[4],
        call[5],
      ]),
    ).toEqual([
      [1, "message-1", 77],
      [2, "message-2", 77],
      [3, "message-3", 77],
    ]);
    expect(finishActions).toHaveLength(3);

    mocks.api.search.mockResolvedValue({ conversations: [], nextCursor: null });
    const searchesBeforeSettlement = mocks.api.search.mock.calls.length;
    await act(async () => {
      finishActions[0]();
      await Promise.resolve();
    });
    expect(mocks.api.search).toHaveBeenCalledTimes(searchesBeforeSettlement);
    act(() => finishActions.slice(1).forEach((finish) => finish()));
    await waitFor(() =>
      expect(mocks.api.search.mock.calls.length).toBeGreaterThan(
        searchesBeforeSettlement,
      ),
    );
  });

  it("restores only a failed batch target without undoing a newer mailbox action", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "second@example.com",
      account_name: "Second inbox",
    };
    const first = {
      ...mocks.message,
      id: "batch-first",
      uid: 11,
      mailbox_uid_validity: 101,
      thread_id: "batch-thread-first",
      subject: "Batch first",
      received_at: "2026-07-19T12:00:00Z",
    };
    const failed = {
      ...mocks.message,
      id: "batch-failed",
      account_id: secondAccount.id,
      uid: 22,
      mailbox_uid_validity: 202,
      thread_id: "batch-thread-failed",
      subject: "Batch failed",
      received_at: "2026-07-19T11:00:00Z",
    };
    const newer = {
      ...mocks.message,
      id: "newer-action",
      uid: 33,
      mailbox_uid_validity: 303,
      thread_id: "newer-thread",
      subject: "Newer action",
      received_at: "2026-07-19T10:00:00Z",
    };
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([first, failed, newer]),
      nextCursor: null,
    });
    const completions = new Map<
      string,
      { resolve: () => void; reject: (error: Error) => void }
    >();
    mocks.api.action.mockImplementation(
      (...args: unknown[]) =>
        new Promise<undefined>((resolve, reject) => {
          completions.set(args[4] as string, {
            resolve: () => resolve(undefined),
            reject,
          });
        }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    for (const subject of ["Batch first", "Batch failed"]) {
      const row = (await screen.findByText(subject)).closest("button")!;
      fireEvent.click(within(row).getByRole("checkbox", { name: "Select" }));
    }
    expect(screen.getByText("2 selected")).toBeVisible();
    fireEvent.click(
      within(document.querySelector(".mail-list-panel")!).getByRole("button", {
        name: "Archive",
      }),
    );

    await waitFor(() => {
      expect(screen.queryByText("Batch first")).not.toBeInTheDocument();
      expect(screen.queryByText("Batch failed")).not.toBeInTheDocument();
    });
    expect(screen.getByText("Newer action")).toBeVisible();
    expect(completions.has("batch-first")).toBe(true);
    expect(completions.has("batch-failed")).toBe(true);

    fireEvent.click(screen.getByText("Newer action").closest("button")!);
    fireEvent.keyDown(document.documentElement, { key: "e" });
    await waitFor(() => {
      expect(screen.queryByText("Newer action")).not.toBeInTheDocument();
      expect(completions.has("newer-action")).toBe(true);
    });

    await act(async () => {
      completions.get("newer-action")!.resolve();
      await Promise.resolve();
    });
    expect(screen.queryByText("Newer action")).not.toBeInTheDocument();

    // The next catalogue load represents the authoritative server outcome:
    // only the failed member remains in Inbox.
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([failed]),
      nextCursor: null,
    });
    await act(async () => {
      completions.get("batch-first")!.resolve();
      completions
        .get("batch-failed")!
        .reject(new Error("second account archive failed"));
      await Promise.resolve();
    });

    expect(await screen.findByText("Batch failed")).toBeVisible();
    expect(screen.queryByText("Batch first")).not.toBeInTheDocument();
    expect(screen.queryByText("Newer action")).not.toBeInTheDocument();
    expect(
      screen.getByText("Archived 1; 1 could not be archived"),
    ).toBeVisible();
    expect(screen.getByText("1 selected")).toBeVisible();
    expect(mocks.api.action.mock.calls).toEqual(
      expect.arrayContaining([
        ["account-1", "INBOX", 11, "archive", "batch-first", 101],
        ["account-2", "INBOX", 22, "archive", "batch-failed", 202],
        ["account-1", "INBOX", 33, "archive", "newer-action", 303],
      ]),
    );
  });

  it("does not restore an old mailbox-action snapshot after returning to its account", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "second@example.com",
      account_name: "Second inbox",
    };
    const secondMessage = {
      ...mocks.message,
      id: "message-2",
      account_id: secondAccount.id,
      thread_id: "thread-2",
      subject: "Second account action row",
    };
    const refreshedFirstMessage = {
      ...mocks.message,
      id: "fresh-message-1",
      thread_id: "fresh-thread-1",
      subject: "Fresh first-account action state",
    };
    let rejectAction: ((error: Error) => void) | undefined;
    let firstAccountLoads = 0;
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      const accountIds = args[1] as string[];
      if (accountIds.length === 1 && accountIds[0] === secondAccount.id) {
        return {
          conversations: groupMessages([secondMessage]),
          nextCursor: null,
        };
      }
      if (accountIds.length === 1 && accountIds[0] === mocks.account.id) {
        firstAccountLoads += 1;
        if (firstAccountLoads > 2) return new Promise(() => undefined);
        return {
          conversations: groupMessages([
            firstAccountLoads === 1 ? mocks.message : refreshedFirstMessage,
          ]),
          nextCursor: null,
        };
      }
      return {
        conversations: groupMessages([mocks.message]),
        nextCursor: null,
      };
    });
    mocks.api.action.mockImplementationOnce(
      () =>
        new Promise<undefined>((_resolve, reject) => {
          rejectAction = reject;
        }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.click(await screen.findByTitle(mocks.account.email));
    await waitFor(() => expect(firstAccountLoads).toBe(1));
    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    fireEvent.keyDown(document.documentElement, { key: "e" });
    await waitFor(() => expect(rejectAction).toBeTypeOf("function"));
    fireEvent.click(screen.getByTitle(secondAccount.email));
    expect(await screen.findByText(secondMessage.subject)).toBeVisible();
    fireEvent.click(screen.getByTitle(mocks.account.email));
    expect(
      await screen.findByText(refreshedFirstMessage.subject),
    ).toBeVisible();

    act(() => rejectAction!(new Error("account A archive failed late")));
    await waitFor(() => expect(firstAccountLoads).toBeGreaterThan(2));

    expect(screen.getByText(refreshedFirstMessage.subject)).toBeVisible();
    expect(screen.queryByText("Unread thread")).not.toBeInTheDocument();
  });

  it("opens a new composer for the selected header mailbox", async () => {
    const selectedAddress = "selected+header@example.com";
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([
        {
          ...mocks.message,
          from_name: "Selected Sender",
          from_address: selectedAddress,
        },
      ]),
      nextCursor: null,
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    mocks.openComposeWindow.mockClear();
    fireEvent.click(
      await screen.findByRole("button", {
        name: `Email ${selectedAddress}`,
      }),
    );

    expect(mocks.openComposeWindow).toHaveBeenCalledOnce();
    expect(mocks.openComposeWindow).toHaveBeenCalledWith({
      accountId: "account-1",
      to: selectedAddress,
    });
  });

  it("leaves native Copy actions to the Rust clipboard path", async () => {
    const writeText = vi.fn();
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    await screen.findByRole("button", { name: "Email sender@example.com" });
    const menuHandler = mocks.nativeMenuHandlers.at(-1)!;
    act(() =>
      menuHandler(
        `copy-email-address:${encodeNativeMenuAddress("sender@example.com")}`,
      ),
    );

    expect(writeText).not.toHaveBeenCalled();

    act(() => menuHandler("copy-email-address-failed"));
    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "Could not copy text",
        "error",
      ),
    );
  });

  it("opens a composer from the native address context menu", async () => {
    const selectedAddress = "séndér@example.com";
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    const address = await screen.findByRole("button", {
      name: "Email sender@example.com",
    });
    fireEvent.contextMenu(address, { clientX: 140, clientY: 90 });
    await waitFor(() =>
      expect(mocks.api.showEmailAddressContextMenu).toHaveBeenCalledOnce(),
    );
    mocks.openComposeWindow.mockClear();
    const menuHandler = mocks.nativeMenuHandlers.at(-1)!;
    act(() =>
      menuHandler(
        `compose-email-address:account-1:${encodeNativeMenuAddress(selectedAddress)}`,
      ),
    );

    expect(mocks.openComposeWindow).toHaveBeenCalledWith({
      accountId: "account-1",
      to: selectedAddress,
    });
  });

  it("updates every concrete duplicate copy instead of synthetic winner state", async () => {
    const hiddenInbox = {
      ...mocks.message,
      id: "duplicate-inbox",
      uid: 10,
      message_id: "<duplicate@example.test>",
      received_at: "2026-07-19T09:00:00Z",
      is_read: false,
      is_flagged: true,
      has_attachments: true,
    };
    const visibleArchive = {
      ...mocks.message,
      id: "duplicate-archive",
      uid: 11,
      mailbox: "Archive",
      message_id: "<duplicate@example.test>",
      received_at: "2026-07-19T10:00:00Z",
      is_read: true,
      is_flagged: false,
      has_attachments: false,
    };
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([visibleArchive, hiddenInbox]),
      nextCursor: null,
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = await screen.findByText("Unread thread");
    fireEvent.click(row.closest("button")!);
    await waitFor(() =>
      expect(mocks.api.setRead).toHaveBeenCalledWith(hiddenInbox.id, true, 77),
    );
    expect(mocks.api.setRead).not.toHaveBeenCalledWith(
      visibleArchive.id,
      true,
      77,
    );

    fireEvent.click(screen.getByRole("button", { name: "Remove star" }));
    await waitFor(() => {
      expect(mocks.api.setStarred).toHaveBeenCalledWith(
        hiddenInbox.id,
        false,
        77,
      );
      expect(mocks.api.setStarred).toHaveBeenCalledWith(
        visibleArchive.id,
        false,
        77,
      );
    });
  });

  it("opens a conversation with its newest message focused", async () => {
    const older = {
      ...mocks.message,
      id: "message-older",
      uid: 1,
      received_at: "2026-07-19T09:00:00Z",
      snippet: "Earlier preview",
    };
    const latest = {
      ...mocks.message,
      id: "message-latest",
      uid: 2,
      received_at: "2026-07-19T10:00:00Z",
      snippet: "Newest preview",
    };
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([latest, older]),
      nextCursor: null,
    });
    mocks.api.content.mockImplementation(async (id: string) => ({
      body_text: id === latest.id ? "Newest body" : "Earlier body",
      attachments: [],
    }));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = await screen.findByText("Unread thread");
    fireEvent.click(row.closest("button")!);

    expect(await screen.findByText("Newest body")).toBeVisible();
    expect(mocks.api.content).toHaveBeenCalledWith(latest.id, 77);
    expect(mocks.api.content).not.toHaveBeenCalledWith(older.id, 77);
  });

  it("opens a main-list thread in a dedicated window on double-click", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = await screen.findByText("Unread thread");
    fireEvent.doubleClick(row.closest("button")!);

    expect(mocks.openReaderWindow).toHaveBeenCalledWith({
      target: {
        accountId: mocks.account.id,
        localMessageId: mocks.message.id,
        rfcMessageId: undefined,
        threadId: mocks.message.thread_id,
        mailbox: "INBOX",
      },
      focusedMessageId: mocks.message.id,
    });
  });

  it("categorizes the clicked message with its captured mailbox epoch", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.contextMenu(
      (await screen.findByText("Unread thread")).closest("button")!,
      { clientX: 140, clientY: 90 },
    );
    const categorize = await screen.findByRole("menuitem", {
      name: "Categorize",
      hidden: true,
    });
    fireEvent.mouseEnter(categorize);
    fireEvent.click(categorize);
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "People", hidden: true }),
    );

    await waitFor(() =>
      expect(mocks.api.setCategory).toHaveBeenCalledWith(
        "message-1",
        "people",
        77,
      ),
    );
  });

  it("replaces an in-flight body when a UIDVALIDITY rollover reuses its stable message id", async () => {
    let currentEpoch = 77;
    let resolveOldContent:
      ((content: import("./types").MessageContent) => void) | undefined;
    const oldContent = new Promise<import("./types").MessageContent>(
      (resolve) => {
        resolveOldContent = resolve;
      },
    );
    const oldMessage = {
      ...mocks.message,
      subject: "Old epoch header",
      mailbox_uid_validity: 77,
      unsubscribe_kind: "one_click" as const,
    };
    const replacement = {
      ...oldMessage,
      subject: "Replacement epoch header",
      mailbox_uid_validity: 78,
    };
    mocks.api.search.mockImplementation(async () => ({
      conversations: groupMessages([
        currentEpoch === 77 ? oldMessage : replacement,
      ]),
      nextCursor: null,
    }));
    mocks.api.content.mockImplementation(
      (_id: string, epoch: number | null = null) =>
        epoch === 77
          ? oldContent
          : Promise.resolve({
              body_text: "Replacement epoch body",
              attachments: [],
            }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Old epoch header")).closest("button")!,
    );
    await waitFor(() =>
      expect(mocks.api.content).toHaveBeenCalledWith("message-1", 77),
    );
    await waitFor(() => expect(mocks.mailChangedHandlers).not.toHaveLength(0));

    currentEpoch = 78;
    await act(async () => mocks.mailChangedHandlers.at(-1)!());

    expect(
      await screen.findByRole("heading", { name: "Replacement epoch header" }),
    ).toBeVisible();
    expect(await screen.findByText("Replacement epoch body")).toBeVisible();
    expect(mocks.api.content).toHaveBeenCalledWith("message-1", 78);

    fireEvent.click(screen.getByRole("button", { name: "Unsubscribe" }));
    await waitFor(() =>
      expect(mocks.api.unsubscribe).toHaveBeenCalledWith("message-1", 78),
    );

    await act(async () =>
      resolveOldContent?.({ body_text: "Old epoch body", attachments: [] }),
    );
    expect(screen.queryByText("Old epoch body")).not.toBeInTheDocument();
    expect(screen.getByText("Replacement epoch body")).toBeVisible();
  });

  it("opens a targeted notification in a dedicated conversation window", async () => {
    const older = {
      ...mocks.message,
      id: "message-notified",
      uid: 1,
      received_at: "2026-07-19T09:00:00Z",
      snippet: "Notified preview",
    };
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.notificationActionHandlers).not.toHaveLength(0),
    );
    await act(async () => {
      await mocks.notificationActionHandlers.at(-1)!({
        accountId: mocks.account.id,
        messageId: older.id,
        rfcMessageId: "<notified@example.com>",
        threadId: older.thread_id,
        count: 1,
      });
    });

    expect(mocks.openReaderWindow).toHaveBeenCalledWith({
      target: {
        accountId: mocks.account.id,
        localMessageId: older.id,
        rfcMessageId: "<notified@example.com>",
        threadId: older.thread_id,
        mailbox: "INBOX",
      },
      focusedMessageId: older.id,
    });
    expect(mocks.windowApi.show).not.toHaveBeenCalled();
    expect(mocks.windowApi.setFocus).not.toHaveBeenCalled();
  });

  it("keeps grouped notifications in the main Inbox", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.notificationActionHandlers).not.toHaveLength(0),
    );
    await act(async () => {
      await mocks.notificationActionHandlers.at(-1)!({ count: 2 });
    });

    expect(mocks.openReaderWindow).not.toHaveBeenCalled();
    expect(mocks.windowApi.show).toHaveBeenCalled();
    expect(mocks.windowApi.setFocus).toHaveBeenCalled();
    expect(mocks.api.search.mock.calls).toEqual(
      expect.arrayContaining([
        expect.arrayContaining(["", [mocks.account.id], "INBOX"]),
      ]),
    );
  });

  it("falls back to the main Inbox when a notification reader cannot open", async () => {
    mocks.openReaderWindow.mockRejectedValueOnce(new Error("window failed"));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.notificationActionHandlers).not.toHaveLength(0),
    );
    await act(async () => {
      await mocks.notificationActionHandlers.at(-1)!({
        accountId: mocks.account.id,
        messageId: mocks.message.id,
        threadId: mocks.message.thread_id,
        count: 1,
      });
    });

    expect(mocks.showNativeMessage).toHaveBeenCalled();
    expect(mocks.windowApi.show).toHaveBeenCalled();
    expect(mocks.windowApi.setFocus).toHaveBeenCalled();
  });

  it("returns a failed reader target to its account Inbox", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.readerFailureHandlers).not.toHaveLength(0),
    );

    await act(async () => {
      await mocks.readerFailureHandlers.at(-1)!({
        accountId: mocks.account.id,
      });
    });

    expect(mocks.windowApi.show).toHaveBeenCalled();
    expect(mocks.windowApi.setFocus).toHaveBeenCalled();
    expect(mocks.api.search.mock.calls).toEqual(
      expect.arrayContaining([
        expect.arrayContaining(["", [mocks.account.id], "INBOX"]),
      ]),
    );
  });

  it("refreshes the main mailbox after a reader-window mutation", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.readerMutationHandlers).not.toHaveLength(0),
    );
    const searchesBefore = mocks.api.search.mock.calls.length;

    act(() => mocks.readerMutationHandlers.at(-1)!());

    await waitFor(() =>
      expect(mocks.api.search.mock.calls.length).toBeGreaterThan(
        searchesBefore,
      ),
    );
    expect(mocks.api.starredCount).toHaveBeenCalled();
  });

  it("paints the first 100 conversations before classifying in the background", async () => {
    let finishClassification: (count: number) => void = () => undefined;
    mocks.api.classifyPending.mockImplementationOnce(
      () =>
        new Promise<number>((resolve) => {
          finishClassification = resolve;
        }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    expect(await screen.findByText("Unread thread")).toBeInTheDocument();
    expect(screen.getByText("Classifying messages…")).toBeInTheDocument();
    expect(mocks.api.search).toHaveBeenCalledWith(
      "",
      ["account-1"],
      "INBOX",
      false,
      false,
      100,
      null,
    );
    expect(mocks.api.search.mock.invocationCallOrder[0]).toBeLessThan(
      mocks.api.classifyPending.mock.invocationCallOrder[0],
    );

    finishClassification(0);
    await waitFor(() =>
      expect(
        screen.queryByText("Classifying messages…"),
      ).not.toBeInTheDocument(),
    );
  });

  it("keeps an unclassified Smart Inbox message visible and readable while classification is pending or fails", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    const pendingMessage = {
      ...mocks.message,
      subject: "Pending classification arrival",
      snippet: "Visible before the classifier finishes",
      category: null,
      classification_source: null,
    } satisfies MailSummary;
    let rejectClassification: (error: Error) => void = () => undefined;
    mocks.api.classifyPending.mockImplementationOnce(
      () =>
        new Promise<number>((_resolve, reject) => {
          rejectClassification = reject;
        }),
    );
    mocks.api.smartInbox.mockResolvedValue(
      smartInboxPage([
        {
          id: "other",
          conversations: groupMessages([pendingMessage]),
          nextCursor: null,
        },
      ]),
    );
    mocks.api.content.mockResolvedValue({
      body_text: "Reader opened the pending message",
      attachments: [],
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = (await screen.findByText(pendingMessage.subject)).closest(
      "button",
    )!;
    expect(screen.getByText("Classifying messages…")).toBeVisible();
    fireEvent.click(row);
    expect(
      await screen.findByText("Reader opened the pending message"),
    ).toBeVisible();
    expect(mocks.api.content).toHaveBeenCalledWith(pendingMessage.id, 77);

    await act(async () => {
      rejectClassification(new Error("classifier unavailable"));
      await Promise.resolve();
    });
    await waitFor(() =>
      expect(
        screen.queryByText("Classifying messages…"),
      ).not.toBeInTheDocument(),
    );
    expect(
      screen
        .getAllByText(pendingMessage.subject)
        .some((element) => element.closest(".mail-item")),
    ).toBe(true);
    expect(mocks.showNativeMessage).toHaveBeenCalledWith(
      "Something went wrong",
      "classifier unavailable",
      "error",
    );
  });

  it("refreshes after classification completes even when no rows were returned", async () => {
    let finishClassification: (count: number) => void = () => undefined;
    mocks.api.classifyPending.mockImplementationOnce(
      () =>
        new Promise<number>((resolve) => {
          finishClassification = resolve;
        }),
    );
    mocks.api.search
      .mockResolvedValueOnce({
        conversations: groupMessages([
          { ...mocks.message, subject: "Before classifier" },
        ]),
        nextCursor: null,
      })
      .mockResolvedValueOnce({
        conversations: groupMessages([
          { ...mocks.message, subject: "After classifier" },
        ]),
        nextCursor: null,
      });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    expect(await screen.findByText("Before classifier")).toBeVisible();
    finishClassification(0);
    expect(await screen.findByText("After classifier")).toBeVisible();
    expect(mocks.api.search).toHaveBeenCalledTimes(2);
  });

  it("drains one more classification pass when hydration arrives mid-run", async () => {
    let finishFirstPass: (count: number) => void = () => undefined;
    mocks.api.classifyPending
      .mockImplementationOnce(
        () =>
          new Promise<number>((resolve) => {
            finishFirstPass = resolve;
          }),
      )
      .mockResolvedValueOnce(0);

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    await waitFor(() =>
      expect(mocks.hydratedHandlers.length).toBeGreaterThan(0),
    );
    await waitFor(() =>
      expect(mocks.api.classifyPending).toHaveBeenCalledTimes(1),
    );
    act(() => mocks.hydratedHandlers.at(-1)!());
    finishFirstPass(0);
    await waitFor(() =>
      expect(mocks.api.classifyPending).toHaveBeenCalledTimes(2),
    );
  });

  it("reloads the visible catalogue after a quiet background mail refresh", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    await waitFor(() =>
      expect(mocks.mailChangedHandlers.length).toBeGreaterThan(0),
    );
    const searchesBeforeRefresh = mocks.api.search.mock.calls.length;
    act(() => mocks.mailChangedHandlers.at(-1)!());
    await waitFor(() =>
      expect(mocks.api.search.mock.calls.length).toBeGreaterThan(
        searchesBeforeRefresh,
      ),
    );
  });

  it("uses the returned nextCursor for a load-more request", async () => {
    const cursor = {
      received_at: "2026-07-19T09:00:00Z",
      id: "message-1",
    };
    mocks.api.search
      .mockResolvedValueOnce({
        conversations: groupMessages([mocks.message]),
        nextCursor: cursor,
      })
      .mockResolvedValueOnce({
        conversations: groupMessages([mocks.message]),
        nextCursor: cursor,
      })
      .mockResolvedValueOnce({
        conversations: groupMessages([
          {
            ...mocks.message,
            id: "message-2",
            thread_id: "thread-2",
            uid: 2,
            subject: "Older thread",
          },
        ]),
        nextCursor: null,
      });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    const scroller = document.querySelector(".mail-scroll") as HTMLDivElement;
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2_000 },
      scrollTop: { configurable: true, value: 1_300 },
      clientHeight: { configurable: true, value: 500 },
    });
    fireEvent.scroll(scroller);

    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenLastCalledWith(
        "",
        ["account-1"],
        "INBOX",
        false,
        false,
        100,
        cursor,
      ),
    );
    expect(await screen.findByText("Older thread")).toBeVisible();
  });

  it("drops a stale pagination result after the mailbox changes", async () => {
    const cursor = {
      received_at: "2026-07-19T09:00:00Z",
      id: "message-1",
    };
    let finishOlderPage: ((page: MailThreadPage) => void) | undefined;
    const sentMessage = {
      ...mocks.message,
      id: "sent-message",
      thread_id: "sent-thread",
      mailbox: "Sent",
      subject: "Current Sent result",
    };
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      const mailbox = args[2];
      const nextCursor = args[6];
      if (mailbox === "Sent") {
        return {
          conversations: groupMessages([sentMessage]),
          nextCursor: null,
        };
      }
      if (nextCursor) {
        return new Promise<MailThreadPage>((resolve) => {
          finishOlderPage = resolve;
        });
      }
      return {
        conversations: groupMessages([mocks.message]),
        nextCursor: cursor,
      };
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    const scroller = document.querySelector(".mail-scroll") as HTMLDivElement;
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2_000 },
      scrollTop: { configurable: true, value: 1_300 },
      clientHeight: { configurable: true, value: 500 },
    });
    fireEvent.scroll(scroller);
    await waitFor(() => expect(finishOlderPage).toBeTypeOf("function"));

    fireEvent.click(screen.getByRole("button", { name: "Sent" }));
    expect(await screen.findByText("Current Sent result")).toBeVisible();

    await act(async () =>
      finishOlderPage?.({
        conversations: groupMessages([
          {
            ...mocks.message,
            id: "stale-message",
            thread_id: "stale-thread",
            subject: "Stale Inbox result",
          },
        ]),
        nextCursor: null,
      }),
    );

    expect(screen.queryByText("Stale Inbox result")).not.toBeInTheDocument();
    expect(screen.getByText("Current Sent result")).toBeVisible();
  });

  it("does not continue a stale local query into a remote provider search", async () => {
    let resolveFirstQuery: (page: MailThreadPage) => void = () => undefined;
    const currentResult = {
      ...mocks.message,
      id: "current-query-message",
      thread_id: "current-query-thread",
      subject: "Current query result",
    };
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      const query = args[0];
      if (query === "first query") {
        return new Promise<MailThreadPage>((resolve) => {
          resolveFirstQuery = resolve;
        });
      }
      if (query === "second query") {
        return {
          conversations: groupMessages([currentResult]),
          nextCursor: null,
        };
      }
      return {
        conversations: groupMessages([mocks.message]),
        nextCursor: null,
      };
    });
    mocks.api.searchRemote.mockResolvedValue(remoteSearchResult([]));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("textbox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "first query" } });
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "first query",
        ["account-1"],
        "INBOX",
        false,
        false,
        100,
        null,
      ),
    );

    fireEvent.change(search, { target: { value: "second query" } });
    expect(await screen.findByText("Current query result")).toBeVisible();
    await waitFor(() =>
      expect(mocks.api.searchRemote).toHaveBeenCalledWith(
        "second query",
        ["account-1"],
        "INBOX",
        false,
        false,
      ),
    );

    await act(async () => {
      resolveFirstQuery({
        conversations: groupMessages([
          {
            ...mocks.message,
            id: "stale-query-message",
            thread_id: "stale-query-thread",
            subject: "Stale query result",
          },
        ]),
        nextCursor: null,
      });
    });

    expect(mocks.api.searchRemote).not.toHaveBeenCalledWith(
      "first query",
      ["account-1"],
      "INBOX",
      false,
      false,
    );
    expect(screen.queryByText("Stale query result")).not.toBeInTheDocument();
    expect(screen.getByText("Current query result")).toBeVisible();
  });

  it("clears stale pagination state so the current query can load its next page", async () => {
    const firstCursor = {
      received_at: "2026-07-19T09:00:00Z",
      id: "message-1",
    };
    const currentCursor = {
      received_at: "2026-07-19T08:00:00Z",
      id: "current-query-message",
    };
    let resolveStalePage: ((page: MailThreadPage) => void) | undefined;
    const currentResult = {
      ...mocks.message,
      id: "current-query-message",
      thread_id: "current-query-thread",
      subject: "Current paged query",
    };
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      const query = args[0];
      const cursor = args[6];
      if (cursor === firstCursor) {
        return new Promise<MailThreadPage>((resolve) => {
          resolveStalePage = resolve;
        });
      }
      if (query === "current page" && cursor === currentCursor) {
        return {
          conversations: groupMessages([
            {
              ...currentResult,
              id: "current-older-message",
              thread_id: "current-older-thread",
              subject: "Current older result",
            },
          ]),
          nextCursor: null,
        };
      }
      if (query === "current page") {
        return {
          conversations: groupMessages([currentResult]),
          nextCursor: currentCursor,
        };
      }
      return {
        conversations: groupMessages([mocks.message]),
        nextCursor: firstCursor,
      };
    });
    mocks.api.searchRemote.mockResolvedValue(remoteSearchResult([]));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Unread thread");
    const scroller = document.querySelector(".mail-scroll") as HTMLDivElement;
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2_000 },
      scrollTop: { configurable: true, value: 1_300 },
      clientHeight: { configurable: true, value: 500 },
    });
    fireEvent.scroll(scroller);
    await waitFor(() => expect(resolveStalePage).toBeTypeOf("function"));
    expect(document.querySelector(".mail-page-loader")).not.toBeNull();

    fireEvent.change(screen.getByRole("textbox", { name: "Search mail" }), {
      target: { value: "current page" },
    });
    expect(await screen.findByText("Current paged query")).toBeVisible();
    expect(document.querySelector(".mail-page-loader")).toBeNull();

    await act(async () => {
      resolveStalePage?.({
        conversations: groupMessages([
          {
            ...mocks.message,
            id: "stale-older-message",
            thread_id: "stale-older-thread",
            subject: "Stale older result",
          },
        ]),
        nextCursor: null,
      });
    });
    fireEvent.scroll(scroller);

    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "current page",
        ["account-1"],
        "INBOX",
        false,
        false,
        100,
        currentCursor,
      ),
    );
    expect(await screen.findByText("Current older result")).toBeVisible();
    expect(screen.queryByText("Stale older result")).not.toBeInTheDocument();
  });

  it("does not reuse the old cursor while a new query first page is pending", async () => {
    const oldCursor = {
      received_at: "2026-07-19T09:00:00Z",
      id: "message-1",
    };
    const newCursor = {
      received_at: "2026-07-19T08:00:00Z",
      id: "new-query-message",
    };
    let resolveNewFirstPage: ((page: MailThreadPage) => void) | undefined;
    const newMessage = {
      ...mocks.message,
      id: "new-query-message",
      thread_id: "new-query-thread",
      subject: "New query first page",
    };
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      const query = args[0];
      const cursor = args[6];
      if (query === "new query" && cursor === null) {
        return new Promise<MailThreadPage>((resolve) => {
          resolveNewFirstPage = resolve;
        });
      }
      if (query === "new query" && cursor === newCursor) {
        return {
          conversations: groupMessages([
            {
              ...newMessage,
              id: "new-query-older",
              thread_id: "new-query-older-thread",
              subject: "New query older page",
            },
          ]),
          nextCursor: null,
        };
      }
      return {
        conversations: groupMessages([mocks.message]),
        nextCursor: oldCursor,
      };
    });
    mocks.api.searchRemote.mockResolvedValue(remoteSearchResult([]));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Unread thread");
    const scroller = document.querySelector(".mail-scroll") as HTMLDivElement;
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2_000 },
      scrollTop: { configurable: true, value: 1_300 },
      clientHeight: { configurable: true, value: 500 },
    });
    fireEvent.change(screen.getByRole("textbox", { name: "Search mail" }), {
      target: { value: "new query" },
    });
    await waitFor(() => expect(resolveNewFirstPage).toBeTypeOf("function"));

    const callsBeforePendingScroll = mocks.api.search.mock.calls.length;
    fireEvent.scroll(scroller);
    expect(mocks.api.search).toHaveBeenCalledTimes(callsBeforePendingScroll);
    expect(mocks.api.search).not.toHaveBeenCalledWith(
      "new query",
      ["account-1"],
      "INBOX",
      false,
      false,
      100,
      oldCursor,
    );

    await act(async () => {
      resolveNewFirstPage?.({
        conversations: groupMessages([newMessage]),
        nextCursor: newCursor,
      });
    });
    expect(await screen.findByText(newMessage.subject)).toBeVisible();
    fireEvent.scroll(scroller);
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "new query",
        ["account-1"],
        "INBOX",
        false,
        false,
        100,
        newCursor,
      ),
    );
    expect(await screen.findByText("New query older page")).toBeVisible();
  });

  it("does not reuse a Smart section cursor while its first page refresh is pending", async () => {
    const oldCursor = {
      received_at: "2026-07-19T09:00:00Z",
      id: "old-smart-message",
    };
    const newCursor = {
      received_at: "2026-07-19T08:00:00Z",
      id: "new-smart-message",
    };
    let resolveRefresh: ((page: SmartInboxPage) => void) | undefined;
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.smartInbox
      .mockResolvedValueOnce(
        smartInboxPage([
          {
            id: "people",
            conversations: groupMessages([
              { ...mocks.message, category: "people" as const },
            ]),
            nextCursor: oldCursor,
          },
        ]),
      )
      .mockImplementationOnce(
        () =>
          new Promise<SmartInboxPage>((resolve) => {
            resolveRefresh = resolve;
          }),
      );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Unread thread");
    await waitFor(() =>
      expect(mocks.mailChangedHandlers.length).toBeGreaterThan(0),
    );
    act(() => mocks.mailChangedHandlers.at(-1)!());
    await waitFor(() => expect(resolveRefresh).toBeTypeOf("function"));

    const callsBeforePendingShowMore = mocks.api.search.mock.calls.length;
    fireEvent.click(screen.getByRole("button", { name: "Show more" }));
    expect(mocks.api.search).toHaveBeenCalledTimes(callsBeforePendingShowMore);
    expect(mocks.api.search).not.toHaveBeenCalledWith(
      "",
      ["account-1"],
      "INBOX",
      true,
      false,
      20,
      oldCursor,
      "people",
      true,
      false,
    );

    await act(async () => {
      resolveRefresh?.(
        smartInboxPage([
          {
            id: "people",
            conversations: groupMessages([
              {
                ...mocks.message,
                id: "new-smart-message",
                thread_id: "new-smart-thread",
                subject: "New Smart first page",
                category: "people" as const,
              },
            ]),
            nextCursor: newCursor,
          },
        ]),
      );
    });
    expect(await screen.findByText("New Smart first page")).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Show more" }));
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "",
        ["account-1"],
        "INBOX",
        true,
        false,
        20,
        newCursor,
        "people",
        true,
        false,
      ),
    );
  });

  it("loads every account through one Smart Inbox batch", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "other@example.com",
    };
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.smartInbox.mockResolvedValue(smartInboxPage([]));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.api.smartInbox).toHaveBeenCalledWith(
        ["account-1", "account-2"],
        3,
      ),
    );
    expect(mocks.api.search).not.toHaveBeenCalled();
  });

  it("rejects local and remote search rows outside the selected account scope", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "other@example.com",
      account_name: "Other",
    };
    const firstMessage = {
      ...mocks.message,
      subject: "Selected account result",
    };
    const leakedMessage = {
      ...mocks.message,
      id: "message-2",
      account_id: secondAccount.id,
      thread_id: "thread-2",
      subject: "Other account result",
    };
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([firstMessage, leakedMessage]),
      nextCursor: null,
    });
    mocks.api.searchRemote.mockResolvedValue(
      remoteSearchResult([firstMessage]),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    expect(await screen.findByText("Selected account result")).toBeVisible();
    expect(screen.getByText("Other account result")).toBeVisible();

    fireEvent.click(screen.getByTitle("other@example.com"));

    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenLastCalledWith(
        "",
        ["account-2"],
        "INBOX",
        false,
        false,
        100,
        null,
      ),
    );
    expect(screen.getByText("Other account result")).toBeVisible();
    expect(
      screen.queryByText("Selected account result"),
    ).not.toBeInTheDocument();

    fireEvent.change(screen.getByRole("textbox", { name: "Search mail" }), {
      target: { value: "account" },
    });

    await waitFor(() =>
      expect(mocks.api.searchRemote).toHaveBeenCalledWith(
        "account",
        ["account-2"],
        "INBOX",
        false,
        false,
      ),
    );
    expect(screen.getByText("Other account result")).toBeVisible();
    expect(
      screen.queryByText("Selected account result"),
    ).not.toBeInTheDocument();
  });

  it("hides the prior account rows while the selected account first page is pending", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "other@example.com",
      account_name: "Other",
    };
    const secondMessage = {
      ...mocks.message,
      id: "message-2",
      account_id: secondAccount.id,
      thread_id: "thread-2",
      subject: "Second account result",
    };
    let resolveSecondAccount: ((page: MailThreadPage) => void) | undefined;
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      const accountIds = args[1] as string[];
      if (accountIds.length === 1 && accountIds[0] === secondAccount.id) {
        return new Promise<MailThreadPage>((resolve) => {
          resolveSecondAccount = resolve;
        });
      }
      return {
        conversations: groupMessages([mocks.message]),
        nextCursor: null,
      };
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    expect(await screen.findByText("Unread thread")).toBeVisible();

    fireEvent.click(screen.getByTitle(secondAccount.email));
    await waitFor(() => expect(resolveSecondAccount).toBeTypeOf("function"));
    expect(screen.queryByText("Unread thread")).not.toBeInTheDocument();
    expect(screen.queryByText(secondMessage.subject)).not.toBeInTheDocument();

    await act(async () => {
      resolveSecondAccount?.({
        conversations: groupMessages([secondMessage]),
        nextCursor: null,
      });
    });
    expect(await screen.findByText(secondMessage.subject)).toBeVisible();
  });

  it("hides retained Smart Inbox rows while another account first page is pending", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "other@example.com",
      account_name: "Other",
    };
    const retainedMessage = {
      ...mocks.message,
      category: "people" as const,
      subject: "Retained first-account result",
    };
    let resolveSecondAccount: ((page: SmartInboxPage) => void) | undefined;
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.smartInbox.mockImplementation(async (...args: unknown[]) => {
      const accountIds = args[0] as string[];
      if (accountIds.length === 1 && accountIds[0] === secondAccount.id) {
        return new Promise<SmartInboxPage>((resolve) => {
          resolveSecondAccount = resolve;
        });
      }
      return smartInboxPage([
        {
          id: "people",
          conversations: groupMessages([retainedMessage]),
          nextCursor: null,
        },
      ]);
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.click(
      (await screen.findByText(retainedMessage.subject)).closest("button")!,
    );
    fireEvent.click(screen.getByTitle(secondAccount.email));
    await waitFor(() => expect(resolveSecondAccount).toBeTypeOf("function"));

    expect(screen.queryByText(retainedMessage.subject)).not.toBeInTheDocument();

    await act(async () => {
      resolveSecondAccount?.(smartInboxPage([]));
    });
  });

  it("merges local and remote search without replacing a local thread or leaking another account", async () => {
    const localResult = {
      ...mocks.message,
      subject: "Local complete conversation",
      received_at: "2026-07-19T10:00:00Z",
    };
    const remoteDuplicate = {
      ...localResult,
      id: "remote-duplicate-message",
      subject: "Remote duplicate conversation",
    };
    const remoteUnique = {
      ...mocks.message,
      id: "remote-unique-message",
      thread_id: "remote-unique-thread",
      subject: "Remote unique conversation",
      received_at: localResult.received_at,
    };
    const wrongAccount = {
      ...remoteUnique,
      id: "wrong-account-message",
      account_id: "account-2",
      thread_id: "wrong-account-thread",
      subject: "Wrong account conversation",
    };
    mocks.api.search.mockImplementation(async (...args: unknown[]) => ({
      conversations: args[0] ? groupMessages([localResult]) : [],
      nextCursor: null,
    }));
    mocks.api.searchRemote.mockResolvedValue(
      remoteSearchResult([remoteDuplicate, wrongAccount, remoteUnique]),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.change(
      await screen.findByRole("textbox", { name: "Search mail" }),
      { target: { value: "conversation" } },
    );

    expect(await screen.findByText(localResult.subject)).toBeVisible();
    expect(await screen.findByText(remoteUnique.subject)).toBeVisible();
    expect(screen.queryByText(remoteDuplicate.subject)).not.toBeInTheDocument();
    expect(screen.queryByText(wrongAccount.subject)).not.toBeInTheDocument();
    expect(
      screen
        .getByText(localResult.subject)
        .compareDocumentPosition(screen.getByText(remoteUnique.subject)) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
  });

  it("keeps an appended local page when the remote search completes later", async () => {
    const cursor = {
      received_at: "2026-07-19T09:00:00Z",
      id: "local-first-message",
    };
    const nextCursor = {
      received_at: "2026-07-19T08:00:00Z",
      id: "local-second-message",
    };
    const localFirst = {
      ...mocks.message,
      id: "local-first-message",
      thread_id: "local-first-thread",
      subject: "Local first page",
    };
    const localSecond = {
      ...mocks.message,
      id: "local-second-message",
      thread_id: "local-second-thread",
      subject: "Local second page",
      received_at: "2026-07-19T09:00:00Z",
    };
    const remoteMessage = {
      ...mocks.message,
      id: "remote-late-message",
      thread_id: "remote-late-thread",
      subject: "Remote late result",
      received_at: "2026-07-19T08:00:00Z",
    };
    const localThird = {
      ...mocks.message,
      id: "local-third-message",
      thread_id: "local-third-thread",
      subject: "Local third page",
      received_at: "2026-07-19T08:00:00Z",
    };
    let resolveRemote: ((messages: MailSummary[]) => void) | undefined;
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      if (args[6] === nextCursor) {
        return {
          conversations: groupMessages([localThird]),
          nextCursor: null,
        };
      }
      return {
        conversations: groupMessages([args[6] ? localSecond : localFirst]),
        nextCursor: args[6] ? nextCursor : cursor,
      };
    });
    mocks.api.searchRemote.mockImplementationOnce(
      () =>
        new Promise<RemoteSearchResult>((resolve) => {
          resolveRemote = (messages) => resolve(remoteSearchResult(messages));
        }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.change(
      await screen.findByRole("textbox", { name: "Search mail" }),
      { target: { value: "held remote" } },
    );
    expect(await screen.findByText(localFirst.subject)).toBeVisible();
    await waitFor(() => expect(resolveRemote).toBeTypeOf("function"));
    const scroller = document.querySelector(".mail-scroll") as HTMLDivElement;
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2_000 },
      scrollTop: { configurable: true, value: 1_300 },
      clientHeight: { configurable: true, value: 500 },
    });
    fireEvent.scroll(scroller);
    expect(await screen.findByText(localSecond.subject)).toBeVisible();

    await act(async () => resolveRemote?.([remoteMessage]));

    expect(await screen.findByText(remoteMessage.subject)).toBeVisible();
    expect(screen.getByText(localFirst.subject)).toBeVisible();
    expect(screen.getByText(localSecond.subject)).toBeVisible();
    fireEvent.scroll(scroller);
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "held remote",
        ["account-1"],
        "INBOX",
        false,
        false,
        100,
        nextCursor,
      ),
    );
    expect(await screen.findByText(localThird.subject)).toBeVisible();
  });

  it("rejects Smart Inbox rows outside the selected account scope", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "other@example.com",
      account_name: "Other",
    };
    const firstMessage = {
      ...mocks.message,
      subject: "First Smart result",
      category: "people" as const,
    };
    const secondMessage = {
      ...mocks.message,
      id: "message-2",
      account_id: secondAccount.id,
      thread_id: "thread-2",
      subject: "Second Smart result",
      category: "people" as const,
    };
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.smartInbox.mockResolvedValue(
      smartInboxPage([
        {
          id: "people",
          conversations: groupMessages([firstMessage, secondMessage]),
          nextCursor: null,
        },
      ]),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    expect(await screen.findByText("First Smart result")).toBeVisible();
    expect(screen.getByText("Second Smart result")).toBeVisible();

    fireEvent.click(screen.getByTitle("other@example.com"));

    await waitFor(() =>
      expect(mocks.api.smartInbox).toHaveBeenLastCalledWith(["account-2"], 3),
    );
    expect(await screen.findByText("Second Smart result")).toBeVisible();
    expect(screen.queryByText("First Smart result")).not.toBeInTheDocument();
  });

  it("keeps an empty Unicode search explicitly incomplete when remote search fails", async () => {
    mocks.api.search.mockResolvedValue({ conversations: [], nextCursor: null });
    mocks.api.searchRemote.mockRejectedValue(new Error("offline"));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const query = '雪 "töö" +tag';
    fireEvent.change(
      await screen.findByRole("textbox", { name: "Search mail" }),
      { target: { value: query } },
    );

    await waitFor(() =>
      expect(mocks.api.searchRemote).toHaveBeenCalledWith(
        query,
        ["account-1"],
        "INBOX",
        false,
        false,
      ),
    );
    expect(
      await screen.findByText(
        "Offline results are limited to synced metadata and snippets.",
      ),
    ).toBeVisible();
    expect(screen.getByText("No matches in synced mail")).toBeVisible();
    expect(
      screen.getByText(
        "Dakia could not check the mail server. Reconnect before treating this as a complete result.",
      ),
    ).toBeVisible();
    expect(screen.queryByText("No matching mail")).not.toBeInTheDocument();
  });

  it("warns only when a successful server search reports its result limit", async () => {
    const remoteResults = (count: number) =>
      Array.from({ length: count }, (_, index) => ({
        ...mocks.message,
        id: `remote-cap-${index}`,
        uid: 10_000 + index,
        thread_id: "remote-cap-thread",
        subject: `Remote capped result ${index}`,
      }));
    mocks.api.search.mockImplementation(async (...args: unknown[]) => ({
      conversations: args[0] ? groupMessages([mocks.message]) : [],
      nextCursor: null,
    }));
    mocks.api.searchRemote
      .mockResolvedValueOnce(remoteSearchResult(remoteResults(499)))
      .mockResolvedValueOnce(remoteSearchResult(remoteResults(20), true))
      .mockResolvedValueOnce(remoteSearchResult([]));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("textbox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "under cap" } });
    await waitFor(() =>
      expect(mocks.api.searchRemote).toHaveBeenCalledTimes(1),
    );
    expect(await screen.findByText(/Remote capped result/)).toBeVisible();
    expect(
      screen.queryByText(
        "Server search reached its limit. Narrow your search for more results.",
      ),
    ).not.toBeInTheDocument();

    fireEvent.change(search, { target: { value: "at cap" } });
    await waitFor(() =>
      expect(mocks.api.searchRemote).toHaveBeenCalledTimes(2),
    );
    expect(
      await screen.findByText(
        "Server search reached its limit. Narrow your search for more results.",
      ),
    ).toBeVisible();
    expect(screen.getByText(mocks.message.subject)).toBeVisible();
    mocks.api.action.mockRejectedValueOnce(new Error("offline"));
    const localRow = screen.getByText(mocks.message.subject).closest("button")!;
    fireEvent.click(within(localRow).getByRole("checkbox", { name: "Select" }));
    fireEvent.click(
      within(document.querySelector(".mail-list-panel")!).getByRole("button", {
        name: "Archive",
      }),
    );
    expect(
      await screen.findByText("Could not archive the conversation"),
    ).toBeVisible();
    expect(
      screen.getByText(
        "Server search reached its limit. Narrow your search for more results.",
      ),
    ).toBeVisible();

    fireEvent.change(search, { target: { value: "narrowed query" } });
    await waitFor(() =>
      expect(mocks.api.searchRemote).toHaveBeenCalledWith(
        "narrowed query",
        ["account-1"],
        "INBOX",
        false,
        false,
      ),
    );
    await waitFor(() =>
      expect(
        screen.queryByText(
          "Server search reached its limit. Narrow your search for more results.",
        ),
      ).not.toBeInTheDocument(),
    );
  });

  it("ignores a capped response after a newer query has completed", async () => {
    let resolveStale: ((messages: MailSummary[]) => void) | undefined;
    const capped = Array.from({ length: 500 }, (_, index) => ({
      ...mocks.message,
      id: `stale-cap-${index}`,
      uid: 20_000 + index,
      thread_id: "stale-cap-thread",
      subject: `Stale capped result ${index}`,
    }));
    mocks.api.search.mockResolvedValue({ conversations: [], nextCursor: null });
    mocks.api.searchRemote
      .mockImplementationOnce(
        () =>
          new Promise<RemoteSearchResult>((resolve) => {
            resolveStale = (messages) =>
              resolve(remoteSearchResult(messages, true));
          }),
      )
      .mockResolvedValueOnce(remoteSearchResult([]));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("textbox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "older query" } });
    await waitFor(() => expect(resolveStale).toBeTypeOf("function"));
    fireEvent.change(search, { target: { value: "newer query" } });
    await waitFor(() =>
      expect(mocks.api.searchRemote).toHaveBeenCalledTimes(2),
    );

    await act(async () => resolveStale?.(capped));

    expect(
      screen.queryByText(
        "Server search reached its limit. Narrow your search for more results.",
      ),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByText("Stale capped result 499"),
    ).not.toBeInTheDocument();
  });

  it("retries an incomplete remote search after realtime reconnect and clears the warning", async () => {
    const remoteResult = {
      ...mocks.message,
      id: "remote-after-reconnect",
      thread_id: "remote-thread",
      subject: "Recovered remote result",
    };
    mocks.api.search.mockResolvedValue({ conversations: [], nextCursor: null });
    mocks.api.searchRemote
      .mockRejectedValueOnce(new Error("offline"))
      .mockResolvedValueOnce(remoteSearchResult([remoteResult]));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.change(
      await screen.findByRole("textbox", { name: "Search mail" }),
      { target: { value: "reconnect query" } },
    );
    expect(
      await screen.findByText(
        "Dakia could not check the mail server. Reconnect before treating this as a complete result.",
      ),
    ).toBeVisible();
    await waitFor(() =>
      expect(mocks.mailChangedHandlers.length).toBeGreaterThan(0),
    );

    await act(async () => mocks.mailChangedHandlers.at(-1)!());

    expect(await screen.findByText("Recovered remote result")).toBeVisible();
    expect(
      screen.queryByText(
        "Dakia could not check the mail server. Reconnect before treating this as a complete result.",
      ),
    ).not.toBeInTheDocument();
    expect(mocks.api.searchRemote).toHaveBeenCalledTimes(2);
  });

  it("ignores an older all-account starred count after selecting one account", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "other@example.com",
      account_name: "Other",
    };
    let resolveAllAccounts: (count: number) => void = () => undefined;
    let resolveSelectedAccount: (count: number) => void = () => undefined;
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.starredCount.mockImplementation((...args: unknown[]) => {
      const accountIds = args[0] as string[];
      if (accountIds.length === 0) return Promise.resolve(0);
      return new Promise<number>((resolve) => {
        if (accountIds.length === 2) resolveAllAccounts = resolve;
        else resolveSelectedAccount = resolve;
      });
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.api.starredCount).toHaveBeenCalledWith([
        "account-1",
        "account-2",
      ]),
    );
    fireEvent.click(await screen.findByTitle("me@example.com"));
    await waitFor(() =>
      expect(mocks.api.starredCount).toHaveBeenCalledWith(["account-1"]),
    );

    await act(async () => resolveSelectedAccount(7));
    expect(
      await screen.findByLabelText("7 starred conversations"),
    ).toHaveTextContent("7");

    await act(async () => resolveAllAccounts(40));
    expect(screen.getByLabelText("7 starred conversations")).toHaveTextContent(
      "7",
    );
    expect(
      screen.queryByLabelText("40 starred conversations"),
    ).not.toBeInTheDocument();
  });

  it("clears the previous account scope when a Smart Inbox batch fails", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "other@example.com",
      account_name: "Other",
    };
    const people = {
      ...mocks.message,
      id: "people-message",
      thread_id: "people-thread",
      subject: "People from the previous scope",
      category: "people" as const,
    };
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.smartInbox.mockImplementation(async (...args: unknown[]) => {
      const accountIds = args[0] as string[];
      if (accountIds.length === 2) {
        return smartInboxPage([
          {
            id: "people",
            conversations: groupMessages([people]),
            nextCursor: null,
          },
        ]);
      }
      throw new Error("Smart Inbox unavailable");
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    expect(
      await screen.findByText("People from the previous scope"),
    ).toBeVisible();
    fireEvent.click(await screen.findByTitle("other@example.com"));
    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "Smart Inbox unavailable",
        "error",
      ),
    );
    expect(
      screen.queryByText("People from the previous scope"),
    ).not.toBeInTheDocument();
    expect(mocks.api.search).not.toHaveBeenCalled();
  });

  it("expands only one Smart section by 20 until its cursor is exhausted", async () => {
    const firstCursor = { received_at: "2026-07-19T09:00:00Z", id: "people-1" };
    const secondCursor = {
      received_at: "2026-07-19T08:00:00Z",
      id: "people-21",
    };
    const peopleMessage = (id: string, subject: string): MailSummary => ({
      ...mocks.message,
      id,
      thread_id: id,
      subject,
      category: "people",
    });
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      if (args[7] !== "people") return { conversations: [], nextCursor: null };
      if ((args[6] as { id: string }).id === firstCursor.id) {
        return {
          conversations: groupMessages([
            peopleMessage("people-21", "People next"),
          ]),
          nextCursor: secondCursor,
        };
      }
      return {
        conversations: groupMessages([
          peopleMessage("people-41", "People last"),
        ]),
        nextCursor: null,
      };
    });
    mocks.api.smartInbox.mockResolvedValue(
      smartInboxPage([
        {
          id: "people",
          conversations: groupMessages([
            peopleMessage("people-1", "People first"),
          ]),
          nextCursor: firstCursor,
        },
      ]),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("People first");
    fireEvent.click(screen.getByRole("button", { name: "Show more" }));
    await screen.findByText("People next");
    expect(mocks.api.search).toHaveBeenCalledWith(
      "",
      ["account-1"],
      "INBOX",
      true,
      false,
      20,
      firstCursor,
      "people",
      true,
      false,
    );

    fireEvent.click(screen.getByRole("button", { name: "Show more" }));
    expect(await screen.findByText("People last")).toBeVisible();
    await waitFor(() =>
      expect(
        screen.queryByRole("button", { name: "Show more" }),
      ).not.toBeInTheDocument(),
    );
  });

  it("does not duplicate an in-flight Smart section expansion", async () => {
    const cursor = { received_at: "2026-07-19T09:00:00Z", id: "people-1" };
    let resolveMore:
      | ((page: {
          conversations: ReturnType<typeof groupMessages>;
          nextCursor: null;
        }) => void)
      | undefined;
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      if (args[7] !== "people") {
        return Promise.resolve({ conversations: [], nextCursor: null });
      }
      return new Promise((resolve) => {
        resolveMore = resolve;
      });
    });
    mocks.api.smartInbox.mockResolvedValue(
      smartInboxPage([
        {
          id: "people",
          conversations: groupMessages([
            { ...mocks.message, category: "people" },
          ]),
          nextCursor: cursor,
        },
      ]),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    const more = screen.getByRole("button", { name: "Show more" });
    fireEvent.click(more);
    fireEvent.click(more);
    await waitFor(() => expect(resolveMore).toBeTypeOf("function"));
    expect(
      mocks.api.search.mock.calls.filter(
        (call) => (call as unknown[])[5] === 20,
      ),
    ).toHaveLength(1);
    resolveMore!({ conversations: [], nextCursor: null });
  });

  it("excludes starred conversations from a Seen continuation", async () => {
    const cursor = { received_at: "2026-10-21T01:04:00Z", id: "seen-4" };
    const seen = {
      ...mocks.message,
      id: "seen-4",
      uid: 4,
      thread_id: "seen-4",
      subject: "Read conversation awaiting an external star",
      is_read: true,
    };
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.smartInbox.mockResolvedValue(
      smartInboxPage([
        {
          id: "seen",
          conversations: groupMessages([seen]),
          nextCursor: cursor,
        },
      ]),
    );
    mocks.api.search.mockResolvedValue({ conversations: [], nextCursor: null });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText(seen.subject);
    fireEvent.click(screen.getByRole("button", { name: "Show more" }));
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "",
        ["account-1"],
        "INBOX",
        false,
        false,
        20,
        cursor,
        undefined,
        true,
        true,
      ),
    );
  });

  it("keeps a starred Smart thread visible when it becomes read", async () => {
    const starred = {
      ...mocks.message,
      is_flagged: true,
    };
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.search.mockImplementation(async (...args: unknown[]) =>
      args[4]
        ? { conversations: groupMessages([starred]), nextCursor: null }
        : { conversations: [], nextCursor: null },
    );
    mocks.api.smartInbox.mockResolvedValue(
      smartInboxPage([
        {
          id: "starred",
          conversations: groupMessages([starred]),
          nextCursor: null,
        },
      ]),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = await screen.findByText("Unread thread");
    expect(mocks.api.smartInbox).toHaveBeenCalledWith(["account-1"], 3);

    fireEvent.click(row.closest("button")!);
    await waitFor(() =>
      expect(mocks.api.setRead).toHaveBeenCalledWith("message-1", true, 77),
    );
    expect(
      within(document.querySelector(".mail-list-panel")!).getByText(
        "Unread thread",
      ),
    ).toBeVisible();
  });

  it("keeps an opened Smart thread until another opens, then animates it out", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    let smartInboxLoads = 0;
    let resolveRefreshedSmartInbox:
      ((page: SmartInboxPage) => void) | undefined;
    const nextMessage = {
      ...mocks.message,
      id: "message-2",
      uid: 2,
      thread_id: "thread-2",
      subject: "Next unread thread",
      received_at: "2026-07-19T09:00:00Z",
      is_flagged: true,
    };
    mocks.api.smartInbox.mockImplementation(async () => {
      smartInboxLoads += 1;
      if (smartInboxLoads === 2) {
        return new Promise((resolve) => {
          resolveRefreshedSmartInbox = resolve;
        });
      }
      return smartInboxPage([
        {
          id: "people",
          conversations: groupMessages([mocks.message]),
          nextCursor: null,
        },
        {
          id: "starred",
          conversations: groupMessages([nextMessage]),
          nextCursor: null,
        },
      ]);
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    await waitFor(() =>
      expect(mocks.api.setRead).toHaveBeenCalledWith("message-1", true, 77),
    );
    await waitFor(() =>
      expect(resolveRefreshedSmartInbox).toBeTypeOf("function"),
    );
    await act(async () =>
      resolveRefreshedSmartInbox!(
        smartInboxPage([
          {
            id: "starred",
            conversations: groupMessages([nextMessage]),
            nextCursor: null,
          },
        ]),
      ),
    );
    expect(
      screen.getByRole("heading", { name: "Unread thread" }),
    ).toBeVisible();
    expect(screen.getByRole("button", { name: "Quick reply" })).toBeEnabled();
    expect(
      within(document.querySelector(".mail-list-panel")!).getByText(
        "Unread thread",
      ),
    ).toBeVisible();

    fireEvent.click(screen.getByText("Next unread thread").closest("button")!);
    expect(
      screen.getByText("Unread thread").closest(".mail-item"),
    ).toHaveAttribute("data-smart-exiting", "true");
    await waitFor(() =>
      expect(
        within(document.querySelector(".mail-list-panel")!).queryByText(
          "Unread thread",
        ),
      ).not.toBeInTheDocument(),
    );
  });

  it("shows a retained Smart thread only once when it moves to Seen", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    const nextMessage = {
      ...mocks.message,
      id: "message-2",
      uid: 2,
      thread_id: "thread-2",
      subject: "Next starred thread",
      received_at: "2026-07-19T09:00:00Z",
      is_flagged: true,
    };
    const readMessage = { ...mocks.message, is_read: true };
    let refreshed = false;
    let refreshedLoads = 0;
    let finishRead: (() => void) | undefined;
    mocks.api.setRead.mockImplementationOnce(
      () =>
        new Promise<undefined>((resolve) => {
          finishRead = () => resolve(undefined);
        }),
    );
    mocks.api.smartInbox.mockImplementation(async () => {
      if (refreshed) {
        refreshedLoads += 1;
        return smartInboxPage([
          {
            id: "starred",
            conversations: groupMessages([nextMessage]),
            nextCursor: null,
          },
          {
            id: "seen",
            conversations: groupMessages([readMessage]),
            nextCursor: null,
          },
        ]);
      }
      return smartInboxPage([
        {
          id: "other",
          conversations: groupMessages([mocks.message]),
          nextCursor: null,
        },
      ]);
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    await waitFor(() => expect(finishRead).toBeTypeOf("function"));
    refreshed = true;
    await act(async () => finishRead!());

    const list = within(document.querySelector(".mail-list-panel")!);
    await waitFor(() => expect(refreshedLoads).toBeGreaterThan(0));
    expect(await list.findByText("Next starred thread")).toBeVisible();
    await waitFor(() =>
      expect(list.getAllByText("Unread thread")).toHaveLength(1),
    );
    const other = list.getByRole("region", { name: "Other" });
    expect(within(other).getByText("Unread thread")).toBeVisible();
    expect(list.queryByRole("region", { name: "Seen" })).toBeNull();

    fireEvent.click(within(other).getByRole("checkbox", { name: "Select" }));
    expect(list.getByText("1 selected")).toBeVisible();

    fireEvent.click(list.getByText("Next starred thread").closest("button")!);
    expect(
      list.getByText("Unread thread").closest(".mail-item"),
    ).toHaveAttribute("data-smart-exiting", "true");
    await waitFor(() => {
      const seen = list.getByRole("region", { name: "Seen" });
      expect(within(seen).getByText("Unread thread")).toBeVisible();
      expect(list.queryByRole("region", { name: "Other" })).toBeNull();
      expect(list.getAllByText("Unread thread")).toHaveLength(1);
    });
  });

  it("keeps an earlier Seen thread visible through a failed offline unread rollback", async () => {
    const remoteMessage = {
      ...mocks.message,
      id: "remote-message",
      uid: 3,
      thread_id: "remote-thread",
      subject: "Remote content safety check",
      received_at: "2026-10-21T12:10:26Z",
      is_read: true,
    };
    const arrivalMessage = {
      ...mocks.message,
      id: "arrival-message",
      uid: 4,
      thread_id: "arrival-thread",
      subject: "Dakia fictional Estonian translation arrival",
      received_at: "2026-10-21T12:21:44Z",
      is_read: true,
    };
    let rejectUnread: ((error: Error) => void) | undefined;
    let refreshedLoads = 0;
    let refreshPhase = false;
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.smartInbox.mockImplementation(async () => {
      if (refreshPhase) refreshedLoads += 1;
      return smartInboxPage([
        {
          id: "seen",
          conversations: groupMessages([arrivalMessage, remoteMessage]),
          nextCursor: null,
        },
      ]);
    });
    mocks.api.setRead.mockImplementationOnce(
      () =>
        new Promise<undefined>((_resolve, reject) => {
          rejectUnread = reject;
        }),
    );
    const realSetTimeout = window.setTimeout.bind(window);
    const timeoutSpy = vi
      .spyOn(window, "setTimeout")
      .mockImplementation((handler, timeout, ...args) =>
        timeout === 180 ? 91_803 : realSetTimeout(handler, timeout, ...args),
      );

    try {
      render(
        <MantineProvider>
          <App />
        </MantineProvider>,
      );

      fireEvent.click(
        (await screen.findByText(remoteMessage.subject)).closest("button")!,
      );
      fireEvent.click(
        screen.getByText(arrivalMessage.subject).closest("button")!,
      );
      fireEvent.click(
        await screen.findByRole("button", { name: "Mark as unread" }),
      );
      await waitFor(() => expect(rejectUnread).toBeTypeOf("function"));

      await act(async () => {
        rejectUnread!(new Error("offline unread failed"));
        await Promise.resolve();
      });
      expect(
        screen.getByRole("button", { name: "Mark as unread" }),
      ).toBeVisible();

      refreshPhase = true;
      act(() => mocks.mailChangedHandlers.at(-1)!());
      await waitFor(() => expect(refreshedLoads).toBeGreaterThan(0));

      const seen = within(screen.getByRole("region", { name: "Seen" }));
      const remoteRow = seen.getByText(remoteMessage.subject).closest("button");
      expect(remoteRow).toHaveAttribute("data-smart-exiting", "false");
      expect(seen.getByText(arrivalMessage.subject)).toBeVisible();
    } finally {
      timeoutSpy.mockRestore();
    }
  });

  it("does not reinsert a retained Smart thread after deleting it", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    let smartInboxLoads = 0;
    let resolveStaleSmartInbox: ((page: SmartInboxPage) => void) | undefined;
    mocks.api.smartInbox.mockImplementation(async () => {
      smartInboxLoads += 1;
      if (smartInboxLoads === 2) {
        return new Promise((resolve) => {
          resolveStaleSmartInbox = resolve;
        });
      }
      if (smartInboxLoads > 2) return smartInboxPage([]);
      return smartInboxPage([
        {
          id: "people",
          conversations: groupMessages([mocks.message]),
          nextCursor: null,
        },
      ]);
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const list = document.querySelector<HTMLElement>(".mail-list-panel")!;
    fireEvent.click(
      (await within(list).findByText("Unread thread")).closest("button")!,
    );
    await waitFor(() =>
      expect(mocks.api.setRead).toHaveBeenCalledWith("message-1", true, 77),
    );
    await waitFor(() => expect(resolveStaleSmartInbox).toBeTypeOf("function"));
    expect(within(list).getByText("Unread thread")).toBeVisible();

    fireEvent.contextMenu(
      within(list).getByText("Unread thread").closest("button")!,
    );
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Delete", hidden: true }),
    );

    await waitFor(() =>
      expect(within(list).queryByText("Unread thread")).not.toBeInTheDocument(),
    );
    expect(mocks.api.action).toHaveBeenCalledWith(
      "account-1",
      "INBOX",
      1,
      "trash",
      "message-1",
      77,
    );

    await act(async () =>
      resolveStaleSmartInbox!(
        smartInboxPage([
          {
            id: "people",
            conversations: groupMessages([mocks.message]),
            nextCursor: null,
          },
        ]),
      ),
    );
    expect(within(list).queryByText("Unread thread")).not.toBeInTheDocument();
  });

  it("permanently deletes only the chosen message copy and focuses its remaining sibling", async () => {
    const earlier = {
      ...mocks.message,
      id: "message-earlier",
      uid: 40,
      from_name: "Earlier sender",
      received_at: "2026-07-19T09:00:00Z",
    };
    const latest = {
      ...mocks.message,
      id: "message-latest",
      uid: 41,
      from_name: "Latest sender",
      received_at: "2026-07-19T10:00:00Z",
    };
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([earlier, latest]),
      nextCursor: null,
    });
    mocks.api.content.mockImplementation(async (messageId: string) => ({
      body_text: messageId === earlier.id ? "Earlier body" : "Latest body",
      attachments: [],
    }));
    mocks.confirmNativeAction.mockResolvedValue(true);
    mocks.api.action.mockImplementation(async () => {
      mocks.api.search.mockResolvedValue({
        conversations: groupMessages([earlier]),
        nextCursor: null,
      });
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    expect(await screen.findByText("Latest body")).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Permanently delete" }),
    );

    await waitFor(() =>
      expect(mocks.api.action).toHaveBeenCalledWith(
        "account-1",
        "INBOX",
        41,
        "delete",
        "message-latest",
        77,
      ),
    );
    expect(await screen.findByText("Earlier body")).toBeVisible();
    expect(screen.queryByText("Latest body")).not.toBeInTheDocument();
    expect(
      screen.getByText("Message deletion request completed"),
    ).toBeVisible();
  });

  it("routes Shift+Delete through confirmation to the exact mailbox locator", async () => {
    mocks.confirmNativeAction.mockResolvedValue(true);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    await screen.findByText("Message body");
    fireEvent.keyDown(document.documentElement, {
      key: "Delete",
      shiftKey: true,
    });

    await waitFor(() =>
      expect(mocks.api.action).toHaveBeenCalledWith(
        "account-1",
        "INBOX",
        1,
        "delete",
        "message-1",
        77,
      ),
    );
    expect(mocks.confirmNativeAction).toHaveBeenCalledOnce();
  });

  it("keeps a message visible when permanent deletion fails", async () => {
    mocks.confirmNativeAction.mockResolvedValue(true);
    mocks.api.action.mockRejectedValueOnce(new Error("offline"));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    expect(await screen.findByText("Message body")).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Permanently delete" }),
    );

    expect(
      await screen.findByText("Could not permanently delete this message"),
    ).toBeVisible();
    expect(screen.getByText("Message body")).toBeVisible();
    expect(
      within(document.querySelector(".mail-list-panel")!).getByText(
        "Unread thread",
      ),
    ).toBeVisible();
  });

  it("clears an invalidated pagination loader after permanent deletion", async () => {
    const cursor = {
      received_at: "2026-07-19T09:00:00Z",
      id: "message-1",
    };
    let resolveMore: ((page: MailThreadPage) => void) | undefined;
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      if (args[6]) {
        return new Promise<MailThreadPage>((resolve) => {
          resolveMore = resolve;
        });
      }
      return {
        conversations: groupMessages([mocks.message]),
        nextCursor: cursor,
      };
    });
    mocks.confirmNativeAction.mockResolvedValue(true);

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const list = document.querySelector<HTMLElement>(".mail-list-panel")!;
    fireEvent.click(
      (await within(list).findByText("Unread thread")).closest("button")!,
    );
    await screen.findByText("Message body");
    const scroller = document.querySelector(".mail-scroll") as HTMLDivElement;
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2_000 },
      scrollTop: { configurable: true, value: 1_300 },
      clientHeight: { configurable: true, value: 500 },
    });
    fireEvent.scroll(scroller);
    await waitFor(() => expect(resolveMore).toBeTypeOf("function"));
    expect(document.querySelector(".mail-page-loader")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Permanently delete" }),
    );

    await waitFor(() =>
      expect(
        document.querySelector(".mail-page-loader"),
      ).not.toBeInTheDocument(),
    );
    resolveMore!({ conversations: [], nextCursor: null });
  });

  it("moves to a neighboring conversation after permanently deleting its only message", async () => {
    const deleted = {
      ...mocks.message,
      id: "delete-only-message",
      uid: 70,
      thread_id: "delete-only-thread",
      subject: "Delete this conversation",
      received_at: "2026-07-19T10:00:00Z",
    };
    const neighbor = {
      ...mocks.message,
      id: "neighbor-message",
      uid: 71,
      thread_id: "neighbor-thread",
      subject: "Neighbor conversation",
      received_at: "2026-07-19T09:00:00Z",
    };
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([deleted, neighbor]),
      nextCursor: null,
    });
    mocks.api.content.mockImplementation(async (messageId: string) => ({
      body_text: messageId === neighbor.id ? "Neighbor body" : "Deleted body",
      attachments: [],
    }));
    mocks.confirmNativeAction.mockResolvedValue(true);
    mocks.api.action.mockImplementation(async () => {
      mocks.api.search.mockResolvedValue({
        conversations: groupMessages([neighbor]),
        nextCursor: null,
      });
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Delete this conversation")).closest("button")!,
    );
    expect(await screen.findByText("Deleted body")).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Permanently delete" }),
    );

    expect(await screen.findByText("Neighbor body")).toBeVisible();
    expect(
      screen.getByRole("heading", { name: "Neighbor conversation" }),
    ).toBeVisible();
  });

  it("ignores an out-of-order Smart result after the view changes", async () => {
    let resolveOldSmartInbox: ((page: SmartInboxPage) => void) | undefined;
    let smartInboxCalls = 0;
    const oldPeople = {
      ...mocks.message,
      id: "old",
      thread_id: "old",
      subject: "Old people",
      category: "people" as const,
    };
    const freshPeople = {
      ...mocks.message,
      id: "fresh",
      thread_id: "fresh",
      subject: "Fresh people",
      category: "people" as const,
    };
    localStorage.setItem("dakia.mail-list-view", "smart");
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.smartInbox.mockImplementation(() => {
      smartInboxCalls += 1;
      if (smartInboxCalls === 1) {
        return new Promise((resolve) => {
          resolveOldSmartInbox = resolve;
        });
      }
      return Promise.resolve(
        smartInboxPage([
          {
            id: "people",
            conversations: groupMessages([freshPeople]),
            nextCursor: null,
          },
        ]),
      );
    });
    mocks.api.search.mockImplementation((...args: unknown[]) => {
      if (args[5] === 100) {
        return Promise.resolve({
          conversations: groupMessages([
            { ...mocks.message, subject: "List row" },
          ]),
          nextCursor: null,
        });
      }
      return Promise.resolve({ conversations: [], nextCursor: null });
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() => expect(resolveOldSmartInbox).toBeTypeOf("function"));
    fireEvent.click(screen.getByRole("button", { name: "List" }));
    expect(await screen.findByText("List row")).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Smart" }));
    expect(await screen.findByText("Fresh people")).toBeVisible();
    resolveOldSmartInbox!(
      smartInboxPage([
        {
          id: "people",
          conversations: groupMessages([oldPeople]),
          nextCursor: null,
        },
      ]),
    );
    await Promise.resolve();
    expect(screen.queryByText("Old people")).not.toBeInTheDocument();
  });

  it("does not rewrite IMAP state when the opened conversation is already read", async () => {
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([
        { ...mocks.message, id: "message-2", is_read: true },
      ]),
      nextCursor: null,
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = await screen.findByText("Unread thread");
    fireEvent.click(row.closest("button")!);

    await waitFor(() => expect(mocks.api.content).toHaveBeenCalled());
    expect(mocks.api.setRead).not.toHaveBeenCalled();
  });

  it("hydrates only the selected reply message before opening a quoted reply composer", async () => {
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([
        mocks.message,
        {
          ...mocks.message,
          id: "message-2",
          from_address: "me@example.com",
          received_at: "2026-07-19T11:00:00Z",
        },
      ]),
      nextCursor: null,
    });
    mocks.api.content.mockResolvedValue({
      body_text: "Provider body\r\n> Nested history",
      body_html:
        '<table style="width: 600px"><tbody><tr><td><a href="https://example.com/settings">Manage budgets</a></td></tr></tbody></table>',
      attachments: [],
    });
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = await screen.findByText("Unread thread");
    fireEvent.click(row.closest("button")!);
    await screen.findByRole("button", { name: "Reply" });
    mocks.api.content.mockClear();
    fireEvent.click(screen.getByRole("button", { name: "Reply" }));

    await waitFor(() =>
      expect(mocks.openComposeWindow).toHaveBeenCalledWith(
        expect.objectContaining({
          accountId: "account-1",
          to: "sender@example.com",
          subject: "Re: Unread thread",
          body: expect.stringContaining("> > Nested history"),
          bodyHtml: expect.stringContaining(
            '<blockquote type="cite"><div data-dakia-quoted-email="true"><table',
          ),
        }),
      ),
    );
    expect(mocks.api.content).toHaveBeenCalledTimes(1);
    expect(mocks.api.content).toHaveBeenCalledWith("message-2", 77);
  });

  it("replies and forwards the expanded older message instead of the thread latest message", async () => {
    const older = {
      ...mocks.message,
      id: "older-message",
      subject: "Older question",
      from_name: "Older sender",
      from_address: "older@example.com",
      received_at: "2026-07-19T09:00:00Z",
    };
    const latest = {
      ...mocks.message,
      id: "latest-message",
      subject: "Latest answer",
      from_name: "Latest sender",
      from_address: "latest@example.com",
      received_at: "2026-07-19T10:00:00Z",
    };
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([older, latest]),
      nextCursor: null,
    });
    mocks.api.content.mockImplementation(async (messageId) => ({
      body_text:
        messageId === older.id ? "Older full body" : "Latest full body",
      attachments: [],
    }));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Latest answer")).closest("button")!,
    );
    fireEvent.click(
      await screen.findByRole("button", {
        name: "Expand message from Older sender",
      }),
    );
    await screen.findByText("Older full body");
    mocks.openComposeWindow.mockClear();
    fireEvent.click(screen.getByRole("button", { name: "Quick reply" }));

    await waitFor(() =>
      expect(mocks.openComposeWindow).toHaveBeenCalledWith(
        expect.objectContaining({
          accountId: "account-1",
          to: "Older sender <older@example.com>",
          subject: "Re: Older question",
        }),
      ),
    );
    expect(mocks.api.content).toHaveBeenLastCalledWith(older.id, 77);

    mocks.openComposeWindow.mockClear();
    fireEvent.click(screen.getByRole("button", { name: "Forward" }));
    await waitFor(() =>
      expect(mocks.openComposeWindow).toHaveBeenCalledWith(
        expect.objectContaining({
          accountId: "account-1",
          subject: "Fwd: Older question",
          contextMessageIds: [older.id],
          contextMessageTargets: [
            { messageId: older.id, expectedUidValidity: 77 },
          ],
        }),
      ),
    );
  });

  it("keeps an expanded sent message as the reply provenance", async () => {
    const receivedA = {
      ...mocks.message,
      id: "received-a",
      subject: "Received A",
      from_name: "Sender A",
      from_address: "sender-a@example.com",
      message_id: "<received-a@example.com>",
      received_at: "2026-07-19T09:00:00Z",
    };
    const sentB = {
      ...mocks.message,
      id: "sent-b",
      subject: "Sent B",
      from_name: "Me",
      from_address: "me@example.com",
      to_addresses: "sender-a@example.com",
      cc_addresses: "peer@example.com",
      bcc_addresses: "hidden@example.com",
      message_id: "<sent-b@example.com>",
      reference_ids: "<received-a@example.com>",
      received_at: "2026-07-19T10:00:00Z",
    };
    const receivedC = {
      ...mocks.message,
      id: "received-c",
      subject: "Received C",
      from_name: "Sender C",
      from_address: "sender-c@example.com",
      message_id: "<received-c@example.com>",
      received_at: "2026-07-19T11:00:00Z",
    };
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([receivedA, sentB, receivedC]),
      nextCursor: null,
    });
    mocks.api.content.mockImplementation(async (messageId) =>
      messageId === sentB.id
        ? {
            body_text: "Sent B full body",
            body_html: "<p>Sent B <strong>full body</strong></p>",
            attachments: [
              {
                id: "sent-attachment",
                message_id: "sent-b",
                filename: "notes.pdf",
                mime_type: "application/pdf",
                size_bytes: 42,
                is_inline: false,
                is_potentially_unsafe: false,
                presentation: "downloadable" as const,
              },
            ],
          }
        : { body_text: "Received message body", attachments: [] },
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click((await screen.findByText("Received C")).closest("button")!);
    fireEvent.click(
      await screen.findByRole("button", { name: "Expand message from Me" }),
    );
    await waitFor(() =>
      expect(mocks.api.content).toHaveBeenCalledWith("sent-b", 77),
    );

    for (const action of ["Quick reply", "Reply all"]) {
      mocks.openComposeWindow.mockClear();
      mocks.api.content.mockClear();
      fireEvent.click(screen.getByRole("button", { name: action }));
      await waitFor(() =>
        expect(mocks.openComposeWindow).toHaveBeenCalledWith(
          expect.objectContaining({
            accountId: "account-1",
            subject: "Re: Sent B",
            inReplyTo: "<sent-b@example.com>",
            references: "<received-a@example.com> <sent-b@example.com>",
            body: expect.stringContaining("Sent B full body"),
            bodyHtml: expect.stringContaining(
              '<div data-dakia-quoted-email="true"><p>Sent B <strong>full body</strong></p></div>',
            ),
            contextMessageIds: ["received-a", "sent-b", "received-c"],
            contextMessageTargets: [
              { messageId: "received-a", expectedUidValidity: 77 },
              { messageId: "sent-b", expectedUidValidity: 77 },
              { messageId: "received-c", expectedUidValidity: 77 },
            ],
          }),
        ),
      );
      expect(mocks.api.content).toHaveBeenCalledWith("sent-b", 77);
    }

    mocks.openComposeWindow.mockClear();
    mocks.api.content.mockClear();
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Send again" }),
    );
    await waitFor(() =>
      expect(mocks.openComposeWindow).toHaveBeenCalledWith({
        accountId: "account-1",
        to: "sender-a@example.com",
        cc: "peer@example.com",
        bcc: "hidden@example.com",
        subject: "Sent B",
        body: "Sent B full body",
        bodyHtml: "<p>Sent B <strong>full body</strong></p>",
        forwardMessageId: "sent-b",
        forwardMessageUidValidity: 77,
      }),
    );
    expect(mocks.api.content).toHaveBeenCalledWith("sent-b", 77);
  });

  it("opens Reply All with Reply-To and deduplicated non-self Cc recipients", async () => {
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([
        {
          ...mocks.message,
          reply_to_addresses: "Replies <reply@example.com>",
          to_addresses: "Me <me@example.com>, Peer <peer@example.com>",
          cc_addresses: "PEER@example.com, Other <other@example.com>",
          bcc_addresses: "hidden@example.com",
        },
      ]),
      nextCursor: null,
    });
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    fireEvent.click(await screen.findByRole("button", { name: "Reply all" }));

    await waitFor(() =>
      expect(mocks.openComposeWindow).toHaveBeenCalledWith(
        expect.objectContaining({
          accountId: "account-1",
          to: "Replies <reply@example.com>, Peer <peer@example.com>",
          cc: "Other <other@example.com>",
          subject: "Re: Unread thread",
        }),
      ),
    );
    expect(mocks.openComposeWindow.mock.calls.at(-1)?.[0].cc).not.toContain(
      "hidden@example.com",
    );
  });

  it("keeps a unified-inbox message account through Reply All and Forward", async () => {
    const secondAccount = {
      ...mocks.account,
      id: "account-2",
      email: "me2@example.com",
      account_name: "Second inbox",
    };
    const secondMessage = {
      ...mocks.message,
      id: "message-2",
      account_id: secondAccount.id,
      uid: 42,
      thread_id: "thread-2",
      subject: "Second account message",
      from_address: "sender2@example.com",
      reply_to_addresses: "Replies <reply2@example.com>",
      to_addresses: "Me Two <me2@example.com>, Peer <peer@example.com>",
      body_text: "Second account body",
    };
    mocks.api.accounts.mockResolvedValueOnce([mocks.account, secondAccount]);
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([secondMessage]),
      nextCursor: null,
    });
    mocks.api.content.mockResolvedValue({
      body_text: "Second account full body",
      attachments: [],
    });
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Second account message")).closest("button")!,
    );
    fireEvent.click(await screen.findByRole("button", { name: "Reply all" }));
    await waitFor(() =>
      expect(mocks.openComposeWindow).toHaveBeenCalledWith(
        expect.objectContaining({
          accountId: "account-2",
          to: "Replies <reply2@example.com>, Peer <peer@example.com>",
          subject: "Re: Second account message",
          contextMessageTargets: [
            { messageId: "message-2", expectedUidValidity: 77 },
          ],
        }),
      ),
    );

    mocks.openComposeWindow.mockClear();
    fireEvent.click(screen.getByRole("button", { name: "More actions" }));
    fireEvent.click(await screen.findByRole("menuitem", { name: "Forward" }));
    await waitFor(() =>
      expect(mocks.openComposeWindow).toHaveBeenCalledWith(
        expect.objectContaining({
          accountId: "account-2",
          subject: "Fwd: Second account message",
          body: expect.stringContaining("Second account full body"),
          contextMessageTargets: [
            { messageId: "message-2", expectedUidValidity: 77 },
          ],
        }),
      ),
    );
  });

  it("shows the backend unsubscribe error returned by Tauri", async () => {
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([
        { ...mocks.message, unsubscribe_kind: "mailto" },
      ]),
      nextCursor: null,
    });
    mocks.api.content.mockResolvedValue({
      body_text: "Message body",
      unsubscribe_kind: "mailto",
      attachments: [],
    });
    mocks.api.unsubscribe.mockRejectedValueOnce(
      "invalid unsubscribe email address",
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = await screen.findByText("Unread thread");
    fireEvent.click(row.closest("button")!);
    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));

    await waitFor(() =>
      expect(mocks.api.unsubscribe).toHaveBeenCalledWith("message-1", 77),
    );

    expect(
      await screen.findByText("invalid unsubscribe email address"),
    ).toBeVisible();
  });

  it("offers sender cleanup after opening an unsubscribe page", async () => {
    const cleanupTarget = {
      accountId: "account-1",
      senderName: "Sender",
      senderAddress: "sender@example.com",
    };
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([
        { ...mocks.message, unsubscribe_kind: "web" },
        {
          ...mocks.message,
          id: "message-2",
          uid: 2,
          from_address: "not-sender@example.com",
          subject: "Mixed sender thread",
        },
      ]),
      nextCursor: null,
    });
    mocks.api.content.mockResolvedValue({
      body_text: "Message body",
      unsubscribe_kind: "web",
      attachments: [],
    });
    mocks.api.unsubscribe.mockResolvedValueOnce({
      kind: "opened_web",
      cleanupTarget,
    });
    mocks.api.trashMessagesFromSender.mockResolvedValueOnce({
      matched: 1,
      moved: 1,
      failed: 0,
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Mixed sender thread")).closest("button")!,
    );
    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));
    expect(await screen.findByText("Unsubscribe page opened.")).toBeVisible();

    const cleanupButton = screen.getByRole("button", {
      name: "Move to Trash",
    });
    fireEvent.click(cleanupButton);
    fireEvent.click(cleanupButton);
    await waitFor(() =>
      expect(mocks.api.trashMessagesFromSender).toHaveBeenCalledWith(
        "account-1",
        "sender@example.com",
      ),
    );
    expect(mocks.api.trashMessagesFromSender).toHaveBeenCalledTimes(1);
    expect(await screen.findByText("Moved 1 email to Trash")).toBeVisible();
  });

  it("keeps sender mail hidden through a stale reload while cleanup is pending", async () => {
    let resolveCleanup: (
      result: import("./api").TrashMessagesFromSenderResult,
    ) => void = () => undefined;
    let cleanupResolved = false;
    mocks.api.search.mockImplementation(async () => ({
      conversations: cleanupResolved ? [] : groupMessages([mocks.message]),
      nextCursor: null,
    }));
    mocks.api.trashMessagesFromSender.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveCleanup = resolve;
        }),
    );
    mocks.api.content.mockResolvedValue({
      body_text: "Message body",
      unsubscribe_kind: "one_click",
      attachments: [],
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const list = document.querySelector<HTMLElement>(".mail-list-panel")!;
    fireEvent.click(
      await within(list).findByRole("checkbox", { name: "Select" }),
    );
    expect(within(list).getByText("1 selected")).toBeVisible();
    fireEvent.click(
      (await within(list).findByText("Unread thread")).closest("button")!,
    );
    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "Move to Trash" }),
    );

    await waitFor(() =>
      expect(within(list).queryByText("Unread thread")).not.toBeInTheDocument(),
    );
    expect(within(list).queryByText("0 selected")).not.toBeInTheDocument();
    expect(within(list).queryByText("1 selected")).not.toBeInTheDocument();
    const searchesBeforeReload = mocks.api.search.mock.calls.length;
    act(() => mocks.mailChangedHandlers.at(-1)!());
    await waitFor(() =>
      expect(mocks.api.search.mock.calls.length).toBeGreaterThan(
        searchesBeforeReload,
      ),
    );
    expect(within(list).queryByText("Unread thread")).not.toBeInTheDocument();

    cleanupResolved = true;
    await act(async () => resolveCleanup({ matched: 1, moved: 1, failed: 0 }));
    expect(await screen.findByText("Moved 1 email to Trash")).toBeVisible();
  });

  it("restores sender mail when cleanup and reconciliation both fail", async () => {
    let searchCount = 0;
    mocks.api.search.mockImplementation(async () => {
      searchCount += 1;
      if (searchCount === 1) {
        return {
          conversations: groupMessages([mocks.message]),
          nextCursor: null,
        };
      }
      throw new Error("catalogue unavailable");
    });
    mocks.api.trashMessagesFromSender.mockRejectedValueOnce(
      new Error("provider unavailable"),
    );
    mocks.api.content.mockResolvedValue({
      body_text: "Message body",
      unsubscribe_kind: "one_click",
      attachments: [],
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const list = document.querySelector<HTMLElement>(".mail-list-panel")!;
    fireEvent.click(
      (await within(list).findByText("Unread thread")).closest("button")!,
    );
    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "Move to Trash" }),
    );

    expect(await within(list).findByText("Unread thread")).toBeVisible();
    expect(
      await screen.findByText(
        "Could not move emails from this sender to Trash",
      ),
    ).toBeVisible();
    expect(searchCount).toBeGreaterThan(1);
  });

  it("restores provider failures after a partial sender cleanup", async () => {
    let resolveCleanup: (
      result: import("./api").TrashMessagesFromSenderResult,
    ) => void = () => undefined;
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([mocks.message]),
      nextCursor: null,
    });
    mocks.api.trashMessagesFromSender.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveCleanup = resolve;
        }),
    );
    mocks.api.content.mockResolvedValue({
      body_text: "Message body",
      unsubscribe_kind: "one_click",
      attachments: [],
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const list = document.querySelector<HTMLElement>(".mail-list-panel")!;
    fireEvent.click(
      (await within(list).findByText("Unread thread")).closest("button")!,
    );
    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "Move to Trash" }),
    );
    await waitFor(() =>
      expect(within(list).queryByText("Unread thread")).not.toBeInTheDocument(),
    );

    await act(async () => resolveCleanup({ matched: 2, moved: 1, failed: 1 }));

    expect(await within(list).findByText("Unread thread")).toBeVisible();
    expect(
      await screen.findByText(
        "Moved 1 of 2 emails to Trash; 1 could not be moved",
      ),
    ).toBeVisible();
  });

  it("keeps unsubscribe successful when no safe cleanup target is available", async () => {
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([
        { ...mocks.message, unsubscribe_kind: "one_click" },
      ]),
      nextCursor: null,
    });
    mocks.api.content.mockResolvedValue({
      body_text: "Message body",
      unsubscribe_kind: "one_click",
      attachments: [],
    });
    mocks.api.unsubscribe.mockResolvedValueOnce({
      kind: "completed",
      cleanupTarget: null,
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      (await screen.findByText("Unread thread")).closest("button")!,
    );
    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));

    expect(await screen.findByText("Unsubscribe request sent.")).toBeVisible();
    expect(
      screen.queryByRole("button", { name: "Move to Trash" }),
    ).not.toBeInTheDocument();
  });

  it("uses incremental sync from the inbox toolbar", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    await waitFor(() => expect(mocks.api.classifyPending).toHaveBeenCalled());
    const classificationCallsBeforeSync =
      mocks.api.classifyPending.mock.calls.length;
    fireEvent.click(screen.getByRole("button", { name: "Sync" }));

    await waitFor(() =>
      expect(mocks.api.sync).toHaveBeenCalledWith(
        "account-1",
        expect.any(Function),
        false,
      ),
    );
    await waitFor(() =>
      expect(mocks.api.classifyPending.mock.calls.length).toBeGreaterThan(
        classificationCallsBeforeSync,
      ),
    );
  });

  it("clears stale rows and publishes rebuilt batches in the inbox", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = await screen.findByText("Unread thread");
    fireEvent.click(row.closest("button")!);
    expect(await screen.findByRole("button", { name: "Reply" })).toBeVisible();
    await waitFor(() =>
      expect(mocks.rebuildProgressHandlers.length).toBeGreaterThan(0),
    );
    const rebuildProgress =
      mocks.rebuildProgressHandlers[mocks.rebuildProgressHandlers.length - 1];

    act(() =>
      rebuildProgress({
        accountId: "account-1",
        phase: "finding",
        completed: 0,
        total: null,
      }),
    );

    expect(screen.queryByText("Unread thread")).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Reply" }),
    ).not.toBeInTheDocument();
    expect(
      screen.getAllByText("Checking which messages to download…")[0],
    ).toBeVisible();

    const rebuiltMessage = {
      ...mocks.message,
      id: "rebuilt-message",
      uid: 2,
      thread_id: "rebuilt-thread",
      subject: "Rebuilt message",
    };
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([rebuiltMessage]),
      nextCursor: null,
    });

    act(() =>
      rebuildProgress({
        accountId: "account-1",
        phase: "saving",
        completed: 50,
        total: 100,
      }),
    );

    expect(await screen.findByText("Rebuilt message")).toBeVisible();
  });

  it("reloads committed rows when a rebuild is cancelled after finding", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    await waitFor(() =>
      expect(mocks.rebuildProgressHandlers.length).toBeGreaterThan(0),
    );
    await waitFor(() =>
      expect(mocks.rebuildFinishedHandlers.length).toBeGreaterThan(0),
    );
    const rebuildProgress =
      mocks.rebuildProgressHandlers[mocks.rebuildProgressHandlers.length - 1];
    const rebuildFinished =
      mocks.rebuildFinishedHandlers[mocks.rebuildFinishedHandlers.length - 1];

    act(() =>
      rebuildProgress({
        accountId: "account-1",
        phase: "finding",
        completed: 0,
        total: null,
      }),
    );
    expect(screen.queryByText("Unread thread")).not.toBeInTheDocument();

    act(() =>
      rebuildFinished({ accountId: "account-1", outcome: "cancelled" }),
    );

    expect(await screen.findByText("Unread thread")).toBeVisible();
  });

  it("purges a deleted account and reloads only the remaining account", async () => {
    const remainingAccount = {
      ...mocks.account,
      id: "account-2",
      email: "remaining@example.com",
      account_name: "Remaining inbox",
    };
    const remainingMessage = {
      ...mocks.message,
      id: "message-2",
      account_id: remainingAccount.id,
      thread_id: "thread-2",
      subject: "Remaining thread",
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, remainingAccount]);
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      const accountIds = args[1] as string[];
      return {
        conversations: groupMessages(
          [mocks.message, remainingMessage].filter((message) =>
            accountIds.includes(message.account_id),
          ),
        ),
        nextCursor: null,
      };
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const removedRow = await screen.findByText("Unread thread");
    fireEvent.click(removedRow.closest("button")!);
    expect(await screen.findByRole("button", { name: "Reply" })).toBeVisible();
    await waitFor(() =>
      expect(mocks.accountRemovedHandlers.length).toBeGreaterThan(0),
    );
    mocks.api.search.mockClear();

    act(() => {
      mocks.accountRemovedHandlers.at(-1)!({ accountId: "account-1" });
    });

    expect(screen.queryByTitle("me@example.com")).not.toBeInTheDocument();
    expect(screen.getByTitle("remaining@example.com")).toBeVisible();
    expect(screen.queryByText("Unread thread")).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Reply" }),
    ).not.toBeInTheDocument();
    expect(await screen.findByText("Remaining thread")).toBeVisible();
    expect(mocks.api.search).toHaveBeenCalledWith(
      "",
      ["account-2"],
      "INBOX",
      false,
      false,
      100,
      null,
    );
    const searchCalls = mocks.api.search.mock.calls as unknown as Array<
      [unknown, string[]]
    >;
    expect(
      searchCalls.every(([, accountIds]) =>
        accountIds.every((id) => id !== "account-1"),
      ),
    ).toBe(true);
  });

  it("ignores a late arrival after its account has been removed", async () => {
    const [removal, lateArrival] =
      fixture.events.lateArrivalAfterAccountRemoval;
    const latePayload = JSON.parse(
      JSON.stringify(lateArrival.payload),
    ) as MailArrival;
    const removedAccount = {
      ...mocks.account,
      id: removal.payload.accountId,
      email: "removed@example.com",
      account_name: "Removed inbox",
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, removedAccount]);
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      const accountIds = args[1] as string[];
      return {
        conversations: accountIds.includes(mocks.account.id)
          ? groupMessages([mocks.message])
          : [],
        nextCursor: null,
      };
    });
    mocks.windowApi.isFocused.mockResolvedValue(false);
    mocks.sendNewMailNotification.mockResolvedValue(true);

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    await waitFor(() =>
      expect(mocks.accountRemovedHandlers.length).toBeGreaterThan(0),
    );
    await waitFor(() =>
      expect(mocks.mailArrivedHandlers.length).toBeGreaterThan(0),
    );
    act(() => {
      mocks.accountRemovedHandlers.at(-1)!({ accountId: removedAccount.id });
    });
    await waitFor(() =>
      expect(screen.queryByTitle(removedAccount.email)).not.toBeInTheDocument(),
    );
    localStorage.removeItem("dakia.last-sync-at");
    mocks.sendNewMailNotification.mockClear();
    mocks.api.recordNotificationDelivered.mockClear();

    expect(removal.event).toBe("account-removed");
    expect(lateArrival.event).toBe("mail-arrived");
    expect(latePayload.messages[0].mailbox_uid_validity).toBe(424242);
    await act(async () => {
      mocks.mailArrivedHandlers.at(-1)!(latePayload);
      await new Promise((resolve) => window.setTimeout(resolve, 0));
    });

    expect(localStorage.getItem("dakia.last-sync-at")).toBeNull();
    expect(mocks.sendNewMailNotification).not.toHaveBeenCalled();
    expect(mocks.api.recordNotificationDelivered).not.toHaveBeenCalled();
    expect(
      screen.queryByText(latePayload.messages[0].subject),
    ).not.toBeInTheDocument();
  });

  it("stops an in-flight arrival when account removal overtakes its reload", async () => {
    const [removal, lateArrival] =
      fixture.events.lateArrivalAfterAccountRemoval;
    const latePayload = JSON.parse(
      JSON.stringify(lateArrival.payload),
    ) as MailArrival;
    const removedAccount = {
      ...mocks.account,
      id: removal.payload.accountId,
      email: "removed@example.com",
      account_name: "Removed inbox",
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, removedAccount]);
    mocks.windowApi.isFocused.mockResolvedValue(false);
    mocks.sendNewMailNotification.mockResolvedValue(true);

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Unread thread");
    await waitFor(() =>
      expect(mocks.accountRemovedHandlers.length).toBeGreaterThan(0),
    );
    await waitFor(() =>
      expect(mocks.mailArrivedHandlers.length).toBeGreaterThan(0),
    );

    let resolveArrivalReload: ((page: MailThreadPage) => void) | undefined;
    mocks.api.search.mockImplementationOnce(
      () =>
        new Promise<MailThreadPage>((resolve) => {
          resolveArrivalReload = resolve;
        }),
    );
    act(() => mocks.mailArrivedHandlers.at(-1)!(latePayload));
    await waitFor(() => expect(resolveArrivalReload).toBeTypeOf("function"));

    act(() => {
      mocks.accountRemovedHandlers.at(-1)!({ accountId: removedAccount.id });
    });
    await act(async () => {
      resolveArrivalReload?.({ conversations: [], nextCursor: null });
      await new Promise((resolve) => window.setTimeout(resolve, 0));
    });

    expect(mocks.sendNewMailNotification).not.toHaveBeenCalled();
    expect(mocks.api.recordNotificationDelivered).not.toHaveBeenCalled();
    expect(
      screen.queryByText(latePayload.messages[0].subject),
    ).not.toBeInTheDocument();
  });

  it("rechecks account removal after notification permission resolves", async () => {
    const [removal, lateArrival] =
      fixture.events.lateArrivalAfterAccountRemoval;
    const latePayload = JSON.parse(
      JSON.stringify(lateArrival.payload),
    ) as MailArrival;
    const removedAccount = {
      ...mocks.account,
      id: removal.payload.accountId,
      email: "removed@example.com",
      account_name: "Removed inbox",
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, removedAccount]);
    mocks.windowApi.isFocused.mockResolvedValue(false);
    mocks.sendNewMailNotification.mockResolvedValue(true);

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Unread thread");
    await waitFor(() =>
      expect(mocks.accountRemovedHandlers.length).toBeGreaterThan(0),
    );
    await waitFor(() =>
      expect(mocks.mailArrivedHandlers.length).toBeGreaterThan(0),
    );

    let resolvePermission: (() => void) | undefined;
    mocks.requestInitialNotificationAccess.mockImplementationOnce(
      () =>
        new Promise<undefined>((resolve) => {
          resolvePermission = () => resolve(undefined);
        }),
    );
    act(() => mocks.mailArrivedHandlers.at(-1)!(latePayload));
    await waitFor(() => expect(resolvePermission).toBeTypeOf("function"));

    act(() => {
      mocks.accountRemovedHandlers.at(-1)!({ accountId: removedAccount.id });
    });
    await act(async () => {
      resolvePermission?.();
      await new Promise((resolve) => window.setTimeout(resolve, 0));
    });

    expect(mocks.sendNewMailNotification).not.toHaveBeenCalled();
    expect(mocks.api.recordNotificationDelivered).not.toHaveBeenCalled();
  });

  it("rechecks account removal after window focus resolves", async () => {
    const [removal, lateArrival] =
      fixture.events.lateArrivalAfterAccountRemoval;
    const latePayload = JSON.parse(
      JSON.stringify(lateArrival.payload),
    ) as MailArrival;
    const removedAccount = {
      ...mocks.account,
      id: removal.payload.accountId,
      email: "removed@example.com",
      account_name: "Removed inbox",
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, removedAccount]);
    mocks.sendNewMailNotification.mockResolvedValue(true);
    let resolveFocus: ((focused: boolean) => void) | undefined;
    mocks.windowApi.isFocused.mockImplementationOnce(
      () =>
        new Promise<boolean>((resolve) => {
          resolveFocus = resolve;
        }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Unread thread");
    await waitFor(() =>
      expect(mocks.accountRemovedHandlers.length).toBeGreaterThan(0),
    );
    await waitFor(() =>
      expect(mocks.mailArrivedHandlers.length).toBeGreaterThan(0),
    );

    act(() => mocks.mailArrivedHandlers.at(-1)!(latePayload));
    await waitFor(() => expect(resolveFocus).toBeTypeOf("function"));
    act(() => {
      mocks.accountRemovedHandlers.at(-1)!({ accountId: removedAccount.id });
    });
    await act(async () => {
      resolveFocus?.(false);
      await new Promise((resolve) => window.setTimeout(resolve, 0));
    });

    expect(mocks.sendNewMailNotification).not.toHaveBeenCalled();
    expect(mocks.api.recordNotificationDelivered).not.toHaveBeenCalled();
  });

  it("preserves a concurrent account update when another account is removed", async () => {
    const updatedAccount = {
      ...mocks.account,
      id: "account-2",
      email: "updated@example.com",
      account_name: "Updated inbox",
    };
    const updatedMessage = {
      ...mocks.message,
      id: "message-2",
      account_id: updatedAccount.id,
      thread_id: "thread-2",
      subject: "Updated thread",
    };
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      const accountIds = args[1] as string[];
      return {
        conversations: groupMessages(
          [mocks.message, updatedMessage].filter((message) =>
            accountIds.includes(message.account_id),
          ),
        ),
        nextCursor: null,
      };
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    expect(await screen.findByText("Unread thread")).toBeVisible();
    await waitFor(() =>
      expect(mocks.accountUpdatedHandlers.length).toBeGreaterThan(0),
    );
    await waitFor(() =>
      expect(mocks.accountRemovedHandlers.length).toBeGreaterThan(0),
    );

    act(() => {
      mocks.accountUpdatedHandlers.at(-1)!(updatedAccount);
    });
    expect(screen.getByTitle("updated@example.com")).toBeVisible();
    mocks.api.search.mockClear();

    act(() => {
      mocks.accountRemovedHandlers.at(-1)!({ accountId: "account-1" });
    });

    expect(screen.queryByTitle("me@example.com")).not.toBeInTheDocument();
    expect(screen.getByTitle("updated@example.com")).toBeVisible();
    expect(await screen.findByText("Updated thread")).toBeVisible();
    expect(mocks.api.search).toHaveBeenCalledWith(
      "",
      ["account-2"],
      "INBOX",
      false,
      false,
      100,
      null,
    );
  });

  it("settles on the empty account state when deletion overtakes an in-flight search", async () => {
    let resolveSearch: (page: MailThreadPage) => void = () => undefined;
    mocks.api.search.mockImplementationOnce(
      () =>
        new Promise<MailThreadPage>((resolve) => {
          resolveSearch = resolve;
        }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() => expect(mocks.api.search).toHaveBeenCalled());
    await waitFor(() =>
      expect(mocks.accountRemovedHandlers.length).toBeGreaterThan(0),
    );

    act(() => {
      mocks.accountRemovedHandlers.at(-1)!({ accountId: "account-1" });
    });

    expect(
      await screen.findByText("Bring your inboxes together"),
    ).toBeVisible();

    await act(async () => {
      resolveSearch({
        conversations: groupMessages([mocks.message]),
        nextCursor: null,
      });
    });

    expect(screen.queryByText("Unread thread")).not.toBeInTheDocument();
    expect(screen.getByText("Bring your inboxes together")).toBeVisible();
  });

  it("does not restore a removed account from a stale initial account fetch", async () => {
    let resolveAccounts: (accounts: Array<typeof mocks.account>) => void = () =>
      undefined;
    mocks.api.accounts.mockImplementationOnce(
      () =>
        new Promise<Array<typeof mocks.account>>((resolve) => {
          resolveAccounts = resolve;
        }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.accountRemovedHandlers.length).toBeGreaterThan(0),
    );
    act(() => {
      mocks.accountRemovedHandlers.at(-1)!({ accountId: "account-1" });
    });
    expect(
      await screen.findByText("Bring your inboxes together"),
    ).toBeVisible();

    await act(async () => {
      resolveAccounts([mocks.account]);
    });

    expect(screen.queryByTitle("me@example.com")).not.toBeInTheDocument();
    expect(screen.queryByText("Unread thread")).not.toBeInTheDocument();
    expect(screen.getByText("Bring your inboxes together")).toBeVisible();
    expect(mocks.api.search).not.toHaveBeenCalled();
  });

  it("keeps a deferred account listener active across initial renders", async () => {
    let accountRemovedHandler:
      ((event: { accountId: string }) => void) | undefined;
    let finishListenerSetup: ((unlisten: () => undefined) => void) | undefined;
    const lateUnlisten = vi.fn(() => undefined);
    mocks.onAccountRemoved.mockImplementationOnce((handler) => {
      accountRemovedHandler = handler;
      return new Promise<() => undefined>((resolve) => {
        finishListenerSetup = resolve;
      });
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    expect(await screen.findByText("Unread thread")).toBeVisible();
    await waitFor(() => expect(mocks.onAccountRemoved).toHaveBeenCalledOnce());

    act(() => finishListenerSetup?.(lateUnlisten));
    expect(lateUnlisten).not.toHaveBeenCalled();

    act(() => accountRemovedHandler?.({ accountId: "account-1" }));

    expect(
      await screen.findByText("Bring your inboxes together"),
    ).toBeVisible();
    expect(screen.queryByText("Unread thread")).not.toBeInTheDocument();
  });

  it("unlistens an account listener that resolves after unmount", async () => {
    let finishListenerSetup: ((unlisten: () => undefined) => void) | undefined;
    const lateUnlisten = vi.fn(() => undefined);
    mocks.onAccountRemoved.mockImplementationOnce(
      () =>
        new Promise<() => undefined>((resolve) => {
          finishListenerSetup = resolve;
        }),
    );

    const { unmount } = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() => expect(mocks.onAccountRemoved).toHaveBeenCalledOnce());
    unmount();

    act(() => finishListenerSetup?.(lateUnlisten));

    await waitFor(() => expect(lateUnlisten).toHaveBeenCalledOnce());
  });

  it("unlistens a notification action listener that resolves after unmount", async () => {
    let finishListenerSetup: ((unlisten: () => undefined) => void) | undefined;
    const lateUnlisten = vi.fn(() => undefined);
    mocks.onNotificationAction.mockImplementationOnce(
      () =>
        new Promise<() => undefined>((resolve) => {
          finishListenerSetup = resolve;
        }),
    );

    const { unmount } = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.onNotificationAction).toHaveBeenCalledOnce(),
    );
    unmount();

    act(() => finishListenerSetup?.(lateUnlisten));

    await waitFor(() => expect(lateUnlisten).toHaveBeenCalledOnce());
  });

  it("cleans a desktop notification listener when plugin registration fails", async () => {
    const desktopUnlisten = vi.fn(() => undefined);
    mocks.onNotificationAction.mockRejectedValueOnce(
      new Error("plugin listener failed"),
    );
    mocks.onDesktopNotificationAction.mockResolvedValueOnce(desktopUnlisten);

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "plugin listener failed",
        "error",
      ),
    );
    await waitFor(() => expect(desktopUnlisten).toHaveBeenCalledOnce());
  });

  it("cleans a plugin notification listener when desktop registration fails", async () => {
    const pluginUnlisten = vi.fn(() => undefined);
    mocks.onNotificationAction.mockResolvedValueOnce(pluginUnlisten);
    mocks.onDesktopNotificationAction.mockRejectedValueOnce(
      new Error("desktop listener failed"),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "desktop listener failed",
        "error",
      ),
    );
    await waitFor(() => expect(pluginUnlisten).toHaveBeenCalledOnce());
  });

  it("cleans a desktop notification listener that resolves after plugin registration fails", async () => {
    let finishDesktopRegistration:
      ((unlisten: () => undefined) => void) | undefined;
    const lateUnlisten = vi.fn(() => undefined);
    mocks.onNotificationAction.mockRejectedValueOnce(
      new Error("plugin listener failed"),
    );
    mocks.onDesktopNotificationAction.mockImplementationOnce(
      () =>
        new Promise<() => undefined>((resolve) => {
          finishDesktopRegistration = resolve;
        }),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "plugin listener failed",
        "error",
      ),
    );
    act(() => finishDesktopRegistration?.(lateUnlisten));

    await waitFor(() => expect(lateUnlisten).toHaveBeenCalledOnce());
  });

  it("cleans a Reader mutation listener when failure-listener registration rejects", async () => {
    const mutationUnlisten = vi.fn(() => undefined);
    mocks.onReaderWindowMutated.mockResolvedValueOnce(mutationUnlisten);
    mocks.onReaderWindowFailed.mockRejectedValueOnce(
      new Error("Reader failure listener unavailable"),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "Reader failure listener unavailable",
        "error",
      ),
    );
    expect(mutationUnlisten).toHaveBeenCalledOnce();
  });

  it("cleans a Reader listener that resolves after its sibling registration failed", async () => {
    let finishMutationRegistration:
      ((unlisten: () => undefined) => void) | undefined;
    const lateUnlisten = vi.fn(() => undefined);
    mocks.onReaderWindowMutated.mockImplementationOnce(
      () =>
        new Promise<() => undefined>((resolve) => {
          finishMutationRegistration = resolve;
        }),
    );
    mocks.onReaderWindowFailed.mockRejectedValueOnce(
      new Error("Reader failure listener unavailable"),
    );

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "Reader failure listener unavailable",
        "error",
      ),
    );
    act(() => finishMutationRegistration?.(lateUnlisten));

    await waitFor(() => expect(lateUnlisten).toHaveBeenCalledOnce());
  });

  it("cleans a Reader listener on unmount while its sibling is still registering", async () => {
    const mutationUnlisten = vi.fn(() => undefined);
    mocks.onReaderWindowMutated.mockResolvedValueOnce(mutationUnlisten);
    mocks.onReaderWindowFailed.mockImplementationOnce(
      () => new Promise<() => undefined>(() => undefined),
    );

    const view = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.onReaderWindowFailed).toHaveBeenCalledOnce(),
    );
    await Promise.resolve();
    view.unmount();

    expect(mutationUnlisten).toHaveBeenCalledOnce();
  });
});
