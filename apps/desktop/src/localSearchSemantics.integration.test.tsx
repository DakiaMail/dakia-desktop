import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import App from "./App";
import type { Account, MailSummary, MailThread } from "./types";

const mocks = vi.hoisted(() => ({
  api: {
    accounts: vi.fn(),
    action: vi.fn(),
    aiAvailable: vi.fn(),
    classifyPending: vi.fn(),
    configureTray: vi.fn(),
    content: vi.fn(),
    mailRebuildStatus: vi.fn(),
    recordNotificationDelivered: vi.fn(),
    search: vi.fn(),
    searchRemote: vi.fn(),
    setCategory: vi.fn(),
    setRead: vi.fn(),
    setStarred: vi.fn(),
    smartInbox: vi.fn(),
    starredCount: vi.fn(),
    startRealtimeSync: vi.fn(),
    summarize: vi.fn(),
    sync: vi.fn(),
    trashMessagesFromSender: vi.fn(),
    unsubscribe: vi.fn(),
  },
  checkForUpdate: vi.fn(),
}));

vi.mock("./api", () => ({ api: mocks.api }));
vi.mock("./features", () => ({ AI_FEATURES_VISIBLE: false }));
vi.mock("./nativeFeedback", () => ({
  confirmNativeAction: vi.fn(async () => false),
  showNativeMessage: vi.fn(async () => undefined),
}));
vi.mock("./notifications", () => ({
  onNotificationAction: vi.fn(async () => () => undefined),
  readNotificationSettings: () => ({
    enabled: false,
    soundEnabled: false,
    showPreview: false,
  }),
  requestInitialNotificationAccess: vi.fn(async () => undefined),
  sendNewMailNotification: vi.fn(async () => false),
}));
vi.mock("./updater", () => ({
  checkForUpdate: mocks.checkForUpdate,
  downloadUpdate: vi.fn(async () => undefined),
  installUpdateAndRelaunch: vi.fn(async () => undefined),
}));
vi.mock("./analytics", () => ({
  readAnalyticsSettings: () => ({ consent: "disabled" }),
  reportUsageIfConsented: vi.fn(async () => false),
  listenForAnalyticsConsent: vi.fn(async () => () => undefined),
  setAnalyticsConsent: vi.fn(),
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    isFocused: vi.fn(async () => true),
    setFocus: vi.fn(async () => undefined),
    show: vi.fn(async () => undefined),
    startDragging: vi.fn(async () => undefined),
  }),
}));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    listen: vi.fn(async () => () => undefined),
  }),
}));
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getAllWebviewWindows: vi.fn(async () => []),
}));

const firstAccount: Account = {
  id: "account-1",
  email: "first@example.test",
  account_name: "First inbox",
  display_name: "First User",
  provider_id: "fastmail",
  auth: { type: "password", username: "first@example.test" },
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

const secondAccount: Account = {
  ...firstAccount,
  id: "account-2",
  email: "second@example.test",
  account_name: "Second inbox",
  display_name: "Second User",
  auth: { type: "password", username: "second@example.test" },
};

function message(id: string, accountId: string, subject: string): MailSummary {
  return {
    id,
    account_id: accountId,
    mailbox: "INBOX",
    uid: 42,
    mailbox_uid_validity: 77,
    message_id: "<colliding@example.test>",
    thread_id: "colliding-thread",
    subject,
    from_address: "sender@example.test",
    to_addresses: "second@example.test",
    received_at: "2026-10-21T08:00:00Z",
    snippet: "雪 töö tag",
    body_text: "",
    is_read: false,
    is_flagged: false,
    has_attachments: false,
  };
}

function thread(item: MailSummary): MailThread {
  return {
    id: `${item.account_id}:${item.thread_id}`,
    accountId: item.account_id,
    threadId: item.thread_id,
    messages: [item],
    sourceMessages: [item],
    latest: item,
    messageCount: 1,
    unread: true,
    hasAttachments: false,
    participants: [item.from_address],
  };
}

describe("local search App semantics", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
    sessionStorage.clear();
    localStorage.setItem("dakia.mail-list-view", "list");
    window.history.replaceState({}, "", "/");
    delete (window as Window & { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__;

    mocks.api.accounts.mockResolvedValue([firstAccount, secondAccount]);
    mocks.api.search.mockResolvedValue({
      conversations: [],
      nextCursor: null,
    });
    mocks.api.searchRemote.mockResolvedValue({
      messages: [],
      limitReached: false,
    });
    mocks.api.smartInbox.mockResolvedValue({ sections: [] });
    mocks.api.starredCount.mockResolvedValue(0);
    mocks.api.classifyPending.mockResolvedValue(0);
    mocks.api.mailRebuildStatus.mockResolvedValue([]);
    mocks.api.configureTray.mockResolvedValue(undefined);
    mocks.api.startRealtimeSync.mockResolvedValue(undefined);
    mocks.api.aiAvailable.mockResolvedValue(false);
    mocks.api.sync.mockResolvedValue({ syncedCount: 0, newMessages: [] });
    mocks.api.setRead.mockResolvedValue(undefined);
    mocks.api.setStarred.mockResolvedValue(undefined);
    mocks.api.action.mockResolvedValue(undefined);
    mocks.api.summarize.mockResolvedValue("");
    mocks.api.unsubscribe.mockResolvedValue({ kind: "completed" });
    mocks.api.trashMessagesFromSender.mockResolvedValue({
      matched: 0,
      moved: 0,
      failed: 0,
    });
    mocks.checkForUpdate.mockResolvedValue(null);
  });

  it("forwards escaped Unicode unchanged and rejects leaked rows from another account", async () => {
    const query = '雪 "töö" +tag';
    const cursor = {
      received_at: "2026-10-21T08:00:00Z",
      id: "selected-result",
    };
    const selected = message(
      "selected-result",
      secondAccount.id,
      "Selected Unicode result",
    );
    const leaked = message(
      "leaked-result",
      firstAccount.id,
      "Leaked other-account result",
    );
    const selectedOlder = {
      ...message(
        "selected-older-result",
        secondAccount.id,
        "Selected older Unicode result",
      ),
      received_at: "2026-10-20T08:00:00Z",
    };
    const leakedOlder = {
      ...message(
        "leaked-older-result",
        firstAccount.id,
        "Leaked older other-account result",
      ),
      received_at: "2026-10-20T08:00:00Z",
    };
    mocks.api.search.mockImplementation(async (...args: unknown[]) => {
      if (args[0] === query && args[6] === cursor) {
        return {
          conversations: [thread(selectedOlder), thread(leakedOlder)],
          nextCursor: null,
        };
      }
      return args[0] === query
        ? {
            conversations: [thread(selected), thread(leaked)],
            nextCursor: cursor,
          }
        : { conversations: [], nextCursor: null };
    });

    render(
      <MantineProvider>
        <App />
      </MantineProvider>,
    );
    fireEvent.click(await screen.findByTitle(secondAccount.email));
    fireEvent.change(screen.getByRole("textbox", { name: "Search mail" }), {
      target: { value: query },
    });

    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        query,
        [secondAccount.id],
        "INBOX",
        false,
        false,
        100,
        null,
      ),
    );
    expect(await screen.findByText(selected.subject)).toBeVisible();
    expect(screen.queryByText(leaked.subject)).not.toBeInTheDocument();

    const scroller = document.querySelector(".mail-scroll") as HTMLDivElement;
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 2_000 },
      scrollTop: { configurable: true, value: 1_300 },
      clientHeight: { configurable: true, value: 500 },
    });
    fireEvent.scroll(scroller);

    await waitFor(() =>
      expect(mocks.api.search).toHaveBeenCalledWith(
        query,
        [secondAccount.id],
        "INBOX",
        false,
        false,
        100,
        cursor,
      ),
    );
    expect(await screen.findByText(selectedOlder.subject)).toBeVisible();
    expect(screen.queryByText(leakedOlder.subject)).not.toBeInTheDocument();
  });
});
