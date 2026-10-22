import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import { ComposeApp } from "./ComposeApp";
import { ReaderWindowApp } from "./ReaderWindowApp";
import type { Account, MailSummary, MailThread } from "./types";

const mocks = vi.hoisted(() => {
  class SendMessageError extends Error {
    constructor(
      readonly kind:
        "not_submitted" | "delivery_uncertain" | "sent_copy_failed",
    ) {
      super("safe send failure");
    }
  }
  return {
    SendMessageError,
    api: {
      accounts: vi.fn(),
      action: vi.fn(),
      aiAvailable: vi.fn(),
      content: vi.fn(),
      conversationForTarget: vi.fn(),
      draft: vi.fn(),
      forwardAttachments: vi.fn(),
      openExternal: vi.fn(),
      readDroppedFiles: vi.fn(),
      send: vi.fn(),
      setRead: vi.fn(),
      setStarred: vi.fn(),
      showEmailAddressContextMenu: vi.fn(),
      summarize: vi.fn(),
      trashMessagesFromSender: vi.fn(),
      unsubscribe: vi.fn(),
    },
    showNativeMessage: vi.fn(),
    confirmNativeAction: vi.fn(),
    openedUrls: [] as string[],
  };
});

vi.mock("./api", () => ({
  api: mocks.api,
  SendMessageError: mocks.SendMessageError,
}));
vi.mock("./features", () => ({ AI_FEATURES_VISIBLE: false }));
vi.mock("./nativeFeedback", () => ({
  confirmNativeAction: mocks.confirmNativeAction,
  showNativeMessage: mocks.showNativeMessage,
}));
vi.mock("./analytics", () => ({
  readAnalyticsSettings: () => ({ consent: "disabled" }),
  reportUsageIfConsented: vi.fn(async () => false),
  listenForAnalyticsConsent: vi.fn(async () => () => undefined),
  setAnalyticsConsent: vi.fn(),
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    close: vi.fn(async () => undefined),
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

const selectedAccount: Account = {
  ...firstAccount,
  id: "account-2",
  email: "second@example.test",
  account_name: "Second inbox",
  display_name: "Second User",
  auth: { type: "password", username: "second@example.test" },
};

function message(overrides: Partial<MailSummary> = {}): MailSummary {
  return {
    id: "account-2:colliding-message-id",
    account_id: selectedAccount.id,
    mailbox: "INBOX",
    uid: 27,
    mailbox_uid_validity: 8080,
    message_id: "<recipient-intent@example.test>",
    thread_id: "colliding-thread-id",
    subject: "Fictional recipient intent",
    from_name: "Original Sender",
    from_address: "sender@example.test",
    to_addresses: selectedAccount.email,
    received_at: "2026-10-21T06:00:00Z",
    snippet: "Fictional recipient test",
    body_text: "Fictional reply body",
    is_read: true,
    is_flagged: false,
    has_attachments: false,
    ...overrides,
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
    unread: false,
    hasAttachments: false,
    participants: [item.from_address],
  };
}

describe("detached Reader recipient intent", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.openedUrls.length = 0;
    localStorage.clear();
    sessionStorage.clear();
    window.history.replaceState({}, "", "/");
    delete (window as Window & { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__;
    Object.defineProperty(globalThis, "indexedDB", {
      configurable: true,
      value: undefined,
    });
    vi.spyOn(window, "open").mockImplementation((url) => {
      mocks.openedUrls.push(String(url));
      return window;
    });
    vi.spyOn(window, "close").mockImplementation(() => undefined);

    mocks.api.accounts.mockResolvedValue([firstAccount, selectedAccount]);
    mocks.api.aiAvailable.mockResolvedValue(false);
    mocks.api.content.mockResolvedValue({
      body_text: "Full fictional reply body",
      body_html: "<p>Full fictional reply body</p>",
      attachments: [],
    });
    mocks.api.draft.mockResolvedValue("");
    mocks.api.forwardAttachments.mockResolvedValue([]);
    mocks.api.openExternal.mockResolvedValue(undefined);
    mocks.api.readDroppedFiles.mockResolvedValue([]);
    mocks.api.send.mockResolvedValue("queued");
    mocks.api.setRead.mockResolvedValue(undefined);
    mocks.api.setStarred.mockImplementation(async (item: MailSummary) => item);
    mocks.api.action.mockResolvedValue(undefined);
    mocks.api.summarize.mockResolvedValue("");
    mocks.api.unsubscribe.mockResolvedValue({
      kind: "completed",
      cleanupTarget: null,
    });
    mocks.api.trashMessagesFromSender.mockResolvedValue({
      matched: 0,
      moved: 0,
      failed: 0,
    });
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  async function openReplyThroughReader(
    selectedMessage: MailSummary,
    action: "Reply" | "Reply all",
  ) {
    const selectedThread = thread(selectedMessage);
    const collidingMessage = message({
      id: "account-1:colliding-message-id",
      account_id: firstAccount.id,
      from_address: "wrong-account@example.test",
      to_addresses: firstAccount.email,
    });
    const collidingThread = thread(collidingMessage);
    const selectedTarget = {
      accountId: selectedAccount.id,
      localMessageId: selectedMessage.id,
      rfcMessageId: selectedMessage.message_id,
      threadId: selectedMessage.thread_id,
      mailbox: selectedMessage.mailbox,
    };
    const collisionTarget = {
      ...selectedTarget,
      accountId: firstAccount.id,
      localMessageId: collidingMessage.id,
    };
    mocks.api.conversationForTarget.mockImplementation(async (target) => {
      const matches = (expected: typeof selectedTarget) =>
        target.accountId === expected.accountId &&
        target.localMessageId === expected.localMessageId &&
        target.rfcMessageId === expected.rfcMessageId &&
        target.threadId === expected.threadId &&
        target.mailbox === expected.mailbox;
      if (matches(selectedTarget)) {
        return selectedThread;
      }
      if (matches(collisionTarget)) {
        return collidingThread;
      }
      return null;
    });
    mocks.api.content.mockImplementation(
      async (messageId, expectedUidValidity) => {
        if (
          messageId !== selectedMessage.id ||
          expectedUidValidity !== selectedMessage.mailbox_uid_validity
        ) {
          throw new Error("Reader requested content for the colliding account");
        }
        return {
          body_text: "Full fictional reply body",
          body_html: "<p>Full fictional reply body</p>",
          attachments: [],
        };
      },
    );

    const readerToken = crypto.randomUUID();
    localStorage.setItem(
      `dakia.reader-seed.${readerToken}`,
      JSON.stringify({
        target: {
          accountId: selectedAccount.id,
          localMessageId: selectedMessage.id,
          rfcMessageId: selectedMessage.message_id,
          threadId: selectedMessage.thread_id,
          mailbox: selectedMessage.mailbox,
        },
        focusedMessageId: selectedMessage.id,
      }),
    );
    const readerUrl = `/?view=reader&seedKey=${readerToken}`;
    expect(readerUrl).not.toContain(selectedAccount.id);
    expect(readerUrl).not.toContain(selectedMessage.id);
    window.history.replaceState({}, "", readerUrl);

    const reader = render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await waitFor(() =>
      expect(mocks.api.conversationForTarget).toHaveBeenCalledWith(
        selectedTarget,
      ),
    );
    fireEvent.click(
      (await screen.findAllByRole("button", { name: action })).at(-1)!,
    );

    await waitFor(() => expect(mocks.openedUrls).toHaveLength(1));
    expect(mocks.api.content).toHaveBeenCalledWith(
      selectedMessage.id,
      selectedMessage.mailbox_uid_validity,
    );
    const composeUrl = mocks.openedUrls[0];
    expect(composeUrl).toMatch(/^\/\?view=compose&seedKey=/);
    expect(composeUrl).not.toContain(selectedAccount.id);
    expect(composeUrl).not.toContain(selectedMessage.id);
    expect(composeUrl).not.toContain("example.test");
    const composeToken = new URL(
      composeUrl,
      window.location.origin,
    ).searchParams.get("seedKey");
    expect(composeToken).toEqual(expect.any(String));
    const storedSeed = JSON.parse(
      localStorage.getItem(`dakia.compose-seed.${composeToken}`) ?? "{}",
    );

    reader.unmount();
    window.history.replaceState({}, "", composeUrl);
    render(<ComposeApp />);
    expect(await screen.findByLabelText("From")).toHaveValue(
      selectedAccount.id,
    );
    return storedSeed;
  }

  it("uses Reply-To instead of From through the final send payload", async () => {
    const selectedMessage = message({
      from_address: "original-sender@example.test",
      reply_to_addresses: "Reply Desk <reply@example.test>",
      to_addresses: selectedAccount.email,
      cc_addresses: "Visible Copy <copy@example.test>",
      bcc_addresses: "Hidden Recipient <hidden@example.test>",
    });

    const storedSeed = await openReplyThroughReader(selectedMessage, "Reply");
    expect(storedSeed).toMatchObject({
      accountId: selectedAccount.id,
      to: "Reply Desk <reply@example.test>",
      inReplyTo: selectedMessage.message_id,
      contextMessageIds: [selectedMessage.id],
    });
    expect(storedSeed).not.toHaveProperty("cc");
    expect(storedSeed).not.toHaveProperty("bcc");
    expect(screen.getByLabelText("To")).toHaveValue(
      "Reply Desk <reply@example.test>",
    );
    fireEvent.click(screen.getByRole("button", { name: "Cc · Bcc" }));
    expect(screen.getByLabelText("Cc")).toHaveValue("");
    expect(screen.getByLabelText("Bcc")).toHaveValue("");
    expect(
      (screen.getByLabelText("To") as HTMLInputElement).value,
    ).not.toContain("original-sender@example.test");

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    expect(mocks.api.send).toHaveBeenCalledWith(
      expect.objectContaining({
        account_id: selectedAccount.id,
        to: ["Reply Desk <reply@example.test>"],
        cc: [],
        bcc: [],
        in_reply_to: selectedMessage.message_id,
      }),
    );
  });

  it("deduplicates Reply All by address while retaining the selected account context", async () => {
    const selectedMessage = message({
      from_name: "Original Sender",
      from_address: "sender@example.test",
      to_addresses:
        "SECOND@example.test, Sender Duplicate <SENDER@example.test>, Peer Person <peer@example.test>, PEER@example.test, First User <first@example.test>",
      cc_addresses:
        "peer@example.test, Other Person <other@example.test>, OTHER@example.test, second@example.test",
      bcc_addresses: "Hidden Recipient <hidden@example.test>",
      reply_to_addresses: undefined,
    });

    const storedSeed = await openReplyThroughReader(
      selectedMessage,
      "Reply all",
    );
    expect(storedSeed).toMatchObject({
      accountId: selectedAccount.id,
      to: "Original Sender <sender@example.test>, Peer Person <peer@example.test>, First User <first@example.test>",
      cc: "Other Person <other@example.test>",
      inReplyTo: selectedMessage.message_id,
    });
    expect(storedSeed).not.toHaveProperty("bcc");
    expect(screen.getByLabelText("To")).toHaveValue(
      "Original Sender <sender@example.test>, Peer Person <peer@example.test>, First User <first@example.test>",
    );
    expect(screen.getByLabelText("Cc")).toHaveValue(
      "Other Person <other@example.test>",
    );
    expect(screen.getByLabelText("Bcc")).toHaveValue("");

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    expect(mocks.api.send).toHaveBeenCalledWith(
      expect.objectContaining({
        account_id: selectedAccount.id,
        to: [
          "Original Sender <sender@example.test>",
          "Peer Person <peer@example.test>",
          "First User <first@example.test>",
        ],
        cc: ["Other Person <other@example.test>"],
        bcc: [],
        in_reply_to: selectedMessage.message_id,
      }),
    );
    expect(JSON.stringify(mocks.api.send.mock.calls[0][0])).not.toContain(
      "hidden@example.test",
    );
    expect(JSON.stringify(mocks.api.send.mock.calls[0][0])).not.toContain(
      "wrong-account@example.test",
    );
  });

  it("keeps Reply-To primary in Reply All and includes a sender already in To once", async () => {
    const selectedMessage = message({
      from_name: "Original Sender",
      from_address: "sender@example.test",
      reply_to_addresses: "Reply Desk <reply@example.test>",
      to_addresses:
        "SECOND@example.test, Sender in To <SENDER@example.test>, Peer Person <peer@example.test>",
      cc_addresses:
        "sender@example.test, peer@example.test, Other Person <other@example.test>",
      bcc_addresses: "Hidden Recipient <hidden@example.test>",
    });

    await openReplyThroughReader(selectedMessage, "Reply all");
    expect(screen.getByLabelText("To")).toHaveValue(
      "Reply Desk <reply@example.test>, Sender in To <SENDER@example.test>, Peer Person <peer@example.test>",
    );
    expect(screen.getByLabelText("Cc")).toHaveValue(
      "Other Person <other@example.test>",
    );
    expect(screen.getByLabelText("Bcc")).toHaveValue("");

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    expect(mocks.api.send).toHaveBeenCalledWith(
      expect.objectContaining({
        account_id: selectedAccount.id,
        to: [
          "Reply Desk <reply@example.test>",
          "Sender in To <SENDER@example.test>",
          "Peer Person <peer@example.test>",
        ],
        cc: ["Other Person <other@example.test>"],
        bcc: [],
      }),
    );
  });

  it("keeps every Reply-To and visible recipient once through a detached Reply All send", async () => {
    const selectedMessage = message({
      from_name: "First Reply Desk",
      from_address: "reply-one@example.test",
      reply_to_addresses:
        "First Reply Desk <reply-one@example.test>, Second Reply Desk <reply-two@example.test>",
      to_addresses:
        "Second inbox <SECOND@example.test>, Sender duplicate <reply-one@example.test>, Second Reply Desk <reply-two@example.test>, Visible Person <visible@example.test>, VISIBLE@example.test",
      cc_addresses:
        "Visible duplicate <VISIBLE@example.test>, Copy Person <copy@example.test>, COPY@example.test, reply-one@example.test",
      bcc_addresses:
        "Hidden recipient <hidden@example.test>, Also Hidden <also-hidden@example.test>",
    });

    const storedSeed = await openReplyThroughReader(
      selectedMessage,
      "Reply all",
    );
    expect(storedSeed).toMatchObject({
      accountId: selectedAccount.id,
      to: "First Reply Desk <reply-one@example.test>, Second Reply Desk <reply-two@example.test>, Visible Person <visible@example.test>",
      cc: "Copy Person <copy@example.test>",
      inReplyTo: selectedMessage.message_id,
    });
    expect(storedSeed).not.toHaveProperty("bcc");
    expect(JSON.stringify(storedSeed)).not.toContain("hidden@example.test");
    expect(screen.getByLabelText("To")).toHaveValue(
      "First Reply Desk <reply-one@example.test>, Second Reply Desk <reply-two@example.test>, Visible Person <visible@example.test>",
    );
    expect(screen.getByLabelText("Cc")).toHaveValue(
      "Copy Person <copy@example.test>",
    );
    expect(screen.getByLabelText("Bcc")).toHaveValue("");

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    expect(mocks.api.send).toHaveBeenCalledWith(
      expect.objectContaining({
        account_id: selectedAccount.id,
        to: [
          "First Reply Desk <reply-one@example.test>",
          "Second Reply Desk <reply-two@example.test>",
          "Visible Person <visible@example.test>",
        ],
        cc: ["Copy Person <copy@example.test>"],
        bcc: [],
        in_reply_to: selectedMessage.message_id,
      }),
    );
    expect(JSON.stringify(mocks.api.send.mock.calls[0][0])).not.toContain(
      "hidden@example.test",
    );
    expect(JSON.stringify(mocks.api.send.mock.calls[0][0])).not.toContain(
      "also-hidden@example.test",
    );
  });

  it("preserves quoted names and group members through the Reader-to-Compose send path", async () => {
    const selectedMessage = message({
      from_name: "Queue Sender",
      from_address: "sender@example.test",
      reply_to_addresses: undefined,
      to_addresses:
        'Visible team: "Doe, Jane" <jane@example.test>, John Member <john@example.test>; Second inbox <SECOND@example.test>',
      cc_addresses: "",
      bcc_addresses: "Hidden recipient <hidden@example.test>",
    });

    const storedSeed = await openReplyThroughReader(
      selectedMessage,
      "Reply all",
    );
    expect(storedSeed).toMatchObject({
      accountId: selectedAccount.id,
      to: 'Queue Sender <sender@example.test>, "Doe, Jane" <jane@example.test>, John Member <john@example.test>',
      inReplyTo: selectedMessage.message_id,
    });
    expect(storedSeed).not.toHaveProperty("bcc");
    expect(screen.getByLabelText("To")).toHaveValue(
      'Queue Sender <sender@example.test>, "Doe, Jane" <jane@example.test>, John Member <john@example.test>',
    );

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    await waitFor(() => expect(mocks.api.send).toHaveBeenCalledOnce());
    expect(mocks.api.send).toHaveBeenCalledWith(
      expect.objectContaining({
        account_id: selectedAccount.id,
        to: [
          "Queue Sender <sender@example.test>",
          '"Doe, Jane" <jane@example.test>',
          "John Member <john@example.test>",
        ],
        cc: [],
        bcc: [],
        in_reply_to: selectedMessage.message_id,
      }),
    );
  });
});
