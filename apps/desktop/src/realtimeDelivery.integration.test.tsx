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
import type {
  Account,
  MailCursor,
  MailSummary,
  MailThread,
  MailThreadPage,
  SmartInboxPage,
} from "./types";

type TauriEvent = { payload: unknown };
type TauriListener = (event: TauriEvent) => void;
type SearchArgs = [
  query: string,
  accountIds: string[],
  mailbox: string | undefined,
  unread: boolean,
  flagged: boolean,
  pageSize: number,
  cursor: MailCursor | null,
];

const realtime = vi.hoisted(() => {
  const pendingListeners: Array<{
    event: string;
    handler: TauriListener;
    resolve: (unlisten: () => void) => void;
  }> = [];
  const listeners = new Map<string, TauriListener[]>();
  const startResolvers: Array<() => void> = [];
  return { listeners, pendingListeners, startResolvers };
});

const mocks = vi.hoisted(() => {
  const account = {
    id: "account-1",
    email: "me@example.test",
    account_name: "Primary inbox",
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
  const secondAccount = {
    ...account,
    id: "account-2",
    email: "second@example.test",
    account_name: "Second inbox",
    auth: { type: "password" as const, username: "second@example.test" },
  } satisfies Account;
  const message = {
    id: "message-1",
    account_id: account.id,
    mailbox: "INBOX",
    uid: 1,
    mailbox_uid_validity: 77,
    thread_id: "thread-1",
    subject: "Recovered startup message",
    from_address: "sender@example.test",
    to_addresses: account.email,
    received_at: "2026-10-20T21:02:00Z",
    snippet: "Persisted before the frontend listener was ready",
    body_text: "",
    is_read: false,
    is_flagged: false,
    has_attachments: false,
  } satisfies MailSummary;
  const unlisten = () => undefined;
  return {
    account,
    secondAccount,
    message,
    api: {
      action: vi.fn(async () => undefined),
      aiAvailable: vi.fn(async () => false),
      accounts: vi.fn(async (): Promise<Account[]> => [account]),
      classifyPending: vi.fn(async () => 0),
      configureTray: vi.fn(async () => undefined),
      content: vi.fn(async (_id: string, _expected?: number | null) => ({
        body_text: "",
        attachments: [],
      })),
      mailRebuildStatus: vi.fn(async () => []),
      recordNotificationDelivered: vi.fn(async () => undefined),
      search: vi.fn(async (..._args: SearchArgs): Promise<MailThreadPage> => ({
        conversations: [],
        nextCursor: null,
      })),
      smartInbox: vi.fn(async (): Promise<SmartInboxPage> => ({
        sections: [],
      })),
      searchRemote: vi.fn(async () => ({
        messages: [],
        limitReached: false,
      })),
      setRead: vi.fn(async () => undefined),
      setStarred: vi.fn(async () => undefined),
      starredCount: vi.fn(async () => 0),
      startRealtimeSync: vi.fn(
        () =>
          new Promise<void>((resolve) => {
            realtime.startResolvers.push(resolve);
          }),
      ),
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
  listen: vi.fn(
    (event: string, handler: TauriListener) =>
      new Promise<() => void>((resolve) => {
        realtime.pendingListeners.push({ event, handler, resolve });
      }),
  ),
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

function page(messages: MailSummary[], nextCursor: MailCursor | null = null) {
  return { conversations: groupMessages(messages), nextCursor };
}

function releaseRealtimeListeners() {
  for (const pending of realtime.pendingListeners.splice(0)) {
    const handlers = realtime.listeners.get(pending.event) ?? [];
    handlers.push(pending.handler);
    realtime.listeners.set(pending.event, handlers);
    pending.resolve(() => {
      const current = realtime.listeners.get(pending.event) ?? [];
      realtime.listeners.set(
        pending.event,
        current.filter((candidate) => candidate !== pending.handler),
      );
    });
  }
}

async function waitForRealtimeStart() {
  await waitFor(() => expect(mocks.api.startRealtimeSync).toHaveBeenCalled());
  expect(realtime.startResolvers).toHaveLength(1);
}

async function finishRealtimeStart() {
  await act(async () => {
    realtime.startResolvers.shift()!();
    await Promise.resolve();
  });
}

function renderApp() {
  return render(
    <MantineProvider>
      <App />
    </MantineProvider>,
  );
}

describe("realtime startup catalogue reconciliation", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    realtime.listeners.clear();
    realtime.pendingListeners.length = 0;
    realtime.startResolvers.length = 0;
    localStorage.clear();
    localStorage.setItem("dakia.analytics", '{"consent":"disabled"}');
    localStorage.setItem("dakia.mail-list-view", "list");
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    mocks.api.accounts.mockResolvedValue([mocks.account]);
    mocks.api.search.mockResolvedValue(page([]));
  });

  it("reads mail persisted before listener registration without waiting for another event", async () => {
    let resolveInitialSearch: (page: MailThreadPage) => void = () => undefined;
    let cataloguePersisted = false;
    mocks.api.classifyPending.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    mocks.api.search.mockImplementation(async () => {
      if (mocks.api.search.mock.calls.length === 1) {
        return new Promise<MailThreadPage>((resolve) => {
          resolveInitialSearch = resolve;
        });
      }
      return cataloguePersisted ? page([mocks.message]) : page([]);
    });
    renderApp();

    await waitFor(() => expect(mocks.api.search).toHaveBeenCalledOnce());
    expect(screen.queryByText(mocks.message.subject)).not.toBeInTheDocument();

    cataloguePersisted = true;
    releaseRealtimeListeners();
    await waitForRealtimeStart();
    await finishRealtimeStart();

    expect(await screen.findByText(mocks.message.subject)).toBeVisible();
    await act(async () => {
      resolveInitialSearch(page([]));
      await Promise.resolve();
    });
    expect(screen.getByText(mocks.message.subject)).toBeVisible();
    expect(mocks.api.search).toHaveBeenCalledTimes(2);
  });

  it("reconciles a missed reply and changed metadata inside an existing thread", async () => {
    const initial = {
      ...mocks.message,
      subject: "Initial thread subject",
      snippet: "Initial preview",
      is_read: true,
    };
    const refreshedInitial = {
      ...initial,
      subject: "Provider-corrected subject",
      snippet: "Provider-corrected preview",
      is_flagged: true,
    };
    const reply = {
      ...mocks.message,
      id: "message-2",
      uid: 2,
      subject: "Re: Provider-corrected subject",
      snippet: "Reply persisted before the listener was ready",
      received_at: "2026-10-20T21:04:00Z",
      is_read: false,
    };
    let cataloguePersisted = false;
    mocks.api.search.mockImplementation(async () =>
      cataloguePersisted ? page([refreshedInitial, reply]) : page([initial]),
    );
    mocks.api.content.mockImplementation(async (id: string) => ({
      body_text: id === reply.id ? "Recovered reply body" : "Initial body",
      attachments: [],
    }));
    renderApp();

    expect(await screen.findByText(initial.subject)).toBeVisible();
    cataloguePersisted = true;
    releaseRealtimeListeners();
    await waitForRealtimeStart();
    await finishRealtimeStart();

    const latestSubject = await screen.findByText(reply.subject);
    const row = latestSubject.closest("button")!;
    expect(row).toHaveAttribute("data-unread", "true");
    expect(
      screen.getByLabelText("2 messages in this conversation"),
    ).toBeVisible();
    expect(screen.getByText(reply.snippet)).toBeVisible();
    expect(screen.queryByText(initial.subject)).not.toBeInTheDocument();
    fireEvent.click(row);
    expect(await screen.findByText("Recovered reply body")).toBeVisible();
    expect(mocks.api.content).toHaveBeenCalledWith(reply.id, 77);
  });

  it("keeps pending read and star intent while adding a missed reply", async () => {
    const initial = {
      ...mocks.message,
      subject: "Pending action thread",
      snippet: "Unread before opening",
      is_read: false,
      is_flagged: false,
    };
    const reply = {
      ...initial,
      id: "pending-reply",
      uid: 2,
      subject: "Re: Pending action thread",
      snippet: "Reply committed during startup",
      received_at: "2026-10-20T21:05:00Z",
      is_read: true,
    };
    let cataloguePersisted = false;
    mocks.api.search.mockImplementation(async () =>
      cataloguePersisted ? page([initial, reply]) : page([initial]),
    );
    mocks.api.content.mockResolvedValue({
      body_text: "Pending action body",
      attachments: [],
    });
    let finishRead: (() => void) | undefined;
    mocks.api.setRead.mockImplementationOnce(
      () =>
        new Promise<undefined>((resolve) => {
          finishRead = () => resolve(undefined);
        }),
    );
    mocks.api.setStarred.mockImplementationOnce(
      () => new Promise(() => undefined),
    );
    renderApp();

    const initialRow = (await screen.findByText(initial.subject)).closest(
      "button",
    )!;
    releaseRealtimeListeners();
    await waitForRealtimeStart();
    fireEvent.click(initialRow);
    await waitFor(() => expect(mocks.api.setRead).toHaveBeenCalledOnce());
    fireEvent.click(initialRow.querySelector(".mail-star")!);
    await waitFor(() => expect(mocks.api.setStarred).toHaveBeenCalledOnce());
    expect(initialRow).toHaveAttribute("data-unread", "false");
    expect(initialRow.querySelector(".mail-star")).toHaveAttribute(
      "data-active",
      "true",
    );

    cataloguePersisted = true;
    await finishRealtimeStart();

    const reconciledRow = (await screen.findByText(reply.subject)).closest(
      "button",
    )!;
    expect(reconciledRow).toHaveAttribute("data-unread", "false");
    expect(reconciledRow.querySelector(".mail-star")).toHaveAttribute(
      "data-active",
      "true",
    );
    expect(
      screen.getByLabelText("2 messages in this conversation"),
    ).toBeVisible();
  });

  it("does not replace a newer query after a delayed realtime start", async () => {
    const queryMessage = {
      ...mocks.message,
      id: "query-message",
      thread_id: "query-thread",
      subject: "Current query result",
    };
    mocks.api.search.mockImplementation(async (...args: SearchArgs) =>
      args[0] === "current query" ? page([queryMessage]) : page([]),
    );
    renderApp();

    await waitFor(() => expect(mocks.api.search).toHaveBeenCalledTimes(2));
    releaseRealtimeListeners();
    await waitForRealtimeStart();
    fireEvent.change(screen.getByRole("textbox", { name: "Search mail" }), {
      target: { value: "current query" },
    });
    expect(await screen.findByText(queryMessage.subject)).toBeVisible();
    const searchesBeforeStartFinished = mocks.api.search.mock.calls.length;

    await finishRealtimeStart();

    expect(screen.getByText(queryMessage.subject)).toBeVisible();
    expect(mocks.api.search).toHaveBeenCalledTimes(searchesBeforeStartFinished);
    expect(mocks.api.search).toHaveBeenLastCalledWith(
      "current query",
      [mocks.account.id],
      "INBOX",
      false,
      false,
      100,
      null,
    );
  });

  it("does not replace a newer account or mailbox after a delayed realtime start", async () => {
    const scopedMessage = {
      ...mocks.message,
      id: "second-sent-message",
      account_id: mocks.secondAccount.id,
      mailbox: "Sent",
      thread_id: "second-sent-thread",
      subject: "Second account sent message",
    };
    mocks.api.accounts.mockResolvedValue([mocks.account, mocks.secondAccount]);
    mocks.api.search.mockImplementation(async (...args: SearchArgs) => {
      const [, accountIds, mailbox] = args;
      return accountIds.length === 1 &&
        accountIds[0] === mocks.secondAccount.id &&
        mailbox === "Sent"
        ? page([scopedMessage])
        : page([]);
    });
    renderApp();

    await waitFor(() => expect(mocks.api.search).toHaveBeenCalledTimes(2));
    releaseRealtimeListeners();
    await waitForRealtimeStart();
    fireEvent.click(screen.getByRole("button", { name: "Second inbox" }));
    fireEvent.click(screen.getByRole("button", { name: "Sent" }));
    expect(await screen.findByText(scopedMessage.subject)).toBeVisible();
    const searchesBeforeStartFinished = mocks.api.search.mock.calls.length;

    await finishRealtimeStart();

    expect(screen.getByText(scopedMessage.subject)).toBeVisible();
    expect(mocks.api.search).toHaveBeenCalledTimes(searchesBeforeStartFinished);
    expect(mocks.api.search).toHaveBeenLastCalledWith(
      "",
      [mocks.secondAccount.id],
      "Sent",
      false,
      false,
      100,
      null,
    );
  });

  it("does not reset an appended page after a delayed realtime start", async () => {
    const cursor = {
      received_at: "2026-10-20T20:00:00Z",
      id: mocks.message.id,
    };
    const olderMessage = {
      ...mocks.message,
      id: "older-message",
      uid: 2,
      thread_id: "older-thread",
      subject: "Older page message",
      received_at: "2026-10-20T19:00:00Z",
    };
    const missedTopMessage = {
      ...mocks.message,
      id: "missed-top-message",
      uid: 3,
      thread_id: "missed-top-thread",
      subject: "Missed startup top message",
      received_at: "2026-10-20T21:03:00Z",
    };
    const refreshedExistingMessage = {
      ...mocks.message,
      subject: "Provider-refreshed first-page copy",
      is_flagged: true,
    };
    let cataloguePersisted = false;
    mocks.api.search.mockImplementation(
      async (...args: SearchArgs): Promise<MailThreadPage> =>
        args[6]
          ? page([olderMessage])
          : page(
              cataloguePersisted
                ? [missedTopMessage, refreshedExistingMessage]
                : [mocks.message],
              cursor,
            ),
    );
    renderApp();

    expect(await screen.findByText(mocks.message.subject)).toBeVisible();
    const scroller = document.querySelector(".mail-scroll") as HTMLDivElement;
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2_000 },
      scrollTop: { configurable: true, value: 1_300 },
      clientHeight: { configurable: true, value: 500 },
    });
    fireEvent.scroll(scroller);
    expect(await screen.findByText(olderMessage.subject)).toBeVisible();
    cataloguePersisted = true;
    releaseRealtimeListeners();
    await waitForRealtimeStart();
    const searchesBeforeStartFinished = mocks.api.search.mock.calls.length;

    await finishRealtimeStart();

    expect(await screen.findByText(missedTopMessage.subject)).toBeVisible();
    const refreshedRow = screen
      .getByText(refreshedExistingMessage.subject)
      .closest("button")!;
    expect(refreshedRow).toBeVisible();
    expect(refreshedRow.querySelector(".mail-star")).toHaveAttribute(
      "data-active",
      "true",
    );
    expect(screen.queryByText(mocks.message.subject)).not.toBeInTheDocument();
    expect(screen.getByText(olderMessage.subject)).toBeVisible();
    expect(mocks.api.search).toHaveBeenCalledTimes(
      searchesBeforeStartFinished + 1,
    );
    fireEvent.scroll(scroller);
    await act(async () => Promise.resolve());
    expect(mocks.api.search).toHaveBeenCalledTimes(
      searchesBeforeStartFinished + 1,
    );
  });

  it("merges a missed Smart first page without resetting its appended section", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    const cursor = {
      received_at: "2026-10-20T20:00:00Z",
      id: "smart-first",
    };
    const smartMessage = (
      id: string,
      subject: string,
      receivedAt: string,
    ): MailSummary => ({
      ...mocks.message,
      id,
      uid: id === "smart-first" ? 10 : id === "smart-older" ? 11 : 12,
      thread_id: id,
      subject,
      received_at: receivedAt,
      category: "people",
    });
    const first = smartMessage(
      "smart-first",
      "Smart first page message",
      "2026-10-20T20:00:00Z",
    );
    const older = smartMessage(
      "smart-older",
      "Smart appended message",
      "2026-10-20T19:00:00Z",
    );
    const missedTop = smartMessage(
      "smart-missed",
      "Smart missed startup message",
      "2026-10-20T21:03:00Z",
    );
    let cataloguePersisted = false;
    mocks.api.smartInbox.mockImplementation(async () => ({
      sections: [
        {
          id: "people" as const,
          conversations: groupMessages(
            cataloguePersisted ? [missedTop, first] : [first],
          ),
          nextCursor: cursor,
        },
      ],
    }));
    mocks.api.search.mockImplementation(async (...args: SearchArgs) =>
      args[6] === cursor ? page([older]) : page([]),
    );
    renderApp();

    expect(await screen.findByText(first.subject)).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Show more" }));
    expect(await screen.findByText(older.subject)).toBeVisible();
    cataloguePersisted = true;
    releaseRealtimeListeners();
    await waitForRealtimeStart();
    const smartLoadsBeforeStartFinished =
      mocks.api.smartInbox.mock.calls.length;

    await finishRealtimeStart();

    expect(await screen.findByText(missedTop.subject)).toBeVisible();
    expect(screen.getByText(first.subject)).toBeVisible();
    expect(screen.getByText(older.subject)).toBeVisible();
    expect(mocks.api.smartInbox).toHaveBeenCalledTimes(
      smartLoadsBeforeStartFinished + 1,
    );
    expect(
      screen.queryByRole("button", { name: "Show more" }),
    ).not.toBeInTheDocument();
  });

  it("retains an opened Other row until an authoritative Starred update supersedes it", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    const arrival = {
      ...mocks.message,
      id: "uid-4-external-star",
      uid: 4,
      thread_id: "uid-4-external-star-thread",
      subject: "Fictional external star arrival",
      snippet: "The mounted Reader must keep this message visible.",
      body_text: "The mounted Reader must keep this message visible.",
      is_read: false,
      is_flagged: false,
    };
    const other = {
      ...arrival,
      id: "other-after-external-star",
      uid: 5,
      thread_id: "other-after-external-star-thread",
      subject: "Another Other conversation",
      is_read: false,
    };
    let snapshot: "initial" | "absent" | "starred" | "starred-omitted" =
      "initial";
    mocks.api.smartInbox.mockImplementation(async () => ({
      sections:
        snapshot === "starred"
          ? [
              {
                id: "starred" as const,
                conversations: groupMessages([
                  { ...arrival, is_flagged: true },
                ]),
                nextCursor: null,
              },
              {
                id: "other" as const,
                conversations: groupMessages([other]),
                nextCursor: null,
              },
            ]
          : [
              {
                id: "other" as const,
                conversations: groupMessages(
                  snapshot === "initial" ? [arrival, other] : [other],
                ),
                nextCursor: null,
              },
            ],
    }));
    mocks.api.content.mockResolvedValue({
      body_text: arrival.body_text,
      attachments: [],
    });
    let finishRead: (() => void) | undefined;
    mocks.api.setRead.mockImplementationOnce(
      () =>
        new Promise<undefined>((resolve) => {
          finishRead = () => resolve(undefined);
        }),
    );
    renderApp();

    const initialRow = (await screen.findByText(arrival.subject)).closest(
      "button",
    )!;
    releaseRealtimeListeners();
    await waitForRealtimeStart();
    await finishRealtimeStart();

    fireEvent.click(initialRow);
    await waitFor(() =>
      expect(mocks.api.setRead).toHaveBeenCalledWith(
        arrival.id,
        true,
        arrival.mailbox_uid_validity,
      ),
    );
    expect(
      await screen.findByRole("heading", { name: arrival.subject }),
    ).toBeVisible();

    const emitMailChanged = async () => {
      await act(async () => {
        for (const handler of realtime.listeners.get("mail-changed") ?? []) {
          handler({ payload: { accountId: mocks.account.id } });
        }
        await Promise.resolve();
      });
    };
    const list = document.querySelector<HTMLElement>(".mail-list-panel")!;
    const reader = document.querySelector<HTMLElement>("#main-content.reader")!;

    snapshot = "absent";
    await emitMailChanged();
    const otherSection = within(list).getByRole("region", { name: "Other" });
    await waitFor(() =>
      expect(
        within(otherSection).getAllByText(arrival.subject, {
          selector: ".mail-subject",
        }),
      ).toHaveLength(1),
    );
    expect(
      within(reader).getByRole("button", { name: "Star conversation" }),
    ).toBeVisible();

    snapshot = "starred";
    await emitMailChanged();
    await waitFor(() =>
      expect(
        within(list).getAllByText(arrival.subject, {
          selector: ".mail-subject",
        }),
      ).toHaveLength(1),
    );
    const starred = within(list).getByRole("region", { name: "Starred" });
    expect(
      within(starred).getByText(arrival.subject, { selector: ".mail-subject" }),
    ).toBeVisible();
    expect(
      within(starred).getByRole("button", { name: "Remove star" }),
    ).toBeVisible();
    expect(
      within(otherSection).queryByText(arrival.subject, {
        selector: ".mail-subject",
      }),
    ).toBeNull();
    expect(
      within(reader).getByRole("button", { name: "Remove star" }),
    ).toBeVisible();

    await act(async () => finishRead!());
    await waitFor(() =>
      expect(
        within(otherSection).queryByText(arrival.subject, {
          selector: ".mail-subject",
        }),
      ).toBeNull(),
    );
    expect(
      within(reader).getByRole("button", { name: "Remove star" }),
    ).toBeVisible();

    snapshot = "starred-omitted";
    await emitMailChanged();
    await waitFor(() =>
      expect(
        within(list).queryByText(arrival.subject, {
          selector: ".mail-subject",
        }),
      ).toBeNull(),
    );
    expect(
      screen.getByRole("heading", { name: arrival.subject }),
    ).toBeVisible();
    expect(
      within(reader).getByRole("button", { name: "Remove star" }),
    ).toBeVisible();

    fireEvent.click(
      within(otherSection)
        .getByText(other.subject, { selector: ".mail-subject" })
        .closest("button")! as HTMLButtonElement,
    );
    expect(
      within(list).queryByText(arrival.subject, {
        selector: ".mail-subject",
      }),
    ).toBeNull();
  });

  it("does not retain a Seen row when watcher startup reconciles it into Starred", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    const external = {
      ...mocks.message,
      id: "uid-4-startup-external-star",
      uid: 4,
      thread_id: "uid-4-startup-external-star-thread",
      subject: "Fictional startup external star",
      is_read: true,
      is_flagged: false,
    };
    let snapshot: "seen" | "starred" = "seen";
    mocks.api.smartInbox.mockImplementation(async () => ({
      sections:
        snapshot === "seen"
          ? [
              {
                id: "seen" as const,
                conversations: groupMessages([external]),
                nextCursor: null,
              },
            ]
          : [
              {
                id: "starred" as const,
                conversations: groupMessages([
                  { ...external, is_flagged: true },
                ]),
                nextCursor: null,
              },
            ],
    }));
    renderApp();

    expect(await screen.findByText(external.subject)).toBeVisible();
    releaseRealtimeListeners();
    await waitForRealtimeStart();
    snapshot = "starred";
    await finishRealtimeStart();

    const list = document.querySelector<HTMLElement>(".mail-list-panel")!;
    await waitFor(() =>
      expect(
        within(list).getAllByText(external.subject, {
          selector: ".mail-subject",
        }),
      ).toHaveLength(1),
    );
    expect(within(list).getByRole("region", { name: "Starred" })).toBeVisible();
    expect(within(list).queryByRole("region", { name: "Seen" })).toBeNull();
  });

  it("moves a read Seen conversation to Starred after a production mail-changed event", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    const external = {
      ...mocks.message,
      id: "uid-4-read-external-star",
      uid: 4,
      thread_id: "uid-4-read-external-star-thread",
      subject: "Fictional read external star",
      snippet: "A second client flagged this read conversation.",
      body_text: "A second client flagged this read conversation.",
      is_read: true,
      is_flagged: false,
    };
    let snapshot: "seen" | "starred" = "seen";
    mocks.api.smartInbox.mockImplementation(async () => ({
      sections:
        snapshot === "seen"
          ? [
              {
                id: "seen" as const,
                conversations: groupMessages([external]),
                nextCursor: null,
              },
            ]
          : [
              {
                id: "starred" as const,
                conversations: groupMessages([
                  { ...external, is_flagged: true },
                ]),
                nextCursor: null,
              },
            ],
    }));
    mocks.api.content.mockResolvedValue({
      body_text: external.body_text,
      attachments: [],
    });
    renderApp();

    const row = (await screen.findByText(external.subject)).closest("button")!;
    fireEvent.click(row);
    expect(
      await screen.findByRole("heading", { name: external.subject }),
    ).toBeVisible();
    releaseRealtimeListeners();
    await waitForRealtimeStart();
    await finishRealtimeStart();

    snapshot = "starred";
    await act(async () => {
      for (const handler of realtime.listeners.get("mail-changed") ?? []) {
        handler({ payload: { accountId: mocks.account.id } });
      }
      await Promise.resolve();
    });

    const list = document.querySelector<HTMLElement>(".mail-list-panel")!;
    await waitFor(() =>
      expect(
        within(list).getAllByText(external.subject, {
          selector: ".mail-subject",
        }),
      ).toHaveLength(1),
    );
    const starred = within(list).getByRole("region", { name: "Starred" });
    expect(
      within(starred).getByText(external.subject, {
        selector: ".mail-subject",
      }),
    ).toBeVisible();
    expect(
      within(starred).getByRole("button", { name: "Remove star" }),
    ).toBeVisible();
    expect(within(list).queryByRole("region", { name: "Seen" })).toBeNull();
    expect(
      within(
        document.querySelector<HTMLElement>("#main-content.reader")!,
      ).getByRole("button", { name: "Remove star" }),
    ).toBeVisible();
  });
});
