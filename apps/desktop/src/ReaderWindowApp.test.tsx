import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ReaderWindowSeed } from "./readerWindow";
import type { MailSummary, MailThread } from "./types";

const mocks = vi.hoisted(() => ({
  api: {
    accounts: vi.fn(),
    action: vi.fn(),
    aiAvailable: vi.fn(),
    content: vi.fn(),
    conversationForTarget: vi.fn(),
    setRead: vi.fn(),
    setStarred: vi.fn(),
    showEmailAddressContextMenu: vi.fn(),
    summarize: vi.fn(),
    unsubscribe: vi.fn(),
    trashMessagesFromSender: vi.fn(),
  },
  closeReaderWindow: vi.fn(),
  notifyReaderWindowMutated: vi.fn(),
  notifyReaderWindowFailed: vi.fn(),
  onReaderTarget: vi.fn(),
  readReaderSeed: vi.fn(),
  openComposeWindow: vi.fn(),
  showNativeMessage: vi.fn(),
  nativeMenuHandlers: [] as Array<(action: string) => void>,
}));

vi.mock("./api", () => ({ api: mocks.api }));
vi.mock("./composeWindow", () => ({
  openComposeWindow: mocks.openComposeWindow,
}));
vi.mock("./readerWindow", () => ({
  closeReaderWindow: mocks.closeReaderWindow,
  notifyReaderWindowMutated: mocks.notifyReaderWindowMutated,
  notifyReaderWindowFailed: mocks.notifyReaderWindowFailed,
  onReaderTarget: mocks.onReaderTarget,
  readReaderSeed: mocks.readReaderSeed,
}));
vi.mock("./nativeFeedback", () => ({
  showNativeMessage: mocks.showNativeMessage,
}));
vi.mock("./nativeWindows", () => ({
  onNativeMenuAction: vi.fn(async (handler: (action: string) => void) => {
    mocks.nativeMenuHandlers.push(handler);
    return () => undefined;
  }),
}));
vi.mock("./components/Reader", () => ({
  Reader: ({
    message,
    messages,
    onArchive,
    onComposeTo,
    onAddressContextMenu,
    onPermanentDelete,
    onSendAgain,
    onUnsubscribe,
  }: {
    message?: MailSummary;
    messages?: MailSummary[];
    onArchive: () => void;
    onComposeTo: (message: MailSummary, address: string) => void;
    onAddressContextMenu: (message: MailSummary, address: string) => void;
    onPermanentDelete: (message: MailSummary) => void;
    onSendAgain: (message: MailSummary) => void;
    onUnsubscribe: (message: MailSummary) => void;
  }) => (
    <div>
      <span data-testid="focused-message">{message?.id}</span>
      <span data-testid="conversation-count">{messages?.length}</span>
      <button type="button" onClick={onArchive}>
        Archive
      </button>
      <button type="button" onClick={() => message && onSendAgain(message)}>
        Send again
      </button>
      <button
        type="button"
        onClick={() =>
          message && onComposeTo(message, "müller+news@example.com")
        }
      >
        Compose to address
      </button>
      <button
        type="button"
        onClick={() =>
          message && onAddressContextMenu(message, "müller+news@example.com")
        }
      >
        Address context menu
      </button>
      <button
        type="button"
        onClick={() => message && onPermanentDelete(message)}
      >
        Permanently delete
      </button>
      <button type="button" onClick={() => message && onUnsubscribe(message)}>
        Unsubscribe
      </button>
    </div>
  ),
}));

const messages: MailSummary[] = [
  {
    id: "message-1",
    account_id: "account-1",
    mailbox: "INBOX",
    uid: 1,
    thread_id: "thread-1",
    subject: "Project",
    from_address: "mara@example.com",
    to_addresses: "alex@example.com",
    received_at: "2026-08-10T09:00:00Z",
    snippet: "Earlier",
    body_text: "Earlier",
    is_read: true,
    is_flagged: false,
    has_attachments: false,
  },
  {
    id: "message-2",
    account_id: "account-1",
    mailbox: "INBOX",
    uid: 2,
    thread_id: "thread-1",
    subject: "Project",
    from_address: "alex@example.com",
    to_addresses: "mara@example.com",
    received_at: "2026-08-10T10:00:00Z",
    snippet: "Focused",
    body_text: "Focused",
    is_read: false,
    is_flagged: false,
    has_attachments: false,
  },
];

const thread: MailThread = {
  id: "account-1:thread-1",
  accountId: "account-1",
  threadId: "thread-1",
  messages,
  latest: messages[1],
  unread: true,
  hasAttachments: false,
  participants: [],
};

function encodeNativeMenuAddress(value: string) {
  const bytes = new TextEncoder().encode(value);
  let binary = "";
  bytes.forEach((byte) => (binary += String.fromCharCode(byte)));
  return btoa(binary)
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "");
}

describe("ReaderWindowApp", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.nativeMenuHandlers.length = 0;
    mocks.readReaderSeed.mockReturnValue({
      target: {
        accountId: "account-1",
        threadId: "thread-1",
        localMessageId: "message-2",
      },
      focusedMessageId: "message-1",
    });
    mocks.api.accounts.mockResolvedValue([
      { id: "account-1", email: "alex@example.com" },
    ]);
    mocks.api.conversationForTarget.mockResolvedValue(thread);
    mocks.api.aiAvailable.mockResolvedValue(false);
    mocks.api.action.mockResolvedValue(undefined);
    mocks.api.setRead.mockResolvedValue(undefined);
    mocks.api.showEmailAddressContextMenu.mockResolvedValue(undefined);
    mocks.api.unsubscribe.mockResolvedValue({
      kind: "completed",
      cleanupTarget: {
        accountId: "account-1",
        senderName: "Mara",
        senderAddress: "mara@example.com",
      },
    });
    mocks.api.trashMessagesFromSender.mockResolvedValue({
      matched: 1,
      moved: 1,
      failed: 0,
    });
    mocks.onReaderTarget.mockResolvedValue(() => undefined);
    mocks.notifyReaderWindowMutated.mockResolvedValue(undefined);
  });

  it("loads the whole target conversation while passing the focused message to Reader", async () => {
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    expect(await screen.findByTestId("focused-message")).toHaveTextContent(
      "message-1",
    );
    expect(screen.getByTestId("conversation-count")).toHaveTextContent("2");
    expect(mocks.api.conversationForTarget).toHaveBeenCalledWith({
      accountId: "account-1",
      threadId: "thread-1",
      localMessageId: "message-2",
    });
    await waitFor(() =>
      expect(mocks.api.setRead).toHaveBeenCalledWith("message-2", true),
    );
    expect(mocks.notifyReaderWindowMutated).toHaveBeenCalledWith({
      accountId: "account-1",
      threadId: "thread-1",
      messageIds: ["message-2"],
      mutation: "read",
    });
  });

  it("only closes after every mailbox action succeeds and refreshes main", async () => {
    const remainingThread = {
      ...thread,
      messages: [messages[0]],
      latest: messages[0],
    };
    mocks.api.conversationForTarget
      .mockResolvedValueOnce(thread)
      .mockResolvedValueOnce(remainingThread);
    mocks.api.action.mockRejectedValueOnce(new Error("offline"));
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    const archive = await screen.findByRole("button", { name: "Archive" });
    await waitFor(() => expect(mocks.api.setRead).toHaveBeenCalled());
    mocks.notifyReaderWindowMutated.mockClear();

    fireEvent.click(archive);
    await waitFor(() => expect(mocks.api.action).toHaveBeenCalledTimes(2));
    expect(mocks.closeReaderWindow).not.toHaveBeenCalled();
    expect(mocks.notifyReaderWindowMutated).toHaveBeenCalledWith({
      accountId: "account-1",
      threadId: "thread-1",
      messageIds: ["message-2"],
      mutation: "archive",
    });
    await waitFor(() =>
      expect(screen.getByTestId("conversation-count")).toHaveTextContent("1"),
    );

    mocks.api.action.mockResolvedValue(undefined);
    mocks.notifyReaderWindowMutated.mockClear();
    fireEvent.click(screen.getByRole("button", { name: "Archive" }));
    await waitFor(() => expect(mocks.closeReaderWindow).toHaveBeenCalledOnce());
    expect(mocks.api.action).toHaveBeenCalledTimes(3);
    expect(mocks.notifyReaderWindowMutated).toHaveBeenCalledWith({
      accountId: "account-1",
      threadId: "thread-1",
      messageIds: ["message-1"],
      mutation: "archive",
    });
  });

  it("offers sender cleanup, preserves mixed-sender mail, and refreshes main", async () => {
    const remainingThread = {
      ...thread,
      messages: [messages[1]],
      sourceMessages: [messages[1]],
      latest: messages[1],
      unread: true,
    };
    mocks.api.conversationForTarget
      .mockResolvedValueOnce(thread)
      .mockResolvedValueOnce(remainingThread);
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));
    expect(
      await screen.findByText("feedback.unsubscribeSuccess"),
    ).toBeVisible();
    fireEvent.click(
      screen.getByRole("button", {
        name: "feedback.unsubscribeCleanupAction",
      }),
    );

    await waitFor(() =>
      expect(mocks.api.trashMessagesFromSender).toHaveBeenCalledWith(
        "account-1",
        "mara@example.com",
      ),
    );
    await waitFor(() =>
      expect(screen.getByTestId("conversation-count")).toHaveTextContent("1"),
    );
    expect(mocks.notifyReaderWindowMutated).toHaveBeenLastCalledWith({
      accountId: "account-1",
      threadId: "thread-1",
      messageIds: ["message-1"],
      mutation: "trash",
    });
  });

  it("does not replace a new reader target when an old sender cleanup finishes", async () => {
    let readerTargetHandler: (seed: ReaderWindowSeed) => void = () => undefined;
    let resolveCleanup: (
      result: import("./api").TrashMessagesFromSenderResult,
    ) => void = () => undefined;
    const messageB: MailSummary = {
      ...messages[1],
      id: "message-b",
      account_id: "account-2",
      thread_id: "thread-b",
      subject: "Target B",
    };
    const threadB: MailThread = {
      ...thread,
      id: "account-2:thread-b",
      accountId: "account-2",
      threadId: "thread-b",
      messages: [messageB],
      latest: messageB,
    };
    const seedB: ReaderWindowSeed = {
      target: {
        accountId: "account-2",
        threadId: "thread-b",
        localMessageId: "message-b",
      },
      focusedMessageId: "message-b",
    };
    mocks.onReaderTarget.mockImplementationOnce(async (handler) => {
      readerTargetHandler = handler;
      return () => undefined;
    });
    mocks.api.accounts.mockResolvedValue([
      { id: "account-1", email: "alex@example.com" },
      { id: "account-2", email: "sam@example.com" },
    ]);
    mocks.api.conversationForTarget.mockImplementation(async (target) =>
      target.accountId === "account-2" ? threadB : thread,
    );
    mocks.api.trashMessagesFromSender.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveCleanup = resolve;
        }),
    );
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));
    fireEvent.click(
      await screen.findByRole("button", {
        name: "feedback.unsubscribeCleanupAction",
      }),
    );
    await waitFor(() =>
      expect(mocks.api.trashMessagesFromSender).toHaveBeenCalledOnce(),
    );

    act(() => readerTargetHandler(seedB));
    expect(await screen.findByTestId("focused-message")).toHaveTextContent(
      "message-b",
    );

    await act(async () => resolveCleanup({ matched: 1, moved: 1, failed: 0 }));
    await waitFor(() =>
      expect(mocks.api.conversationForTarget.mock.calls.length).toBeGreaterThan(
        2,
      ),
    );
    expect(screen.getByTestId("focused-message")).toHaveTextContent(
      "message-b",
    );
  });

  it("restores the reader snapshot when partial cleanup cannot reconcile", async () => {
    mocks.api.conversationForTarget
      .mockResolvedValueOnce(thread)
      .mockRejectedValueOnce(new Error("catalogue unavailable"));
    mocks.api.trashMessagesFromSender.mockResolvedValueOnce({
      matched: 2,
      moved: 1,
      failed: 1,
    });
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));
    fireEvent.click(
      await screen.findByRole("button", {
        name: "feedback.unsubscribeCleanupAction",
      }),
    );

    expect(
      await screen.findByText("feedback.unsubscribeCleanupPartial"),
    ).toBeVisible();
    expect(screen.getByTestId("conversation-count")).toHaveTextContent("2");
    expect(mocks.closeReaderWindow).not.toHaveBeenCalled();
  });

  it("closes an empty reader even when notifying the main window fails", async () => {
    const senderOnlyThread: MailThread = {
      ...thread,
      messages: [messages[0]],
      latest: messages[0],
      unread: false,
    };
    mocks.readReaderSeed.mockReturnValue({
      target: {
        accountId: "account-1",
        threadId: "thread-1",
        localMessageId: "message-1",
      },
      focusedMessageId: "message-1",
    });
    mocks.api.conversationForTarget
      .mockResolvedValueOnce(senderOnlyThread)
      .mockResolvedValueOnce(null);
    mocks.notifyReaderWindowMutated
      .mockRejectedValueOnce(new Error("main window unavailable"))
      .mockRejectedValueOnce(new Error("main window unavailable"));
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "Unsubscribe" }));
    fireEvent.click(
      await screen.findByRole("button", {
        name: "feedback.unsubscribeCleanupAction",
      }),
    );

    await waitFor(() => expect(mocks.closeReaderWindow).toHaveBeenCalledOnce());
    expect(mocks.api.trashMessagesFromSender).toHaveBeenCalledWith(
      "account-1",
      "mara@example.com",
    );
  });

  it("routes a native archive command to the visible reader conversation", async () => {
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await screen.findByTestId("focused-message");
    await waitFor(() =>
      expect(mocks.nativeMenuHandlers.length).toBeGreaterThan(1),
    );

    act(() => mocks.nativeMenuHandlers.at(-1)?.("archive"));

    await waitFor(() => expect(mocks.api.action).toHaveBeenCalledTimes(2));
    expect(mocks.closeReaderWindow).toHaveBeenCalledOnce();
  });

  it("routes address interactions through the dedicated reader window", async () => {
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await screen.findByTestId("focused-message");

    fireEvent.click(screen.getByRole("button", { name: "Compose to address" }));
    expect(mocks.openComposeWindow).toHaveBeenCalledWith({
      accountId: "account-1",
      to: "müller+news@example.com",
    });

    fireEvent.click(
      screen.getByRole("button", { name: "Address context menu" }),
    );
    expect(mocks.api.showEmailAddressContextMenu).toHaveBeenCalledWith(
      "account-1",
      "müller+news@example.com",
      "actions.copy",
      "reader.newMessageTo",
    );

    await waitFor(() =>
      expect(mocks.nativeMenuHandlers.length).toBeGreaterThan(1),
    );
    act(() =>
      mocks.nativeMenuHandlers.at(-1)?.(
        `compose-email-address:account-1:${encodeNativeMenuAddress("müller+news@example.com")}`,
      ),
    );
    expect(mocks.openComposeWindow).toHaveBeenLastCalledWith({
      accountId: "account-1",
      to: "müller+news@example.com",
    });

    act(() =>
      mocks.nativeMenuHandlers.at(-1)?.(
        `compose-email-address:wrong-account:${encodeNativeMenuAddress("attacker@example.com")}`,
      ),
    );
    expect(mocks.openComposeWindow).not.toHaveBeenCalledWith({
      accountId: "wrong-account",
      to: "attacker@example.com",
    });

    act(() => mocks.nativeMenuHandlers.at(-1)?.("copy-email-address-failed"));
    await waitFor(() =>
      expect(mocks.showNativeMessage).toHaveBeenCalledWith(
        "errors.generic",
        "errors.copyFailed",
        "error",
      ),
    );
  });

  it("opens a clean editable copy when sending a message again", async () => {
    mocks.api.content.mockResolvedValue({
      body_text: "Exact original body",
      body_html: "<p>Exact <strong>original</strong> body</p>",
      attachments: [
        {
          id: "attachment-1",
          message_id: "message-1",
          filename: "report.pdf",
          mime_type: "application/pdf",
          size_bytes: 42,
          is_inline: false,
          is_potentially_unsafe: false,
          presentation: "downloadable",
        },
      ],
    });
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await screen.findByTestId("focused-message");

    fireEvent.click(screen.getByRole("button", { name: "Send again" }));

    await waitFor(() =>
      expect(mocks.openComposeWindow).toHaveBeenCalledWith({
        accountId: "account-1",
        to: "alex@example.com",
        subject: "Project",
        body: "Exact original body",
        bodyHtml: "<p>Exact <strong>original</strong> body</p>",
        forwardMessageId: "message-1",
      }),
    );
  });

  it("removes only the permanently deleted message and keeps the conversation open", async () => {
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await screen.findByTestId("focused-message");

    fireEvent.click(screen.getByRole("button", { name: "Permanently delete" }));

    await waitFor(() =>
      expect(screen.getByTestId("conversation-count")).toHaveTextContent("1"),
    );
    expect(mocks.api.action).toHaveBeenCalledWith(
      "account-1",
      "INBOX",
      1,
      "delete",
    );
    expect(mocks.notifyReaderWindowMutated).toHaveBeenCalledWith({
      accountId: "account-1",
      threadId: "thread-1",
      messageIds: ["message-1"],
      mutation: "delete",
    });
    expect(mocks.closeReaderWindow).not.toHaveBeenCalled();
  });

  it("returns an unresolved reader target to the main Inbox", async () => {
    mocks.api.conversationForTarget.mockResolvedValueOnce(null);
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(mocks.notifyReaderWindowFailed).toHaveBeenCalledWith({
        accountId: "account-1",
      }),
    );
    expect(mocks.showNativeMessage).toHaveBeenCalled();
    expect(mocks.closeReaderWindow).toHaveBeenCalled();
  });

  it("does not let a slower prior target overwrite a retargeted window", async () => {
    let resolveFirst: (value: MailThread) => void = () => undefined;
    const first = new Promise<MailThread>((resolve) => {
      resolveFirst = resolve;
    });
    const retargetedMessage = {
      ...messages[1],
      id: "message-retargeted",
      thread_id: "thread-2",
      subject: "Retargeted",
    };
    const retargetedThread = {
      ...thread,
      id: "account-1:thread-2",
      threadId: "thread-2",
      messages: [retargetedMessage],
      latest: retargetedMessage,
    };
    mocks.api.conversationForTarget
      .mockImplementationOnce(() => first)
      .mockResolvedValueOnce(retargetedThread);
    const { ReaderWindowApp } = await import("./ReaderWindowApp");
    render(
      <MantineProvider>
        <ReaderWindowApp />
      </MantineProvider>,
    );
    await waitFor(() => expect(mocks.onReaderTarget).toHaveBeenCalled());
    const retarget = mocks.onReaderTarget.mock.calls[0][0];

    await act(async () => {
      retarget({
        target: { accountId: "account-1", threadId: "thread-2" },
        focusedMessageId: retargetedMessage.id,
      });
    });
    expect(await screen.findByTestId("focused-message")).toHaveTextContent(
      retargetedMessage.id,
    );

    await act(async () => resolveFirst(thread));
    expect(screen.getByTestId("focused-message")).toHaveTextContent(
      retargetedMessage.id,
    );
  });
});
