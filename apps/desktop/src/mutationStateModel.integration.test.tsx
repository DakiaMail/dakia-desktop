import { MantineProvider } from "@mantine/core";
import { StrictMode } from "react";
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
  MailSummary,
  RemoteSearchResult,
  SmartInboxPage,
} from "./types";

type Deferred = {
  promise: Promise<undefined>;
  resolve: () => void;
  reject: (error: Error) => void;
};

function deferred(): Deferred {
  let resolve!: () => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<undefined>((accept, decline) => {
    resolve = () => accept(undefined);
    reject = decline;
  });
  return { promise, resolve, reject };
}

/**
 * A deliberately small intent-level oracle. It knows only that pending rows
 * are hidden, successful moves leave Inbox, and failed moves remain selected
 * for recovery. It does not reproduce App's React state or restoration code.
 */
class MutationOracle {
  private remoteInbox = new Set<string>();
  private pending = new Set<string>();
  private failed = new Set<string>();

  constructor(ids: string[]) {
    this.remoteInbox = new Set(ids);
  }

  begin(ids: string[]) {
    ids.forEach((id) => this.pending.add(id));
  }

  observeRemote(ids: string[]) {
    this.remoteInbox = new Set(ids);
  }

  settle(outcomes: Record<string, "succeeded" | "failed">) {
    for (const [id, outcome] of Object.entries(outcomes)) {
      this.pending.delete(id);
      if (outcome === "succeeded") this.remoteInbox.delete(id);
      else this.failed.add(id);
    }
  }

  visibleIds() {
    return [...this.remoteInbox].filter((id) => !this.pending.has(id)).sort();
  }

  selectedIds() {
    return [...this.failed].sort();
  }
}

const mocks = vi.hoisted(() => {
  const noopListener = vi.fn(async () => () => undefined);
  const mailChangedHandlers: Array<() => void> = [];
  const accountRemovedHandlers: Array<(event: { accountId: string }) => void> =
    [];
  return {
    mailChangedHandlers,
    accountRemovedHandlers,
    noopListener,
    api: {
      accounts: vi.fn(),
      action: vi.fn(),
      aiAvailable: vi.fn(async () => false),
      classifyPending: vi.fn(),
      configureTray: vi.fn(async () => undefined),
      content: vi.fn(async () => ({ body_text: "", attachments: [] })),
      mailRebuildStatus: vi.fn(async () => []),
      recordNotificationDelivered: vi.fn(async () => undefined),
      search: vi.fn(),
      searchRemote: vi.fn(async (): Promise<RemoteSearchResult> => ({
        messages: [],
        limitReached: false,
      })),
      setCategory: vi.fn(async () => undefined),
      setRead: vi.fn(async () => undefined),
      setStarred: vi.fn(async () => undefined),
      showEmailAddressContextMenu: vi.fn(async () => undefined),
      smartInbox: vi.fn(async () => ({ sections: [] }) as SmartInboxPage),
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
    showNativeMessage: vi.fn(async () => undefined),
  };
});

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    isFocused: vi.fn(async () => true),
    setFocus: vi.fn(async () => undefined),
    show: vi.fn(async () => undefined),
    startDragging: vi.fn(async () => undefined),
  }),
}));

vi.mock("./api", () => ({ api: mocks.api }));
vi.mock("./features", () => ({ AI_FEATURES_VISIBLE: false }));
vi.mock("./composeWindow", () => ({
  onComposeSent: mocks.noopListener,
  onOutboxChanged: mocks.noopListener,
  openComposeWindow: vi.fn(),
}));
vi.mock("./feedback", () => ({
  createFeedbackComposeSeed: vi.fn(async () => ({
    accountId: "account-a",
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
  showNativeMessage: mocks.showNativeMessage,
}));
vi.mock("./notifications", () => ({
  onNotificationAction: mocks.noopListener,
  readNotificationSettings: () => ({
    enabled: false,
    soundEnabled: false,
    showPreview: false,
  }),
  requestInitialNotificationAccess: vi.fn(async () => undefined),
  sendNewMailNotification: vi.fn(async () => false),
}));
vi.mock("./nativeWindows", () => ({
  onAccountConnected: mocks.noopListener,
  onAccountRemoved: vi.fn(
    async (handler: (event: { accountId: string }) => void) => {
      mocks.accountRemovedHandlers.push(handler);
      return () => undefined;
    },
  ),
  onAccountUpdated: mocks.noopListener,
  onMailArrived: mocks.noopListener,
  onMailChanged: vi.fn(async (handler: () => void) => {
    mocks.mailChangedHandlers.push(handler);
    return () => undefined;
  }),
  onMailHydrated: mocks.noopListener,
  onMailIndexRebuilt: mocks.noopListener,
  onMailRebuildFinished: mocks.noopListener,
  onMailRebuildProgress: mocks.noopListener,
  onMailSyncState: mocks.noopListener,
  onDesktopNotificationAction: mocks.noopListener,
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
vi.mock("./analytics", () => ({
  readAnalyticsSettings: () => ({ consent: "disabled" }),
  reportUsageIfConsented: vi.fn(async () => false),
  listenForAnalyticsConsent: vi.fn(async () => () => undefined),
  setAnalyticsConsent: vi.fn(),
}));

const accountA: Account = {
  id: "account-a",
  email: "a@example.test",
  account_name: "Mailbox A",
  display_name: "User A",
  provider_id: "fictional",
  auth: { type: "password", username: "a@example.test" },
  imap_host: "imap.example.test",
  imap_port: 993,
  imap_security: "tls",
  smtp_host: "smtp.example.test",
  smtp_port: 465,
  smtp_security: "tls",
  archive_mailbox: "Archive",
  spam_mailbox: "Spam",
  enabled: true,
};

const accountB: Account = {
  ...accountA,
  id: "account-b",
  email: "b@example.test",
  account_name: "Mailbox B",
  display_name: "User B",
  auth: { type: "password", username: "b@example.test" },
};

function message(
  id: string,
  uid: number,
  account: Account,
  subject = id,
): MailSummary {
  return {
    id,
    account_id: account.id,
    mailbox: "INBOX",
    uid,
    mailbox_uid_validity: account.id === accountA.id ? 1101 : 2202,
    message_id: `<${id}@example.test>`,
    thread_id: `thread-${id}`,
    subject,
    from_address: `${id}@sender.example.test`,
    to_addresses: account.email,
    received_at: `2026-10-20T10:${String(59 - uid).padStart(2, "0")}:00Z`,
    snippet: subject,
    body_text: subject,
    is_read: false,
    is_flagged: false,
    has_attachments: false,
  };
}

function visibleIds(): string[] {
  return [...document.querySelectorAll<HTMLButtonElement>(".mail-item")]
    .map((row) => row.textContent ?? "")
    .flatMap((copy) =>
      ["a", "b", "c", "d", "e"].filter((id) => copy.includes(`Subject ${id}`)),
    )
    .sort();
}

function selectSubject(subject: string) {
  const row = screen
    .getByText(subject, { selector: ".mail-subject" })
    .closest("button")!;
  fireEvent.click(within(row).getByRole("checkbox", { name: "Select" }));
}

function expectSubjectSelected(subject: string, selected: boolean) {
  const row = screen
    .getByText(subject, { selector: ".mail-subject" })
    .closest("button")!;
  const checkbox = within(row).getByRole("checkbox", { name: "Select" });
  if (selected) expect(checkbox).toBeChecked();
  else expect(checkbox).not.toBeChecked();
}

function archiveSelection() {
  fireEvent.click(
    within(document.querySelector(".mail-list-panel")!).getByRole("button", {
      name: "Archive",
    }),
  );
}

describe("mailbox mutation state model", () => {
  beforeEach(() => {
    vi.useRealTimers();
    vi.clearAllMocks();
    mocks.mailChangedHandlers.length = 0;
    mocks.accountRemovedHandlers.length = 0;
    localStorage.clear();
    sessionStorage.clear();
    localStorage.setItem("dakia.mail-list-view", "list");
    mocks.api.accounts.mockResolvedValue([accountA, accountB]);
    // Hold classification forever so it cannot schedule an unrelated reload
    // while the mutation schedule is under test.
    mocks.api.classifyPending.mockImplementation(
      () => new Promise(() => undefined),
    );
  });

  it("converges overlapping partial batches and a multi-device refresh in completion order", async () => {
    const initial = [
      message("a", 1, accountA, "Subject a original"),
      message("b", 2, accountB, "Subject b original"),
      message("c", 3, accountA, "Subject c original"),
      message("d", 4, accountB, "Subject d original"),
      message("e", 5, accountA, "Subject e original"),
    ];
    let providerSnapshot = initial;
    mocks.api.search.mockImplementation(async () => ({
      conversations: groupMessages(providerSnapshot),
      nextCursor: null,
    }));
    const actions = new Map<string, Deferred>();
    mocks.api.action.mockImplementation((...args: unknown[]) => {
      const gate = deferred();
      actions.set(args[4] as string, gate);
      return gate.promise;
    });
    const oracle = new MutationOracle(initial.map((item) => item.id));

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Subject a original", {
      selector: ".mail-subject",
    });
    await waitFor(() => expect(mocks.mailChangedHandlers).toHaveLength(1));

    selectSubject("Subject a original");
    selectSubject("Subject b original");
    oracle.begin(["a", "b"]);
    archiveSelection();
    await waitFor(() => expect(visibleIds()).toEqual(["c", "d", "e"]));

    selectSubject("Subject c original");
    selectSubject("Subject d original");
    oracle.begin(["c", "d"]);
    archiveSelection();
    await waitFor(() => expect(visibleIds()).toEqual(oracle.visibleIds()));
    expect(visibleIds()).toEqual(["e"]);

    providerSnapshot = [
      {
        ...initial[0],
        subject: "Subject a from another device",
        is_read: true,
      },
      {
        ...initial[3],
        subject: "Subject d from another device",
        is_flagged: true,
      },
      {
        ...initial[4],
        subject: "Subject e from another device",
        is_read: true,
      },
    ];
    oracle.observeRemote(["a", "d", "e"]);
    await act(async () => {
      mocks.mailChangedHandlers[0]();
      await Promise.resolve();
    });
    await screen.findByText("Subject e from another device", {
      selector: ".mail-subject",
    });
    expect(visibleIds()).toEqual(oracle.visibleIds());

    await act(async () => {
      actions.get("c")!.resolve();
      actions.get("d")!.reject(new Error("newer batch failed for d"));
      await Promise.resolve();
    });
    oracle.settle({ c: "succeeded", d: "failed" });
    await waitFor(() => expect(visibleIds()).toEqual(oracle.visibleIds()));
    const restoredD = screen
      .getByText("Subject d from another device", {
        selector: ".mail-subject",
      })
      .closest("button")!;
    expect(restoredD).toBeVisible();
    expect(
      within(restoredD).getByRole("button", { name: "Remove star" }),
    ).toHaveAttribute("data-active", "true");
    expect(screen.getByText("1 selected")).toBeVisible();

    await act(async () => {
      actions.get("a")!.reject(new Error("older batch failed for a"));
      actions.get("b")!.resolve();
      await Promise.resolve();
    });
    oracle.settle({ a: "failed", b: "succeeded" });

    const restoredA = (
      await screen.findByText("Subject a from another device", {
        selector: ".mail-subject",
      })
    ).closest("button")!;
    expect(restoredA).toHaveAttribute("data-unread", "false");
    expect(
      screen.getByText("Subject d from another device", {
        selector: ".mail-subject",
      }),
    ).toBeVisible();
    expect(
      screen.getByText("Subject e from another device", {
        selector: ".mail-subject",
      }),
    ).toBeVisible();
    expect(visibleIds()).toEqual(oracle.visibleIds());
    expect(oracle.selectedIds()).toEqual(["a", "d"]);
    expect(screen.getByText("2 selected")).toBeVisible();
    expectSubjectSelected("Subject a from another device", true);
    expectSubjectSelected("Subject d from another device", true);
    expectSubjectSelected("Subject e from another device", false);
    expect(mocks.api.action).toHaveBeenCalledTimes(4);
    expect(mocks.api.action.mock.calls).toEqual([
      ["account-a", "INBOX", 1, "archive", "a", 1101],
      ["account-b", "INBOX", 2, "archive", "b", 2202],
      ["account-a", "INBOX", 3, "archive", "c", 1101],
      ["account-b", "INBOX", 4, "archive", "d", 2202],
    ]);
  });

  it("does not reconcile after unmount during the failure restoration delay", async () => {
    const only = message("a", 1, accountA, "Subject a original");
    mocks.api.search.mockResolvedValue({
      conversations: groupMessages([only]),
      nextCursor: null,
    });
    const action = deferred();
    mocks.api.action.mockReturnValue(action.promise);

    const view = render(
      <StrictMode>
        <MantineProvider>
          <App />
        </MantineProvider>
      </StrictMode>,
    );
    await screen.findByText("Subject a original", {
      selector: ".mail-subject",
    });
    selectSubject("Subject a original");
    archiveSelection();
    await waitFor(() =>
      expect(
        screen.queryByText("Subject a original", {
          selector: ".mail-subject",
        }),
      ).not.toBeInTheDocument(),
    );

    vi.useFakeTimers();
    try {
      await act(async () => {
        action.reject(new Error("provider rejected archive"));
        await Promise.resolve();
      });
      expect(
        screen.getByText("Could not archive the conversation"),
      ).toBeVisible();
      const searchesAtCancellation = mocks.api.search.mock.calls.length;
      view.unmount();
      await act(async () => {
        await vi.advanceTimersByTimeAsync(260);
      });
      expect(mocks.api.search).toHaveBeenCalledTimes(searchesAtCancellation);
      expect(mocks.showNativeMessage).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not restore a failed target absent from the newest authoritative page", async () => {
    const target = message("a", 1, accountA, "Subject a original");
    const neighbor = message("e", 5, accountA, "Subject e original");
    let providerSnapshot = [target, neighbor];
    mocks.api.search.mockImplementation(async () => ({
      conversations: groupMessages(providerSnapshot),
      nextCursor: null,
    }));
    const action = deferred();
    mocks.api.action.mockReturnValue(action.promise);

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Subject a original", {
      selector: ".mail-subject",
    });
    await waitFor(() => expect(mocks.mailChangedHandlers).toHaveLength(1));
    selectSubject("Subject a original");
    archiveSelection();
    await waitFor(() => expect(visibleIds()).toEqual(["e"]));

    providerSnapshot = [
      { ...neighbor, subject: "Subject e authoritative", is_read: true },
    ];
    await act(async () => {
      mocks.mailChangedHandlers[0]();
      await Promise.resolve();
    });
    await screen.findByText("Subject e authoritative", {
      selector: ".mail-subject",
    });

    await act(async () => {
      action.reject(new Error("provider rejected stale archive"));
      await Promise.resolve();
    });

    await waitFor(() => expect(visibleIds()).toEqual(["e"]));
    expect(
      screen.queryByText("Subject a original", {
        selector: ".mail-subject",
      }),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("1 selected")).not.toBeInTheDocument();
  });

  it("restores a failed target when an incomplete list page cannot prove absence", async () => {
    const target = message("a", 1, accountA, "Subject a original");
    const neighbor = message("e", 5, accountA, "Subject e original");
    let page = {
      conversations: groupMessages([target, neighbor]),
      nextCursor: null as null | { received_at: string; id: string },
    };
    mocks.api.search.mockImplementation(async () => page);
    const action = deferred();
    mocks.api.action.mockReturnValue(action.promise);

    const view = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Subject a original", {
      selector: ".mail-subject",
    });
    await waitFor(() => expect(mocks.mailChangedHandlers).toHaveLength(1));
    selectSubject("Subject a original");
    archiveSelection();
    await waitFor(() => expect(visibleIds()).toEqual(["e"]));

    page = {
      conversations: groupMessages([
        { ...neighbor, subject: "Subject e first page" },
      ]),
      nextCursor: { received_at: neighbor.received_at, id: neighbor.id },
    };
    await act(async () => {
      mocks.mailChangedHandlers[0]();
      await Promise.resolve();
    });
    await screen.findByText("Subject e first page", {
      selector: ".mail-subject",
    });

    vi.useFakeTimers();
    try {
      await act(async () => {
        action.reject(new Error("provider rejected archive"));
        await Promise.resolve();
      });
      expect(
        screen.getByText("Subject a original", { selector: ".mail-subject" }),
      ).toBeVisible();
      expectSubjectSelected("Subject a original", true);
    } finally {
      view.unmount();
      vi.useRealTimers();
    }
  });

  it("uses list observations when Smart view is showing a special folder", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    const target = message("a", 1, accountA, "Subject a original");
    let page = {
      conversations: groupMessages([target]),
      nextCursor: null,
    };
    mocks.api.search.mockImplementation(async () => page);
    const action = deferred();
    mocks.api.action.mockReturnValue(action.promise);

    const view = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.click(await screen.findByRole("button", { name: "Starred" }));
    await screen.findByText("Subject a original", {
      selector: ".mail-subject",
    });
    await waitFor(() => expect(mocks.mailChangedHandlers).toHaveLength(1));
    selectSubject("Subject a original");
    archiveSelection();

    page = {
      conversations: groupMessages([
        { ...target, subject: "Subject a latest list", is_flagged: true },
      ]),
      nextCursor: null,
    };
    await act(async () => {
      mocks.mailChangedHandlers[0]();
      await Promise.resolve();
    });

    vi.useFakeTimers();
    try {
      await act(async () => {
        action.reject(new Error("provider rejected archive"));
        await Promise.resolve();
      });
      const restored = screen
        .getByText("Subject a latest list", { selector: ".mail-subject" })
        .closest("button")!;
      expect(
        within(restored).getByRole("button", { name: "Remove star" }),
      ).toHaveAttribute("data-active", "true");
      expectSubjectSelected("Subject a latest list", true);
    } finally {
      view.unmount();
      vi.useRealTimers();
    }
  });

  it("restores the latest remote-only query result after a failed action", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    const target = message("a", 1, accountA, "Subject a remote original");
    let remoteSnapshot = [target];
    mocks.api.smartInbox.mockResolvedValue({ sections: [] });
    mocks.api.search.mockResolvedValue({ conversations: [], nextCursor: null });
    mocks.api.searchRemote.mockImplementation(async () => ({
      messages: remoteSnapshot,
      limitReached: false,
    }));
    const action = deferred();
    mocks.api.action.mockReturnValue(action.promise);

    const view = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await waitFor(() => expect(mocks.mailChangedHandlers).toHaveLength(1));

    vi.useFakeTimers();
    try {
      fireEvent.change(screen.getByRole("textbox", { name: "Search mail" }), {
        target: { value: "remote only" },
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(250);
      });
      expect(
        screen.getByText("Subject a remote original", {
          selector: ".mail-subject",
        }),
      ).toBeVisible();

      selectSubject("Subject a remote original");
      archiveSelection();
      remoteSnapshot = [
        {
          ...target,
          subject: "Subject a remote latest",
          is_read: true,
          is_flagged: true,
        },
      ];
      await act(async () => {
        mocks.mailChangedHandlers[0]();
        await Promise.resolve();
      });
      await act(async () => {
        action.reject(new Error("provider rejected archive"));
        await Promise.resolve();
      });

      const restored = screen
        .getByText("Subject a remote latest", { selector: ".mail-subject" })
        .closest("button")!;
      expect(restored).toHaveAttribute("data-unread", "false");
      expect(
        within(restored).getByRole("button", { name: "Remove star" }),
      ).toHaveAttribute("data-active", "true");
      expectSubjectSelected("Subject a remote latest", true);
    } finally {
      view.unmount();
      vi.useRealTimers();
    }
  });

  it("keeps the complete local conversation when remote query returns a subset", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    const older = {
      ...message("a", 1, accountA, "Subject a local older"),
      thread_id: "shared-query-thread",
      received_at: "2026-10-20T10:00:00Z",
    };
    const latest = {
      ...message("b", 2, accountA, "Subject b local latest"),
      thread_id: "shared-query-thread",
      received_at: "2026-10-20T11:00:00Z",
    };
    let localSnapshot = [older, latest];
    let remoteSnapshot = [{ ...older, subject: "Subject a remote subset" }];
    mocks.api.smartInbox.mockResolvedValue({ sections: [] });
    mocks.api.search.mockImplementation(async () => ({
      conversations: groupMessages(localSnapshot),
      nextCursor: null,
    }));
    mocks.api.searchRemote.mockImplementation(async () => ({
      messages: remoteSnapshot,
      limitReached: false,
    }));
    const action = deferred();
    mocks.api.action.mockReturnValue(action.promise);

    const view = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await waitFor(() => expect(mocks.mailChangedHandlers).toHaveLength(1));

    vi.useFakeTimers();
    try {
      fireEvent.change(screen.getByRole("textbox", { name: "Search mail" }), {
        target: { value: "shared conversation" },
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(250);
      });
      expect(
        screen.getByText("Subject b local latest", {
          selector: ".mail-subject",
        }),
      ).toBeVisible();

      selectSubject("Subject b local latest");
      archiveSelection();
      await act(async () => {
        mocks.mailChangedHandlers[0]();
        await Promise.resolve();
      });
      await act(async () => {
        action.reject(new Error("provider rejected archive"));
        await Promise.resolve();
      });

      expect(
        screen.getByText("Subject b local latest", {
          selector: ".mail-subject",
        }),
      ).toBeVisible();
      expect(
        screen.queryByText("Subject a remote subset", {
          selector: ".mail-subject",
        }),
      ).not.toBeInTheDocument();
      expectSubjectSelected("Subject b local latest", true);

      const retry = deferred();
      mocks.api.action.mockClear();
      mocks.api.action.mockReturnValue(retry.promise);
      archiveSelection();
      await act(async () => {
        await Promise.resolve();
      });
      expect(mocks.api.action).toHaveBeenCalledTimes(2);
      expect(mocks.api.action.mock.calls.map((call) => call[4]).sort()).toEqual(
        ["a", "b"],
      );
      expect(
        screen.queryByText("Subject b local latest", {
          selector: ".mail-subject",
        }),
      ).not.toBeInTheDocument();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(260);
      });
      await act(async () => {
        mocks.mailChangedHandlers[0]();
        await Promise.resolve();
      });
      expect(
        screen.queryByText("Subject b local latest", {
          selector: ".mail-subject",
        }),
      ).not.toBeInTheDocument();

      localSnapshot = [];
      remoteSnapshot = [];
      await act(async () => {
        retry.resolve();
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(
        screen.queryByText("Subject b local latest", {
          selector: ".mail-subject",
        }),
      ).not.toBeInTheDocument();
    } finally {
      view.unmount();
      vi.useRealTimers();
    }
  });

  it("restores the newest exclusive Smart section membership after a stale action failure", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    const target = message("a", 1, accountA, "Subject a original");
    let sections: SmartInboxPage["sections"] = [
      {
        id: "people" as const,
        conversations: groupMessages([target]),
        nextCursor: null,
      },
    ];
    mocks.api.smartInbox.mockImplementation(async () => ({ sections }));
    const action = deferred();
    mocks.api.action.mockReturnValue(action.promise);

    const view = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Subject a original", {
      selector: ".mail-subject",
    });
    await waitFor(() => expect(mocks.mailChangedHandlers).toHaveLength(1));
    selectSubject("Subject a original");
    archiveSelection();

    const newest = {
      ...target,
      subject: "Subject a newest Smart membership",
      is_flagged: true,
    };
    sections = [
      { id: "people" as const, conversations: [], nextCursor: null },
      {
        id: "starred" as const,
        conversations: groupMessages([newest]),
        nextCursor: null,
      },
    ];
    await act(async () => {
      mocks.mailChangedHandlers[0]();
      await Promise.resolve();
    });

    vi.useFakeTimers();
    try {
      await act(async () => {
        action.reject(new Error("provider rejected archive"));
        await Promise.resolve();
      });
      expect(screen.queryByRole("region", { name: "People" })).toBeNull();
      expect(
        within(screen.getByRole("region", { name: "Starred" })).getByText(
          "Subject a newest Smart membership",
          { selector: ".mail-subject" },
        ),
      ).toBeVisible();
      expect(
        screen.getAllByText("Subject a newest Smart membership", {
          selector: ".mail-subject",
        }),
      ).toHaveLength(1);
    } finally {
      view.unmount();
      vi.useRealTimers();
    }
  });

  it("does not restore a failed target absent from complete Smart sections", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    const target = message("a", 1, accountA, "Subject a original");
    const neighbor = message("e", 5, accountA, "Subject e original");
    let sections: SmartInboxPage["sections"] = [
      {
        id: "people",
        conversations: groupMessages([target, neighbor]),
        nextCursor: null,
      },
    ];
    mocks.api.smartInbox.mockImplementation(async () => ({ sections }));
    const action = deferred();
    mocks.api.action.mockReturnValue(action.promise);

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Subject a original", {
      selector: ".mail-subject",
    });
    await waitFor(() => expect(mocks.mailChangedHandlers).toHaveLength(1));
    selectSubject("Subject a original");
    archiveSelection();

    sections = [
      {
        id: "people",
        conversations: groupMessages([
          { ...neighbor, subject: "Subject e authoritative Smart" },
        ]),
        nextCursor: null,
      },
    ];
    await act(async () => {
      mocks.mailChangedHandlers[0]();
      await Promise.resolve();
    });
    await screen.findByText("Subject e authoritative Smart", {
      selector: ".mail-subject",
    });
    await act(async () => {
      action.reject(new Error("provider rejected stale archive"));
      await Promise.resolve();
    });

    await waitFor(() =>
      expect(
        screen.queryByText("Subject a original", {
          selector: ".mail-subject",
        }),
      ).not.toBeInTheDocument(),
    );
    expect(screen.queryByText("1 selected")).not.toBeInTheDocument();
  });

  it("restores a failed target when incomplete Smart sections cannot prove absence", async () => {
    localStorage.setItem("dakia.mail-list-view", "smart");
    const target = message("a", 1, accountA, "Subject a original");
    const neighbor = message("e", 5, accountA, "Subject e original");
    let sections: SmartInboxPage["sections"] = [
      {
        id: "people" as const,
        conversations: groupMessages([target, neighbor]),
        nextCursor: null as null | { received_at: string; id: string },
      },
    ];
    mocks.api.smartInbox.mockImplementation(async () => ({ sections }));
    const action = deferred();
    mocks.api.action.mockReturnValue(action.promise);

    const view = render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    await screen.findByText("Subject a original", {
      selector: ".mail-subject",
    });
    await waitFor(() => expect(mocks.mailChangedHandlers).toHaveLength(1));
    selectSubject("Subject a original");
    archiveSelection();

    sections = [
      {
        id: "people" as const,
        conversations: groupMessages([
          { ...neighbor, subject: "Subject e Smart first page" },
        ]),
        nextCursor: { received_at: neighbor.received_at, id: neighbor.id },
      },
    ];
    await act(async () => {
      mocks.mailChangedHandlers[0]();
      await Promise.resolve();
    });
    await screen.findByText("Subject e Smart first page", {
      selector: ".mail-subject",
    });

    vi.useFakeTimers();
    try {
      await act(async () => {
        action.reject(new Error("provider rejected archive"));
        await Promise.resolve();
      });
      expect(
        screen.getByText("Subject a original", { selector: ".mail-subject" }),
      ).toBeVisible();
      expectSubjectSelected("Subject a original", true);
    } finally {
      view.unmount();
      vi.useRealTimers();
    }
  });
});
