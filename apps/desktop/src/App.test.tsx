import { MantineProvider } from "@mantine/core";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { beforeEach, describe, expect, it, type Mock, vi } from "vitest";
import "./i18n";
import App from "./App";
import { groupMessages } from "./threads";
import type {
  Account,
  AccountConnection,
  MailRebuildFinished,
  MailRebuildProgress,
  MailSummary,
  SearchProgressUpdate,
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
  const rebuildFinishedHandlers: Array<(result: MailRebuildFinished) => void> =
    [];
  const hydratedHandlers: Array<() => void> = [];
  const mailChangedHandlers: Array<() => void> = [];
  const nativeMenuHandlers: Array<(action: string) => void> = [];
  const notificationActionHandlers: Array<
    (extra: Record<string, unknown>) => void | Promise<void>
  > = [];
  const desktopNotificationActionHandlers: Array<
    (extra: Record<string, unknown>) => void | Promise<void>
  > = [];
  const contactedPeopleHandlers: Array<
    (change: import("./types").ContactedPeopleChanged) => void
  > = [];
  const searchProgressHandlers: Array<
    (progress: SearchProgressUpdate) => void
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
      accounts: vi.fn(async (): Promise<Account[]> => [account]),
      listSearchMailboxes: vi.fn(
        async (): Promise<import("./types").SearchMailbox[]> => [],
      ),
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
      search: vi.fn(
        async (
          _text: string,
          _accountIds: string[],
          _mailbox: string | undefined,
          _unreadOnly: boolean,
          _flaggedOnly: boolean,
          _limit: number,
          _cursor: import("./types").MailCursor | null,
        ) => ({
          conversations: groupMessages([message]),
          nextCursor: null as import("./types").MailCursor | null,
        }),
      ),
      smartInbox: vi.fn(async (): Promise<SmartInboxPage> => ({
        sections: [],
      })),
      searchRemote: vi.fn(async () => []),
      startSearchV2: vi.fn(
        async (
          _request: import("./types").SearchRequestV2,
        ): Promise<import("./types").SearchPageV2> => {
          throw new Error("v2 unavailable");
        },
      ),
      nextSearchPageV2: vi.fn(
        async (
          _request: import("./types").SearchRequestV2,
        ): Promise<import("./types").SearchPageV2> => {
          throw new Error("v2 unavailable");
        },
      ),
      cancelSearchV2: vi.fn(async () => undefined),
      contactedPeopleSettings: vi.fn(async () => ({ enabled: true })),
      suggestContactedPeople: vi.fn(
        async (): Promise<import("./types").ContactedPersonSuggestion[]> => [],
      ),
      showEmailAddressContextMenu: vi.fn(async () => undefined),
      setRead: vi.fn(async () => undefined),
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
    nativeMenuHandlers,
    notificationActionHandlers,
    desktopNotificationActionHandlers,
    readerMutationHandlers,
    readerFailureHandlers,
    contactedPeopleHandlers,
    searchProgressHandlers,
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
    onContactedPeopleChanged: vi.fn(
      async (
        handler: (change: import("./types").ContactedPeopleChanged) => void,
      ) => {
        contactedPeopleHandlers.push(handler);
        return unlisten;
      },
    ),
    onSearchProgress: vi.fn(
      async (handler: (progress: SearchProgressUpdate) => void) => {
        searchProgressHandlers.push(handler);
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
  onMailArrived: mocks.noopListener,
  onMailChanged: mocks.onMailChanged,
  onMailHydrated: mocks.onMailHydrated,
  onMailIndexRebuilt: mocks.noopListener,
  onMailRebuildProgress: mocks.onMailRebuildProgress,
  onMailRebuildFinished: mocks.onMailRebuildFinished,
  onMailSyncState: mocks.noopListener,
  onDesktopNotificationAction: mocks.onDesktopNotificationAction,
  onNativeMenuAction: mocks.onNativeMenuAction,
  onContactedPeopleChanged: mocks.onContactedPeopleChanged,
  onSearchProgress: mocks.onSearchProgress,
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
    mocks.rebuildProgressHandlers.length = 0;
    mocks.rebuildFinishedHandlers.length = 0;
    mocks.hydratedHandlers.length = 0;
    mocks.mailChangedHandlers.length = 0;
    mocks.nativeMenuHandlers.length = 0;
    mocks.notificationActionHandlers.length = 0;
    mocks.desktopNotificationActionHandlers.length = 0;
    mocks.contactedPeopleHandlers.length = 0;
    mocks.searchProgressHandlers.length = 0;
    mocks.readerMutationHandlers.length = 0;
    mocks.readerFailureHandlers.length = 0;
    mocks.openReaderWindow.mockClear();
    mocks.accountRemovedHandlers.length = 0;
    mocks.accountUpdatedHandlers.length = 0;
    mocks.accountConnectedHandlers.length = 0;
    localStorage.clear();
    mocks.api.accounts.mockResolvedValue([mocks.account]);
    mocks.api.listSearchMailboxes.mockResolvedValue([]);
    mocks.api.action.mockResolvedValue(undefined);
    localStorage.setItem("dakia.mail-list-view", "list");
    mocks.api.classifyPending.mockResolvedValue(0);
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([mocks.message]),
      nextCursor: null,
    });
    mocks.api.smartInbox.mockResolvedValue(smartInboxPage([]));
    mocks.api.startSearchV2.mockRejectedValue(new Error("v2 unavailable"));
    mocks.api.nextSearchPageV2.mockRejectedValue(new Error("v2 unavailable"));
    mocks.api.contactedPeopleSettings.mockResolvedValue({ enabled: true });
    mocks.api.starredCount.mockResolvedValue(0);
    mocks.api.content.mockResolvedValue({
      body_text: "Message body",
      attachments: [],
    });
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

  it("offers every selectable provider-catalogued folder to search", async () => {
    mocks.api.listSearchMailboxes.mockResolvedValue([
      { localPath: "Projects/Client A", selectable: true },
      { localPath: "Projects/NoSelect", selectable: false },
    ]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.api.listSearchMailboxes).toHaveBeenCalled(),
    );
    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.focus(search);
    const clientFolder = await screen.findByRole("option", {
      name: "In Projects/Client A",
    });
    expect(
      screen.queryByRole("option", { name: "In Projects/NoSelect" }),
    ).toBeNull();
    fireEvent.click(clientFolder);
    expect(search).toHaveValue('in:"Projects/Client A"');
  });

  it("clears an invalid folder draft error after choosing a nested folder", async () => {
    const orion = {
      ...mocks.message,
      id: "orion-folder-message",
      thread_id: "orion-folder-thread",
      mailbox: "Projects/Orion",
      subject: "Orion folder result",
    };
    mocks.api.listSearchMailboxes.mockResolvedValue([
      { localPath: "Projects/Orion", selectable: true },
    ]);
    mocks.api.startSearchV2.mockImplementation(async (request) => {
      if (request.raw_query === "in:") {
        throw {
          category: "parse",
          message: "The search query is invalid.",
        } satisfies import("./types").SearchErrorV2;
      }
      return {
        conversations:
          request.raw_query === "in:Projects/Orion"
            ? groupMessages([orion])
            : groupMessages([mocks.message]),
        match_evidence: {},
        coverage: [],
        continuation: null,
        session_id: request.client_request_id!,
        revision: 1,
      };
    });
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "in:" } });
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "The search query is invalid.",
    );

    fireEvent.focus(search);
    fireEvent.click(
      await screen.findByRole("option", { name: "In Projects/Orion" }),
    );

    expect(await screen.findByText("Orion folder result")).toBeVisible();
    expect(search).toHaveValue("in:Projects/Orion");
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("refreshes provider-catalogued folders after sync without restarting the account", async () => {
    mocks.api.listSearchMailboxes
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([
        { localPath: "Projects/New client", selectable: true },
      ]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    await waitFor(() =>
      expect(mocks.api.listSearchMailboxes).toHaveBeenCalledOnce(),
    );
    fireEvent.click(screen.getByRole("button", { name: "Sync" }));
    await waitFor(() => expect(mocks.api.sync).toHaveBeenCalledOnce());
    const search = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(search);
    expect(
      await screen.findByRole("option", {
        name: "In Projects/New client",
      }),
    ).toBeVisible();
  });

  it("replaces disabled account folders with the active catalogue", async () => {
    const disabled = { ...mocks.account, enabled: false };
    mocks.api.listSearchMailboxes
      .mockResolvedValueOnce([
        { localPath: "Projects/Disabled account", selectable: true },
      ])
      .mockResolvedValueOnce([]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.focus(search);
    expect(
      await screen.findByRole("option", {
        name: "In Projects/Disabled account",
      }),
    ).toBeVisible();
    await waitFor(() =>
      expect(mocks.accountUpdatedHandlers.length).toBeGreaterThan(0),
    );

    act(() => mocks.accountUpdatedHandlers.at(-1)!(disabled));

    await waitFor(() =>
      expect(
        screen.queryByRole("option", { name: "In Projects/Disabled account" }),
      ).toBeNull(),
    );
  });

  it("removes deleted account folders while keeping remaining account folders", async () => {
    const remaining = {
      ...mocks.account,
      id: "account-2",
      email: "remaining@example.com",
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, remaining]);
    mocks.api.listSearchMailboxes
      .mockResolvedValueOnce([
        { localPath: "Projects/Deleted account", selectable: true },
      ])
      .mockResolvedValueOnce([
        { localPath: "Projects/Remaining account", selectable: true },
      ]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.focus(search);
    expect(
      await screen.findByRole("option", {
        name: "In Projects/Deleted account",
      }),
    ).toBeVisible();
    await waitFor(() =>
      expect(mocks.accountRemovedHandlers.length).toBeGreaterThan(0),
    );

    act(() => mocks.accountRemovedHandlers.at(-1)!({ accountId: "account-1" }));

    await waitFor(() =>
      expect(
        screen.queryByRole("option", { name: "In Projects/Deleted account" }),
      ).toBeNull(),
    );
    expect(
      await screen.findByRole("option", {
        name: "In Projects/Remaining account",
      }),
    ).toBeVisible();
  });

  it("keeps observed mailbox paths available when the catalogue command fails", async () => {
    const observed = {
      ...mocks.message,
      id: "observed-folder-message",
      thread_id: "observed-folder-thread",
      mailbox: "Projects/Observed",
      subject: "Observed folder message",
    };
    mocks.api.listSearchMailboxes.mockRejectedValue(
      new Error("command unavailable"),
    );
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([observed]),
      nextCursor: null,
    });
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Observed folder message");
    const search = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(search);
    expect(
      await screen.findByRole("option", { name: "In Projects/Observed" }),
    ).toBeVisible();
  });

  it("never exposes an opaque mailbox locator from the observed-message fallback", async () => {
    const opaque = {
      ...mocks.message,
      id: "opaque-folder-message",
      thread_id: "opaque-folder-thread",
      mailbox: "Mailbox::@dakia-mailbox-v1:c2VjcmV0:ZGlzcGxheQ",
      subject: "Opaque folder message",
    };
    mocks.api.listSearchMailboxes.mockRejectedValue(
      new Error("command unavailable"),
    );
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([opaque]),
      nextCursor: null,
    });
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Opaque folder message");
    const search = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.focus(search);
    expect(
      screen.queryByRole("option", {
        name: "In Mailbox::@dakia-mailbox-v1:c2VjcmV0:ZGlzcGxheQ",
      }),
    ).toBeNull();
  });

  it("keeps provider search explicit while local preview follows typing", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    mocks.api.searchRemote.mockClear();
    fireEvent.change(search, { target: { value: "invoice" } });

    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledWith(
        expect.objectContaining({
          raw_query: "invoice",
          account_ids: ["account-1"],
          execution_mode: "local",
        }),
      ),
    );
    expect(mocks.api.searchRemote).not.toHaveBeenCalled();

    fireEvent.keyDown(search, { key: "Enter" });
    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledWith(
        expect.objectContaining({ raw_query: "invoice" }),
      ),
    );
    expect(mocks.api.searchRemote).not.toHaveBeenCalled();
    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "Something went wrong",
        "v2 unavailable",
        "error",
      ),
    );
  });

  it("uses a bounded local V2 preview and exposes its continuation without provider work", async () => {
    const sparseMatch: MailSummary = {
      ...mocks.message,
      id: "sparse-preview-match",
      thread_id: "sparse-preview-thread",
      subject: "Sparse preview match",
    };
    mocks.api.search.mockResolvedValue({ conversations: [], nextCursor: null });
    mocks.api.startSearchV2.mockImplementation(async (request) => ({
      conversations: [],
      match_evidence: {},
      coverage: [],
      continuation: "preview-next",
      session_id: request.client_request_id!,
      revision: 1,
    }));
    mocks.api.nextSearchPageV2.mockImplementation(async (request) => ({
      conversations: groupMessages([sparseMatch]),
      match_evidence: {},
      coverage: [],
      continuation: null,
      session_id: request.client_request_id!,
      revision: 2,
    }));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "sparse" } });

    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledWith(
        expect.objectContaining({
          raw_query: "sparse",
          execution_mode: "local",
        }),
      ),
    );
    expect(mocks.api.searchRemote).not.toHaveBeenCalled();
    expect(
      await screen.findByRole("button", { name: "Show more" }),
    ).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Show more" }));
    expect(await screen.findByText("Sparse preview match")).toBeVisible();
    expect(mocks.api.nextSearchPageV2).toHaveBeenCalledWith(
      expect.objectContaining({ continuation: "preview-next" }),
    );
  });

  it("cancels a stale local V2 preview before it can publish matches", async () => {
    let resolvePreview:
      ((page: import("./types").SearchPageV2) => void) | undefined;
    const staleMatch: MailSummary = {
      ...mocks.message,
      id: "stale-preview-match",
      thread_id: "stale-preview-thread",
      subject: "Stale preview match",
    };
    mocks.api.search.mockResolvedValue({ conversations: [], nextCursor: null });
    mocks.api.startSearchV2.mockImplementationOnce(
      () =>
        new Promise<import("./types").SearchPageV2>((resolve) => {
          resolvePreview = resolve;
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "first draft" } });
    await waitFor(() => expect(mocks.api.startSearchV2).toHaveBeenCalled());
    const request = mocks.api.startSearchV2.mock.calls[0][0];
    fireEvent.change(search, { target: { value: "second draft" } });
    await waitFor(() =>
      expect(mocks.api.cancelSearchV2).toHaveBeenCalledWith(
        request.client_request_id,
      ),
    );

    await act(async () => {
      resolvePreview?.({
        conversations: groupMessages([staleMatch]),
        match_evidence: {},
        coverage: [],
        continuation: null,
        session_id: request.client_request_id!,
        revision: 1,
      });
    });
    expect(screen.queryByText("Stale preview match")).not.toBeInTheDocument();
  });

  it("shows structured local search errors inline without opening a native dialog", async () => {
    mocks.api.startSearchV2.mockImplementation(async (request) => {
      if (request.raw_query === 'subject:"') {
        throw {
          category: "parse",
          position: 8,
          message: "Missing closing quote at character 8.",
        } satisfies import("./types").SearchErrorV2;
      }
      return {
        conversations: groupMessages([mocks.message]),
        match_evidence: {},
        coverage: [],
        continuation: null,
        session_id: request.client_request_id!,
        revision: 1,
      };
    });
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: 'subject:"' } });

    const alert = await screen.findByText(
      "Missing closing quote at character 8.",
    );
    expect(alert).toHaveAttribute("role", "alert");
    expect(alert).toBeVisible();
    expect(mocks.showNativeMessage).not.toHaveBeenCalled();
    expect(screen.queryByText("[object Object]")).not.toBeInTheDocument();
  });

  it("shows a structured provider search error inline with its safe message", async () => {
    mocks.api.startSearchV2.mockRejectedValue({
      category: "unsupported",
      unsupported_operator: "memo",
      message: "memo: is not supported.",
    } satisfies import("./types").SearchErrorV2);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "memo:project" } });
    fireEvent.keyDown(search, { key: "Enter" });

    const alert = await screen.findByText("memo: is not supported.");
    expect(alert).toHaveAttribute("role", "alert");
    expect(alert).toBeVisible();
    expect(mocks.showNativeMessage).not.toHaveBeenCalled();
  });

  it("uses the local contacted-people index for people search suggestions", async () => {
    mocks.api.suggestContactedPeople.mockResolvedValue([
      { address: "alex@example.com", display_name: "Alex" },
    ]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "from:al" } });
    await waitFor(() =>
      expect(mocks.api.suggestContactedPeople).toHaveBeenCalledWith(
        "al",
        undefined,
      ),
    );
    fireEvent.focus(search);
    expect(await screen.findByText("Alex <alex@example.com>")).toBeVisible();
  });

  it("requests people suggestions from an open group and unfinished quoted people filter", async () => {
    mocks.api.suggestContactedPeople.mockResolvedValue([
      { address: "alice@example.com", display_name: "Alice" },
    ]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, {
      target: { value: "(from:al" },
    });
    await waitFor(() =>
      expect(mocks.api.suggestContactedPeople).toHaveBeenCalledWith(
        "al",
        undefined,
      ),
    );

    fireEvent.change(search, { target: { value: 'from:"Alice S' } });
    await waitFor(() =>
      expect(mocks.api.suggestContactedPeople).toHaveBeenCalledWith(
        "Alice S",
        undefined,
      ),
    );
  });

  it("ignores late people suggestions from an earlier search token", async () => {
    const resolvers = new Map<
      string,
      (people: import("./types").ContactedPersonSuggestion[]) => void
    >();
    const suggest = mocks.api.suggestContactedPeople as Mock<
      (prefix: string) => Promise<import("./types").ContactedPersonSuggestion[]>
    >;
    suggest.mockImplementation(
      (prefix: string) =>
        new Promise<import("./types").ContactedPersonSuggestion[]>(
          (resolve) => {
            resolvers.set(prefix, resolve);
          },
        ),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "from:al" } });
    await waitFor(() =>
      expect(mocks.api.suggestContactedPeople).toHaveBeenCalledWith(
        "al",
        undefined,
      ),
    );

    fireEvent.change(search, { target: { value: "to:bo" } });
    await act(async () => {
      resolvers.get("al")?.([
        { address: "alex@example.com", display_name: "Alex" },
      ]);
    });
    fireEvent.focus(search);
    expect(screen.queryByText("Alex <alex@example.com>")).toBeNull();

    await waitFor(() =>
      expect(mocks.api.suggestContactedPeople).toHaveBeenCalledWith(
        "bo",
        undefined,
      ),
    );
    await act(async () => {
      resolvers.get("bo")?.([
        { address: "bob@example.com", display_name: "Bob" },
      ]);
    });
    fireEvent.focus(search);
    fireEvent.click(await screen.findByText("Bob <bob@example.com>"));
    expect(search).toHaveValue("to:bob@example.com");
  });

  it("clears open people suggestions immediately when contacted-people data changes", async () => {
    const alex = { address: "alex@example.com", display_name: "Alex" };
    mocks.api.suggestContactedPeople.mockResolvedValue([alex]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "from:al" } });
    fireEvent.focus(search);
    expect(await screen.findByText("Alex <alex@example.com>")).toBeVisible();
    await waitFor(() => expect(mocks.contactedPeopleHandlers).toHaveLength(1));
    const callsBeforeClear = mocks.api.suggestContactedPeople.mock.calls.length;
    mocks.api.suggestContactedPeople.mockResolvedValue([]);

    await act(async () => {
      mocks.contactedPeopleHandlers[0]({ enabled: true, cleared: true });
    });
    expect(
      screen.queryByText("Alex <alex@example.com>"),
    ).not.toBeInTheDocument();
    await waitFor(() =>
      expect(mocks.api.suggestContactedPeople).toHaveBeenCalledTimes(
        callsBeforeClear + 1,
      ),
    );

    await act(async () => {
      mocks.contactedPeopleHandlers[0]({ enabled: false, cleared: false });
    });
    expect(
      screen.queryByText("Alex <alex@example.com>"),
    ).not.toBeInTheDocument();
    expect(mocks.api.suggestContactedPeople).toHaveBeenCalledTimes(
      callsBeforeClear + 1,
    );
  });

  it("shows mixed v2 coverage and continues the submitted v2 session", async () => {
    const work: Account = {
      ...mocks.account,
      id: "account-2",
      email: "work@example.com",
      account_name: "Work",
    };
    const secondMessage: MailSummary = {
      ...mocks.message,
      id: "message-2",
      thread_id: "thread-2",
      subject: "Second page",
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, work]);
    mocks.api.startSearchV2.mockImplementation(async (request) => ({
      conversations: groupMessages([mocks.message]),
      match_evidence: {
        "account-1:thread-1": {
          primary_message_id: "message-1",
          matched_message_ids: ["message-1"],
          match_count: 1,
          excerpt: "Receipt matched in the message body",
        },
      },
      coverage: [
        {
          account_id: "account-1",
          mailbox: "INBOX",
          state: "provider_searched",
        },
        {
          account_id: "account-2",
          mailbox: "Archive",
          state: "offline",
          detail: "No connection",
        },
        {
          account_id: "account-2",
          mailbox: "Sent",
          state: "provider_partial",
        },
      ],
      continuation: "next-page",
      session_id: request.client_request_id!,
      revision: 1,
    }));
    mocks.api.nextSearchPageV2.mockImplementation(async (request) => ({
      conversations: groupMessages([secondMessage]),
      match_evidence: {},
      coverage: [
        {
          account_id: "account-1",
          mailbox: "INBOX",
          state: "local_body_index",
        },
      ],
      continuation: null,
      session_id: request.client_request_id!,
      revision: 2,
    }));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "receipt" } });
    fireEvent.keyDown(search, { key: "Enter" });

    const coverage = await screen.findByRole("status", {
      name: "Search status",
    });
    expect(coverage).toHaveTextContent("Work couldn’t be searched. Try again.");
    expect(coverage).not.toHaveTextContent("Inbox");
    expect(coverage).not.toHaveTextContent("Provider searched");
    expect(
      await screen.findByText("Receipt matched in the message body"),
    ).toBeVisible();
    expect(
      screen.getByText(/Matched message from sender@example.com/),
    ).toBeVisible();

    const scroller = document.querySelector(".mail-scroll") as HTMLDivElement;
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2_000 },
      scrollTop: { configurable: true, value: 1_300 },
      clientHeight: { configurable: true, value: 500 },
    });
    fireEvent.scroll(scroller);
    await waitFor(() =>
      expect(mocks.api.nextSearchPageV2).toHaveBeenCalledWith(
        expect.objectContaining({ continuation: "next-page" }),
      ),
    );
    expect(await screen.findByText("Second page")).toBeVisible();
    expect(coverage).toHaveTextContent("Work couldn’t be searched");
  });

  it("merges live coverage only from the active search session and never regresses its revision", async () => {
    let resolveSearch: (page: import("./types").SearchPageV2) => void = () =>
      undefined;
    mocks.api.startSearchV2.mockImplementation(
      () =>
        new Promise<import("./types").SearchPageV2>((resolve) => {
          resolveSearch = resolve;
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "receipt" } });
    fireEvent.keyDown(search, { key: "Enter" });

    await waitFor(() => expect(mocks.api.startSearchV2).toHaveBeenCalled());
    await waitFor(() => expect(mocks.searchProgressHandlers).toHaveLength(1));
    const request = vi.mocked(mocks.api.startSearchV2).mock.calls[0][0];
    const sessionId = request.client_request_id!;

    await act(async () => {
      mocks.searchProgressHandlers[0]({
        sessionId,
        revision: 4,
        coverage: [
          {
            account_id: "account-1",
            mailbox: "INBOX",
            state: "offline",
          },
          {
            account_id: "account-1",
            mailbox: "INBOX",
            state: "local_catalogue",
          },
          {
            account_id: "account-1",
            mailbox: "INBOX",
            state: "local_body_index",
          },
        ],
      });
    });
    const coverage = await screen.findByRole("status", {
      name: "Search status",
    });
    expect(coverage).toHaveTextContent(
      "Inbox couldn’t be searched. Check your connection and try again.",
    );

    await act(async () => {
      mocks.searchProgressHandlers[0]({
        sessionId,
        revision: 5,
        coverage: [
          {
            account_id: "account-1",
            mailbox: "INBOX",
            state: "provider_searched",
          },
        ],
      });
    });
    await waitFor(() =>
      expect(
        screen.queryByRole("status", { name: "Search status" }),
      ).not.toBeInTheDocument(),
    );

    await act(async () => {
      mocks.searchProgressHandlers[0]({
        sessionId: "older-session",
        revision: 99,
        coverage: [
          {
            account_id: "account-1",
            mailbox: "INBOX",
            state: "provider_searched",
          },
        ],
      });
      mocks.searchProgressHandlers[0]({
        sessionId,
        revision: 3,
        coverage: [
          {
            account_id: "account-1",
            mailbox: "Archive",
            state: "provider_searched",
          },
        ],
      });
    });
    expect(
      screen.queryByRole("status", { name: "Search status" }),
    ).not.toBeInTheDocument();

    await act(async () => {
      resolveSearch({
        conversations: groupMessages([mocks.message]),
        match_evidence: {},
        coverage: [],
        continuation: null,
        session_id: sessionId,
        revision: 4,
      });
    });
    expect(
      screen.queryByRole("status", { name: "Search status" }),
    ).not.toBeInTheDocument();
  });

  it("sends the exact V2 request contract with a reserved client request ID", async () => {
    mocks.api.startSearchV2.mockImplementation(async (request) => ({
      conversations: groupMessages([mocks.message]),
      match_evidence: {},
      coverage: [],
      continuation: null,
      session_id: request.client_request_id!,
      revision: 1,
    }));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "receipt" } });
    fireEvent.keyDown(search, { key: "Enter" });

    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledTimes(1),
    );
    expect(mocks.api.startSearchV2.mock.calls[0]?.[0]).toEqual({
      client_request_id: expect.any(String),
      raw_query: "receipt",
      account_ids: ["account-1"],
      scope: { mailbox: "INBOX" },
      execution_mode: "hybrid",
      page_size: 500,
    });
    expect(mocks.api.startSearchV2.mock.calls[0]?.[0]).not.toHaveProperty(
      "session_id",
    );
  });

  it("lets explicit folder predicates replace the selected mailbox in local and V2 search", async () => {
    mocks.api.startSearchV2.mockImplementation(async (request) => ({
      conversations: groupMessages([mocks.message]),
      match_evidence: {},
      coverage: [],
      continuation: null,
      session_id: request.client_request_id!,
      revision: 1,
    }));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("combobox", { name: "Search mail" });

    const expectExplicitFolderSearch = async (rawQuery: string) => {
      mocks.api.search.mockClear();
      mocks.api.startSearchV2.mockClear();
      fireEvent.change(search, { target: { value: rawQuery } });
      fireEvent.keyDown(search, { key: "Enter" });
      await waitFor(() =>
        expect(mocks.api.search).toHaveBeenLastCalledWith(
          rawQuery,
          ["account-1"],
          undefined,
          false,
          false,
          100,
          null,
        ),
      );
      await waitFor(() =>
        expect(mocks.api.startSearchV2).toHaveBeenLastCalledWith(
          expect.objectContaining({
            raw_query: rawQuery,
            scope: { mailbox: null },
          }),
        ),
      );
    };

    await expectExplicitFolderSearch("in:Sent");

    const nav = screen.getByRole("navigation", { name: "Dakia" });
    fireEvent.click(within(nav).getByRole("button", { name: "Archive" }));
    await expectExplicitFolderSearch('in:"Projects/Client work"');
    await expectExplicitFolderSearch("in:*");
    await expectExplicitFolderSearch("in:Spam OR in:Trash");
  });

  it("cancels a reserved client request before its V2 start promise resolves", async () => {
    let resolveStart:
      ((page: import("./types").SearchPageV2) => void) | undefined;
    mocks.api.startSearchV2.mockImplementationOnce(
      () =>
        new Promise<import("./types").SearchPageV2>((resolve) => {
          resolveStart = resolve;
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "pending" } });
    fireEvent.keyDown(search, { key: "Enter" });
    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledTimes(1),
    );
    const request = mocks.api.startSearchV2.mock.calls[0]?.[0]!;

    fireEvent.change(search, { target: { value: "edited" } });
    await waitFor(() =>
      expect(mocks.api.cancelSearchV2).toHaveBeenCalledWith(
        request.client_request_id,
      ),
    );
    fireEvent.change(search, { target: { value: "" } });
    expect(mocks.api.cancelSearchV2).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolveStart?.({
        conversations: groupMessages([mocks.message]),
        match_evidence: {},
        coverage: [],
        continuation: null,
        session_id: request.client_request_id!,
        revision: 1,
      });
    });
    expect(
      screen.queryByText("Receipt matched in the message body"),
    ).toBeNull();
  });

  it("auto-drains a bounded submitted search when its first page has no matches", async () => {
    const laterMatch: MailSummary = {
      ...mocks.message,
      id: "bounded-match",
      thread_id: "bounded-thread",
      subject: "Match beyond the first candidate page",
    };
    mocks.api.search.mockResolvedValue({ conversations: [], nextCursor: null });
    mocks.api.startSearchV2.mockImplementation(async (request) => ({
      conversations: [],
      match_evidence: {},
      coverage: [],
      continuation: "bounded-next",
      session_id: request.client_request_id!,
      revision: 1,
    }));
    mocks.api.nextSearchPageV2.mockImplementation(async (request) => ({
      conversations: groupMessages([laterMatch]),
      match_evidence: {},
      coverage: [],
      continuation: null,
      session_id: request.client_request_id!,
      revision: 2,
    }));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "bounded" } });
    fireEvent.keyDown(search, { key: "Enter" });

    expect(
      await screen.findByText("Match beyond the first candidate page"),
    ).toBeVisible();
    expect(mocks.api.nextSearchPageV2).toHaveBeenCalledWith(
      expect.objectContaining({ continuation: "bounded-next" }),
    );
  });

  it("uses bounded V2 pages for an explicit local-only search", async () => {
    localStorage.setItem("dakia.search.local-only", "true");
    mocks.api.startSearchV2.mockImplementation(async (request) => ({
      conversations: [],
      match_evidence: {},
      coverage: [],
      continuation: null,
      session_id: request.client_request_id!,
      revision: 1,
    }));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "local only" } });
    fireEvent.keyDown(search, { key: "Enter" });

    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledWith(
        expect.objectContaining({ execution_mode: "local" }),
      ),
    );
    expect(mocks.api.searchRemote).not.toHaveBeenCalled();
  });

  it("cancels an in-flight automatic continuation when the draft changes", async () => {
    const staleMatch: MailSummary = {
      ...mocks.message,
      id: "stale-bounded-match",
      thread_id: "stale-bounded-thread",
      subject: "Stale bounded match",
    };
    let resolveNext:
      ((page: import("./types").SearchPageV2) => void) | undefined;
    mocks.api.search.mockResolvedValue({ conversations: [], nextCursor: null });
    mocks.api.startSearchV2.mockImplementation(async (request) => ({
      conversations: [],
      match_evidence: {},
      coverage: [],
      continuation: "bounded-next",
      session_id: request.client_request_id!,
      revision: 1,
    }));
    mocks.api.nextSearchPageV2.mockImplementation(
      () =>
        new Promise<import("./types").SearchPageV2>((resolve) => {
          resolveNext = resolve;
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "bounded" } });
    fireEvent.keyDown(search, { key: "Enter" });
    await waitFor(() => expect(mocks.api.nextSearchPageV2).toHaveBeenCalled());

    fireEvent.change(search, { target: { value: "new draft" } });
    await act(async () => {
      resolveNext?.({
        conversations: groupMessages([staleMatch]),
        match_evidence: {},
        coverage: [],
        continuation: null,
        session_id: mocks.api.startSearchV2.mock.calls[0][0].client_request_id!,
        revision: 2,
      });
    });

    expect(screen.queryByText("Stale bounded match")).not.toBeInTheDocument();
  });

  it("does not revive a cancelled V2 request with the legacy remote search", async () => {
    let rejectStart: ((error: Error) => void) | undefined;
    mocks.api.startSearchV2.mockImplementationOnce(
      () =>
        new Promise<import("./types").SearchPageV2>((_resolve, reject) => {
          rejectStart = reject;
        }),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "cancelled" } });
    fireEvent.keyDown(search, { key: "Enter" });
    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledWith(
        expect.objectContaining({ raw_query: "cancelled" }),
      ),
    );
    fireEvent.change(search, { target: { value: "new draft" } });
    await act(async () => rejectStart?.(new Error("cancelled")));

    expect(mocks.api.searchRemote).not.toHaveBeenCalled();
    expect(mocks.showNativeMessage).not.toHaveBeenCalled();
  });

  it("replaces a local conversation with V2 evidence and opens its primary message", async () => {
    const providerMatch: MailSummary = {
      ...mocks.message,
      id: "provider-match",
      subject: "Provider match",
      received_at: "2026-07-20T10:00:00Z",
    };
    mocks.api.startSearchV2.mockImplementation(async (request) => ({
      conversations: groupMessages([mocks.message, providerMatch]),
      match_evidence: {
        "account-1:thread-1": {
          primary_message_id: "provider-match",
          matched_message_ids: ["provider-match"],
          match_count: 1,
          excerpt: "Fresh provider match",
        },
      },
      coverage: [],
      continuation: null,
      session_id: request.client_request_id!,
      revision: 1,
    }));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "provider" } });
    fireEvent.keyDown(search, { key: "Enter" });

    const match = await screen.findByText("Fresh provider match");
    expect(screen.getByText("Provider match")).toBeVisible();
    fireEvent.click(match.closest("button")!);
    await waitFor(() =>
      expect(mocks.api.content).toHaveBeenCalledWith("provider-match"),
    );
    expect(
      await screen.findByRole("heading", { name: "Provider match" }),
    ).toBeVisible();
  });

  it("does not record a rejected search in recent history", async () => {
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("combobox", { name: "Search mail" });
    mocks.api.search.mockImplementation(async (rawQuery: string) => {
      if (rawQuery === 'subject:"') throw new Error("Malformed search");
      return {
        conversations: groupMessages([mocks.message]),
        nextCursor: null,
      };
    });

    fireEvent.change(search, { target: { value: 'subject:"' } });
    fireEvent.keyDown(search, { key: "Enter" });
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        'subject:"',
        ["account-1"],
        "INBOX",
        false,
        false,
        100,
        null,
      ),
    );
    fireEvent.focus(search);
    expect(screen.queryByText('subject:"')).not.toBeInTheDocument();
  });

  it("restores valid recent and saved search arrays after reload", async () => {
    localStorage.setItem(
      "dakia.search.recent",
      JSON.stringify(["subject:invoice"]),
    );
    localStorage.setItem(
      "dakia.search.saved",
      JSON.stringify([
        {
          id: "saved-invoice",
          name: "Invoices",
          raw_query: "subject:invoice",
          account_ids: ["account-1"],
          local_only: false,
          created_at: "2026-09-06T00:00:00Z",
        },
      ]),
    );
    const view = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.focus(search);
    expect(await screen.findByText("subject:invoice")).toBeVisible();
    expect(screen.getByRole("button", { name: "Invoices" })).toBeVisible();

    view.unmount();
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    expect(
      await screen.findByRole("button", { name: "Invoices" }),
    ).toBeVisible();
  });

  it("rejects malformed or wrong-shape persisted search arrays", async () => {
    localStorage.setItem(
      "dakia.search.recent",
      JSON.stringify({ 0: "subject:invoice" }),
    );
    localStorage.setItem(
      "dakia.search.saved",
      JSON.stringify([{ name: "Missing fields" }]),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.focus(search);
    expect(screen.queryByText("subject:invoice")).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Missing fields" }),
    ).not.toBeInTheDocument();
  });

  it("keeps a multi-account saved search active after one account is removed", async () => {
    const work: Account = {
      ...mocks.account,
      id: "account-2",
      email: "work@example.com",
      account_name: "Work",
    };
    const saved = {
      id: "saved-both",
      name: "Both accounts",
      raw_query: "subject:invoice",
      account_ids: ["account-1", "account-2"],
      local_only: true,
      created_at: "2026-09-06T00:00:00Z",
    };
    localStorage.setItem("dakia.search.saved", JSON.stringify([saved]));
    mocks.api.accounts.mockResolvedValue([mocks.account, work]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.click(
      await screen.findByRole("button", { name: "Both accounts" }),
    );
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "subject:invoice",
        ["account-1", "account-2"],
        "INBOX",
        false,
        false,
        100,
        null,
      ),
    );

    await act(async () => {
      mocks.accountRemovedHandlers[0]({ accountId: "account-1" });
    });
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "subject:invoice",
        ["account-2"],
        "INBOX",
        false,
        false,
        100,
        null,
      ),
    );
    expect(JSON.parse(localStorage.getItem("dakia.search.saved")!)).toEqual([
      saved,
    ]);
  });

  it("preserves an active saved search's exact account scope when saving it again", async () => {
    const work: Account = {
      ...mocks.account,
      id: "account-2",
      email: "work@example.com",
      account_name: "Work",
    };
    const third: Account = {
      ...mocks.account,
      id: "account-3",
      email: "third@example.com",
      account_name: "Third",
    };
    const saved = {
      id: "saved-both",
      name: "Both accounts",
      raw_query: "subject:invoice",
      account_ids: ["account-1", "account-2"],
      local_only: true,
      created_at: "2026-09-06T00:00:00Z",
    };
    localStorage.setItem("dakia.search.saved", JSON.stringify([saved]));
    mocks.api.accounts.mockResolvedValue([mocks.account, work, third]);
    const prompt = vi.spyOn(window, "prompt").mockReturnValue("Resaved");
    try {
      render(
        <MantineProvider>
          <App />
        </MantineProvider>,
      );
      fireEvent.click(
        await screen.findByRole("button", { name: "Both accounts" }),
      );
      const search = screen.getByRole("combobox", { name: "Search mail" });
      fireEvent.focus(search);
      fireEvent.click(
        await screen.findByRole("button", { name: "Save search" }),
      );

      await waitFor(() => expect(prompt).toHaveBeenCalledOnce());
      const searches = JSON.parse(localStorage.getItem("dakia.search.saved")!);
      expect(searches.at(-1)).toEqual(
        expect.objectContaining({
          name: "Resaved",
          account_ids: ["account-1", "account-2"],
        }),
      );
    } finally {
      prompt.mockRestore();
    }
  });

  it("keeps a single-account saved search unavailable when its account is removed", async () => {
    const work: Account = {
      ...mocks.account,
      id: "account-2",
      email: "work@example.com",
      account_name: "Work",
    };
    localStorage.setItem(
      "dakia.search.saved",
      JSON.stringify([
        {
          id: "saved-personal",
          name: "Personal invoices",
          raw_query: "subject:invoice",
          account_ids: ["account-1"],
          local_only: true,
          created_at: "2026-09-06T00:00:00Z",
        },
      ]),
    );
    mocks.api.accounts.mockResolvedValue([mocks.account, work]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.click(
      await screen.findByRole("button", { name: "Personal invoices" }),
    );
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "subject:invoice",
        ["account-1"],
        "INBOX",
        false,
        false,
        100,
        null,
      ),
    );
    mocks.api.search.mockClear();
    await act(async () => {
      mocks.accountRemovedHandlers[0]({ accountId: "account-1" });
    });
    await waitFor(() =>
      expect(
        screen.getByText(
          "This saved search needs an enabled account that is not available.",
        ),
      ).toBeVisible(),
    );
    expect(mocks.api.search).not.toHaveBeenCalled();
    expect(JSON.parse(localStorage.getItem("dakia.search.saved")!)).toEqual([
      {
        id: "saved-personal",
        name: "Personal invoices",
        raw_query: "subject:invoice",
        account_ids: ["account-1"],
        local_only: true,
        created_at: "2026-09-06T00:00:00Z",
      },
    ]);
  });

  it("keeps a saved search scoped to active accounts and restores it after re-enabling an account", async () => {
    const disabled: Account = {
      ...mocks.account,
      id: "account-2",
      email: "work@example.com",
      account_name: "Work",
      enabled: false,
    };
    const saved = {
      id: "saved-both",
      name: "Both accounts",
      raw_query: "subject:invoice",
      account_ids: ["account-1", "account-2"],
      local_only: true,
      created_at: "2026-09-06T00:00:00Z",
    };
    localStorage.setItem("dakia.search.saved", JSON.stringify([saved]));
    mocks.api.accounts.mockResolvedValue([mocks.account, disabled]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    fireEvent.click(
      await screen.findByRole("button", { name: "Both accounts" }),
    );
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "subject:invoice",
        ["account-1"],
        "INBOX",
        false,
        false,
        100,
        null,
      ),
    );

    await waitFor(() =>
      expect(mocks.accountUpdatedHandlers.length).toBeGreaterThan(0),
    );
    mocks.api.search.mockClear();
    await act(async () => {
      mocks.accountUpdatedHandlers.at(-1)!({ ...disabled, enabled: true });
    });

    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "subject:invoice",
        ["account-1", "account-2"],
        "INBOX",
        false,
        false,
        100,
        null,
      ),
    );
    expect(JSON.parse(localStorage.getItem("dakia.search.saved")!)).toEqual([
      saved,
    ]);
  });

  it("does not widen a saved search when its only account is disabled", async () => {
    const work: Account = {
      ...mocks.account,
      id: "account-2",
      email: "work@example.com",
      account_name: "Work",
    };
    localStorage.setItem(
      "dakia.search.saved",
      JSON.stringify([
        {
          id: "saved-personal",
          name: "Personal invoices",
          raw_query: "subject:invoice",
          account_ids: ["account-1"],
          local_only: true,
          created_at: "2026-09-06T00:00:00Z",
        },
      ]),
    );
    mocks.api.accounts.mockResolvedValue([mocks.account, work]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    await screen.findByText("Unread thread");
    mocks.api.search.mockClear();
    fireEvent.click(
      await screen.findByRole("button", { name: "Personal invoices" }),
    );
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        "subject:invoice",
        ["account-1"],
        "INBOX",
        false,
        false,
        100,
        null,
      ),
    );
    await waitFor(() =>
      expect(mocks.accountUpdatedHandlers.length).toBeGreaterThan(0),
    );
    mocks.api.search.mockClear();
    await act(async () => {
      mocks.accountUpdatedHandlers.at(-1)!({
        ...mocks.account,
        enabled: false,
      });
    });

    await waitFor(() =>
      expect(
        screen.getByText(
          "This saved search needs an enabled account that is not available.",
        ),
      ).toBeVisible(),
    );
    expect(mocks.api.search).not.toHaveBeenCalled();
  });

  it("does not widen a disabled account selection to other accounts", async () => {
    const disabled: Account = {
      ...mocks.account,
      id: "account-2",
      email: "work@example.com",
      account_name: "Work",
      enabled: false,
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, disabled]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );

    const disabledAccount = await screen.findByRole("button", {
      name: "Work",
    });
    expect(disabledAccount).toBeDisabled();
    expect(disabledAccount).toHaveAttribute(
      "title",
      "This account is disabled or no longer available. Enable it before searching.",
    );
    await screen.findByText("Unread thread");
    const callsBeforeClick = mocks.api.search.mock.calls.length;
    fireEvent.click(disabledAccount);
    await act(async () => undefined);
    expect(mocks.api.search).toHaveBeenCalledTimes(callsBeforeClick);
  });

  it("keeps disabled accounts out of preview search and people ranking", async () => {
    const disabled: Account = {
      ...mocks.account,
      id: "account-disabled",
      email: "disabled@example.com",
      account_name: "Disabled",
      enabled: false,
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, disabled]);
    mocks.api.suggestContactedPeople.mockResolvedValue([
      { address: "alex@example.com", display_name: "Alex" },
    ]);
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const nav = await screen.findByRole("navigation", { name: "Dakia" });
    expect(
      await within(nav).findByRole("button", { name: "Disabled" }),
    ).toBeDisabled();
    const search = screen.getByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "from:al" } });
    fireEvent.focus(search);
    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledWith(
        expect.objectContaining({
          raw_query: "from:al",
          account_ids: ["account-1"],
          execution_mode: "local",
        }),
      ),
    );
    await waitFor(() =>
      expect(mocks.api.suggestContactedPeople).toHaveBeenLastCalledWith(
        "al",
        undefined,
      ),
    );
  });

  it("does not let a stale persisted saved search send a missing account ID", async () => {
    localStorage.setItem(
      "dakia.search.saved",
      JSON.stringify([
        {
          id: "saved-stale",
          name: "Old account search",
          raw_query: "subject:invoice",
          account_ids: ["deleted-account"],
          local_only: true,
          created_at: "2026-09-06T00:00:00Z",
        },
      ]),
    );
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const staleSearch = await screen.findByRole("button", {
      name: "Old account search",
    });
    expect(staleSearch).toBeDisabled();
    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenLastCalledWith(
        "",
        ["account-1"],
        "INBOX",
        false,
        false,
        100,
        null,
      ),
    );
    expect(
      mocks.api.search.mock.calls.some(([_, accountIds]) =>
        accountIds.includes("deleted-account"),
      ),
    ).toBe(false);
  });

  it("cancels an active v2 session when the draft is edited, cleared, or submitted again", async () => {
    mocks.api.startSearchV2.mockImplementation(async (request) => ({
      conversations: groupMessages([mocks.message]),
      match_evidence: {},
      coverage: [],
      continuation: null,
      session_id: request.client_request_id!,
      revision: 1,
    }));
    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("combobox", { name: "Search mail" });

    fireEvent.change(search, { target: { value: "first" } });
    fireEvent.keyDown(search, { key: "Enter" });
    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledWith(
        expect.objectContaining({ raw_query: "first" }),
      ),
    );
    const firstRequest = mocks.api.startSearchV2.mock.calls.find(
      ([request]) =>
        request.raw_query === "first" && request.execution_mode === "hybrid",
    )?.[0]!;
    fireEvent.change(search, { target: { value: "edited" } });
    await waitFor(() =>
      expect(mocks.api.cancelSearchV2).toHaveBeenCalledWith(
        firstRequest.client_request_id,
      ),
    );

    fireEvent.keyDown(search, { key: "Enter" });
    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledWith(
        expect.objectContaining({ raw_query: "edited" }),
      ),
    );
    const editedRequest = mocks.api.startSearchV2.mock.calls.find(
      ([request]) =>
        request.raw_query === "edited" && request.execution_mode === "hybrid",
    )?.[0]!;
    fireEvent.change(search, { target: { value: "" } });
    await waitFor(() =>
      expect(mocks.api.cancelSearchV2).toHaveBeenCalledTimes(3),
    );
    expect(mocks.api.cancelSearchV2).toHaveBeenCalledWith(
      editedRequest.client_request_id,
    );
    fireEvent.change(search, { target: { value: "again" } });
    fireEvent.keyDown(search, { key: "Enter" });
    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledWith(
        expect.objectContaining({ raw_query: "again" }),
      ),
    );
    const againRequest = mocks.api.startSearchV2.mock.calls.find(
      ([request]) => request.raw_query === "again",
    )?.[0]!;
    fireEvent.keyDown(search, { key: "Enter" });
    await waitFor(() =>
      expect(mocks.api.cancelSearchV2).toHaveBeenCalledTimes(4),
    );
    expect(mocks.api.cancelSearchV2).toHaveBeenCalledWith(
      againRequest.client_request_id,
    );
  });

  it("cancels an active v2 session when the app unmounts", async () => {
    mocks.api.startSearchV2.mockImplementation(async (request) => ({
      conversations: groupMessages([mocks.message]),
      match_evidence: {},
      coverage: [],
      continuation: null,
      session_id: request.client_request_id!,
      revision: 1,
    }));
    const view = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    const search = await screen.findByRole("combobox", { name: "Search mail" });
    fireEvent.change(search, { target: { value: "unmount" } });
    fireEvent.keyDown(search, { key: "Enter" });
    await waitFor(() =>
      expect(mocks.api.startSearchV2).toHaveBeenCalledWith(
        expect.objectContaining({ raw_query: "unmount" }),
      ),
    );
    const request = mocks.api.startSearchV2.mock.calls.find(
      ([item]) => item.raw_query === "unmount",
    )?.[0]!;

    view.unmount();
    await waitFor(() =>
      expect(mocks.api.cancelSearchV2).toHaveBeenCalledWith(
        request.client_request_id,
      ),
    );
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
      (mocks.api.action.mock.calls as unknown[][]).map((call) => call[2]),
    ).toEqual([1, 2, 3]);
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
      expect(mocks.api.content).toHaveBeenCalledWith("sent-b"),
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
          }),
        ),
      );
      expect(mocks.api.content).toHaveBeenCalledWith("sent-b");
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
      }),
    );
    expect(mocks.api.content).toHaveBeenCalledWith("sent-b");
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
