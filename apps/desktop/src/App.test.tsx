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
import "./i18n";
import App from "./App";
import { groupMessages } from "./threads";
import type {
  Account,
  MailRebuildProgress,
  MailSummary,
  SmartInboxPage,
  MailThreadPage,
} from "./types";

const smartInboxPage = (
  sections: SmartInboxPage["sections"],
): SmartInboxPage => ({ sections });

const mocks = vi.hoisted(() => {
  const unlisten = () => undefined;
  const rebuildProgressHandlers: Array<
    (progress: MailRebuildProgress) => void
  > = [];
  const hydratedHandlers: Array<() => void> = [];
  const mailChangedHandlers: Array<() => void> = [];
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
      action: vi.fn(async () => undefined),
      aiAvailable: vi.fn(async () => false),
      accounts: vi.fn(async () => [account]),
      classifyPending: vi.fn(async () => 0),
      configureTray: vi.fn(async () => undefined),
      content: vi.fn(
        async (
          _messageId: string,
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
      searchRemote: vi.fn(async () => []),
      setRead: vi.fn(async () => undefined),
      setStarred: vi.fn(async () => undefined),
      starredCount: vi.fn(async () => 0),
      startRealtimeSync: vi.fn(async () => undefined),
      sync: vi.fn(async () => ({ syncedCount: 0, newMessages: [] })),
      unsubscribe: vi.fn(async () => ({ kind: "completed" as const })),
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
    openReaderWindow: vi.fn(async () => undefined),
    noopListener: vi.fn(async () => unlisten),
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
    accountRemovedHandlers,
    accountUpdatedHandlers,
    rebuildProgressHandlers,
    hydratedHandlers,
    mailChangedHandlers,
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
    onMailHydrated: vi.fn(async (handler: () => void) => {
      hydratedHandlers.push(handler);
      return unlisten;
    }),
    onMailChanged: vi.fn(async (handler: () => void) => {
      mailChangedHandlers.push(handler);
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
  onOutboxChanged: mocks.noopListener,
  openComposeWindow: mocks.openComposeWindow,
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
  onAccountConnected: mocks.noopListener,
  onAccountRemoved: mocks.onAccountRemoved,
  onAccountUpdated: mocks.onAccountUpdated,
  onMailArrived: mocks.noopListener,
  onMailChanged: mocks.onMailChanged,
  onMailHydrated: mocks.onMailHydrated,
  onMailIndexRebuilt: mocks.noopListener,
  onMailRebuildProgress: mocks.onMailRebuildProgress,
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

describe("App read state", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.rebuildProgressHandlers.length = 0;
    mocks.hydratedHandlers.length = 0;
    mocks.mailChangedHandlers.length = 0;
    mocks.nativeMenuHandlers.length = 0;
    mocks.notificationActionHandlers.length = 0;
    mocks.desktopNotificationActionHandlers.length = 0;
    mocks.readerMutationHandlers.length = 0;
    mocks.readerFailureHandlers.length = 0;
    mocks.openReaderWindow.mockClear();
    mocks.accountRemovedHandlers.length = 0;
    mocks.accountUpdatedHandlers.length = 0;
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

  it("does not probe an AI provider while AI features are hidden", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    expect(mocks.api.aiAvailable).not.toHaveBeenCalled();
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

  it("marks an unread conversation as read when opened", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const row = await screen.findByText("Unread thread");
    fireEvent.click(row.closest("button")!);

    await waitFor(() =>
      expect(mocks.api.setRead).toHaveBeenCalledWith("message-1", true),
    );
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

  it("reports a clipboard failure when copying a header mailbox", async () => {
    const writeText = vi.fn().mockRejectedValue(new Error("Clipboard denied"));
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
    const address = await screen.findByRole("button", {
      name: "Email sender@example.com",
    });
    fireEvent.contextMenu(address, { clientX: 140, clientY: 90 });
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Copy", hidden: true }),
    );

    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith("sender@example.com"),
    );
    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "Could not copy text",
        "error",
      ),
    );
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
      expect(mocks.api.setRead).toHaveBeenCalledWith(hiddenInbox.id, true),
    );
    expect(mocks.api.setRead).not.toHaveBeenCalledWith(visibleArchive.id, true);

    fireEvent.click(screen.getByRole("button", { name: "Remove star" }));
    await waitFor(() => {
      expect(mocks.api.setStarred).toHaveBeenCalledWith(hiddenInbox.id, false);
      expect(mocks.api.setStarred).toHaveBeenCalledWith(
        visibleArchive.id,
        false,
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
    expect(mocks.api.content).toHaveBeenCalledWith(latest.id);
    expect(mocks.api.content).not.toHaveBeenCalledWith(older.id);
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
      expect(mocks.api.setRead).toHaveBeenCalledWith("message-1", true),
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
      expect(mocks.api.setRead).toHaveBeenCalledWith("message-1", true),
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
      expect(mocks.api.setRead).toHaveBeenCalledWith("message-1", true),
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
            '<blockquote type="cite">Provider body<br>&gt; Nested history</blockquote>',
          ),
        }),
      ),
    );
    expect(mocks.api.content).toHaveBeenCalledTimes(1);
    expect(mocks.api.content).toHaveBeenCalledWith("message-2");
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
    expect(mocks.api.content).toHaveBeenLastCalledWith(older.id);

    mocks.openComposeWindow.mockClear();
    fireEvent.click(screen.getByRole("button", { name: "Forward" }));
    await waitFor(() =>
      expect(mocks.openComposeWindow).toHaveBeenCalledWith(
        expect.objectContaining({
          accountId: "account-1",
          subject: "Fwd: Older question",
          contextMessageIds: [older.id],
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
    mocks.api.content.mockImplementation(async (messageId) => ({
      body_text:
        messageId === sentB.id ? "Sent B full body" : "Received message body",
      attachments: [],
    }));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click((await screen.findByText("Received C")).closest("button")!);
    fireEvent.click(
      await screen.findByRole("button", { name: "Expand message from Me" }),
    );
    await screen.findByText("Sent B full body");

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
            bodyHtml: expect.stringContaining("Sent B full body"),
            contextMessageIds: ["received-a", "sent-b", "received-c"],
          }),
        ),
      );
      expect(mocks.api.content).toHaveBeenCalledWith("sent-b");
    }
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

    expect(
      await screen.findByText("invalid unsubscribe email address"),
    ).toBeVisible();
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
});
